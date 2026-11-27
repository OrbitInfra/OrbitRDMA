//! Payload WR chains and CQ events using the retained verbs function table.
use super::*;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataOpcode {
    Read,
    Write,
    Send,
    Receive,
    FetchAdd,
    CompareSwap,
}
impl DataOpcode {
    pub(crate) fn wc(self) -> u32 {
        match self {
            Self::Read => ffi::IBV_WC_RDMA_READ,
            Self::Write => ffi::IBV_WC_RDMA_WRITE,
            Self::Send => ffi::IBV_WC_SEND,
            Self::Receive => ffi::IBV_WC_RECV,
            Self::FetchAdd => ffi::IBV_WC_FETCH_ADD,
            Self::CompareSwap => ffi::IBV_WC_COMP_SWAP,
        }
    }
}
/// A single-SGE operation on a reliable connected queue pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opcode {
    Read {
        address: u64,
        rkey: u32,
    },
    Write {
        address: u64,
        rkey: u32,
    },
    Send,
    /// Send a message and invalidate the specified remote key.
    SendWithInvalidate {
        rkey: u32,
    },
    Receive,
}

/// One contiguous local segment of a scatter/gather list.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Segment(ffi::ibv_sge);
impl Segment {
    pub fn new(address: u64, length: u32, lkey: u32) -> Self {
        Self(ffi::ibv_sge {
            addr: address,
            length,
            lkey,
        })
    }
}

enum Segments {
    One(Segment),
    Many(Vec<Segment>),
}
impl Segments {
    fn slice(&self) -> &[Segment] {
        match self {
            Self::One(x) => std::slice::from_ref(x),
            Self::Many(xs) => xs,
        }
    }
    fn native(&mut self) -> (*mut ffi::ibv_sge, i32) {
        let xs = match self {
            Self::One(x) => std::slice::from_mut(x),
            Self::Many(xs) => xs.as_mut_slice(),
        };
        (xs.as_mut_ptr().cast(), xs.len() as i32)
    }
}

/// Standard 64-bit RC atomic operands. Completion returns the previous remote value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomicOperation {
    FetchAdd(u64),
    CompareSwap { compare: u64, swap: u64 },
}

/// A native work request. Constructing a descriptor does not access memory.
pub struct WorkRequest {
    id: u64,
    opcode: Opcode,
    segments: Segments,
    immediate: Option<u32>,
    atomic: Option<AtomicOperation>,
    datagram: Option<(shared::Ah, u32, u32)>,
    signaled: bool,
    fence: bool,
}
impl WorkRequest {
    /// Start with a signaled operation on one registered local byte range.
    pub fn new(id: u64, opcode: Opcode, address: u64, length: u32, lkey: u32) -> Self {
        Self {
            id,
            opcode,
            segments: Segments::One(Segment::new(address, length, lkey)),
            immediate: None,
            atomic: None,
            datagram: None,
            signaled: true,
            fence: false,
        }
    }
    /// A single WR with multiple local SGEs. The provider must support the selected list length.
    pub fn scatter(id: u64, opcode: Opcode, segments: Vec<Segment>) -> Result<Self, Error> {
        if segments.is_empty() || segments.len() > i32::MAX as usize {
            return Err(Error::Invalid("scatter/gather length"));
        }
        Ok(Self {
            id,
            opcode,
            segments: Segments::Many(segments),
            immediate: None,
            atomic: None,
            datagram: None,
            signaled: true,
            fence: false,
        })
    }
    /// Attach a host-order immediate value to a SEND or RDMA WRITE.
    pub fn with_immediate(mut self, immediate: u32) -> Result<Self, Error> {
        if self.atomic.is_some() || !matches!(self.opcode, Opcode::Send | Opcode::Write { .. }) {
            return Err(Error::Invalid("immediate requires SEND or WRITE"));
        }
        self.immediate = Some(immediate);
        Ok(self)
    }
    /// A signaled atomic with one writable eight-byte result SGE.
    pub fn atomic(
        id: u64,
        result: Segment,
        address: u64,
        rkey: u32,
        op: AtomicOperation,
    ) -> Result<Self, Error> {
        if address % 8 != 0 || result.0.addr % 8 != 0 || result.0.length != 8 {
            return Err(Error::Invalid("atomic alignment or width"));
        }
        let mut wr = Self::new(
            id,
            Opcode::Write { address, rkey },
            result.0.addr,
            8,
            result.0.lkey,
        );
        wr.atomic = Some(op);
        Ok(wr)
    }
    pub(crate) fn datagram(&mut self, route: &shared::DatagramRoute) -> RdmaResult<()> {
        self.datagram = Some((route.ah.ok_or(Error::Closed)?, route.qpn, route.qkey));
        Ok(())
    }
    /// Request a send CQE for this work request. Receives always produce a CQE.
    pub fn signaled(mut self, enabled: bool) -> Self {
        self.signaled = enabled;
        self
    }
    /// Order this send-queue request after prior outstanding RDMA READs.
    pub fn fenced(mut self, enabled: bool) -> Self {
        self.fence = enabled;
        self
    }
}
pub(crate) fn local_key(mr: Mr) -> u32 {
    // SAFETY: the in-flight loan holds this MR and its immutable local key.
    unsafe { (*mr.0.as_ptr()).lkey }
}

