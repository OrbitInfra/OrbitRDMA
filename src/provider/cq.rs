//! Native completion queues, polling and completion-channel events.

use super::*;

/// Create a CQ optionally attached to a shared completion channel and validate its capacity.
///
/// # Safety
/// The device and optional channel must be live, the output empty and the capacity representable as a positive C int. Retain the CQ even if capacity validation fails.
pub unsafe extern "C" fn create_cq(
    handles: *mut CqHandles,
    device: *const DeviceHandles,
    capacity: u32,
    channel: *mut c_void,
) -> c_int {
    let Ok(depth) = i32::try_from(capacity) else {
        return libc::EINVAL;
    };
    if depth == 0 {
        return libc::EINVAL;
    }
    // SAFETY: the owner retains the output slot, context and optional channel through creation.
    let (h, ctx) = unsafe {
        (
            &mut *handles,
            crate::sys::Context::borrowed((*device).context),
        )
    };
    if !h.cq.is_null() {
        return libc::EINVAL;
    }
    let (Ok(ctx), Ok(api)) = (ctx, crate::sys::get()) else {
        return libc::EINVAL;
    };
    // SAFETY: a nonnull pointer denotes the channel retained by the caller; a null pointer is allowed.
    let ch = unsafe { crate::sys::Channel::borrowed(channel) }.ok();
    match api.cq(ctx, ch, depth) {
        Err(e) => provider_errno(e),
        Ok((cq, sized)) => {
            h.cq = cq.raw();
            if sized { 0 } else { libc::EOPNOTSUPP }
        }
    }
}

/// Create a nonblocking close-on-exec event channel.
///
/// # Safety
/// The device must be live and the slot empty. Retain a partially created channel on error.
pub unsafe extern "C" fn create_completion_channel(
    handles: *mut ChannelHandles,
    device: *const DeviceHandles,
) -> c_int {
    // SAFETY: The caller owns writable channel handles and a live device context throughout
    // creation.
    unsafe {
        (*handles).channel = ffi::ibv_create_comp_channel((*device).context.cast()).cast();
        if (*handles).channel.is_null() {
            return failure();
        }
        let fd = (*(*handles).channel.cast::<ffi::ibv_comp_channel>()).fd;
        (*handles).fd = fd;
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
            return failure();
        }
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags == -1 || libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) == -1 {
            return failure();
        }
        0
    }
}

/// Consume a single notification and return its CQ.
///
/// # Safety
/// The channel must be live, the output writable and the corresponding CQ retained through acknowledgment.
pub unsafe extern "C" fn get_completion_event(channel: *mut c_void, cq: *mut *mut c_void) -> c_int {
    // SAFETY: The monitor retains the channel and provides a writable CQ output for this event.
    unsafe {
        let mut native_cq = std::ptr::null_mut();
        let mut context = std::ptr::null_mut();
        if ffi::ibv_get_cq_event(channel.cast(), &mut native_cq, &mut context) != 0 {
            return failure();
        }
        *cq = native_cq.cast();
        0
    }
}

/// Acknowledge one consumed CQ notification.
///
/// # Safety
/// The CQ must be live with one unacknowledged event.
pub unsafe extern "C" fn ack_completion_event(cq: *mut c_void) {
    // SAFETY: The monitor obtained one unacknowledged event for this CQ from the open channel.
    unsafe { ffi::ibv_ack_cq_events(cq.cast(), 1) };
}

/// Arm a CQ notification for the next completion.
///
/// # Safety
/// The CQ and its context must be live through this call.
pub unsafe extern "C" fn arm_completion(cq: *mut c_void) -> c_int {
    // SAFETY: The lane owns the live CQ while its provider notification callback is invoked.
    unsafe {
        let cq = cq.cast::<ffi::ibv_cq>();
        let Some(arm) = (*(*cq).context).ops.req_notify_cq else {
            return libc::EOPNOTSUPP;
        };
        arm(cq, 0)
    }
}
