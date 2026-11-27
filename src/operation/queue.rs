//! Per-QP accounting adapted from OrbiTE's send/receive queues and slice retirement.
use super::*;
use crate::memory::Loan;
use crate::queue::QueueData;
use crate::resource::Result;
use crate::verbs::{Opcode, WorkRequest};
use crate::{ConnectionParams, QueueConfig, RemoteAccess};
use std::sync::atomic::{AtomicU64, Ordering};

mod atomic;
mod datagram;
pub use datagram::{DatagramAddress, DatagramQueue};
mod srq;
mod vectored;
pub use srq::SharedReceiveQueue;

struct Pending {
    id: u64,
    group: u64,
    opcode: sys::DataOpcode,
    signaled: bool,
    length: u32,
    affects_peer: bool,
    state: Arc<Mutex<State>>,
    loan: Loan,
    extra: Vec<Loan>,
}
impl Pending {
    fn finish(
        self,
        outcome: Outcome,
        immediate: Option<Immediate>,
        datagram: Option<DatagramReceipt>,
    ) {
        let mut state = lock(&self.state);
        drop((self.loan, self.extra));
        if state.outcome.is_none() {
            state.immediate = immediate;
            state.datagram = datagram;
            state.outcome = Some(outcome);
        }
        state.retired = true;
        state.quarantine = None;
    }
    fn failed(self, reason: TransferFailure) {
        let remote_effect_unknown = self.affects_peer;
        self.finish(
            Outcome::Failed {
                reason,
                remote_effect_unknown,
            },
            None,
            None,
        );
    }
}
pub(crate) struct PayloadQueue {
    pending: Vec<Pending>,
    pub(crate) datagram: bool,
    receive_count: usize,
    reusable_state: Arc<Mutex<State>>,
    config: QueueConfig,
    used: bool,
    pub(crate) fenced: bool,
    pub(super) armed: bool,
    last_read: bool,
}
impl PayloadQueue {
    pub(crate) fn new(config: QueueConfig) -> Self {
        Self {
            pending: Vec::new(),
            datagram: false,
            receive_count: 0,
            reusable_state: Arc::default(),
            config,
            used: false,
            fenced: false,
            armed: false,
            last_read: false,
        }
    }
    fn next_state(&mut self) -> Arc<Mutex<State>> {
        // Only the cache owns this slot after both the native WR and the last
        // observer have retired. A retained receipt must always keep its identity.
        if let Some(state) = Arc::get_mut(&mut self.reusable_state) {
            *state.get_mut().unwrap_or_else(|p| p.into_inner()) = State::default();
            self.reusable_state.clone()
        } else {
            Arc::default()
        }
    }
    pub(crate) fn used(&self) -> bool {
        self.used
    }
    pub(super) fn contains(&self, id: u64) -> bool {
        self.pending.iter().any(|p| p.id == id)
    }
    pub(crate) fn quarantine(&mut self, fault: Fault) {
        self.fenced = true;
        for p in &self.pending {
            let mut s = lock(&p.state);
            s.fail(TransferFailure::QueueClosed, p.affects_peer);
            s.quarantine = Some(fault);
        }
    }
    pub(crate) fn drained(&mut self) {
        self.fenced = true;
        self.receive_count = 0;
        for p in self.pending.drain(..) {
            p.failed(TransferFailure::QueueClosed);
        }
    }
    fn capacity(&self, count: usize, receive: bool) -> Result<()> {
        if self.fenced {
            return Err(Error::Closed);
        }
        let used = if receive {
            self.receive_count
        } else {
            self.pending.len() - self.receive_count
        };
        let depth = if receive {
            self.config.receive_depth
        } else {
            self.config.send_depth
        };
        if count > (depth as usize).saturating_sub(used) {
            return Err(Error::Exhausted("QP work requests"));
        }
        Ok(())
    }
    fn take(&mut self, index: usize) -> Pending {
        let p = self.pending.swap_remove(index);
        if p.opcode == sys::DataOpcode::Receive {
            self.receive_count -= 1;
        }
        p
    }
    pub(crate) fn completion(
        &mut self,
        qpn: u32,
        entry: crate::verbs::CompletionEntry,
    ) -> Result<()> {
        let wc = entry.work;
        use sys::WorkCompletion::*;
        let (id, actual_qpn) = match wc {
            Success { id, qpn, .. } | Error { id, qpn, .. } => (id, qpn),
        };
        if actual_qpn != qpn {
            return Err(crate::Error::Invalid("foreign completion QP"));
        }
        let index = self
            .pending
            .iter()
            .position(|p| p.id == id)
            .ok_or(crate::Error::Invalid("stale or duplicate WR completion"))?;
        match wc {
            Error { status, vendor, .. } => {
                self.fenced = true;
                self.take(index).failed(TransferFailure::Completion {
                    status,
                    vendor_error: vendor,
                });
            }
            Success {
                opcode,
                bytes,
                invalidated,
                flags,
                ..
            } => {
                let tail = &self.pending[index];
                let received = tail.opcode == sys::DataOpcode::Receive;
                let write_notice = received && opcode == sys::ffi::IBV_WC_RECV_RDMA_WITH_IMM;
                if !tail.signaled
                    || (self.datagram
                        && received
                        && (bytes < 40 || write_notice || entry.source_qp.is_none()))
                    || (!write_notice && opcode != tail.opcode.wc())
                    || (received
                        && flags & sys::ffi::IBV_WC_WITH_IMM != 0
                        && entry.immediate.is_none())
                    || (write_notice && entry.immediate.is_none())
                    || (tail.opcode == sys::DataOpcode::Read && bytes != tail.length)
                    || (received && !write_notice && bytes > tail.length)
                {
                    return Err(crate::Error::Invalid(
                        "completion opcode, signaling or byte count",
                    ));
                }
                let group = tail.group;
                let singleton = tail.id == group;
                let tail = self.take(index);
                let immediate = entry.immediate.map(|value| {
                    if write_notice {
                        Immediate::RemoteWrite {
                            value,
                            written: bytes,
                        }
                    } else {
                        Immediate::Message(value)
                    }
                });
                let datagram =
                    entry
                        .source_qp
                        .filter(|_| self.datagram && received)
                        .map(|source_qp| DatagramReceipt {
                            source_qp,
                            has_grh: flags & sys::ffi::IBV_WC_GRH != 0,
                            payload_bytes: bytes - 40,
                        });
                let tail_length = tail.length;
                tail.finish(
                    Outcome::Success {
                        bytes: if write_notice {
                            0
                        } else if received {
                            bytes
                        } else {
                            tail_length
                        },
                        invalidated_rkey: invalidated.filter(|_| received),
                    },
                    immediate,
                    datagram,
                );
                if !singleton {
                    let mut i = 0;
                    while i < self.pending.len() {
                        if self.pending[i].group == group {
                            let p = self.take(i);
                            let bytes = p.length;
                            p.finish(
                                Outcome::Success {
                                    bytes,
                                    invalidated_rkey: None,
                                },
                                None,
                                None,
                            );
                        } else {
                            i += 1;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl Request {
    fn native_opcode(
        &self,
        peer: &ConnectionParams,
        local: &ConnectionParams,
        len: u32,
    ) -> Result<Opcode> {
        let opcode = match self.transfer {
            Transfer::Read { remote, offset } | Transfer::Write { remote, offset } => {
                if remote.qpn != peer.qpn || remote.rkey == 0 {
                    return Err(Error::Invalid("remote window or peer QP"));
                }
                let direction = if matches!(self.transfer, Transfer::Read { .. }) {
                    if local.initiator_depth.min(peer.responder_depth) == 0 {
                        return Err(Error::Unsupported("RDMA read depth"));
                    }
                    RemoteAccess::Read
                } else {
                    RemoteAccess::Write
                };
                let address = remote.checked_address(offset, len.into(), direction)?;
                let rkey = remote.rkey;
                match direction {
                    RemoteAccess::Read => Opcode::Read { address, rkey },
                    RemoteAccess::Write => Opcode::Write { address, rkey },
                }
            }
            Transfer::Send => Opcode::Send,
            Transfer::Receive => Opcode::Receive,
        };
        Ok(opcode)
    }
}

// IDs 1 and 2 are reserved for the private window protocol. Never recycle a payload ID,
// even across QP incarnations with identical native numbers.
static NEXT_ID: AtomicU64 = AtomicU64::new(3);

pub(crate) fn reserve_ids(count: usize) -> Result<u64> {
    let mut first = NEXT_ID.load(Ordering::Relaxed);
    loop {
        let next = first
            .checked_add(count as u64)
            .ok_or(Error::Exhausted("work request identities"))?;
        match NEXT_ID.compare_exchange_weak(first, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(first),
            Err(actual) => first = actual,
        }
    }
}

struct Signaling {
    group: u64,
    signaled: bool,
    fence: bool,
}

fn post_status(receipt: sys::transfer::PostReceipt, receive: bool) -> Result<PostStatus> {
    native_post_status(
        receipt,
        if receive {
            "ibv_post_recv"
        } else {
            "ibv_post_send"
        },
    )
}
fn native_post_status(
    receipt: sys::transfer::PostReceipt,
    operation: &'static str,
) -> Result<PostStatus> {
    match (receipt.code, receipt.accepted) {
        (code, Some(0)) => Err(Error::Native { operation, code }),
        (0, Some(_)) => Ok(PostStatus::Complete),
        (code, Some(_)) => Ok(PostStatus::Partial { code }),
        (code, None) => Ok(PostStatus::Uncertain { code }),
    }
}

impl QueueData {
    pub(crate) fn send_capacity(&self, count: usize) -> Result<()> {
        self.payload
            .capacity(count.saturating_add(self.windows.pending()), false)
    }
    fn prepare(
        &mut self,
        r: &Request,
        peer: &ConnectionParams,
        local: &ConnectionParams,
        id: u64,
        signaling: Signaling,
    ) -> Result<(Pending, WorkRequest)> {
        let length =
            u32::try_from(r.buffer.bounds().len()).map_err(|_| Error::Invalid("SGE length"))?;
        let native_opcode = r.native_opcode(peer, local, length)?;
        let receive = matches!(r.transfer, Transfer::Receive);
        let destination = receive || matches!(r.transfer, Transfer::Read { .. });
        let loan = r
            .buffer
            .dma(&self.domain, destination, receive, length as usize)?;
        let wr = WorkRequest::new(
            id,
            native_opcode,
            loan.address(),
            length,
            sys::local_key(loan.mr()),
        )
        .signaled(signaling.signaled)
        .fenced(signaling.fence);
        Ok((
            Pending {
                id,
                group: signaling.group,
                opcode: r.transfer.opcode(),
                signaled: signaling.signaled,
                length,
                affects_peer: r.transfer.affects_peer(),
                state: self.payload.next_state(),
                loan,
                extra: Vec::new(),
            },
            wr,
        ))
    }
    fn reserve_pending(&mut self, count: usize, receive: bool) -> Result<()> {
        if receive {
            if self.srq.is_some() {
                return Err(Error::Invalid("post receives on the shared receive queue"));
            }
            self.payload.capacity(count, true)?;
        } else {
            self.send_capacity(count)?;
        }
        self.payload
            .pending
            .try_reserve(count)
            .map_err(|_| Error::Exhausted("in-flight WRs"))
    }
    fn accept(
        &mut self,
        p: Pending,
        buffer: crate::RegisteredRange,
        status: PostStatus,
    ) -> Operation {
        if let PostStatus::Uncertain { code } = status {
            lock(&p.state).fail(TransferFailure::Post { code }, p.affects_peer);
        }
        if p.opcode == sys::DataOpcode::Receive {
            self.payload.receive_count += 1;
        } else {
            self.payload.last_read = matches!(
                p.opcode,
                sys::DataOpcode::Read | sys::DataOpcode::FetchAdd | sys::DataOpcode::CompareSwap
            );
        }
        self.payload.fenced |= status != PostStatus::Complete;
        self.payload.used = true;
        let operation = Operation {
            id: p.id,
            buffer,
            state: p.state.clone(),
            affects_peer: p.affects_peer,
        };
        self.payload.pending.push(p);
        operation
    }
    fn post_one(
        &mut self,
        r: &Request,
        peer: &ConnectionParams,
        local: &ConnectionParams,
        immediate: Option<u32>,
    ) -> Result<(Pending, PostStatus)> {
        let qp = self.qp.ok_or(Error::Closed)?;
        let receive = matches!(r.transfer, Transfer::Receive);
        self.reserve_pending(1, receive)?;
        let id = reserve_ids(1)?;
        let (p, mut wr) = self.prepare(
            r,
            peer,
            local,
            id,
            Signaling {
                group: id,
                signaled: true,
                fence: self.payload.last_read,
            },
        )?;
        if let Some(value) = immediate {
            wr = wr.with_immediate(value)?;
        }
        let receipt = self.api.post_payload(qp, std::slice::from_mut(&mut wr))?;
        Ok((p, post_status(receipt, receive)?))
    }
    fn post(
        &mut self,
        requests: &mut Vec<Request>,
        peer: ConnectionParams,
        local: ConnectionParams,
    ) -> Result<Submission> {
        let qp = self.qp.ok_or(Error::Closed)?;
        let first = requests.first().ok_or(Error::Invalid("empty WR list"))?;
        let receive = matches!(first.transfer, Transfer::Receive);
        if requests
            .iter()
            .any(|r| matches!(r.transfer, Transfer::Receive) != receive)
        {
            return Err(Error::Invalid("mixed SQ and RQ list"));
        }
        self.reserve_pending(requests.len(), receive)?;
        let mut records = sys::storage(requests.len())?;
        let mut wrs = sys::storage(requests.len())?;
        let mut operations = sys::storage(requests.len())?;
        let base = reserve_ids(requests.len())?;
        let mut group = base;
        let mut fence = self.payload.last_read;
        let interval = self
            .payload
            .config
            .signal_interval
            .min((self.payload.config.send_depth / 2).max(1));
        for (i, r) in requests.iter().enumerate() {
            let id = base + i as u64;
            let opcode = r.transfer.opcode();
            let end = requests
                .get(i + 1)
                .is_none_or(|next| matches!(next.transfer, Transfer::Read { .. }));
            let signaled = receive
                || opcode == sys::DataOpcode::Read
                || end
                || id - group + 1 >= u64::from(interval);
            let (p, wr) = self.prepare(
                r,
                &peer,
                &local,
                id,
                Signaling {
                    group,
                    signaled,
                    fence,
                },
            )?;
            records.push(p);
            wrs.push(wr);
            fence = opcode == sys::DataOpcode::Read;
            if signaled {
                group = id + 1;
            }
        }
        let receipt = self.api.post_payload(qp, &mut wrs)?;
        let status = post_status(receipt, receive)?;
        let (count, _) = receipt.possible_prefix(requests.len());
        // All allocations and validation precede the native call. Each possible owner
        // now moves to the QP regardless of whether the caller keeps its handle.
        for (r, p) in requests.drain(..count).zip(records) {
            operations.push(self.accept(p, r.buffer, status));
        }
        Ok(Submission {
            status,
            operations,
            unposted: std::mem::take(requests),
        })
    }
    pub(crate) fn completion(
        &mut self,
        qpn: u32,
        entry: crate::verbs::CompletionEntry,
    ) -> Result<()> {
        if let Some(srq) = &self.srq {
            if srq.dispatch(qpn, entry)? {
                return Ok(());
            }
        }
        if !self.windows.completion(qpn, entry.work)? {
            self.payload.completion(qpn, entry)?;
        }
        Ok(())
    }
    fn outstanding(&self) -> Result<usize> {
        Ok(self.payload.pending.len()
            + self.windows.pending()
            + self.srq.as_ref().map_or(Ok(0), |s| s.outstanding())?)
    }
    fn progress(&mut self, qpn: u32) -> Result<usize> {
        let cq = self.cq.ok_or(Error::Closed)?;
        let pending = self.outstanding()?;
        if pending <= 1 {
            if let Some(wc) = self.api.poll_entry(cq, self.payload.datagram)? {
                self.completion(qpn, wc)?;
                return Ok(1);
            }
            return Ok(0);
        }
        use crate::verbs::CompletionBatch;
        let mut batch = CompletionBatch::default();
        let count = self
            .api
            .poll_batch(cq, &mut batch, pending.min(CompletionBatch::CAPACITY))?;
        let mut error = None;
        for wc in batch.entries_for(self.payload.datagram) {
            // Polling consumed all entries. A malformed receipt must not lose a later
            // independent, valid completion. Its own unproven loans stay in the QP.
            if let Err(e) = self.completion(qpn, wc) {
                error.get_or_insert(e);
            }
        }
        error.map_or(Ok(count), Err)
    }
    // A one-shot arm survives timeouts and dropped waiters. Poll again after
    // arming so a completion between the first poll and the arm cannot be lost.
    pub(super) fn poll_or_arm(
        &mut self,
        qpn: u32,
        deadline: Option<Instant>,
    ) -> Result<Option<usize>> {
        loop {
            let n = self.progress(qpn)?;
            if n != 0
                || self.outstanding()? == 0
                || deadline.is_some_and(|end| Instant::now() >= end)
            {
                return Ok(Some(n));
            }
            if self.payload.armed {
                return Ok(None);
            }
            self.api.arm(self.cq.ok_or(Error::Closed)?)?;
            self.payload.armed = true;
        }
    }
    fn wait(&mut self, qpn: u32, deadline: Instant) -> Result<usize> {
        let cq = self.cq.ok_or(Error::Closed)?;
        let ch = self.channel.ok_or(Error::Closed)?;
        loop {
            if let Some(n) = self.poll_or_arm(qpn, Some(deadline))? {
                return Ok(n);
            }
            if !self.api.wait_event(ch, cq, deadline)? {
                return self.progress(qpn);
            }
            self.payload.armed = false;
        }
    }
}
impl QueuePair {
    fn posting_queue(&mut self) -> Result<(&mut QueueData, ConnectionParams, ConnectionParams)> {
        let peer = self.peer.ok_or(Error::Invalid("QP has not reached RTS"))?;
        let local = self.parameters();
        Ok((self.owner.get_mut()?, peer, local))
    }
    /// Posts one signaled request, keeping the WR on the stack. An ambiguous provider
    /// error still returns an operation that owns its physical loan until retirement.
    pub fn submit_one(
        &mut self,
        request: Request,
    ) -> std::result::Result<SingleSubmission, RejectedOne> {
        self.submit_single(request, None)
    }
    /// Posts a single SEND or WRITE carrying a host-order 32-bit receive notification.
    pub fn submit_with_immediate(
        &mut self,
        request: Request,
        value: u32,
    ) -> std::result::Result<SingleSubmission, RejectedOne> {
        self.submit_single(request, Some(value))
    }
    fn submit_single(
        &mut self,
        request: Request,
        immediate: Option<u32>,
    ) -> std::result::Result<SingleSubmission, RejectedOne> {
        let admitted = self.posting_queue().and_then(|(d, peer, local)| {
            let (p, status) = d.post_one(&request, &peer, &local, immediate)?;
            Ok((d, p, status))
        });
        match admitted {
            Ok((d, p, status)) => Ok(SingleSubmission {
                status,
                operation: d.accept(p, request.buffer, status),
            }),
            Err(error) => Err(RejectedOne { error, request }),
        }
    }

    /// Posts one SQ list (READ/WRITE/SEND) or one RQ list (RECV). Submission is serialized
    /// with progress and retirement. A known unposted suffix is returned immediately.
    ///
    /// ```no_run
    /// # use orbit_rdma::{Operation, QueuePair, RegisteredMemory, RemoteDescriptor, Request, Transfer};
    /// fn write_page(qp: &mut QueuePair, mr: &RegisteredMemory, remote: RemoteDescriptor)
    ///     -> Result<Operation, Box<dyn std::error::Error>> {
    ///     let request = Request::new(mr.range(0..4096)?, Transfer::Write { remote, offset: 0 });
    ///     let mut posted = qp.submit(vec![request])?;
    ///     // Even an uncertain post returns a tracked operation. Drive its CQ or close the QP.
    ///     posted.operations.pop().ok_or_else(|| "missing tracked write".into())
    /// }
    /// ```
    pub fn submit(
        &mut self,
        mut requests: Vec<Request>,
    ) -> std::result::Result<Submission, Rejected> {
        if requests.len() == 1 {
            let mut operations = match sys::storage(1) {
                Ok(v) => v,
                Err(error) => return Err(Rejected { error, requests }),
            };
            if let Some(request) = requests.pop() {
                return match self.submit_one(request) {
                    Ok(one) => {
                        operations.push(one.operation);
                        Ok(Submission {
                            status: one.status,
                            operations,
                            unposted: requests,
                        })
                    }
                    Err(rejected) => {
                        requests.push(rejected.request);
                        Err(Rejected {
                            error: rejected.error,
                            requests,
                        })
                    }
                };
            }
        }
        let result = (|| {
            let peer = self.peer.ok_or(Error::Invalid("QP has not reached RTS"))?;
            let local = self.parameters();
            self.owner.get_mut()?.post(&mut requests, peer, local)
        })();
        result.map_err(|error| Rejected { error, requests })
    }
    /// Polls up to 32 actual CQEs. A successful group tail may retire multiple WRs.
    pub fn progress(&mut self) -> Result<usize> {
        let qpn = self.parameters().qpn;
        let d = self.owner.get_mut()?;
        let result = d.progress(qpn);
        if result.is_err() {
            d.payload.fenced = true;
        }
        result
    }
    /// Polls, arms notifications, polls again and waits until progress or the deadline.
    /// A channel event is acknowledged separately from the CQEs it announces.
    pub fn wait_for_progress(&mut self, deadline: Instant) -> Result<usize> {
        let qpn = self.parameters().qpn;
        let d = self.owner.get_mut()?;
        let result = d.wait(qpn, deadline);
        if result.is_err() {
            d.payload.fenced = true;
        }
        result
    }
}
