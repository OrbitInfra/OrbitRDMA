//! Per-QP accounting adapted from OrbiTE's send/receive queues and slice retirement.
use super::*;
use crate::device::{Charge, Permit};
use crate::memory::Loan;
use crate::queue::QueueData;
use crate::resource::Result;
use crate::verbs::{Opcode, WorkRequest};
use crate::{ConnectionParams, QueueConfig, RemoteAccess};
use std::sync::atomic::{AtomicU64, Ordering};

struct Pending {
    id: u64,
    group: u64,
    opcode: sys::DataOpcode,
    signaled: bool,
    length: u32,
    affects_peer: bool,
    state: Arc<Mutex<State>>,
    loan: Loan,
    permit: Permit,
}
impl Pending {
    fn finish(self, outcome: Outcome) {
        let mut state = lock(&self.state);
        drop((self.loan, self.permit));
        state.outcome.get_or_insert(outcome);
        state.retired = true;
    }
    fn failed(self, reason: TransferFailure) {
        let remote_effect_unknown = self.affects_peer;
        self.finish(Outcome::Failed {
            reason,
            remote_effect_unknown,
        });
    }
}
pub(crate) struct PayloadQueue {
    pending: Vec<Pending>,
    config: QueueConfig,
    used: bool,
    fenced: bool,
    armed: bool,
    last_read: bool,
}
impl PayloadQueue {
    pub(crate) fn new(config: QueueConfig) -> Self {
        Self {
            pending: Vec::new(),
            config,
            used: false,
            fenced: false,
            armed: false,
            last_read: false,
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
        for p in self.pending.drain(..) {
            p.failed(TransferFailure::QueueClosed);
        }
    }
    fn capacity(&self, count: usize, receive: bool) -> Result<()> {
        if self.fenced {
            return Err(Error::Closed);
        }
        let used = self
            .pending
            .iter()
            .filter(|p| (p.opcode == sys::DataOpcode::Receive) == receive)
            .count();
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
    fn completion(&mut self, qpn: u32, wc: sys::WorkCompletion) -> Result<()> {
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
                self.pending
                    .swap_remove(index)
                    .failed(TransferFailure::Completion {
                        status,
                        vendor_error: vendor,
                    });
            }
            Success {
                opcode,
                bytes,
                invalidated,
                ..
            } => {
                let tail = &self.pending[index];
                if !tail.signaled
                    || opcode != tail.opcode.wc()
                    || (tail.opcode == sys::DataOpcode::Read && bytes != tail.length)
                    || (tail.opcode == sys::DataOpcode::Receive && bytes > tail.length)
                {
                    return Err(crate::Error::Invalid(
                        "completion opcode, signaling or byte count",
                    ));
                }
                let group = tail.group;
                let mut i = 0;
                while i < self.pending.len() {
                    if self.pending[i].group != group {
                        i += 1;
                        continue;
                    }
                    let p = self.pending.swap_remove(i);
                    let n = if p.opcode == sys::DataOpcode::Receive {
                        bytes
                    } else {
                        p.length
                    };
                    let invalidated_rkey =
                        invalidated.filter(|_| p.opcode == sys::DataOpcode::Receive);
                    p.finish(Outcome::Success {
                        bytes: n,
                        invalidated_rkey,
                    });
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

fn reserve_ids(count: usize) -> Result<u64> {
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

impl QueueData {
    fn post(
        &mut self,
        requests: &mut Vec<Request>,
        peer: ConnectionParams,
        local: ConnectionParams,
    ) -> Result<Submission> {
        let qp = self.qp.ok_or(Error::Closed)?;
        let first = requests.first().ok_or(Error::Invalid("empty WR list"))?;
        let receive = matches!(first.transfer, Transfer::Receive);
        self.payload.capacity(requests.len(), receive)?;
        if requests
            .iter()
            .any(|r| matches!(r.transfer, Transfer::Receive) != receive)
        {
            return Err(Error::Invalid("mixed SQ and RQ list"));
        }
        let mut records = sys::storage(requests.len())?;
        let mut wrs = sys::storage(requests.len())?;
        let mut operations = sys::storage(requests.len())?;
        self.payload
            .pending
            .try_reserve(requests.len())
            .map_err(|_| Error::Exhausted("in-flight WRs"))?;
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
            let length =
                u32::try_from(r.buffer.bounds().len()).map_err(|_| Error::Invalid("SGE length"))?;
            let native_opcode = r.native_opcode(&peer, &local, length)?;
            let destination = matches!(r.transfer, Transfer::Read { .. } | Transfer::Receive);
            let loan = r.buffer.dma(&self.domain, destination)?;
            let permit = self.domain.reserve(Charge::Payload {
                bytes: length as usize,
                receive,
            })?;
            let end = requests
                .get(i + 1)
                .is_none_or(|next| matches!(next.transfer, Transfer::Read { .. }));
            let signaled = receive
                || opcode == sys::DataOpcode::Read
                || end
                || id - group + 1 >= u64::from(interval);
            wrs.push(
                WorkRequest::new(
                    id,
                    native_opcode,
                    loan.address(),
                    length,
                    sys::local_key(loan.mr()?),
                )
                .signaled(signaled)
                .fenced(fence),
            );
            records.push(Pending {
                id,
                group,
                opcode,
                signaled,
                length,
                affects_peer: r.transfer.affects_peer(),
                state: Arc::default(),
                loan,
                permit,
            });
            fence = opcode == sys::DataOpcode::Read;
            if signaled {
                group = id + 1;
            }
        }
        let receipt = self.api.post_payload(qp, &mut wrs)?;
        if receipt.accepted == Some(0) {
            return Err(Error::Native {
                operation: if receive {
                    "ibv_post_recv"
                } else {
                    "ibv_post_send"
                },
                code: receipt.code,
            });
        }
        self.payload.used = true;
        let status = match (receipt.code, receipt.accepted) {
            (0, Some(_)) => PostStatus::Complete,
            (code, Some(_)) => PostStatus::Partial { code },
            (code, None) => PostStatus::Uncertain { code },
        };
        self.payload.fenced |= receipt.code != 0;
        let count = receipt.accepted.unwrap_or(requests.len());
        // All allocations and validation precede the native call. Each possible owner
        // now moves to the QP regardless of whether the caller keeps its handle.
        for (r, p) in requests.drain(..count).zip(records) {
            if let PostStatus::Uncertain { code } = status {
                lock(&p.state).fail(TransferFailure::Post { code }, p.affects_peer);
            }
            if !receive {
                self.payload.last_read = p.opcode == sys::DataOpcode::Read;
            }
            operations.push(Operation {
                id: p.id,
                buffer: r.buffer,
                state: p.state.clone(),
                affects_peer: p.affects_peer,
            });
            self.payload.pending.push(p);
        }
        Ok(Submission {
            status,
            operations,
            unposted: std::mem::take(requests),
        })
    }
    fn progress(&mut self, qpn: u32) -> Result<usize> {
        let cq = self.cq.ok_or(Error::Closed)?;
        for count in 0..32 {
            let Some(wc) = self.api.poll_one(cq)? else {
                return Ok(count);
            };
            self.payload.completion(qpn, wc)?;
        }
        Ok(32)
    }
    fn wait(&mut self, qpn: u32, deadline: Instant) -> Result<usize> {
        let cq = self.cq.ok_or(Error::Closed)?;
        let ch = self.channel.ok_or(Error::Closed)?;
        loop {
            let n = self.progress(qpn)?;
            if n != 0 || self.payload.pending.is_empty() || Instant::now() >= deadline {
                return Ok(n);
            }
            if !self.payload.armed {
                self.api.arm(cq)?;
                self.payload.armed = true;
                // Poll again after arming, covering the poll-to-arm race.
                continue;
            }
            if !self.api.wait_event(ch, cq, deadline)? {
                return self.progress(qpn);
            }
            self.payload.armed = false;
        }
    }
}
impl QueuePair {
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
