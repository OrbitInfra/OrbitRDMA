//! Optional SRQ and UD provider resources obtained only when requested.
use super::*;

pub(crate) type Srq = Handle<ffi::ibv_srq>;
pub(crate) type DestroySrq = unsafe extern "C" fn(*mut ffi::ibv_srq) -> c_int;

type CreateSrq =
    unsafe extern "C" fn(*mut ffi::ibv_pd, *mut ffi::ibv_srq_init_attr) -> *mut ffi::ibv_srq;
impl Native {
    fn srq_ops(&self) -> RdmaResult<(CreateSrq, DestroySrq)> {
        // SAFETY: these are the system-header signatures and the Native table retains the DSO.
        unsafe {
            Ok((
                *self
                    ._library
                    .get(b"ibv_create_srq\0")
                    .map_err(|_| Error::Unsupported("ibv_create_srq"))?,
                *self
                    ._library
                    .get(b"ibv_destroy_srq\0")
                    .map_err(|_| Error::Unsupported("ibv_destroy_srq"))?,
            ))
        }
    }
    pub(crate) fn create_srq(&self, pd: Pd, depth: u32) -> RdmaResult<(Srq, DestroySrq, bool)> {
        let (create, destroy) = self.srq_ops()?;
        let mut attr = ffi::ibv_srq_init_attr {
            attr: ffi::ibv_srq_attr {
                max_wr: depth,
                max_sge: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let srq = handle(
            // SAFETY: the owner retains pd throughout SRQ life; attr is initialized and writable.
            unsafe { create(pd.0.as_ptr(), &mut attr) },
            "ibv_create_srq",
        )?;
        Ok((
            srq,
            destroy,
            attr.attr.max_wr >= depth && attr.attr.max_sge >= 1,
        ))
    }
    pub(crate) fn post_srq(
        &self,
        srq: Srq,
        id: u64,
        address: u64,
        length: u32,
        lkey: u32,
    ) -> RdmaResult<transfer::PostReceipt> {
        let ptr = srq.0.as_ptr();
        // SAFETY: the exclusively borrowed SRQ owner retains its context and provider.
        let post = unsafe { (*(*ptr).context).ops.post_srq_recv }
            .ok_or(Error::Unsupported("ibv_post_srq_recv"))?;
        let mut sge = ffi::ibv_sge {
            addr: address,
            length,
            lkey,
        };
        let mut wr = ffi::ibv_recv_wr {
            wr_id: id,
            sg_list: &mut sge,
            num_sge: 1,
            ..Default::default()
        };
        let mut bad = std::ptr::null_mut();
        // SAFETY: this stack WR lives through post; the caller retains the DMA loan until CQE or SRQ destruction.
        let code = unsafe { post(ptr, &mut wr, &mut bad) };
        Ok(transfer::PostReceipt::receipt(
            code,
            bad,
            std::slice::from_ref(&wr),
        ))
    }
}

pub(crate) type Ah = Handle<ffi::ibv_ah>;
type DestroyAh = unsafe extern "C" fn(*mut ffi::ibv_ah) -> c_int;
pub(crate) struct DatagramRoute {
    pub ah: Option<Ah>,
    destroy: DestroyAh,
    pub qpn: u32,
    pub qkey: u32,
    pub mtu: usize,
}
impl DatagramRoute {
    pub fn close(&mut self) -> std::result::Result<(), Fault> {
        destroy(&mut self.ah, "ibv_destroy_ah", self.destroy)
    }
}
impl Native {
    pub(crate) fn ud_route(
        &self,
        pd: Pd,
        local: crate::Port,
        peer: crate::DatagramAddress,
    ) -> RdmaResult<DatagramRoute> {
        type Create =
            unsafe extern "C" fn(*mut ffi::ibv_pd, *mut ffi::ibv_ah_attr) -> *mut ffi::ibv_ah;
        // SAFETY: both symbols have the shown signatures in verbs.h and the device retains this DSO.
        let (create, destroy): (Create, DestroyAh) = unsafe {
            (
                *self
                    ._library
                    .get(b"ibv_create_ah\0")
                    .map_err(|_| Error::Unsupported("ibv_create_ah"))?,
                *self
                    ._library
                    .get(b"ibv_destroy_ah\0")
                    .map_err(|_| Error::Unsupported("ibv_destroy_ah"))?,
            )
        };
        let mut attr = ffi::ibv_ah_attr {
            port_num: local.number,
            dlid: peer.lid,
            ..Default::default()
        };
        if local.link != crate::Link::InfiniBand || peer.gid != [0; 16] {
            attr.is_global = 1;
            attr.grh.dgid.raw = peer.gid;
            attr.grh.sgid_index = local.gid_index;
            attr.grh.hop_limit = 64;
        }
        // SAFETY: this owner keeps pd live and retains the resulting AH until sends have retired.
        let ah = handle(unsafe { create(pd.0.as_ptr(), &mut attr) }, "ibv_create_ah")?;
        Ok(DatagramRoute {
            ah: Some(ah),
            destroy,
            qpn: peer.qpn,
            qkey: peer.qkey,
            mtu: 128usize << local.mtu.min(peer.mtu),
        })
    }
}
