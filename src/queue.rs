//! Each RC QP owns one private CQ and completion channel.
pub(crate) mod group;
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
    pub shared_cq: Option<Arc<group::CqOwner>>,
    pub channel: Option<sys::Channel>,
    pub payload: crate::operation::PayloadQueue,
    pub route: Option<sys::shared::DatagramRoute>,
    pub srq: Option<crate::SharedReceiveQueue>,
    pub max_sge: u32,
    pub atomics: bool,
    pub windows: crate::window::Windows,
    #[cfg(feature = "tokio")]
    pub notifier: Option<crate::operation::notification::Notifier>,
    _permit: Permit,
}
impl QueueData {
    fn destroy_qp(&mut self) -> std::result::Result<(), Fault> {
        if !self.windows.is_empty() {
            // RESET removes outstanding local WRs, including a delayed or uncertain
            // bind. RXE can reject QP destruction while a type-2 MW is still bound.
            self.api.reset(
                self.qp
                    .ok_or(Fault("reset shared-window QP", libc::EBADF))?,
            )?;
            self.windows.detach()?;
        }
        sys::destroy(&mut self.qp, "ibv_destroy_qp", self.api.ibv_destroy_qp)
    }
}
impl Cleanup for QueueData {
    fn cleanup(&mut self) -> std::result::Result<(), Fault> {
        #[cfg(feature = "tokio")]
        self.notifier.take();
        if let Err(fault) = self.destroy_qp() {
            self.payload.quarantine(fault);
            return Err(fault);
        }
        self.payload.drained();
        if let Some(route) = &mut self.route {
            route.close()?;
        }
        let windows = self.windows.drain();
        if self.shared_cq.is_some() {
            self.cq.take();
        } else {
            sys::destroy(&mut self.cq, "ibv_destroy_cq", self.api.ibv_destroy_cq)?;
        }
        sys::destroy(
            &mut self.channel,
            "ibv_destroy_comp_channel",
            self.api.ibv_destroy_comp_channel,
        )?;
        windows
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
        self.queue_pair_with_sge(config, 1)
    }
    /// Request a per-WR scatter/gather capacity for both send and receive queues.
    pub fn queue_pair_with_sge(&self, config: QueueConfig, max_sge: u32) -> Result<QueuePair> {
        self.create_qp(config, max_sge, false, None, None, None)
    }
    /// Opt in to standard 64-bit remote atomics on an atomic-capable device.
    pub fn atomic_queue_pair(&self, config: QueueConfig, max_sge: u32) -> Result<QueuePair> {
        self.create_qp(config, max_sge, true, None, None, None)
    }
    pub fn queue_pair_with_srq(
        &self,
        config: QueueConfig,
        max_sge: u32,
        srq: &crate::SharedReceiveQueue,
    ) -> Result<QueuePair> {
        self.create_qp(config, max_sge, false, Some(srq.clone()), None, None)
    }
    pub(crate) fn create_qp(
        &self,
        config: QueueConfig,
        max_sge: u32,
        atomics: bool,
        srq: Option<crate::SharedReceiveQueue>,
        ud_key: Option<u32>,
        shared_cq: Option<Arc<group::CqOwner>>,
    ) -> Result<QueuePair> {
        let domain = self.domain()?;
        let shared = srq.as_ref().map(|s| s.native(domain)).transpose()?;
        let caps = domain.caps;
        if atomics && (!caps.atomics || config.read_depth == 0) {
            return Err(Error::Unsupported("RC atomic operations"));
        }
        if max_sge == 0 || max_sge > caps.max_sge {
            return Err(Error::Unsupported("requested QP scatter/gather limit"));
        }
        let depth = config
            .send_depth
            .checked_add(shared.map_or(config.receive_depth, |(_, depth)| depth))
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
        let mut payload = crate::operation::PayloadQueue::new(config);
        payload.datagram = ud_key.is_some();
        let mut owner = Owner::new(QueueData {
            domain: domain.clone(),
            api,
            qp: None,
            cq: None,
            channel: None,
            shared_cq,
            payload,
            srq,
            route: None,
            max_sge,
            atomics,
            windows: Default::default(),
            #[cfg(feature = "tokio")]
            notifier: None,
            _permit: domain.reserve(Charge::Queue)?,
        });
        let create = (|| {
            let r = owner.get_mut()?;
            let cq = if let Some(shared) = &r.shared_cq {
                let cq = shared.handle()?;
                r.cq = Some(cq);
                cq
            } else {
                let channel = api.channel(ctx)?;
                r.channel = Some(channel);
                let (cq, enough_entries) = api.cq(ctx, Some(channel), depth as i32)?;
                r.cq = Some(cq);
                if !enough_entries {
                    return Err(Error::Unsupported("requested CQ depth"));
                }
                cq
            };
            let (qp, qpn, sized) = api.qp(
                pd,
                cq,
                cq,
                sys::Creation {
                    send_depth: config.send_depth,
                    receive_depth: config.receive_depth,
                    max_sge,
                    srq: shared.map(|(handle, _)| handle),
                    kind: if ud_key.is_some() {
                        sys::ffi::IBV_QPT_UD
                    } else {
                        sys::ffi::IBV_QPT_RC
                    },
                },
            )?;
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
            let access = sys::RC_READ_WRITE
                | if atomics {
                    sys::ffi::IBV_ACCESS_REMOTE_ATOMIC
                } else {
                    0
                };
            if let Some(qkey) = ud_key {
                api.start_ud(qp, port.number, qkey, config.psn)?;
            } else {
                api.init(qp, port.number, access)?;
            }
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
        self.unconnected()?;
        let data = self.owner.get()?;
        peer.validate(self.local.link)?;
        let port = data.domain.port;
        let (_, ctx, _) = data.domain.native()?;
        if data.api.port(ctx, port.number, port.gid_index)? != port {
            return Err(Error::Invalid("selected route changed"));
        }
        let qp = data.qp.ok_or(Error::Closed)?;
        if let Err(e) = data.api.connect(
            qp,
            sys::RcPath {
                port: port.number,
                gid_index: port.gid_index,
                global: port.link != crate::Link::InfiniBand || peer.gid != [0; 16],
                mtu: self.local.mtu.min(peer.mtu),
                remote_qpn: peer.qpn,
                remote_psn: peer.psn,
                local_psn: self.local.psn,
                lid: peer.lid,
                gid: peer.gid,
                initiator: self.local.initiator_depth.min(peer.responder_depth),
                responder: self.local.responder_depth.min(peer.initiator_depth),
                service_level: 0,
                traffic_class: 0,
                hop_limit: 64,
            },
        ) {
            return Err(self.owner.rollback(e));
        }
        self.peer = Some(*peer);
        Ok(())
    }
    pub(crate) fn unconnected(&self) -> Result<()> {
        self.owner.get()?;
        if self.peer.is_some() {
            return Err(Error::Invalid("QP is already connected"));
        }
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
    /// Retires remaining grants, the QP, CQ and completion channel. With live
    /// windows it resets the QP and detaches their keys before destroying the QP.
    /// The backing stays held through QP destruction and MR deregistration.
    /// A failed native teardown step is terminal.
    pub fn close(&mut self) -> Result<()> {
        self.owner.release().map(drop)
    }
}
