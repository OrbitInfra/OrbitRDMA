//! Provider operations for engines that already own their QPs, shared CQs and memory.
//! These operations retain partial native handles in the caller's output slots. The
//! caller must arrange completions, retirement, and quarantine on a failed destroy.
#![allow(unsafe_code)]
use std::ffi::{CStr, c_char, c_int, c_void};
pub mod abi;
use abi as ffi;
mod cq;
mod device;
pub mod inventory;
mod memory;
pub use crate::keys::KeyHistory;
pub use crate::window::control::SharedWindowControl;
#[cfg(feature = "mlx5")]
pub mod mlx5;
pub mod port;
mod rc;
pub use cq::*;
pub use device::*;
pub use memory::*;
pub use rc::*;
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadDepth {
    pub initiator: u8,
    pub responder: u8,
}
impl ReadDepth {
    pub fn clamped(self, limits: Self) -> Self {
        // TENT preserves a requested zero and uses one when the HCA reports no limit.
        Self {
            initiator: self.initiator.min(limits.initiator.max(1)),
            responder: self.responder.min(limits.responder.max(1)),
        }
    }
}

#[repr(C)]
pub struct DeviceHandles {
    pub context: *mut c_void,
    pub pd: *mut c_void,
    pub async_fd: c_int,
    pub read_limits: ReadDepth,
    pub supports_atomics: bool,
    pub request_gpu_keys: bool,
    pub gpu_keys: GpuKeyRange,
}

impl Default for DeviceHandles {
    fn default() -> Self {
        Self {
            context: std::ptr::null_mut(),
            pd: std::ptr::null_mut(),
            async_fd: -1,
            read_limits: ReadDepth::default(),
            supports_atomics: false,
            request_gpu_keys: false,
            gpu_keys: Default::default(),
        }
    }
}

// SAFETY: libibverbs contexts and PDs support calls from multiple threads. RdmaContext owns
// these handles behind a mutex and retains them through every child endpoint.
unsafe impl Send for DeviceHandles {}

#[repr(C)]
pub struct ConnectionHandles {
    pub cq: *mut c_void,
    pub receive_cq: *mut c_void,
    pub qp: *mut c_void,
    pub private_pd: *mut c_void,
    pub read_depth: ReadDepth,
}

impl Default for ConnectionHandles {
    fn default() -> Self {
        Self {
            cq: std::ptr::null_mut(),
            receive_cq: std::ptr::null_mut(),
            qp: std::ptr::null_mut(),
            private_pd: std::ptr::null_mut(),
            read_depth: ReadDepth::default(),
        }
    }
}

#[repr(C)]
pub struct CqHandles {
    pub cq: *mut c_void,
}

