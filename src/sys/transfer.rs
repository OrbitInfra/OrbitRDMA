//! Payload WR chains and CQ events using the retained verbs function table.
use super::*;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataOpcode {
    Read,
    Write,
    Send,
    Receive,
}
impl DataOpcode {
    pub(crate) fn wc(self) -> u32 {
        match self {
            Self::Read => ffi::IBV_WC_RDMA_READ,
            Self::Write => ffi::IBV_WC_RDMA_WRITE,
            Self::Send => ffi::IBV_WC_SEND,
            Self::Receive => ffi::IBV_WC_RECV,
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

/// A native work request. Constructing a descriptor does not access memory.
pub struct WorkRequest {
    id: u64,
    opcode: Opcode,
    sge: ffi::ibv_sge,
    signaled: bool,
    fence: bool,
}
impl WorkRequest {
    /// Start with a signaled operation on one registered local byte range.
    pub fn new(id: u64, opcode: Opcode, address: u64, length: u32, lkey: u32) -> Self {
        Self {
            id,
            opcode,
            sge: ffi::ibv_sge {
                addr: address,
                length,
                lkey,
            },
            signaled: true,
            fence: false,
        }
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
    fn receipt<T>(code: i32, bad: *mut T, chain: &[T]) -> Self {
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
        if (w.opcode == Opcode::Receive) != receive
            || w.sge.length == 0
            || w.sge.addr.checked_add(u64::from(w.sge.length)).is_none()
        {
            return Err(Error::Invalid("work request queue or local range"));
        }
        if let Opcode::Read { address, .. } | Opcode::Write { address, .. } = w.opcode {
            if address.checked_add(u64::from(w.sge.length)).is_none() {
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
                wr.sg_list = &mut w.sge;
                wr.num_sge = 1;
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
                wr.sg_list = &mut w.sge;
                wr.num_sge = 1;
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

/// Poll one native CQE. An empty poll returns `Ok(None)`.
///
/// # Safety
///
/// `cq` must point to a live, compatible libibverbs CQ. The caller must retain
/// its context and provider and own dispatch for every QP sharing this CQ.
pub unsafe fn poll(cq: NonNull<c_void>) -> Result<Option<WorkCompletion>, Error> {
    let cq = cq.cast::<ffi::ibv_cq>().as_ptr();
    // SAFETY: the caller retains this CQ and its context throughout the call.
    let poll = unsafe { (*(*cq).context).ops.poll_cq }.ok_or(Error::Unsupported("ibv_poll_cq"))?;
    let mut wc = ffi::ibv_wc {
        status: ffi::IBV_WC_GENERAL_ERR,
        opcode: ffi::IBV_WC_LOCAL_INV,
        ..Default::default()
    };
    // SAFETY: the provider has space for exactly one WC initialized to failure.
    match unsafe { poll(cq, 1, &mut wc) } {
        0 => Ok(None),
        1 if wc.status != ffi::IBV_WC_SUCCESS => Ok(Some(WorkCompletion::Error {
            id: wc.wr_id,
            qpn: wc.qp_num,
            status: wc.status,
            vendor: wc.vendor_err,
        })),
        1 => {
            let invalidated = if wc.wc_flags & ffi::IBV_WC_WITH_INV != 0 {
                // SAFETY: the flags on this successful CQE select the host-order key in the initialized union.
                Some(unsafe { wc.__bindgen_anon_1.invalidated_rkey })
            } else {
                None
            };
            Ok(Some(WorkCompletion::Success {
                id: wc.wr_id,
                qpn: wc.qp_num,
                opcode: wc.opcode,
                bytes: wc.byte_len,
                flags: wc.wc_flags,
                invalidated,
            }))
        }
        count => Err(Error::Native {
            operation: "ibv_poll_cq",
            code: count,
        }),
    }
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

    pub(crate) fn arm(&self, cq: Cq) -> RdmaResult<()> {
        // SAFETY: the CQ owner serializes its one-shot arm and polling.
        let notify = unsafe { (*(*cq.0.as_ptr()).context).ops.req_notify_cq }
            .ok_or(Error::Unsupported("ibv_req_notify_cq"))?;
        // SAFETY: the CQ remains live and zero requests notification for any completion.
        check("ibv_req_notify_cq", unsafe { notify(cq.0.as_ptr(), 0) })
    }

    pub(crate) fn wait_event(&self, ch: Channel, cq: Cq, deadline: Instant) -> RdmaResult<bool> {
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
            let (mut got, mut context) = (std::ptr::null_mut(), std::ptr::null_mut());
            // SAFETY: both outputs are writable, and no other poller consumes this channel.
            if unsafe { (self.ibv_get_cq_event)(ch.0.as_ptr(), &mut got, &mut context) } != 0 {
                match io::Error::last_os_error().kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => continue,
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
