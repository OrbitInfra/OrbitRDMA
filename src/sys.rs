// Host verbs table and CPU fixture adapted from OrbiTE. See the README for provenance.
#![allow(unsafe_code)]

use std::ffi::{CStr, OsStr, OsString, c_char, c_int, c_uint, c_void};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::ptr::NonNull;
use std::sync::OnceLock;

use libloading::Library;

use crate::LoadError;

mod device;
mod memory;
pub(crate) mod port;
mod queue;
pub(crate) mod transfer;
pub(crate) use device::{Caps, Context, Pd};
pub(crate) use memory::{Bind, Mr, Mw};
pub(crate) use memory::{WindowBind, post_window};
pub(crate) use queue::{Channel, Cq, Qp, RcPath};
pub(crate) const RC_READ_WRITE: u32 = ffi::IBV_ACCESS_REMOTE_READ | ffi::IBV_ACCESS_REMOTE_WRITE;
pub(crate) use transfer::{DataOpcode, WorkCompletion, local_key, storage};

// Private C layouts generated from the build host's verbs.h.
#[allow(
    dead_code,
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case
)]
#[allow(clippy::all, clippy::undocumented_unsafe_blocks)]
pub(crate) mod ffi {
    include!(concat!(env!("OUT_DIR"), "/verbs.rs"));
}

type PortSpeed = unsafe extern "C" fn(*mut ffi::ibv_context, u32, *mut u64) -> c_int;

macro_rules! exports {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $ret:ty = $missing:expr;)+) => {
        pub(crate) struct Native {
            // Loading the whole host ABI is part of discovery, including exports
            // that will first be called when resource owners are added.
            $(#[allow(dead_code)] pub(crate) $name: unsafe extern "C" fn($($ty),*) -> $ret,)+
            #[allow(dead_code)]
            pub(crate) port_speed: Option<PortSpeed>,
            #[cfg(feature = "mlx5")]
            pub(crate) dmabuf: Option<crate::provider::DmaBufVerb>,
            _library: Library,
        }

        impl Native {
            fn load(path: &OsStr) -> Result<Self, LoadError> {
                // SAFETY: the system soname names the trusted host verbs runtime;
                // tests supply DSOs built locally against the same system headers.
                let library = unsafe { Library::new(path) }
                    .map_err(|error| LoadError::Unavailable(error.to_string()))?;
                Ok(Self {
                    $(
                        // SAFETY: each type matches the named verbs.h export. The
                        // table owns the DSO for the lifetime of every pointer.
                        $name: *unsafe { library.get(concat!(stringify!($name), "\0").as_bytes()) }
                            .map_err(|_| LoadError::MissingSymbol(stringify!($name)))?,
                    )+
                    // SAFETY: newer rdma-core provides this port-speed ABI; older
                    // versions omit the symbol. Its owner is the same DSO.
                    port_speed: unsafe { library.get::<PortSpeed>(b"ibv_query_port_speed\0") }
                        .ok().map(|symbol| *symbol),
                    #[cfg(feature = "mlx5")]
                    // SAFETY: the optional dma-buf entry point has the rdma-core ABI and the table retains the DSO.
                    dmabuf: unsafe { library.get::<crate::provider::DmaBufVerb>(b"ibv_reg_dmabuf_mr\0") }
                        .ok().map(|symbol| *symbol),
                    _library: library,
                })
            }
        }

        #[cfg(feature = "provider")]
        pub(crate) mod callbacks {
            use super::{c_char,c_int,c_uint,c_void,ffi};
            $(
            #[doc = concat!("Call the loaded rdma-core `", stringify!($name), "` export.")]
            ///
            /// # Safety
            /// All inputs, parent lifetimes, exclusive output slots and destruction ordering
            /// must satisfy the rdma-core contract for this function.
            pub unsafe fn $name($($arg: $ty),*) -> $ret {
                match crate::sys::get() {
                    // SAFETY: the caller satisfies the named native contract and the table pins its code.
                    Ok(api) => unsafe { (api.$name)($($arg),*) },
                    Err(_) => {
                        // SAFETY: errno belongs to this calling thread.
                        unsafe { *libc::__errno_location() = libc::ENOSYS };
                        $missing
                    }
                }
            }
            )+
        }

        #[cfg(feature = "provider")]
        pub(crate) fn unavailable(error: &LoadError) -> &'static str {
            match error {
                $(LoadError::MissingSymbol(name) if *name == stringify!($name) => concat!("libibverbs.so.1: missing ", stringify!($name)),)+
                _ => "libibverbs.so.1: could not load the host rdma-core library",
            }
        }

        #[cfg(test)]
        const REQUIRED: &[&str] = &[$(stringify!($name)),+];
    };
}