/// The synchronous result of a single native post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PostReceipt {
    /// Exact accepted prefix, or `None` when the entire chain may be in flight.
    pub accepted: Option<usize>,
    /// Provider status, or EPROTO for a contradictory success receipt.
    pub code: i32,
}
impl PostReceipt {
    /// Conservative ownership after an engine adapter supplies a receipt. A
    /// contradictory count retains the entire reservation, like an unknown bad WR.
    pub fn possible_prefix(self, submitted: usize) -> (usize, bool) {
        match self.accepted {
            Some(n) if n <= submitted && (self.code != 0 || n == submitted) => (n, false),
            _ => (submitted, true),
        }
    }
    pub(super) fn receipt<T>(code: i32, bad: *mut T, chain: &[T]) -> Self {
        if code == 0 {
            return if bad.is_null() {
                Self {
                    accepted: Some(chain.len()),
                    code,
                }
            } else {
                Self {
                    accepted: None,
                    code: libc::EPROTO,
                }
            };
        }
        // A provider may return a foreign pointer. Never dereference or subtract it.
        Self {
            accepted: chain.iter().position(|wr| std::ptr::eq(wr, bad)),
            code,
        }
    }
}

/// A successful completion or the four fields verbs defines on a failed CQE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkCompletion {
    Success {
        id: u64,
        qpn: u32,
        opcode: u32,
        bytes: u32,
        flags: u32,
        invalidated: Option<u32>,
    },
    Error {
        id: u64,
        qpn: u32,
        status: u32,
        vendor: u32,
    },
}
pub(crate) fn storage<T>(capacity: usize) -> RdmaResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(capacity)
        .map_err(|_| Error::Exhausted("work request storage"))?;
    Ok(v)
}

// Single messages and receives need no heap allocation.
fn chain<T: Default>(
    count: usize,
    submit: impl FnOnce(&mut [T]) -> PostReceipt,
) -> RdmaResult<PostReceipt> {
    if count == 1 {
        return Ok(submit(std::slice::from_mut(&mut T::default())));
    }
    let mut wrs = storage(count)?;
    wrs.resize_with(count, T::default);
    Ok(submit(&mut wrs))
}

