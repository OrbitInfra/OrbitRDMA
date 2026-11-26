//! Adapted from OrbiTE's sys/memory.rs. Only the exact bound window grants remote access.
use super::transfer::PostReceipt;
use super::*;
use crate::RemoteAccess;

pub(crate) type Mr = Handle<ffi::ibv_mr>;
pub(crate) type Mw = Handle<ffi::ibv_mw>;

pub(crate) struct Bind {
    pub mw: Mw,
    pub mr: Mr,
    pub address: u64,
    pub length: usize,
    pub access: RemoteAccess,
}
impl Native {
    pub(crate) fn register(
        &self,
        pd: Pd,
        ptr: NonNull<u8>,
        len: usize,
        bind: bool,
    ) -> RdmaResult<Mr> {
        let mut access = ffi::IBV_ACCESS_LOCAL_WRITE;
        if bind {
            access |= ffi::IBV_ACCESS_MW_BIND;
        }
        handle(
            // SAFETY: the caller owns the entire stable host range and PD until successful deregistration.
            unsafe { (self.ibv_reg_mr)(pd.0.as_ptr(), ptr.as_ptr().cast(), len, access as c_int) },
            "ibv_reg_mr",
        )
    }
    pub(crate) fn window(&self, pd: Pd) -> RdmaResult<(Mw, u32)> {
        // SAFETY: this PD and its context remain live. Provider ops are immutable for the context.
        let ops = unsafe { &(*(*pd.0.as_ptr()).context).ops };
        let alloc = ops
            .alloc_mw
            .ok_or(Error::Unsupported("type 2 MW allocation"))?;
        if ops.dealloc_mw.is_none() || ops.post_send.is_none() || ops.poll_cq.is_none() {
            return Err(Error::Unsupported("window bind, completion and retirement"));
        }
        let mw = handle(
            // SAFETY: capability was checked at registration; the window owner keeps pd alive.
            unsafe { alloc(pd.0.as_ptr(), ffi::IBV_MW_TYPE_2) },
            "ibv_alloc_mw",
        )?;
        // SAFETY: this is the initial key of the newly allocated native window.
        Ok((mw, unsafe { (*mw.0.as_ptr()).rkey }))
    }
    pub(crate) fn free_window(&self, slot: &mut Option<Mw>) -> std::result::Result<(), Fault> {
        if let Some(mw) = *slot {
            // SAFETY: the owner retains the MW's PD and context until the destroy receipt.
            let close = unsafe { (*(*mw.0.as_ptr()).context).ops.dealloc_mw }
                .ok_or(Fault("ibv_dealloc_mw", libc::ENOSYS))?;
            destroy(slot, "ibv_dealloc_mw", close)?;
        }
        Ok(())
    }
    pub(crate) fn post_window(&self, qp: Qp, key: u32, binding: Option<Bind>) -> RdmaResult<()> {
        let id = if binding.is_some() { 1 } else { 2 };
        let binding = binding.map(|b| WindowBind {
            mr: b.mr,
            mw: b.mw,
            address: b.address,
            length: b.length,
            access: match b.access {
                RemoteAccess::Read => ffi::IBV_ACCESS_REMOTE_READ,
                RemoteAccess::Write => ffi::IBV_ACCESS_REMOTE_WRITE,
            },
        });
        let result = post_window(qp, id, key, binding)?;
        check("ibv_post_send window", result.code)
    }

    pub(crate) fn completion(&self, cq: Cq, qpn: u32, bind: bool) -> RdmaResult<bool> {
        let Some(wc) = self.poll_one(cq)? else {
            return Ok(false);
        };
        let expected_id = if bind { 1 } else { 2 };
        let expected_opcode = if bind {
            ffi::IBV_WC_BIND_MW
        } else {
            ffi::IBV_WC_LOCAL_INV
        };
        match wc {
            WorkCompletion::Error {
                id,
                qpn: which,
                status,
                ..
            } if id == expected_id && which == qpn => Err(Error::Native {
                operation: "window completion",
                code: status as i32,
            }),
            WorkCompletion::Success {
                id,
                qpn: which,
                opcode,
                ..
            } if id == expected_id && which == qpn && opcode == expected_opcode => Ok(true),
            _ => Err(Error::Invalid("foreign window CQE or opcode")),
        }
    }
}

pub(crate) struct WindowBind {
    pub mw: Mw,
    pub mr: Mr,
    pub address: u64,
    pub length: usize,
    pub access: u32,
}
pub(crate) fn post_window(
    qp: Qp,
    id: u64,
    key: u32,
    binding: Option<WindowBind>,
) -> RdmaResult<PostReceipt> {
    let mut wr = ffi::ibv_send_wr {
        wr_id: id,
        send_flags: ffi::IBV_SEND_SIGNALED,
        ..Default::default()
    };
    if let Some(b) = binding {
        wr.opcode = ffi::IBV_WR_BIND_MW;
        wr.__bindgen_anon_2.bind_mw.mw = b.mw.0.as_ptr();
        wr.__bindgen_anon_2.bind_mw.rkey = key;
        wr.__bindgen_anon_2.bind_mw.bind_info = ffi::ibv_mw_bind_info {
            mr: b.mr.0.as_ptr(),
            addr: b.address,
            length: b.length as u64,
            mw_access_flags: b.access,
        };
    } else {
        wr.opcode = ffi::IBV_WR_LOCAL_INV;
        wr.__bindgen_anon_1.invalidate_rkey = key;
    }
    // SAFETY: the QP's owner serializes posts and retains every referenced resource.
    let post = unsafe { (*(*qp.0.as_ptr()).context).ops.post_send }
        .ok_or(Error::Unsupported("post_send"))?;
    let mut bad = std::ptr::null_mut();
    // SAFETY: the provider consumes the stack WR during this call. On any
    // submit error the owner tears down the QP before freeing potential DMA targets.
    let code = unsafe { post(qp.0.as_ptr(), &mut wr, &mut bad) };
    Ok(PostReceipt::receipt(code, bad, std::slice::from_ref(&wr)))
}