exports! {
    ibv_get_device_list(count: *mut c_int) -> *mut *mut ffi::ibv_device = std::ptr::null_mut();
    ibv_free_device_list(list: *mut *mut ffi::ibv_device) -> () = ();
    ibv_get_device_name(device: *mut ffi::ibv_device) -> *const c_char = std::ptr::null();
    ibv_open_device(device: *mut ffi::ibv_device) -> *mut ffi::ibv_context = std::ptr::null_mut();
    ibv_close_device(context: *mut ffi::ibv_context) -> c_int = libc::ENOSYS;
    ibv_query_device(context: *mut ffi::ibv_context, attr: *mut ffi::ibv_device_attr) -> c_int = libc::ENOSYS;
    ibv_query_port(context: *mut ffi::ibv_context, port: u8, attr: *mut ffi::_compat_ibv_port_attr) -> c_int = libc::ENOSYS;
    ibv_query_gid(context: *mut ffi::ibv_context, port: u8, index: c_int, gid: *mut ffi::ibv_gid) -> c_int = libc::ENOSYS;
    _ibv_query_gid_ex(context: *mut ffi::ibv_context, port: u32, index: u32, entry: *mut ffi::ibv_gid_entry, flags: u32, size: usize) -> c_int = libc::ENOSYS;
    ibv_alloc_pd(context: *mut ffi::ibv_context) -> *mut ffi::ibv_pd = std::ptr::null_mut();
    ibv_dealloc_pd(pd: *mut ffi::ibv_pd) -> c_int = libc::ENOSYS;
    ibv_reg_mr(pd: *mut ffi::ibv_pd, address: *mut c_void, length: usize, access: c_int) -> *mut ffi::ibv_mr = std::ptr::null_mut();
    ibv_dereg_mr(mr: *mut ffi::ibv_mr) -> c_int = libc::ENOSYS;
    ibv_create_cq(context: *mut ffi::ibv_context, count: c_int, cq_context: *mut c_void, channel: *mut ffi::ibv_comp_channel, vector: c_int) -> *mut ffi::ibv_cq = std::ptr::null_mut();
    ibv_destroy_cq(cq: *mut ffi::ibv_cq) -> c_int = libc::ENOSYS;
    ibv_create_comp_channel(context: *mut ffi::ibv_context) -> *mut ffi::ibv_comp_channel = std::ptr::null_mut();
    ibv_destroy_comp_channel(channel: *mut ffi::ibv_comp_channel) -> c_int = libc::ENOSYS;
    ibv_get_cq_event(channel: *mut ffi::ibv_comp_channel, cq: *mut *mut ffi::ibv_cq, context: *mut *mut c_void) -> c_int = libc::ENOSYS;
    ibv_ack_cq_events(cq: *mut ffi::ibv_cq, count: c_uint) -> () = ();
    ibv_create_qp(pd: *mut ffi::ibv_pd, attr: *mut ffi::ibv_qp_init_attr) -> *mut ffi::ibv_qp = std::ptr::null_mut();
    ibv_modify_qp(qp: *mut ffi::ibv_qp, attr: *mut ffi::ibv_qp_attr, mask: c_int) -> c_int = libc::ENOSYS;
    ibv_query_qp(qp: *mut ffi::ibv_qp, attr: *mut ffi::ibv_qp_attr, mask: c_int, init: *mut ffi::ibv_qp_init_attr) -> c_int = libc::ENOSYS;
    ibv_destroy_qp(qp: *mut ffi::ibv_qp) -> c_int = libc::ENOSYS;
    ibv_get_async_event(context: *mut ffi::ibv_context, event: *mut ffi::ibv_async_event) -> c_int = libc::ENOSYS;
    ibv_ack_async_event(event: *mut ffi::ibv_async_event) -> () = ();
}

pub(super) fn get() -> Result<&'static Native, LoadError> {
    static NATIVE: OnceLock<Result<Native, LoadError>> = OnceLock::new();
    NATIVE
        .get_or_init(|| Native::load(OsStr::new("libibverbs.so.1")))
        .as_ref()
        .map_err(Clone::clone)
}

struct DeviceList<'a> {
    native: &'a Native,
    list: NonNull<*mut ffi::ibv_device>,
    count: usize,
}

