//! Each RC QP owns one private CQ and completion channel.
use crate::device::{Charge, Domain, Permit};
use crate::resource::{Cleanup, Fault, Owner, Result};
use crate::{Device, Error, Link, sys};
use std::sync::Arc;

/// Requested queue capacities, initial 24-bit packet sequence number and RDMA read depth.
#[derive(Debug, Clone, Copy)]
pub struct QueueConfig {
    /// Maximum ordered SEND/WRITE group per post. READ and RECV each get a receipt.
    pub signal_interval: u32,
    pub send_depth: u32,
    pub receive_depth: u32,
    pub psn: u32,
    pub read_depth: u8,
}
impl QueueConfig {
    pub fn new(psn: u32) -> Self {
        Self {
            signal_interval: 1,
            send_depth: 16,
            receive_depth: 16,
            psn,
            read_depth: 1,
        }
    }
}
/// Exchange these fields over the application's existing control channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionParams {
    pub qpn: u32,
    pub psn: u32,
    pub link: Link,
    pub lid: u16,
    pub gid: [u8; 16],
    pub mtu: u8,
    pub initiator_depth: u8,
    pub responder_depth: u8,
}
impl ConnectionParams {
    fn validate(&self, link: Link) -> Result<()> {
        if self.link != link
            || !(2..=0xff_ffff).contains(&self.qpn)
            || self.psn > 0xff_ffff
            || !(1..=5).contains(&self.mtu)
            || (link != Link::InfiniBand && self.gid == [0; 16])
            || self.gid[0] == 0xff
            || (link == Link::InfiniBand && self.lid == 0)
        {
            return Err(Error::Invalid("RC connection parameters"));
        }
        Ok(())
    }
}
pub(crate) struct QueueData {
    pub domain: Arc<Domain>,
    pub api: &'static sys::Native,
    pub qp: Option<sys::Qp>,
    pub cq: Option<sys::Cq>,
    pub channel: Option<sys::Channel>,
    pub payload: crate::operation::PayloadQueue,
    _permit: Permit,
}
impl Cleanup for QueueData {
    fn cleanup(&mut self) -> std::result::Result<(), Fault> {
        if let Err(fault) = sys::destroy(&mut self.qp, "ibv_destroy_qp", self.api.ibv_destroy_qp) {
            self.payload.quarantine(fault);
            return Err(fault);
        }
        self.payload.drained();
        sys::destroy(&mut self.cq, "ibv_destroy_cq", self.api.ibv_destroy_cq)?;
        sys::destroy(
            &mut self.channel,
            "ibv_destroy_comp_channel",
            self.api.ibv_destroy_comp_channel,
        )
    }
}

/// A reliable connected queue pair with a dedicated completion queue and channel.
pub struct QueuePair {
    pub(crate) owner: Owner<QueueData>,
    local: ConnectionParams,
    pub(crate) peer: Option<ConnectionParams>,
}
impl Device {
    /// Creates an RC queue pair in INIT. Its PSN comes from the caller's connection policy.
    pub fn queue_pair(&self, config: QueueConfig) -> Result<QueuePair> {
        let domain = self.domain()?;
        let caps = domain.caps;
        let depth = config
            .send_depth
            .checked_add(config.receive_depth)
            .filter(|&v| v <= caps.max_cqe)
            .ok_or(Error::Invalid("completion queue depth"))?;
        if config.psn > 0xff_ffff
            || config.send_depth == 0
            || config.receive_depth == 0
            || config.signal_interval == 0
            || config.send_depth.max(config.receive_depth) > caps.max_wr
        {
            return Err(Error::Invalid("RC queue configuration"));
        }
        let (api, ctx, pd) = domain.native()?;
        let port = domain.port;
        if api.port(ctx, port.number, port.gid_index)? != port {
            return Err(Error::Invalid("selected route changed"));
        }
        let mut owner = Owner::new(QueueData {
            domain: domain.clone(),
            api,
            qp: None,
            cq: None,
            channel: None,
            payload: crate::operation::PayloadQueue::new(config),
            _permit: domain.reserve(Charge::Queue)?,
        });
        let create = (|| {
            let r = owner.get_mut()?;
            let channel = api.channel(ctx)?;
            r.channel = Some(channel);
            let (cq, enough_entries) = api.cq(ctx, channel, depth as i32)?;
            r.cq = Some(cq);
            if !enough_entries {
                return Err(Error::Unsupported("requested CQ depth"));
            }
            let (qp, qpn, sized) = api.qp(pd, cq, &config)?;
            r.qp = Some(qp);
            if !sized {
                return Err(Error::Unsupported("requested QP depth"));
            }
            let local = ConnectionParams {
                qpn,
                psn: config.psn,
                link: port.link,
                lid: port.lid,
                gid: port.gid,
                mtu: port.mtu,
                initiator_depth: config.read_depth.min(caps.initiator),
                responder_depth: config.read_depth.min(caps.responder),
            };
            local.validate(port.link)?;
            api.init(qp, port.number)?;
            Ok(local)
        })();
        let local = create.map_err(|e| owner.rollback(e))?;
        Ok(QueuePair {
            owner,
            local,
            peer: None,
        })
    }
}
impl QueuePair {
    /// Returns the local address, queue identity and limits to send to the peer.
    pub fn parameters(&self) -> ConnectionParams {
        self.local
    }
    /// Moves INIT to RTR and RTS using the parameters supplied by the peer.
    ///
    /// ```no_run
    /// # use orbit_rdma::{ConnectionParams, Device, Error, QueueConfig, QueuePair};
    /// fn connect_peer(device: &Device, psn: u32,
    ///     exchange: impl FnOnce(ConnectionParams) -> Result<ConnectionParams, Error>,
    /// ) -> Result<QueuePair, Error> {
    ///     let mut qp = device.queue_pair(QueueConfig::new(psn))?;
    ///     let peer = exchange(qp.parameters())?;
    ///     qp.connect(&peer)?;
    ///     Ok(qp)
    /// }
    /// ```
    pub fn connect(&mut self, peer: &ConnectionParams) -> Result<()> {
        let data = self.owner.get()?;
        if self.peer.is_some() {
            return Err(Error::Invalid("QP is already connected"));
        }
        peer.validate(self.local.link)?;
        let port = data.domain.port;
        let (_, ctx, _) = data.domain.native()?;
        if data.api.port(ctx, port.number, port.gid_index)? != port {
            return Err(Error::Invalid("selected route changed"));
        }
        let qp = data.qp.ok_or(Error::Closed)?;
        if let Err(e) = data.api.connect(qp, port, &self.local, peer) {
            return Err(self.owner.rollback(e));
        }
        self.peer = Some(*peer);
        Ok(())
    }
    pub(crate) fn connected(&self) -> Result<&QueueData> {
        let data = self.owner.get()?;
        if self.peer.is_none() {
            return Err(Error::Invalid("QP has not reached RTS"));
        }
        Ok(data)
    }
    pub(crate) fn drained(&self) -> bool {
        self.owner.retained().is_none_or(|r| r.qp.is_none())
    }
    /// Destroys the QP, CQ and channel in that order. A failed destroy is terminal.
    pub fn close(&mut self) -> Result<()> {
        self.owner.release().map(drop)
    }
}