impl Default for CqHandles {
    fn default() -> Self {
        Self {
            cq: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
pub struct ChannelHandles {
    pub channel: *mut c_void,
    pub fd: c_int,
}

impl Default for ChannelHandles {
    fn default() -> Self {
        Self {
            channel: std::ptr::null_mut(),
            fd: -1,
        }
    }
}

// SAFETY: Access is serialized by the completion-channel owner; its consumer is joined first.
unsafe impl Send for ChannelHandles {}

// General Services QP: never an RC data QP. Nonzero so a provider that
// clears or ignores the requested QPN cannot pass the readback probe.
pub const GPU_PROBE_QPN: u32 = 1;

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct GpuKeyRange {
    pub pdn: u32,
    pub base: u32,
    pub count: u32,
}

pub struct AsyncEvent {
    pub kind: u32,
    pub port: u8,
    native: std::mem::MaybeUninit<ffi::ibv_async_event>,
}

impl Default for AsyncEvent {
    fn default() -> Self {
        Self {
            kind: 0,
            port: 0,
            native: std::mem::MaybeUninit::uninit(),
        }
    }
}

/// Native registration and authorization handles retained by their caller through retirement.
#[repr(C)]
pub struct Memory {
    pub mr: *mut c_void,
    pub mw: *mut c_void,
    pub window_key: u32,
    /// Allocate an exact-range MR on a fresh protection domain for this connection.
    pub remote_mr: bool,
    /// The caller already holds a local-only parent for an indirect mlx5 key.
    pub gpu_parent: bool,
    pub gpu_key: *mut c_void,
}
impl Default for Memory {
    fn default() -> Self {
        Self {
            mr: std::ptr::null_mut(),
            mw: std::ptr::null_mut(),
            window_key: 0,
            remote_mr: false,
            gpu_parent: false,
            gpu_key: std::ptr::null_mut(),
        }
    }
}

/// Native connection address. It carries no remote memory authority.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct Endpoint {
    pub qpn: u32,
    pub psn: u32,
    pub lid: u16,
    pub mtu: u8,
    pub link_layer: u8,
    pub gid: [u8; 16],
}
/// Load and retain the system libibverbs table. Failure is cached process-wide.
pub fn install() -> Result<(), crate::LoadError> {
    crate::sys::get().map(|_| ())
}

/// Static diagnostic suitable for engines using allocation-free error variants.
pub fn load_diagnostic(error: &crate::LoadError) -> &'static str {
    crate::sys::unavailable(error)
}
fn failure() -> c_int {
    std::io::Error::last_os_error()
        .raw_os_error()
        .filter(|code| *code != 0)
        .unwrap_or(libc::EIO)
}

macro_rules! destroy {
    ($name:ident, $owner:ident, $field:ident, $verb:ident) => {
        #[doc = concat!("Retire the native handle with ", stringify!($verb), " and clear it only on success.")]
        ///
        /// # Safety
        /// The caller must exclusively own the initialized slot, drain its children first,
        /// and retain the backing on failure. An ambiguous destruction must never be retried.
        pub unsafe extern "C" fn $name(h: *mut $owner) -> c_int {
            // SAFETY: Resources owns every handle; clearing follows native
            // success only, and the caller stops before destroying dependencies.
            unsafe {
                let handle = &mut (*h).$field;
                if handle.is_null() {
                    return 0;
                }
                let error = ffi::$verb((*handle).cast());
                if error == 0 {
                    *handle = std::ptr::null_mut();
                }
                error
            }
        }
    };
}
destroy!(destroy_qp, ConnectionHandles, qp, ibv_destroy_qp);
destroy!(deregister, Memory, mr, ibv_dereg_mr);
destroy!(destroy_cq, CqHandles, cq, ibv_destroy_cq);
destroy!(
    close_completion_channel,
    ChannelHandles,
    channel,
    ibv_destroy_comp_channel
);
destroy!(dealloc_pd, DeviceHandles, pd, ibv_dealloc_pd);
destroy!(close_device, DeviceHandles, context, ibv_close_device);

#[derive(Default)]
#[repr(C)]
pub struct PostResult {
    pub accepted: usize,
    pub uncertain: bool,
}

pub fn provider_errno(error: crate::Error) -> c_int {
    match error {
        crate::Error::Invalid(_) => libc::EINVAL,
        crate::Error::Exhausted(_) => libc::ENOMEM,
        crate::Error::Unsupported(_) => libc::EOPNOTSUPP,
        // A positive count other than one exceeds the single supplied CQ slot.
        crate::Error::Native {
            operation: "ibv_poll_cq",
            code,
        } if code > 0 => libc::EPROTO,
        crate::Error::Native { code, .. } if code != 0 => code.checked_abs().unwrap_or(libc::EIO),
        crate::Error::Load(_) => libc::ENOSYS,
        _ => libc::EPROTO,
    }
}

impl PostResult {
    pub fn record(
        &mut self,
        posted: std::result::Result<crate::verbs::PostReceipt, crate::Error>,
    ) -> c_int {
        match posted {
            Ok(receipt) => {
                self.accepted = receipt.accepted.unwrap_or(0);
                self.uncertain = receipt.accepted.is_none();
                receipt.code
            }
            Err(error) => provider_errno(error),
        }
    }
}
