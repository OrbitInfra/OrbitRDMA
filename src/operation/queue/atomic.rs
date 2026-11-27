use super::*;
use crate::verbs::Segment;
use crate::{AtomicOperation, AtomicTarget, RegisteredRange};

impl QueuePair {
    /// Post a signaled 64-bit atomic. The result range receives the old remote
    /// value. An uncertain receipt keeps that range owned and must not be replayed.
    pub fn submit_atomic(
        &mut self,
        result: &RegisteredRange,
        target: AtomicTarget,
        offset: u64,
        op: AtomicOperation,
    ) -> Result<SingleSubmission> {
        let (d, peer, local) = self.posting_queue()?;
        if !d.atomics || local.initiator_depth.min(peer.responder_depth) == 0 {
            return Err(Error::Unsupported("RC atomic operations"));
        }
        if target.qpn != peer.qpn
            || target.rkey == 0
            || result.bounds().len() != 8
            || target.address.checked_add(target.length).is_none()
            || offset.checked_add(8).is_none_or(|end| end > target.length)
        {
            return Err(Error::Invalid("atomic target or result range"));
        }
        let qp = d.qp.ok_or(Error::Closed)?;
        d.reserve_pending(1, false)?;
        let loan = result.dma(&d.domain, true, false, 8)?;
        let id = reserve_ids(1)?;
        let mut wr = WorkRequest::atomic(
            id,
            Segment::new(loan.address(), 8, sys::local_key(loan.mr())),
            target.address + offset,
            target.rkey,
            op,
        )?
        .fenced(d.payload.last_read);
        let opcode = match op {
            AtomicOperation::FetchAdd(_) => sys::DataOpcode::FetchAdd,
            AtomicOperation::CompareSwap { .. } => sys::DataOpcode::CompareSwap,
        };
        let pending = Pending {
            id,
            group: id,
            opcode,
            signaled: true,
            length: 8,
            affects_peer: true,
            state: Arc::default(),
            loan,
            extra: Vec::new(),
        };
        let receipt = d.api.post_payload(qp, std::slice::from_mut(&mut wr))?;
        let status = post_status(receipt, false)?;
        Ok(SingleSubmission {
            status,
            operation: d.accept(pending, result.clone(), status),
        })
    }
}