/// Post one SQ or RQ chain. Local validation and allocation errors precede any
/// provider call. After a call, even a provider error returns a [`PostReceipt`].
///
/// # Safety
///
/// `qp` must point to a live, compatible libibverbs RC QP and its provider must
/// remain loaded. All SGEs must refer to registered, permitted memory in its PD.
/// Keep the QP, registrations, and backing of every possibly accepted request
/// live and exclude conflicting access through completion or proven QP drain.
/// The caller owns peer authorization, posting order and completion accounting.
pub unsafe fn post(qp: NonNull<c_void>, entries: &mut [WorkRequest]) -> Result<PostReceipt, Error> {
    let receive = entries
        .first()
        .ok_or(Error::Invalid("empty work request chain"))?
        .opcode
        == Opcode::Receive;
    for w in entries.iter() {
        if (w.opcode == Opcode::Receive) != receive {
            return Err(Error::Invalid("mixed SQ and RQ list"));
        }
        let mut length = 0u32;
        for &Segment(s) in w.segments.slice() {
            if s.length == 0 || s.addr.checked_add(u64::from(s.length)).is_none() {
                return Err(Error::Invalid("work request local range"));
            }
            length = length
                .checked_add(s.length)
                .ok_or(Error::Invalid("work request length"))?;
        }
        if let Opcode::Read { address, .. } | Opcode::Write { address, .. } = w.opcode {
            if address.checked_add(u64::from(length)).is_none() {
                return Err(Error::Invalid("remote work request range"));
            }
        }
    }
    let qp = qp.cast::<ffi::ibv_qp>().as_ptr();
    // SAFETY: the caller retains this QP and provider context throughout the post.
    let ops = unsafe { &(*(*qp).context).ops };
    if receive {
        let submit = ops.post_recv.ok_or(Error::Unsupported("ibv_post_recv"))?;
        chain(entries.len(), |wrs: &mut [ffi::ibv_recv_wr]| {
            let mut next = std::ptr::null_mut();
            for (wr, w) in wrs.iter_mut().zip(entries).rev() {
                wr.wr_id = w.id;
                (wr.sg_list, wr.num_sge) = w.segments.native();
                wr.next = next;
                next = wr;
            }
            let mut bad = std::ptr::null_mut();
            // SAFETY: the WR/SGE chain is stationary through this synchronous provider call.
            let code = unsafe { submit(qp, next, &mut bad) };
            PostReceipt::receipt(code, bad, wrs)
        })
    } else {
        let submit = ops.post_send.ok_or(Error::Unsupported("ibv_post_send"))?;
        chain(entries.len(), |wrs: &mut [ffi::ibv_send_wr]| {
            let mut next = std::ptr::null_mut();
            for (wr, w) in wrs.iter_mut().zip(entries).rev() {
                wr.wr_id = w.id;
                (wr.sg_list, wr.num_sge) = w.segments.native();
                wr.opcode = match w.opcode {
                    Opcode::Read { address, rkey } | Opcode::Write { address, rkey } => {
                        wr.wr.rdma.remote_addr = address;
                        wr.wr.rdma.rkey = rkey;
                        if matches!(w.opcode, Opcode::Read { .. }) {
                            ffi::IBV_WR_RDMA_READ
                        } else {
                            ffi::IBV_WR_RDMA_WRITE
                        }
                    }
                    Opcode::SendWithInvalidate { rkey } => {
                        wr.__bindgen_anon_1.invalidate_rkey = rkey;
                        ffi::IBV_WR_SEND_WITH_INV
                    }
                    Opcode::Send => ffi::IBV_WR_SEND,
                    Opcode::Receive => {
                        return PostReceipt {
                            accepted: Some(0),
                            code: libc::EINVAL,
                        };
                    }
                };
                if let (Some(op), Opcode::Write { address, rkey }) = (w.atomic, w.opcode) {
                    wr.wr.atomic.remote_addr = address;
                    wr.wr.atomic.rkey = rkey;
                    let (opcode, compare_add, swap) = match op {
                        AtomicOperation::FetchAdd(add) => {
                            (ffi::IBV_WR_ATOMIC_FETCH_AND_ADD, add, 0)
                        }
                        AtomicOperation::CompareSwap { compare, swap } => {
                            (ffi::IBV_WR_ATOMIC_CMP_AND_SWP, compare, swap)
                        }
                    };
                    wr.opcode = opcode;
                    wr.wr.atomic.compare_add = compare_add;
                    wr.wr.atomic.swap = swap;
                }
                if let Some(value) = w.immediate {
                    wr.__bindgen_anon_1.imm_data = value.to_be();
                    wr.opcode = if w.opcode == Opcode::Send {
                        ffi::IBV_WR_SEND_WITH_IMM
                    } else {
                        ffi::IBV_WR_RDMA_WRITE_WITH_IMM
                    };
                }
                if let Some((ah, qpn, qkey)) = w.datagram {
                    wr.wr.ud.ah = ah.0.as_ptr();
                    wr.wr.ud.remote_qpn = qpn;
                    wr.wr.ud.remote_qkey = qkey;
                }
                if w.signaled {
                    wr.send_flags |= ffi::IBV_SEND_SIGNALED;
                }
                if w.fence {
                    wr.send_flags |= ffi::IBV_SEND_FENCE;
                }
                wr.next = next;
                next = wr;
            }
            let mut bad = std::ptr::null_mut();
            // SAFETY: WRs and SGEs are live during post. The caller retains all DMA targets.
            let code = unsafe { submit(qp, next, &mut bad) };
            PostReceipt::receipt(code, bad, wrs)
        })
    }
}

