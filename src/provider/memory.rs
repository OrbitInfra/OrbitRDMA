use super::*;
/// Optional rdma-core dma-buf registration entry point. The caller owns the export FD.
#[cfg(feature = "mlx5")]
pub type DmaBufVerb =
    unsafe extern "C" fn(*mut c_void, u64, usize, u64, c_int, c_int) -> *mut c_void;
#[cfg(feature = "mlx5")]
pub fn dmabuf_verb() -> Option<DmaBufVerb> {
    crate::sys::get().ok()?.dmabuf
}
/// Backing chosen by the caller. An unavailable required export always fails closed.
#[derive(Clone, Copy)]
pub enum RegistrationSource {
    Host,
    Unavailable,
    #[cfg(feature = "mlx5")]
    DmaBuf {
        base: u64,
        size: usize,
        fd: c_int,
        register: DmaBufVerb,
    },
}
// All native CUDA MRs, including private-PD grants, pass this boundary.
/// Register a subrange on an existing protection domain.
///
/// # Safety
/// The PD, address, backing and dma-buf callback/FD if present must remain valid through deregistration.
pub unsafe fn register_on_pd(
    source: RegistrationSource,
    pd: *mut c_void,
    buffer: *mut c_void,
    length: usize,
    access: c_int,
) -> *mut c_void {
    match source {
        RegistrationSource::Unavailable => return std::ptr::null_mut(),
        RegistrationSource::Host => {}
        #[cfg(feature = "mlx5")]
        RegistrationSource::DmaBuf {
            base,
            size,
            fd,
            register,
        } => {
            let Some(offset) = (buffer as u64).checked_sub(base) else {
                return std::ptr::null_mut();
            };
            if offset
                .checked_add(length as u64)
                .is_none_or(|end| end > size as u64)
            {
                return std::ptr::null_mut();
            }
            // SAFETY: the caller retains this FD, PD and exclusive backing until the native MR retires.
            return unsafe { register(pd, offset, length, buffer as u64, fd, access) };
        }
    }
    // SAFETY: host backing or a positively selected peermem route is retained by
    // the registration owner. Export errors never reach this branch.
    unsafe { ffi::ibv_reg_mr(pd.cast(), buffer, length, access).cast() }
}

/// Register local memory, optionally enabling window binding on the parent.
///
/// # Safety
/// Provide empty exclusive output slots, a live device and stable memory through successful deregistration.
pub unsafe fn register_memory(
    handles: *mut Memory,
    device: *const DeviceHandles,
    buffer: *mut c_void,
    length: usize,
    window_bind: bool,
    source: RegistrationSource,
) -> c_int {
    // SAFETY: the MR owner retains the device and entire host registration.
    // Window-bind capability never grants access through the parent MR rkey.
    unsafe {
        (*handles).mr = register_on_pd(
            source,
            (*device).pd,
            buffer,
            length,
            (ffi::IBV_ACCESS_LOCAL_WRITE
                | if window_bind {
                    ffi::IBV_ACCESS_MW_BIND
                } else {
                    0
                }) as c_int,
        )
        .cast();
        if (*handles).mr.is_null() {
            failure()
        } else {
            0
        }
    }
}

