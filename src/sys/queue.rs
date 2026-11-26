//! Adapted from OrbiTE sys/qp.rs. RC attributes and masks are paired by transition.
use super::*;
use crate::{ConnectionParams, Port, QueueConfig};

pub(crate) type Channel = Handle<ffi::ibv_comp_channel>;
pub(crate) type Cq = Handle<ffi::ibv_cq>;
pub(crate) type Qp = Handle<ffi::ibv_qp>;

impl Native {
    pub(crate) fn channel(&self, ctx: Context) -> RdmaResult<Channel> {
        handle(
            // SAFETY: the queue owner retains the context until channel destruction.
            unsafe { (self.ibv_create_comp_channel)(ctx.0.as_ptr()) },
            "ibv_create_comp_channel",
        )
    }
    pub(crate) fn cq(&self, ctx: Context, channel: Channel, depth: i32) -> RdmaResult<(Cq, bool)> {
        let cq = handle(
            // SAFETY: this channel belongs to ctx and outlives the new CQ.
            unsafe {
                (self.ibv_create_cq)(
                    ctx.0.as_ptr(),
                    depth,
                    std::ptr::null_mut(),
                    channel.0.as_ptr(),
                    0,
                )
            },
            "ibv_create_cq",
        )?;
        // SAFETY: the newly created CQ reports its actual number of entries.
        Ok((cq, unsafe { (*cq.0.as_ptr()).cqe } >= depth))
    }
    pub(crate) fn qp(&self, pd: Pd, cq: Cq, config: &QueueConfig) -> RdmaResult<(Qp, u32, bool)> {
        let mut init = ffi::ibv_qp_init_attr {
            send_cq: cq.0.as_ptr(),
            recv_cq: cq.0.as_ptr(),
            qp_type: ffi::IBV_QPT_RC,
            cap: ffi::ibv_qp_cap {
                max_send_wr: config.send_depth,
                max_recv_wr: config.receive_depth,
                max_send_sge: 1,
                max_recv_sge: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let qp = handle(
            // SAFETY: the PD and CQ belong to one device; the owner retains both.
            unsafe { (self.ibv_create_qp)(pd.0.as_ptr(), &mut init) },
            "ibv_create_qp",
        )?;
        // SAFETY: the new QP is live and qp_num is immutable through destruction.
        let qpn = unsafe { (*qp.0.as_ptr()).qp_num };
        Ok((
            qp,
            qpn,
            init.cap.max_send_wr >= config.send_depth
                && init.cap.max_recv_wr >= config.receive_depth
                && init.cap.max_send_sge >= 1
                && init.cap.max_recv_sge >= 1,
        ))
    }
    fn modify(
        &self,
        qp: Qp,
        mut attr: ffi::ibv_qp_attr,
        mask: u32,
        op: &'static str,
    ) -> RdmaResult<()> {
        // SAFETY: the unique queue owner selects the legal mask and serializes native changes.
        check(op, unsafe {
            (self.ibv_modify_qp)(qp.0.as_ptr(), &mut attr, mask as c_int)
        })
    }
    pub(crate) fn init(&self, qp: Qp, port: u8) -> RdmaResult<()> {
        let attr = ffi::ibv_qp_attr {
            qp_state: ffi::IBV_QPS_INIT,
            port_num: port,
            qp_access_flags: ffi::IBV_ACCESS_REMOTE_READ | ffi::IBV_ACCESS_REMOTE_WRITE,
            ..Default::default()
        };
        self.modify(
            qp,
            attr,
            ffi::IBV_QP_STATE
                | ffi::IBV_QP_PKEY_INDEX
                | ffi::IBV_QP_PORT
                | ffi::IBV_QP_ACCESS_FLAGS,
            "ibv_modify_qp INIT",
        )
    }
    pub(crate) fn connect(
        &self,
        qp: Qp,
        port: Port,
        local: &ConnectionParams,
        peer: &ConnectionParams,
    ) -> RdmaResult<()> {
        let mut attr = ffi::ibv_qp_attr {
            qp_state: ffi::IBV_QPS_RTR,
            path_mtu: u32::from(local.mtu.min(peer.mtu)),
            dest_qp_num: peer.qpn,
            rq_psn: peer.psn,
            max_dest_rd_atomic: local.responder_depth.min(peer.initiator_depth),
            min_rnr_timer: 12,
            ..Default::default()
        };
        attr.ah_attr.port_num = port.number;
        attr.ah_attr.dlid = peer.lid;
        if port.link != crate::Link::InfiniBand || peer.gid != [0; 16] {
            attr.ah_attr.is_global = 1;
            attr.ah_attr.grh.dgid.raw = peer.gid;
            attr.ah_attr.grh.sgid_index = port.gid_index;
            attr.ah_attr.grh.hop_limit = 64;
        }
        self.modify(
            qp,
            attr,
            ffi::IBV_QP_STATE
                | ffi::IBV_QP_AV
                | ffi::IBV_QP_PATH_MTU
                | ffi::IBV_QP_DEST_QPN
                | ffi::IBV_QP_RQ_PSN
                | ffi::IBV_QP_MAX_DEST_RD_ATOMIC
                | ffi::IBV_QP_MIN_RNR_TIMER,
            "ibv_modify_qp RTR",
        )?;
        let attr = ffi::ibv_qp_attr {
            qp_state: ffi::IBV_QPS_RTS,
            timeout: 14,
            retry_cnt: 7,
            rnr_retry: 7,
            sq_psn: local.psn,
            max_rd_atomic: local.initiator_depth.min(peer.responder_depth),
            ..Default::default()
        };
        self.modify(
            qp,
            attr,
            ffi::IBV_QP_STATE
                | ffi::IBV_QP_TIMEOUT
                | ffi::IBV_QP_RETRY_CNT
                | ffi::IBV_QP_RNR_RETRY
                | ffi::IBV_QP_SQ_PSN
                | ffi::IBV_QP_MAX_QP_RD_ATOMIC,
            "ibv_modify_qp RTS",
        )
    }
}