fn empty_wc() -> ffi::ibv_wc {
    ffi::ibv_wc {
        status: ffi::IBV_WC_GENERAL_ERR,
        opcode: ffi::IBV_WC_LOCAL_INV,
        ..Default::default()
    }
}

fn decode(wc: &ffi::ibv_wc) -> WorkCompletion {
    if wc.status != ffi::IBV_WC_SUCCESS {
        return WorkCompletion::Error {
            id: wc.wr_id,
            qpn: wc.qp_num,
            status: wc.status,
            vendor: wc.vendor_err,
        };
    }
    let invalidated =
        if wc.wc_flags & (ffi::IBV_WC_WITH_INV | ffi::IBV_WC_WITH_IMM) == ffi::IBV_WC_WITH_INV {
            // SAFETY: the flags on this successful CQE select the host-order key in the initialized union.
            Some(unsafe { wc.__bindgen_anon_1.invalidated_rkey })
        } else {
            None
        };
    WorkCompletion::Success {
        id: wc.wr_id,
        qpn: wc.qp_num,
        opcode: wc.opcode,
        bytes: wc.byte_len,
        flags: wc.wc_flags,
        invalidated,
    }
}

// SAFETY contract inherited by both public polling entry points.
unsafe fn poll_into(cq: NonNull<c_void>, entries: &mut [ffi::ibv_wc]) -> Result<usize, Error> {
    let cq = cq.cast::<ffi::ibv_cq>().as_ptr();
    // SAFETY: the caller retains this CQ and its context throughout the call.
    let poll = unsafe { (*(*cq).context).ops.poll_cq }.ok_or(Error::Unsupported("ibv_poll_cq"))?;
    // SAFETY: both callers provide 1..=32 writable entries and own CQ dispatch.
    let count = unsafe { poll(cq, entries.len() as i32, entries.as_mut_ptr()) };
    if count < 0 || count as usize > entries.len() {
        return Err(Error::Native {
            operation: "ibv_poll_cq",
            code: count,
        });
    }
    Ok(count as usize)
}

/// A CQE and the host-order immediate value from a successful receive, when present.
#[derive(Clone, Copy, Debug)]
pub struct CompletionEntry {
    /// Source QPN field. Verbs defines this field only for a successful UD receive.
    pub source_qp: Option<u32>,
    pub work: WorkCompletion,
    pub immediate: Option<u32>,
}
fn decode_entry(wc: &ffi::ibv_wc, datagram: bool) -> CompletionEntry {
    let immediate = if wc.status == ffi::IBV_WC_SUCCESS
        && wc.wc_flags & ffi::IBV_WC_WITH_IMM != 0
        && wc.wc_flags & ffi::IBV_WC_WITH_INV == 0
        && matches!(wc.opcode, ffi::IBV_WC_RECV | ffi::IBV_WC_RECV_RDMA_WITH_IMM)
    {
        // SAFETY: successful receive with WITH_IMM selects the network-order immediate union member.
        Some(u32::from_be(unsafe { wc.__bindgen_anon_1.imm_data }))
    } else {
        None
    };
    CompletionEntry {
        source_qp: if datagram && wc.status == ffi::IBV_WC_SUCCESS && wc.opcode == ffi::IBV_WC_RECV
        {
            Some(wc.src_qp)
        } else {
            None
        },
        work: decode(wc),
        immediate,
    }
}

