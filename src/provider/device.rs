use super::*;
/// List verbs devices together with their kernel device-node names.
pub fn devices() -> Result<Vec<(String, String)>, crate::Error> {
    crate::sys::get()
        .map_err(crate::Error::Load)?
        .device_nodes()
        .map_err(crate::Error::DeviceList)
}

/// Query the address of an active RC port.
///
/// # Safety
/// The supplied device slot must retain a live verbs context throughout both native queries.
pub unsafe fn query_descriptor(
    device: *const DeviceHandles,
    port: u8,
    gid_index: u8,
) -> std::result::Result<Endpoint, c_int> {
    // SAFETY: The caller retains the opened device throughout the synchronous descriptor query.
    let (descriptor, active, _) = unsafe { read_port(device, port, gid_index) }?;
    if !active {
        return Err(libc::ENETDOWN);
    }
    Ok(descriptor)
}

/// Read address, link state and negotiated speed, including inactive ports.
///
/// # Safety
/// The device slot must identify an open context that remains live during this query.
pub unsafe fn read_port(
    device: *const DeviceHandles,
    port: u8,
    gid_index: u8,
) -> std::result::Result<(Endpoint, bool, Option<u64>), c_int> {
    // SAFETY: The caller retains the device context; the port query and gid query write to
    // local outputs.
    unsafe {
        let attr = super::port::query_port((*device).context.cast(), port)?;
        let active = attr.state == ffi::IBV_PORT_ACTIVE;
        let mtu = attr.active_mtu as u8;
        if !matches!(attr.link_layer, 1 | 2)
            || !(1..=5).contains(&attr.state)
            || mtu > 5
            || (active && mtu == 0)
            || i32::from(gid_index) >= attr.gid_tbl_len
        {
            return Err(libc::EADDRNOTAVAIL);
        }
        let mut gid = ffi::ibv_gid { raw: [0; 16] };
        let code = ffi::ibv_query_gid(
            (*device).context.cast(),
            port,
            i32::from(gid_index),
            &mut gid,
        );
        if code != 0 {
            return Err(code);
        }
        if gid.raw == [0; 16] || std::net::Ipv6Addr::from(gid.raw).is_multicast() {
            return Err(libc::EADDRNOTAVAIL);
        }
        Ok((
            Endpoint {
                lid: attr.lid,
                mtu,
                link_layer: attr.link_layer,
                gid: gid.raw,
                ..Endpoint::default()
            },
            active,
            super::port::link_speed_mbps(&attr),
        ))
    }
}