/// Prepare an exact-range private MR or a window over caller-retained memory.
///
/// # Safety
/// Queue, device, native memory slots and registration source must agree on their protection domain and lifetime. Retain partially created handles after any error.
pub unsafe fn prepare_grant(
    handles: *mut ConnectionHandles,
    memory: *mut Memory,
    device: *const DeviceHandles,
    buffer: *mut c_void,
    length: usize,
    permission: u32,
    source: RegistrationSource,
) -> c_int {
    // SAFETY: one exclusive grant claim owns these bytes and the actual PD/QP.
    // This never resets/reconnects an existing QP or publishes its parent key.
    unsafe {
        let memory = &mut *memory;
        let device = &*device;
        if !(*handles).qp.is_null() && (*(*handles).qp.cast::<ffi::ibv_qp>()).pd != device.pd.cast()
        {
            return libc::EINVAL;
        }
        if memory.gpu_parent {
            // The local GPU MR was registered once on this PD. Authority is
            // created only after the peer accepts the connection and generation.
            if memory.mr.is_null() || permission == 0 || memory.remote_mr {
                return libc::EINVAL;
            }
            let parent = &*memory.mr.cast::<ffi::ibv_mr>();
            if parent.pd != device.pd.cast()
                || (buffer as usize)
                    .checked_sub(parent.addr as usize)
                    .and_then(|n| n.checked_add(length))
                    .is_none_or(|end| end > parent.length)
            {
                return libc::EINVAL;
            }
            return 0;
        }
        if memory.remote_mr {
            if permission == 0 || !(*handles).qp.is_null() || !memory.mr.is_null() {
                return libc::EINVAL;
            }
            (*handles).private_pd = ffi::ibv_alloc_pd(device.context.cast()).cast();
            if (*handles).private_pd.is_null() {
                return failure();
            }
            let mut access = ffi::IBV_ACCESS_LOCAL_WRITE;
            if permission & 1 != 0 {
                access |= ffi::IBV_ACCESS_REMOTE_READ;
            }
            if permission & 2 != 0 {
                access |= ffi::IBV_ACCESS_REMOTE_WRITE;
            }
            memory.mr = register_on_pd(
                source,
                (*handles).private_pd,
                buffer,
                length,
                access as c_int,
            )
            .cast();
            if memory.mr.is_null() {
                return failure();
            }
            memory.window_key = (*memory.mr.cast::<ffi::ibv_mr>()).rkey;
            return 0;
        }
        let access = ffi::IBV_ACCESS_LOCAL_WRITE
            | if permission != 0 {
                ffi::IBV_ACCESS_MW_BIND
            } else {
                0
            };
        if memory.mr.is_null() {
            memory.mr = register_on_pd(source, device.pd, buffer, length, access as c_int);
            if memory.mr.is_null() {
                return failure();
            }
        } else {
            let mr = &*memory.mr.cast::<ffi::ibv_mr>();
            if mr.pd != device.pd.cast()
                || (buffer as usize)
                    .checked_sub(mr.addr as usize)
                    .and_then(|offset| offset.checked_add(length))
                    .is_none_or(|end| end > mr.length)
            {
                return libc::EINVAL;
            }
        }
        if permission != 0 {
            return allocate_window(memory, device);
        }
        0
    }
}

/// Allocate a type-2 window and return its starting key in the supplied memory slot.
///
/// # Safety
/// Supply a live device and an exclusive empty window slot. Retain the PD until window destruction.
pub unsafe extern "C" fn allocate_window(
    memory: *mut Memory,
    device: *const DeviceHandles,
) -> c_int {
    // SAFETY: the caller owns the empty slot and retains its PD. A newly
    // allocated, never-bound MW has no parent MR or QP dependency.
    unsafe {
        let memory = &mut *memory;
        let device = &*device;
        if !memory.mw.is_null() {
            return libc::EINVAL;
        }
        let mut caps: ffi::ibv_device_attr = std::mem::zeroed();
        let code = ffi::ibv_query_device(device.context.cast(), &mut caps);
        if code != 0 {
            return code;
        }
        if caps.device_cap_flags & ffi::IBV_DEVICE_MEM_WINDOW_TYPE_2B == 0 {
            return libc::EOPNOTSUPP;
        }
        let context = &*device.context.cast::<ffi::ibv_context>();
        let Some(allocate) = context.ops.alloc_mw else {
            return libc::EOPNOTSUPP;
        };
        memory.mw = allocate(device.pd.cast(), ffi::IBV_MW_TYPE_2).cast();
        if memory.mw.is_null() {
            return failure();
        }
        let key = (*memory.mw.cast::<ffi::ibv_mw>()).rkey;
        // RdmaContext selects and permanently burns a fresh full key in its PD.
        memory.window_key = key;
        0
    }
}

