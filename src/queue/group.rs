//! Exclusively polled shared CQ. Its QPs reserve space for their entire SQ and RQ.
use super::*;
use crate::{
    RejectedOne, Request, SingleSubmission,
    operation::reserve_ids,
    verbs::{CompletionBatch, WorkCompletion},
};
use std::collections::HashMap;

pub(crate) struct CqOwner(Owner<CqData>);
struct CqData {
    cq: Option<sys::Cq>,
    api: &'static sys::Native,
    _domain: Arc<Domain>,
    _permit: Permit,
}
impl Cleanup for CqData {
    fn cleanup(&mut self) -> std::result::Result<(), Fault> {
        sys::destroy(&mut self.cq, "ibv_destroy_cq", self.api.ibv_destroy_cq)
    }
}
impl CqOwner {
    pub(super) fn handle(&self) -> Result<sys::Cq> {
        self.0.get()?.cq.ok_or(Error::Closed)
    }
}

/// Identifies one QP incarnation even if a provider later recycles its QPN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueId {
    qpn: u32,
    incarnation: u64,
}
struct Member {
    queue: QueuePair,
    id: QueueId,
    reserved: u32,
}
/// A set of RC QPs sharing one physical CQ and one poller. Only the group may
/// poll the CQ; individual operations can observe their own retirement receipts.
pub struct CompletionGroup {
    device: Device,
    cq: Arc<CqOwner>,
    members: HashMap<u32, Member>,
    capacity: u32,
    reserved: u32,
    batch: CompletionBatch,
    fenced: bool,
}
impl Device {
    pub fn completion_group(&self, capacity: u32) -> Result<CompletionGroup> {
        let d = self.domain()?;
        if capacity == 0 || capacity > d.caps.max_cqe || capacity > i32::MAX as u32 {
            return Err(Error::Invalid("shared CQ depth"));
        }
        let (api, ctx, _) = d.native()?;
        let permit = d.reserve(Charge::SharedCompletionQueue)?;
        let (cq, sized) = api.cq(ctx, None, capacity as i32)?;
        let mut owner = Owner::new(CqData {
            cq: Some(cq),
            api,
            _domain: d.clone(),
            _permit: permit,
        });
        if !sized {
            return Err(owner.rollback(Error::Unsupported("shared CQ depth")));
        }
        Ok(CompletionGroup {
            device: Device {
                inner: Some(d.clone()),
            },
            cq: Arc::new(CqOwner(owner)),
            members: HashMap::new(),
            capacity,
            reserved: 0,
            batch: CompletionBatch::default(),
            fenced: false,
        })
    }
}
impl CompletionGroup {
    pub fn add(&mut self, config: QueueConfig) -> Result<QueueId> {
        if self.fenced {
            return Err(Error::Closed);
        }
        self.cq.handle()?;
        let reserved = config
            .send_depth
            .checked_add(config.receive_depth)
            .filter(|n| *n <= self.capacity - self.reserved)
            .ok_or(Error::Exhausted("shared CQ reservations"))?;
        self.members
            .try_reserve(1)
            .map_err(|_| Error::Exhausted("shared CQ members"))?;
        let incarnation = reserve_ids(1)?;
        let mut queue =
            match self
                .device
                .create_qp(config, 1, false, None, None, Some(self.cq.clone()))
            {
                Err(e @ Error::Unquiesced { .. }) => {
                    self.fenced = true;
                    return Err(e);
                }
                other => other?,
            };
        let id = QueueId {
            qpn: queue.parameters().qpn,
            incarnation,
        };
        if self.members.contains_key(&id.qpn) {
            self.fenced = true;
            queue.close()?;
            return Err(Error::Invalid("duplicate live QPN"));
        }
        self.members.insert(
            id.qpn,
            Member {
                queue,
                id,
                reserved,
            },
        );
        self.reserved += reserved;
        Ok(id)
    }
    fn member(&mut self, id: QueueId) -> Result<&mut Member> {
        self.members
            .get_mut(&id.qpn)
            .filter(|m| m.id == id)
            .ok_or(Error::Invalid("stale shared CQ queue identity"))
    }
    pub fn parameters(&mut self, id: QueueId) -> Result<ConnectionParams> {
        Ok(self.member(id)?.queue.parameters())
    }
    pub fn connect(&mut self, id: QueueId, peer: &ConnectionParams) -> Result<()> {
        self.member(id)?.queue.connect(peer)
    }
    pub fn connect_over(
        &mut self,
        id: QueueId,
        stream: &mut (impl std::io::Read + std::io::Write),
    ) -> Result<()> {
        self.member(id)?.queue.connect_over(stream)
    }
    pub fn submit_one(
        &mut self,
        id: QueueId,
        request: Request,
    ) -> std::result::Result<SingleSubmission, RejectedOne> {
        let queue = if self.fenced {
            Err(Error::Closed)
        } else {
            self.member(id)
        };
        match queue {
            Ok(m) => m.queue.submit_one(request),
            Err(error) => Err(RejectedOne { error, request }),
        }
    }
    /// Poll and route all CQEs by native QPN and process-unique WR identity.
    pub fn progress(&mut self) -> Result<usize> {
        let cq = self.cq.handle()?;
        let (api, _, _) = self.device.domain()?.native()?;
        let results = api.poll_batch(cq, &mut self.batch, CompletionBatch::CAPACITY);
        let count = results.inspect_err(|_| self.fenced = true)?;
        let mut first = None;
        for entry in self.batch.entries() {
            let qpn = match entry.work {
                WorkCompletion::Success { qpn, .. } | WorkCompletion::Error { qpn, .. } => qpn,
            };
            let result = self
                .members
                .get_mut(&qpn)
                .ok_or(Error::Invalid("foreign shared CQ QP"))
                .and_then(|m| m.queue.owner.get_mut()?.completion(qpn, entry));
            if let Err(e) = result {
                self.fenced = true;
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(count), Err)
    }
    pub fn close_queue(&mut self, id: QueueId) -> Result<()> {
        let member = self.member(id)?;
        member.queue.close()?;
        let reserved = member.reserved;
        self.reserved -= reserved;
        self.members.remove(&id.qpn);
        Ok(())
    }
    pub fn close(&mut self) -> Result<()> {
        self.fenced = true;
        while let Some(id) = self.members.values().next().map(|m| m.id) {
            self.close_queue(id)?;
        }
        let cq = Arc::get_mut(&mut self.cq).ok_or(Error::Busy)?;
        cq.0.release()?;
        self.device.inner.take();
        Ok(())
    }
}