/// GID identity reported by the kernel for an exact physical port and slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GidEntry {
    pub gid: [u8; 16],
    pub gid_index: u32,
    pub port_num: u32,
    pub gid_type: u32,
    pub ndev_ifindex: u32,
}
/// Read a GID entry, including the kernel network interface index used for route validation.
///
/// # Safety
/// The device context must stay live until the query returns.
pub unsafe fn gid_entry(device: &DeviceHandles, port: u8, index: u8) -> Result<GidEntry, i32> {
    let mut entry = ffi::ibv_gid_entry::default();
    // SAFETY: the caller owns the context and the local output covers the full header layout.
    let code = unsafe {
        ffi::_ibv_query_gid_ex(
            device.context.cast(),
            port.into(),
            index.into(),
            &mut entry,
            0,
            size_of_val(&entry),
        )
    };
    if code != 0 {
        return Err(code);
    }
    // SAFETY: the successful native query initialized all sixteen bytes of this GID.
    let gid = unsafe { entry.gid.raw };
    Ok(GidEntry {
        gid,
        gid_index: entry.gid_index,
        port_num: entry.port_num,
        gid_type: entry.gid_type,
        ndev_ifindex: entry.ndev_ifindex,
    })
}
/// Open the named HCA, query limits and allocate a PD. Partial results stay in the slots.
///
/// # Safety
/// Supply an exclusive empty handle set and a valid NUL-terminated device name. Retain every acquired handle on error.
pub unsafe extern "C" fn open_device(handles: *mut DeviceHandles, name: *const c_char) -> c_int {
    // SAFETY: RdmaContext owns initialized handles, including partial construction.
    unsafe {
        let h = &mut *handles;
        let mut count = 0;
        let devices = ffi::ibv_get_device_list(&mut count);
        if devices.is_null() {
            return failure();
        }
        let mut error = libc::ENODEV;
        for index in 0..count {
            let device = *devices.add(index as usize);
            let device_name = ffi::ibv_get_device_name(device);
            if !device_name.is_null() && CStr::from_ptr(name) == CStr::from_ptr(device_name) {
                #[cfg(feature = "mlx5")]
                if h.request_gpu_keys {
                    h.context = super::mlx5::open(device);
                }
                if h.context.is_null() {
                    h.context = ffi::ibv_open_device(device).cast();
                    h.request_gpu_keys = false;
                }
                error = if h.context.is_null() { failure() } else { 0 };
                break;
            }
        }
        ffi::ibv_free_device_list(devices);
        if error != 0 {
            return error;
        }
        let mut attr = ffi::ibv_device_attr::default();
        let code = ffi::ibv_query_device(h.context.cast(), &mut attr);
        if code != 0 {
            return code;
        }
        h.read_limits = ReadDepth {
            initiator: attr.max_qp_init_rd_atom.clamp(0, 255) as u8,
            responder: attr.max_qp_rd_atom.clamp(0, 255) as u8,
        };
        h.supports_atomics = attr.atomic_cap != ffi::IBV_ATOMIC_NONE;
        h.async_fd = (*h.context.cast::<ffi::ibv_context>()).async_fd;
        let flags = libc::fcntl(h.async_fd, libc::F_GETFL);
        if flags == -1 || libc::fcntl(h.async_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return failure();
        }
        h.pd = ffi::ibv_alloc_pd(h.context.cast()).cast();
        if h.pd.is_null() {
            return failure();
        }
        #[cfg(feature = "mlx5")]
        if h.request_gpu_keys && attr.device_cap_flags & ffi::IBV_DEVICE_MEM_WINDOW_TYPE_2B != 0 {
            h.gpu_keys = super::mlx5::caps(h);
        }
        0
    }
}

/// Read one device event without blocking.
///
/// # Safety
/// The context must be live and event must be exclusively writable. A successful event must be acknowledged once.
pub unsafe extern "C" fn get_async_event(context: *mut c_void, event: *mut AsyncEvent) -> c_int {
    // SAFETY: the context owner keeps the native fd/context alive and is its
    // sole consumer. The provider initializes native only on successful get.
    unsafe {
        if ffi::ibv_get_async_event(context.cast(), (*event).native.as_mut_ptr()) != 0 {
            return failure();
        }
        let native = &*(*event).native.as_ptr();
        (*event).kind = native.event_type;
        if port_event(native.event_type) {
            (*event).port = u8::try_from(native.element.port_num).unwrap_or(0);
        }
        0
    }
}

/// Acknowledge one native asynchronous event.
///
/// # Safety
/// The event must hold one successful get from this live context and must never have been acknowledged.
pub unsafe extern "C" fn ack_async_event(_: *mut c_void, event: *mut AsyncEvent) {
    // SAFETY: each successful get is acknowledged once before touching locks;
    // no QP/CQ pointer from this event is accessed after acknowledgment.
    unsafe { ffi::ibv_ack_async_event((*event).native.as_mut_ptr()) };
}
pub fn port_event(kind: u32) -> bool {
    matches!(
        kind,
        ffi::IBV_EVENT_PORT_ERR
            | ffi::IBV_EVENT_PORT_ACTIVE
            | ffi::IBV_EVENT_GID_CHANGE
            | ffi::IBV_EVENT_LID_CHANGE
    )
}