/// Post a signaled window bind or local invalidation and report the proven post prefix.
///
/// # Safety
/// The caller must retain QP, MW, MR, range and writable receipt through their required completion or drain.
pub unsafe extern "C" fn window(
    handles: *mut ConnectionHandles,
    memory: *mut Memory,
    id: u64,
    address: u64,
    length: usize,
    permission: u32,
    bind: bool,
    result: *mut PostResult,
) -> c_int {
    // SAFETY: these exclusive slots and all registration handles are retained by the caller.
    let (h, m, result) = unsafe { (&*handles, &*memory, &mut *result) };
    *result = PostResult::default();
    if id == 0 || permission & !3 != 0 {
        return libc::EINVAL;
    }
    let (Ok(qp), Ok(mw), Ok(mr)) = (
        // SAFETY: the caller retains this live QP through retirement.
        unsafe { crate::sys::Qp::borrowed(h.qp) },
        // SAFETY: the caller retains this memory window for the entire command.
        unsafe { crate::sys::Mw::borrowed(m.mw) },
        // SAFETY: the caller retains its parent registration through completion.
        unsafe { crate::sys::Mr::borrowed(m.mr) },
    ) else {
        return libc::EINVAL;
    };
    let binding = bind.then_some(crate::sys::WindowBind {
        mw,
        mr,
        address,
        length,
        access: grant_access(permission),
    });
    result.record(crate::sys::post_window(qp, id, m.window_key, binding))
}

/// Destroy a window after retiring its QP access.
///
/// # Safety
/// The slot must be exclusive and its PD must remain live. An ambiguous destroy cannot be retried.
pub unsafe extern "C" fn dealloc_window(handles: *mut Memory) -> c_int {
    // SAFETY: The grant exclusively owns the window handle and keeps its PD live through
    // deallocation.
    unsafe {
        let h = &mut *handles;
        if h.mw.is_null() {
            return 0;
        }
        let mw = h.mw.cast::<ffi::ibv_mw>();
        let Some(close) = (*(*mw).context).ops.dealloc_mw else {
            return libc::EOPNOTSUPP;
        };
        let code = close(mw);
        if code == 0 {
            h.mw = std::ptr::null_mut();
        }
        code
    }
}

/// Look up the local key after checking the entire proposed interval against a live MR.
///
/// # Safety
/// The MR must still be registered, and the caller must prevent concurrent deregistration.
pub unsafe fn local_key(memory: &Memory, address: u64, length: usize) -> Result<u32, c_int> {
    let mr = std::ptr::NonNull::new(memory.mr.cast::<ffi::ibv_mr>()).ok_or(libc::EINVAL)?;
    // SAFETY: the caller has retained the registration.
    let mr = unsafe { mr.as_ref() };
    let offset = address.checked_sub(mr.addr as u64).ok_or(libc::EINVAL)?;
    if address.checked_add(length as u64).is_none()
        || offset
            .checked_add(length as u64)
            .is_none_or(|end| end > mr.length as u64)
    {
        return Err(libc::EINVAL);
    }
    Ok(mr.lkey)
}

/// Read the rkey of a caller-retained registered region.
///
/// # Safety
/// The MR slot must contain a live registration and cannot be concurrently deregistered.
pub unsafe fn remote_key(memory: &Memory) -> u32 {
    // SAFETY: registration and lifetime are guaranteed by the caller.
    unsafe { (*memory.mr.cast::<ffi::ibv_mr>()).rkey }
}

/// Prepare a type-2B window with a private bind-only MR covering exactly this
/// host range. Each concurrently open window needs its own empty handle slots.
/// Use a fresh PD-scoped key and a SharedWindowControl for its BIND and LOCAL_INV.
///
/// # Safety
/// Supply valid exclusive slots, a live matching QP/PD (or a QP yet to be
/// created in that PD), and a stable DMA loan. Retain partial construction
/// outputs on error. After local invalidation, deallocate the MW and deregister
/// the MR before releasing the loan, including on cleanup failure.
pub unsafe fn prepare_shared_window(
    connection: *mut ConnectionHandles,
    memory: &mut Memory,
    device: *const DeviceHandles,
    buffer: *mut c_void,
    length: usize,
    permission: u32,
) -> c_int {
    if !memory.mr.is_null()
        || !memory.mw.is_null()
        || memory.remote_mr
        || memory.gpu_parent
        || buffer.is_null()
        || length == 0
        || !(1..=3).contains(&permission)
    {
        return libc::EINVAL;
    }
    // SAFETY: the caller owns empty slots and retains this exact host range and PD.
    unsafe {
        prepare_grant(
            connection,
            memory,
            device,
            buffer,
            length,
            permission,
            RegistrationSource::Host,
        )
    }
}
