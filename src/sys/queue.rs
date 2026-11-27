//! Adapted from OrbiTE sys/qp.rs. RC attributes and masks are paired by transition.
use super::*;

pub(crate) type Channel = Handle<ffi::ibv_comp_channel>;
pub(crate) type Cq = Handle<ffi::ibv_cq>;
pub(crate) type Qp = Handle<ffi::ibv_qp>;

/// Route and negotiated read limits to program in the two RC transitions.
pub(crate) struct RcPath {
    pub port: u8,
    pub gid_index: u8,
    pub global: bool,
    pub mtu: u8,
    pub remote_qpn: u32,
    pub remote_psn: u32,
    pub local_psn: u32,
    pub lid: u16,
    pub gid: [u8; 16],
    pub initiator: u8,
    pub responder: u8,
    pub service_level: u8,
    pub traffic_class: u8,
    pub hop_limit: u8,
}

pub(crate) struct Creation {
    pub send_depth: u32,
    pub receive_depth: u32,
    pub max_sge: u32,
    pub srq: Option<shared::Srq>,
    pub kind: u32,
}

impl Native {
    pub(crate) fn channel(&self, ctx: Context) -> RdmaResult<Channel> {
        handle(
            // SAFETY: the queue owner retains the context until channel destruction.
            unsafe { (self.ibv_create_comp_channel)(ctx.0.as_ptr()) },
            "ibv_create_comp_channel",
        )
    }
    pub(crate) fn cq(
        &self,
        ctx: Context,
        channel: Option<Channel>,
        depth: i32,
    ) -> RdmaResult<(Cq, bool)> {
        let cq = handle(
            // SAFETY: this channel belongs to ctx and outlives the new CQ.
            unsafe {
                (self.ibv_create_cq)(
                    ctx.0.as_ptr(),
                    depth,
                    std::ptr::null_mut(),
                    channel.map_or(std::ptr::null_mut(), |ch| ch.0.as_ptr()),
                    0,
                )
            },
            "ibv_create_cq",
        )?;
        // SAFETY: the newly created CQ reports its actual number of entries.
        Ok((cq, unsafe { (*cq.0.as_ptr()).cqe } >= depth))
    }
    pub(crate) fn qp(
        &self,
        pd: Pd,
        send: Cq,
        receive: Cq,
        config: Creation,
    ) -> RdmaResult<(Qp, u32, bool)> {
        let Creation {
            send_depth,
            receive_depth,
            max_sge,
            srq,
            kind,
        } = config;
        let mut init = ffi::ibv_qp_init_attr {
            send_cq: send.0.as_ptr(),
            recv_cq: receive.0.as_ptr(),
            qp_type: kind,
            srq: srq.map_or(std::ptr::null_mut(), |s| s.0.as_ptr()),
            cap: ffi::ibv_qp_cap {
                max_send_wr: send_depth,
                max_recv_wr: receive_depth,
                max_send_sge: max_sge,
                max_recv_sge: max_sge,
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
            init.cap.max_send_wr >= send_depth
                && (srq.is_some() || init.cap.max_recv_wr >= receive_depth)
                && init.cap.max_send_sge >= max_sge
                && (srq.is_some() || init.cap.max_recv_sge >= max_sge),
        ))
    }
    fn modify_raw(&self, qp: Qp, mut attr: ffi::ibv_qp_attr, mask: u32) -> c_int {
        // SAFETY: the unique queue owner selects the legal mask and serializes native changes.
        unsafe { (self.ibv_modify_qp)(qp.0.as_ptr(), &mut attr, mask as c_int) }
    }
    fn modify(
        &self,
        qp: Qp,
        attr: ffi::ibv_qp_attr,
        mask: u32,
        op: &'static str,
    ) -> RdmaResult<()> {
        check(op, self.modify_raw(qp, attr, mask))
    }
    #[cfg(feature = "cm")]
    pub(crate) fn error_state(&self, qp: Qp) -> std::result::Result<(), Fault> {
        let rc = self.modify_raw(
            qp,
            ffi::ibv_qp_attr {
                qp_state: ffi::IBV_QPS_ERR,
                ..Default::default()
            },
            ffi::IBV_QP_STATE,
        );
        if rc == 0 {
            Ok(())
        } else {
            Err(Fault("ibv_modify_qp ERR", rc))
        }
    }
    pub(crate) fn reset(&self, qp: Qp) -> std::result::Result<(), Fault> {
        let attr = ffi::ibv_qp_attr {
            qp_state: ffi::IBV_QPS_RESET,
            ..Default::default()
        };
        match self.modify_raw(qp, attr, ffi::IBV_QP_STATE) {
            0 => Ok(()),
            code => Err(Fault("ibv_modify_qp RESET", code)),
        }
    }
    pub(crate) fn init(&self, qp: Qp, port: u8, access: u32) -> RdmaResult<()> {
        let attr = ffi::ibv_qp_attr {
            qp_state: ffi::IBV_QPS_INIT,
            port_num: port,
            qp_access_flags: access,
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
    pub(crate) fn start_ud(&self, qp: Qp, port: u8, qkey: u32, psn: u32) -> RdmaResult<()> {
        self.modify(
            qp,
            ffi::ibv_qp_attr {
                qp_state: ffi::IBV_QPS_INIT,
                port_num: port,
                qkey,
                ..Default::default()
            },
            ffi::IBV_QP_STATE | ffi::IBV_QP_PKEY_INDEX | ffi::IBV_QP_PORT | ffi::IBV_QP_QKEY,
            "ibv_modify_qp UD INIT",
        )?;
        self.modify(
            qp,
            ffi::ibv_qp_attr {
                qp_state: ffi::IBV_QPS_RTR,
                ..Default::default()
            },
            ffi::IBV_QP_STATE,
            "ibv_modify_qp UD RTR",
        )?;
        self.modify(
            qp,
            ffi::ibv_qp_attr {
                qp_state: ffi::IBV_QPS_RTS,
                sq_psn: psn,
                ..Default::default()
            },
            ffi::IBV_QP_STATE | ffi::IBV_QP_SQ_PSN,
            "ibv_modify_qp UD RTS",
        )
    }
    pub(crate) fn connect(&self, qp: Qp, path: RcPath) -> RdmaResult<()> {
        let mut attr = ffi::ibv_qp_attr {
            qp_state: ffi::IBV_QPS_RTR,
            path_mtu: u32::from(path.mtu),
            dest_qp_num: path.remote_qpn,
            rq_psn: path.remote_psn,
            max_dest_rd_atomic: path.responder,
            min_rnr_timer: 12,
            ..Default::default()
        };
        attr.ah_attr.port_num = path.port;
        attr.ah_attr.sl = path.service_level;
        attr.ah_attr.dlid = path.lid;
        if path.global {
            attr.ah_attr.is_global = 1;
            attr.ah_attr.grh.dgid.raw = path.gid;
            attr.ah_attr.grh.sgid_index = path.gid_index;
            attr.ah_attr.grh.hop_limit = path.hop_limit;
            attr.ah_attr.grh.traffic_class = path.traffic_class;
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
            sq_psn: path.local_psn,
            max_rd_atomic: path.initiator,
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
