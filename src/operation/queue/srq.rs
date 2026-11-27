//! Shared RQ receipts may arrive on any attached QP's CQ.
use super::*;
use crate::device::{Charge, Domain, Permit};
use crate::resource::{Cleanup, Owner};
use crate::sys::shared::{DestroySrq, Srq};
use crate::{Device, RegisteredRange};

struct Shared {
    domain: Arc<Domain>,
    handle: Option<Srq>,
    destroy: DestroySrq,
    receipts: PayloadQueue,
    _permit: Permit,
}
impl Cleanup for Shared {
    fn cleanup(&mut self) -> std::result::Result<(), Fault> {
        if let Err(f) = sys::destroy(&mut self.handle, "ibv_destroy_srq", self.destroy) {
            self.receipts.quarantine(f);
            return Err(f);
        }
        self.receipts.drained();
        Ok(())
    }
}
/// One bounded receive pool shared by RC QPs in the same PD. Each QP retains
/// the pool until native QP destruction. Poll the CQ of the QP that received the message.
#[derive(Clone)]
pub struct SharedReceiveQueue(Arc<Mutex<Owner<Shared>>>);
impl Device {
    pub fn shared_receive_queue(&self, depth: u32) -> Result<SharedReceiveQueue> {
        let domain = self.domain()?;
        if depth == 0 || depth > domain.caps.max_wr {
            return Err(Error::Invalid("shared receive depth"));
        }
        let (api, _, pd) = domain.native()?;
        let permit = domain.reserve(Charge::SharedReceiveQueue)?;
        let (handle, destroy, sized) = api.create_srq(pd, depth)?;
        let mut owner = Owner::new(Shared {
            domain: domain.clone(),
            handle: Some(handle),
            destroy,
            receipts: PayloadQueue::new(QueueConfig {
                receive_depth: depth,
                ..QueueConfig::new(0)
            }),
            _permit: permit,
        });
        if !sized {
            return Err(owner.rollback(Error::Unsupported("requested SRQ depth")));
        }
        Ok(SharedReceiveQueue(Arc::new(Mutex::new(owner))))
    }
}
impl SharedReceiveQueue {
    pub(crate) fn native(&self, domain: &Arc<Domain>) -> Result<(Srq, u32)> {
        let guard = lock(&self.0);
        let d = guard.get()?;
        if !Arc::ptr_eq(domain, &d.domain) {
            return Err(Error::Invalid("foreign SRQ protection domain"));
        }
        Ok((
            d.handle.ok_or(Error::Closed)?,
            d.receipts.config.receive_depth,
        ))
    }
    pub(crate) fn contains(&self, id: u64) -> bool {
        lock(&self.0)
            .retained()
            .is_some_and(|s| s.receipts.contains(id))
    }
    pub(crate) fn outstanding(&self) -> Result<usize> {
        Ok(lock(&self.0).get()?.receipts.pending.len())
    }
    pub(super) fn dispatch(&self, qpn: u32, entry: crate::verbs::CompletionEntry) -> Result<bool> {
        let mut g = lock(&self.0);
        let d = g.get_mut()?;
        let id = match entry.work {
            sys::WorkCompletion::Success { id, .. } | sys::WorkCompletion::Error { id, .. } => id,
        };
        if !d.receipts.contains(id) {
            return Ok(false);
        }
        let result = d.receipts.completion(qpn, entry);
        d.receipts.fenced |= result.is_err();
        result.map(|()| true)
    }
    pub fn post(&mut self, buffer: &RegisteredRange) -> Result<SingleSubmission> {
        let mut g = lock(&self.0);
        let d = g.get_mut()?;
        let srq = d.handle.ok_or(Error::Closed)?;
        d.receipts.capacity(1, true)?;
        d.receipts
            .pending
            .try_reserve(1)
            .map_err(|_| Error::Exhausted("SRQ receipts"))?;
        let bytes = buffer.bounds().len();
        let length = u32::try_from(bytes).map_err(|_| Error::Invalid("SRQ receive length"))?;
        let id = reserve_ids(1)?;
        let loan = buffer.dma(&d.domain, true, true, bytes)?;
        let state: Arc<Mutex<State>> = Arc::default();
        let (api, _, _) = d.domain.native()?;
        let receipt = api.post_srq(srq, id, loan.address(), length, sys::local_key(loan.mr()))?;
        let status = native_post_status(receipt, "ibv_post_srq_recv")?;
        if let PostStatus::Uncertain { code } = status {
            lock(&state).fail(TransferFailure::Post { code }, false);
        }
        d.receipts.fenced |= status != PostStatus::Complete;
        d.receipts.receive_count += 1;
        d.receipts.pending.push(Pending {
            id,
            group: id,
            opcode: sys::DataOpcode::Receive,
            signaled: true,
            length,
            affects_peer: false,
            state: state.clone(),
            loan,
            extra: Vec::new(),
        });
        Ok(SingleSubmission {
            status,
            operation: Operation {
                id,
                buffer: buffer.clone(),
                state,
                affects_peer: false,
            },
        })
    }
    /// Close after all attached QPs and other pool handles have retired.
    pub fn close(&mut self) -> Result<()> {
        let owner = Arc::get_mut(&mut self.0).ok_or(Error::Busy)?;
        owner
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .release()
            .map(drop)
    }
}
