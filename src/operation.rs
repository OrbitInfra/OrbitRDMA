//! Payload operations and receipts. A notification can wake a waiter; only
//! a matching work completion or successful QP destruction retires DMA.
use crate::device::lock;
use crate::resource::Fault;
use crate::{Error, QueuePair, RegisteredRange, RemoteDescriptor, sys};
use std::{
    fmt,
    sync::{Arc, Mutex},
    time::Instant,
};

mod queue;
pub(crate) use queue::PayloadQueue;

/// One local range is one verbs work request.
#[derive(Clone, Copy, Debug)]
pub enum Transfer {
    Read {
        remote: RemoteDescriptor,
        offset: u64,
    },
    Write {
        remote: RemoteDescriptor,
        offset: u64,
    },
    Send,
    Receive,
}
impl Transfer {
    fn opcode(self) -> sys::DataOpcode {
        match self {
            Self::Read { .. } => sys::DataOpcode::Read,
            Self::Write { .. } => sys::DataOpcode::Write,
            Self::Send => sys::DataOpcode::Send,
            Self::Receive => sys::DataOpcode::Receive,
        }
    }
    fn affects_peer(self) -> bool {
        matches!(self, Self::Write { .. } | Self::Send)
    }
}

/// The selected range remains available until the physical queue admits the request.
pub struct Request {
    pub buffer: RegisteredRange,
    pub transfer: Transfer,
}
impl Request {
    pub fn new(buffer: RegisteredRange, transfer: Transfer) -> Self {
        Self { buffer, transfer }
    }
}

/// Submitting the list proved zero WRs were accepted. All ranges are returned in input order.
pub struct Rejected {
    pub error: Error,
    pub requests: Vec<Request>,
}
impl fmt::Debug for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rejected")
            .field("error", &self.error)
            .field("requests", &self.requests.len())
            .finish()
    }
}
impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no requests were posted ({})", self.error)
    }
}
impl std::error::Error for Rejected {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Receipt for a single native list post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostStatus {
    Complete,
    /// The provider identified the first unposted WR.
    Partial {
        code: i32,
    },
    /// Every submitted WR remains a possible hardware owner.
    Uncertain {
        code: i32,
    },
}
pub struct Submission {
    pub status: PostStatus,
    /// One handle per accepted or possibly accepted WR, in original order.
    pub operations: Vec<Operation>,
    /// The proven unposted suffix, with its reservations released. Other outstanding
    /// requests may still hold overlapping bytes.
    pub unposted: Vec<Request>,
}
impl fmt::Debug for Submission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Submission")
            .field("status", &self.status)
            .field("operations", &self.operations.len())
            .field("unposted", &self.unposted.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferFailure {
    Post { code: i32 },
    Completion { status: u32, vendor_error: u32 },
    Cancelled,
    QueueClosed,
}
/// Logical outcome of one WR. Failed WRs contribute no proven payload bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Success {
        bytes: u32,
        invalidated_rkey: Option<u32>,
    },
    Failed {
        reason: TransferFailure,
        remote_effect_unknown: bool,
    },
}
pub struct Completion {
    pub buffer: RegisteredRange,
    pub outcome: Outcome,
}
pub enum Reclaim {
    Ready(Completion),
    Pending(Operation),
    /// The native destroy failed. The queue retains its loans and never retries it.
    Unquiesced {
        operation: Operation,
        error: Error,
    },
}
#[derive(Default)]
struct State {
    outcome: Option<Outcome>,
    retired: bool,
    quarantine: Option<Fault>,
}
impl State {
    fn fail(&mut self, why: TransferFailure, affects_peer: bool) {
        self.outcome.get_or_insert(Outcome::Failed {
            reason: why,
            remote_effect_unknown: affects_peer,
        });
    }
}

/// Dropping or forgetting a handle does not release the queue's independent DMA loan.
pub struct Operation {
    id: u64,
    buffer: RegisteredRange,
    state: Arc<Mutex<State>>,
    affects_peer: bool,
}
impl Operation {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn outcome(&self) -> Option<Outcome> {
        lock(&self.state).outcome
    }
    pub fn is_reusable(&self) -> bool {
        lock(&self.state).retired
    }
    /// Records logical cancellation. Poll or close the QP to retire actual hardware access.
    pub fn cancel(&self) {
        lock(&self.state).fail(TransferFailure::Cancelled, self.affects_peer);
    }
    /// Drives this operation's CQ. False means the deadline elapsed while the range was still held.
    pub fn wait_until(&self, queue: &mut QueuePair, deadline: Instant) -> Result<bool, Error> {
        loop {
            {
                let s = lock(&self.state);
                if let Some(fault) = s.quarantine {
                    return Err(fault.unquiesced());
                }
                if s.retired {
                    return Ok(true);
                }
            }
            if !queue.connected()?.payload.contains(self.id) {
                return Err(Error::Invalid("operation belongs to another QP"));
            }
            let progressed = queue.wait_for_progress(deadline)?;
            if progressed == 0 && Instant::now() >= deadline {
                return Ok(self.is_reusable());
            }
        }
    }
    /// Returns the original range only after its DMA loan and physical credits were released.
    pub fn try_reclaim(self) -> Reclaim {
        let state = lock(&self.state);
        let (fault, result) = (state.quarantine, state.outcome.filter(|_| state.retired));
        drop(state);
        if let Some(cause) = fault {
            return Reclaim::Unquiesced {
                operation: self,
                error: cause.unquiesced(),
            };
        }
        match result {
            Some(outcome) => Reclaim::Ready(Completion {
                buffer: self.buffer,
                outcome,
            }),
            None => Reclaim::Pending(self),
        }
    }
}
