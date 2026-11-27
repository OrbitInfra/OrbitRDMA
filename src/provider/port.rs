use super::*;
type QueryPortSpeed = unsafe extern "C" fn(*mut ffi::ibv_context, u32, *mut u64) -> c_int;
#[derive(Clone, Copy, Debug, Default)]
pub struct PortAttributes {
    pub state: u32,
    pub active_mtu: u32,
    pub gid_tbl_len: i32,
    pub active_speed: u32,
    pub lid: u16,
    pub link_layer: u8,
    pub active_width: u8,
}
unsafe fn query_with(
    context: *mut ffi::ibv_context,
    port: u8,
    legacy: Option<crate::sys::port::LegacyQuery>,
) -> Result<PortAttributes, c_int> {
    let mut a = ffi::ibv_port_attr::default();
    // SAFETY: the caller retains the live context, provider and optional legacy callback.
    let code = unsafe { crate::sys::port::query(context, port, &mut a, legacy) };
    if code != 0 {
        return Err(code);
    }
    let active_speed = u32::from(a.active_speed);
    #[cfg(orbit_active_speed_ex)]
    let active_speed = if a.active_speed_ex != 0 {
        a.active_speed_ex
    } else {
        active_speed
    };
    Ok(PortAttributes {
        state: a.state,
        active_mtu: a.active_mtu,
        gid_tbl_len: a.gid_tbl_len,
        active_speed,
        lid: a.lid,
        link_layer: a.link_layer,
        active_width: a.active_width,
    })
}
/// Query the provider port attributes.
///
/// # Safety
/// The caller retains the open context throughout the native call.
pub(super) unsafe fn query_port(
    context: *mut ffi::ibv_context,
    port: u8,
) -> Result<PortAttributes, c_int> {
    let legacy = crate::sys::get().ok().map(|t| t.ibv_query_port);
    // SAFETY: inherited context lifetime and the process table retains the callback.
    unsafe { query_with(context, port, legacy) }
}
pub fn link_speed_mbps(attr: &PortAttributes) -> Option<u64> {
    let speed = attr.active_speed;
    let lane = match speed {
        1 => 2_500,
        2 => 5_000,
        4 | 8 => 10_000,
        16 => 14_000,
        32 => 25_000,
        64 => 50_000,
        128 => 100_000,
        256 => 200_000,
        _ => return None,
    };
    let lanes = match attr.active_width {
        1 => 1,
        2 => 4,
        4 => 8,
        8 => 12,
        16 => 2,
        _ => return None,
    };
    Some(lane * lanes)
}

/// Return the optional effective port rate in Mb/s.
///
/// # Safety
/// The device pointer must reference a live context for the whole call.
pub unsafe fn query_port_speed(
    device: *const DeviceHandles,
    port: u8,
) -> Result<Option<u64>, crate::Error> {
    match crate::sys::get().map_err(crate::Error::Load)?.port_speed {
        // SAFETY: the caller owns the open context; the process-lifetime table owns the symbol.
        Some(query) => unsafe { effective_speed(query, (*device).context.cast(), port).map(Some) },
        None => Ok(None),
    }
}

unsafe fn effective_speed(
    query: QueryPortSpeed,
    context: *mut ffi::ibv_context,
    port: u8,
) -> Result<u64, crate::Error> {
    let mut speed = 0;
    // SAFETY: The retained context and port are passed to the loaded ABI with writable u64
    // output.
    let code = unsafe { query(context, u32::from(port), &mut speed) };
    if code != 0 {
        return Err(crate::Error::Native {
            operation: "ibv_query_port_speed",
            code,
        });
    }
    // The native result is in 100 Mb/s units, not Mb/s or bytes/s.
    speed.checked_mul(100).ok_or(crate::Error::Native {
        operation: "ibv_query_port_speed overflow",
        code: libc::EOVERFLOW,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/port.rs"]
mod tests;
