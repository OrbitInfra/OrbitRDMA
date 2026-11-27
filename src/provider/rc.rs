use super::*;
// The protocol grants only READ/WRITE authority, including on atomic-capable hardware.
pub fn grant_access(permission: u32) -> u32 {
    let mut access = 0;
    if permission & 1 != 0 {
        access |= ffi::IBV_ACCESS_REMOTE_READ;
    }
    if permission & 2 != 0 {
        access |= ffi::IBV_ACCESS_REMOTE_WRITE;
    }
    access
}

/// Create an RC QP on the supplied send/receive CQs and initialize its access mask.
/// On failure an allocated QP remains in the output slot until explicitly destroyed.
///
/// # Safety
/// Supply exclusive empty QP storage, a live PD and matching live CQs. Keep all parents
/// until destruction. The registration owner must already have checked memory authority.
pub unsafe fn open_rc(
    h: &mut ConnectionHandles,
    pd: *mut c_void,
    port: u8,
    permission: u32,
    max_send_wr: u32,
    max_recv_wr: u32,
) -> Result<u32, c_int> {
    if !h.qp.is_null() || permission & !3 != 0 {
        return Err(libc::EINVAL);
    }
    let api = crate::sys::get().map_err(|_| libc::ENOSYS)?;
    // SAFETY: the caller retains the PD and both correctly typed CQs through destruction.
    let (pd, send, receive) = unsafe {
        (
            crate::sys::Pd::borrowed(pd)?,
            crate::sys::Cq::borrowed(h.cq)?,
            crate::sys::Cq::borrowed(h.receive_cq)?,
        )
    };
    let (qp, qpn, sized) = api
        .qp(pd, send, receive, max_send_wr, max_recv_wr)
        .map_err(provider_errno)?;
    h.qp = qp.raw();
    if !sized {
        return Err(libc::EOPNOTSUPP);
    }
    api.init(qp, port, grant_access(permission))
        .map_err(provider_errno)?;
    Ok(qpn)
}

/// Transition an initialized RC QP through RTR and RTS using caller-provided route and depths.
///
/// # Safety
/// The connection slot must retain an exclusively controlled INIT QP, and both endpoint descriptions must be valid for this connection.
pub unsafe extern "C" fn connect(
    handles: *mut ConnectionHandles,
    local: *const Endpoint,
    remote: *const Endpoint,
    port: u8,
    gid_index: u8,
    service_level: u8,
    traffic_class: u8,
    hop_limit: u8,
) -> c_int {
    // SAFETY: the caller supplied live, initialized connection slots for this transition.
    let (h, local, peer) = unsafe { (&*handles, &*local, &*remote) };
    // SAFETY: the caller has exclusive control over the retained INIT QP.
    let qp = match unsafe { crate::sys::Qp::borrowed(h.qp) } {
        Ok(qp) => qp,
        Err(code) => return code,
    };
    let Ok(api) = crate::sys::get() else {
        return libc::ENOSYS;
    };
    api.connect(
        qp,
        crate::sys::RcPath {
            port,
            gid_index,
            global: local.link_layer == 2 || peer.gid != [0; 16],
            mtu: local.mtu.min(peer.mtu),
            remote_qpn: peer.qpn,
            remote_psn: peer.psn,
            local_psn: local.psn,
            lid: peer.lid,
            gid: peer.gid,
            initiator: h.read_depth.initiator,
            responder: h.read_depth.responder,
            service_level,
            traffic_class,
            hop_limit,
        },
    )
    .map_or_else(provider_errno, |()| 0)
}

/// Reset a terminal shared-window QP before detaching still-bound MWs.
/// Keep each private MR and its backing until all dependencies have retired.
///
/// # Safety
/// The caller has fenced this live QP and all affiliated submitters. After a
/// failed reset retain the QP, all attached windows and their registrations.
pub unsafe extern "C" fn reset_qp(h: *mut ConnectionHandles) -> c_int {
    let Ok(api) = crate::sys::get() else {
        return libc::ENOSYS;
    };
    // SAFETY: this exclusive connection remains live through the transition.
    match unsafe { crate::sys::Qp::borrowed((*h).qp) } {
        Ok(qp) => api.reset(qp).map_or_else(|fault| fault.1, |()| 0),
        Err(code) => code,
    }
}
