//! One signaled work request spanning several owned registered ranges.
use super::*;
use crate::verbs::Segment;

impl QueuePair {
    /// Posts one READ, WRITE, SEND or RECV with the local ranges in wire order.
    /// The caller keeps the descriptors, and the queue owns every DMA loan even
    /// if the receipt is dropped. A rejection proves zero ranges were posted.
    pub fn submit_vectored(
        &mut self,
        ranges: &[crate::RegisteredRange],
        transfer: Transfer,
    ) -> Result<VectoredSubmission> {
        let (d, peer, local) = self.posting_queue()?;
        let (first, rest) = ranges
            .split_first()
            .ok_or(Error::Invalid("empty scatter/gather list"))?;
        if ranges.len() > d.max_sge as usize {
            return Err(Error::Invalid("QP scatter/gather limit"));
        }
        let receive = matches!(transfer, Transfer::Receive);
        if matches!(transfer, Transfer::Read { .. })
            && ranges.len() > d.domain.caps.max_read_sge as usize
        {
            return Err(Error::Unsupported("RDMA READ scatter/gather limit"));
        }
        let destination = receive || matches!(transfer, Transfer::Read { .. });
        let qp = d.qp.ok_or(Error::Closed)?;
        d.reserve_pending(1, receive)?;
        let request = Request::new(first.clone(), transfer);
        let length = ranges.iter().try_fold(0u32, |total, r| {
            u32::try_from(r.bounds().len())
                .ok()
                .and_then(|len| total.checked_add(len))
                .ok_or(Error::Invalid("scatter/gather byte count"))
        })?;
        let opcode = request.native_opcode(&peer, &local, length)?;
        let mut extra = sys::storage(rest.len())?;
        let mut segments = sys::storage(ranges.len())?;
        let segment = |l: &Loan| Segment::new(l.address(), l.length as u32, sys::local_key(l.mr()));
        let loan = first.dma(&d.domain, destination, receive, length as usize)?;
        segments.push(segment(&loan));
        for r in rest {
            let next = r.dma_segment(&d.domain, destination)?;
            segments.push(segment(&next));
            extra.push(next);
        }
        let id = reserve_ids(1)?;
        let mut wr = WorkRequest::scatter(id, opcode, segments)?.fenced(d.payload.last_read);
        let pending = Pending {
            id,
            group: id,
            opcode: transfer.opcode(),
            signaled: true,
            length,
            affects_peer: transfer.affects_peer(),
            state: Arc::default(),
            loan,
            extra,
        };
        let receipt = d.api.post_payload(qp, std::slice::from_mut(&mut wr))?;
        let status = post_status(receipt, receive)?;
        Ok(VectoredSubmission {
            status,
            receipt: d.accept(pending, request.buffer, status),
        })
    }
}