/// Poll one native CQE. An empty poll returns `Ok(None)`.
///
/// # Safety
///
/// `cq` must point to a live, compatible libibverbs CQ. The caller must retain
/// its context and provider and own dispatch for every QP sharing this CQ.
pub unsafe fn poll(cq: NonNull<c_void>) -> Result<Option<WorkCompletion>, Error> {
    // SAFETY: the caller supplies the live CQ and owns its dispatch.
    Ok(unsafe { poll_entry(cq) }?.map(|e| e.work))
}

/// Poll one entry, including the immediate value on receive.
///
/// # Safety
///
/// The lifetime and dispatch requirements are identical to [`poll`].
pub unsafe fn poll_entry(cq: NonNull<c_void>) -> Result<Option<CompletionEntry>, Error> {
    // SAFETY: the caller retains the CQ. Generic polling reads only common RC/UD fields.
    unsafe { poll_entry_for(cq, false) }
}
unsafe fn poll_entry_for(
    cq: NonNull<c_void>,
    datagram: bool,
) -> Result<Option<CompletionEntry>, Error> {
    let mut wc = empty_wc();
    // SAFETY: the caller retains the CQ; this slice holds one writable entry.
    let count = unsafe { poll_into(cq, std::slice::from_mut(&mut wc)) }?;
    Ok((count == 1).then(|| decode_entry(&wc, datagram)))
}

/// Reusable storage for one provider poll. Iteration decodes only returned CQEs,
/// in provider order. Failed completions expose only the fields defined by verbs.
pub struct CompletionBatch {
    entries: [ffi::ibv_wc; Self::CAPACITY],
    len: usize,
}
impl Default for CompletionBatch {
    fn default() -> Self {
        Self {
            entries: std::array::from_fn(|_| empty_wc()),
            len: 0,
        }
    }
}
impl CompletionBatch {
    pub const CAPACITY: usize = 32;

    pub fn entries(&self) -> impl ExactSizeIterator<Item = CompletionEntry> + '_ {
        self.entries_for(false)
    }
    pub(crate) fn entries_for(
        &self,
        datagram: bool,
    ) -> impl ExactSizeIterator<Item = CompletionEntry> + '_ {
        self.entries[..self.len]
            .iter()
            .map(move |wc| decode_entry(wc, datagram))
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = WorkCompletion> + '_ {
        self.entries[..self.len].iter().map(decode)
    }
}

/// Read up to `limit` CQEs in one provider call. A failed or empty poll leaves
/// the batch empty. The caller must dispatch the entire returned prefix.
///
/// # Safety
///
/// The CQ, its context and provider must remain live. The caller must own
/// dispatch for all QPs sharing the CQ, as for [`poll`].
pub unsafe fn poll_batch(
    cq: NonNull<c_void>,
    batch: &mut CompletionBatch,
    limit: usize,
) -> Result<usize, Error> {
    batch.len = 0;
    let entries = batch
        .entries
        .get_mut(..limit)
        .filter(|entries| !entries.is_empty())
        .ok_or(Error::Invalid("CQ poll limit"))?;
    // SAFETY: the caller retains the CQ and the checked slice is writable.
    batch.len = unsafe { poll_into(cq, entries) }?;
    Ok(batch.len)
}

impl Native {
    pub(crate) fn post_payload(
        &self,
        qp: Qp,
        entries: &mut [WorkRequest],
    ) -> RdmaResult<PostReceipt> {
        // SAFETY: QueueData serializes the post and retains physical loans for every possible WR.
        unsafe { post(qp.0.cast(), entries) }
    }
    pub(crate) fn poll_one(&self, cq: Cq) -> RdmaResult<Option<WorkCompletion>> {
        // SAFETY: the owning queue holds its context and is the sole consumer of this CQ.
        unsafe { poll(cq.0.cast()) }
    }