impl Drop for DeviceList<'_> {
    fn drop(&mut self) {
        // SAFETY: the guard uniquely owns this list from the retained provider.
        unsafe { (self.native.ibv_free_device_list)(self.list.as_ptr()) };
    }
}

impl DeviceList<'_> {
    fn entries(&self) -> impl Iterator<Item = io::Result<(NonNull<ffi::ibv_device>, &OsStr)>> {
        (0..self.count).map(|index| {
            // SAFETY: the provider returned count entries and the list remains live.
            let raw = unsafe { *self.list.as_ptr().add(index) };
            let device = NonNull::new(raw)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "null device"))?;
            // SAFETY: the live list owns the device and its NUL-terminated name.
            let name = unsafe { (self.native.ibv_get_device_name)(device.as_ptr()) };
            if name.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "null device name",
                ));
            }
            // SAFETY: the returned borrow is bounded by this list.
            let name = unsafe { CStr::from_ptr(name) };
            Ok((device, OsStr::from_bytes(name.to_bytes())))
        })
    }
}

impl Native {
    fn list(&self) -> io::Result<DeviceList<'_>> {
        let mut count = 0;
        // SAFETY: count is writable; the guard releases the returned list on every path.
        let raw = unsafe { (self.ibv_get_device_list)(&mut count) };
        let mut list = DeviceList {
            native: self,
            list: NonNull::new(raw).ok_or_else(io::Error::last_os_error)?,
            count: 0,
        };
        list.count = usize::try_from(count)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative device count"))?;
        Ok(list)
    }
    #[cfg(feature = "provider")]
    pub(crate) fn device_nodes(&self) -> io::Result<Vec<(String, String)>> {
        let list = self.list()?;
        let mut devices = Vec::new();
        for entry in list.entries() {
            let (device, name) = entry?;
            // SAFETY: the retained list owns the device and its fixed-size dev_name field.
            let node = unsafe { &device.as_ref().dev_name };
            let bytes: Vec<_> = node
                .iter()
                .take_while(|ch| **ch != 0)
                .map(|ch| ch.to_ne_bytes()[0])
                .collect();
            if let (Some(name), Ok(node)) = (name.to_str(), String::from_utf8(bytes)) {
                if !node.is_empty() && !node.contains('/') {
                    devices.push((name.to_owned(), node));
                }
            }
        }
        Ok(devices)
    }
    pub(super) fn devices(&self) -> io::Result<Vec<OsString>> {
        self.list()?
            .entries()
            .map(|entry| entry.map(|(_, name)| name.to_owned()))
            .collect()
    }
}

use crate::Error;
use crate::resource::{Fault, Result as RdmaResult};

pub(crate) struct Handle<T>(NonNull<T>);
#[cfg(feature = "provider")]
impl<T> Handle<T> {
    /// The caller retains the correctly typed native object for every use of the token.
    pub(crate) unsafe fn borrowed(raw: *mut c_void) -> Result<Self, i32> {
        NonNull::new(raw.cast()).map(Self).ok_or(libc::EINVAL)
    }
    pub(crate) fn raw(self) -> *mut c_void {
        self.0.as_ptr().cast()
    }
}
impl<T> Copy for Handle<T> {}
impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}
// SAFETY: only thread-safe verbs handles enter this private boundary. An owner
// retains the native parents and serializes destructive operations.
unsafe impl<T> Send for Handle<T> {}
// SAFETY: sharing this token gives no Rust references into provider storage.
unsafe impl<T> Sync for Handle<T> {}

fn handle<T>(raw: *mut T, op: &'static str) -> RdmaResult<Handle<T>> {
    NonNull::new(raw).map(Handle).ok_or_else(|| Error::Native {
        operation: op,
        code: io::Error::last_os_error()
            .raw_os_error()
            .filter(|c| *c != 0)
            .unwrap_or(libc::EIO),
    })
}

fn check(op: &'static str, code: i32) -> RdmaResult<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Native {
            operation: op,
            code,
        })
    }
}

pub(crate) fn destroy<T>(
    slot: &mut Option<Handle<T>>,
    op: &'static str,
    call: unsafe extern "C" fn(*mut T) -> c_int,
) -> std::result::Result<(), Fault> {
    if let Some(handle) = *slot {
        // SAFETY: the caller owns the handle and has retired its dependent objects.
        let code = unsafe { call(handle.0.as_ptr()) };
        if code != 0 {
            return Err(Fault(op, code));
        }
        *slot = None;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/native.rs"]
mod tests;