    pub(crate) fn poll_entry(&self, cq: Cq, datagram: bool) -> RdmaResult<Option<CompletionEntry>> {
        // SAFETY: the owning queue retains the CQ and is its sole consumer.
        unsafe { poll_entry_for(cq.0.cast(), datagram) }
    }
    pub(crate) fn poll_batch(
        &self,
        cq: Cq,
        batch: &mut CompletionBatch,
        limit: usize,
    ) -> RdmaResult<usize> {
        // SAFETY: the owning queue retains the CQ and dispatches every returned CQE.
        unsafe { poll_batch(cq.0.cast(), batch, limit) }
    }

    pub(crate) fn arm(&self, cq: Cq) -> RdmaResult<()> {
        // SAFETY: the CQ owner serializes its one-shot arm and polling.
        let notify = unsafe { (*(*cq.0.as_ptr()).context).ops.req_notify_cq }
            .ok_or(Error::Unsupported("ibv_req_notify_cq"))?;
        // SAFETY: the CQ remains live and zero requests notification for any completion.
        check("ibv_req_notify_cq", unsafe { notify(cq.0.as_ptr(), 0) })
    }

    fn nonblocking_fd(ch: Channel) -> RdmaResult<std::os::fd::RawFd> {
        // SAFETY: this dedicated channel retains the fd for the duration of the wait.
        let fd = unsafe { (*ch.0.as_ptr()).fd };
        if fd < 0 {
            return Err(Error::Invalid("completion channel fd"));
        }
        // A readable fd may race with a spurious wakeup. The event read must honor the deadline too.
        // SAFETY: both fcntl commands target the live privately owned channel fd.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(last_error("completion channel flags"));
        }
        if flags & libc::O_NONBLOCK == 0 {
            // SAFETY: preserve all existing flags on the private fd.
            if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(last_error("nonblocking completion channel"));
            }
        }
        Ok(fd)
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn notification_fd(ch: Channel) -> RdmaResult<std::os::fd::OwnedFd> {
        let fd = Self::nonblocking_fd(ch)?;
        // SAFETY: QueueData retains the private channel while we duplicate its fd.
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        borrowed.try_clone_to_owned().map_err(|err| Error::Native {
            operation: "duplicate completion channel fd",
            code: err.raw_os_error().unwrap_or(libc::EIO),
        })
    }

    pub(crate) fn wait_event(&self, ch: Channel, cq: Cq, deadline: Instant) -> RdmaResult<bool> {
        let fd = Self::nonblocking_fd(ch)?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            let ms = remaining
                .as_nanos()
                .div_ceil(1_000_000)
                .min(i32::MAX as u128) as i32;
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives exactly one writable pollfd with the retained channel fd.
            let n = unsafe { libc::poll(&mut pfd, 1, ms) };
            if n < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(last_error("poll completion channel"));
            }
            if n == 0 {
                continue;
            }
            if pfd.revents & libc::POLLIN == 0 {
                return Err(Error::Native {
                    operation: "completion channel",
                    code: libc::EIO,
                });
            }
            if self.consume_event(ch, cq)? {
                return Ok(true);
            }
        }
    }

    /// The caller has set O_NONBLOCK. Another readiness edge may arrive after EAGAIN.
    pub(crate) fn consume_event(&self, ch: Channel, cq: Cq) -> RdmaResult<bool> {
        loop {
            let (mut got, mut context) = (std::ptr::null_mut(), std::ptr::null_mut());
            // SAFETY: both outputs are writable, and no other poller consumes this channel.
            if unsafe { (self.ibv_get_cq_event)(ch.0.as_ptr(), &mut got, &mut context) } != 0 {
                match io::Error::last_os_error().kind() {
                    io::ErrorKind::WouldBlock => return Ok(false),
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(last_error("ibv_get_cq_event")),
                }
            }
            if got.is_null() {
                return Err(Error::Invalid("null completion event CQ"));
            }
            // SAFETY: a successful get returned a live provider CQ; acknowledge exactly that event.
            unsafe { (self.ibv_ack_cq_events)(got, 1) };
            if got != cq.0.as_ptr() {
                return Err(Error::Invalid("foreign completion event CQ"));
            }
            return Ok(true);
        }
    }
}
fn last_error(operation: &'static str) -> Error {
    Error::Native {
        operation,
        code: io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO),
    }
}
