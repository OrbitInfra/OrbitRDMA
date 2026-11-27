//! Linux RC RDMA transfers with registered host buffers and explicit completion receipts.
//! Exchange [`ConnectionParams`] over your control channel. Grant independent
//! ranges on the same QP with [`QueuePair::expose_window`] and poll
//! [`SharedWindow::revoke`] to learn when a range may be borrowed again.
//! [`QueuePair::expose`] dedicates a connected QP to a single grant.
//! Use [`QueuePair::submit`] to post READ, WRITE, SEND or RECV. Drive the CQ with
//! [`QueuePair::progress`] or [`Operation::wait_until`] and recover each range with
//! [`Operation::try_reclaim`].
#![deny(unsafe_code)]

#[cfg(not(target_os = "linux"))]
compile_error!("orbit-rdma requires a Linux target and rdma-core development headers");

#[cfg(target_os = "linux")]
mod device;
#[cfg(target_os = "linux")]
mod keys;
#[cfg(target_os = "linux")]
mod memory;
#[cfg(target_os = "linux")]
mod operation;
#[cfg(all(target_os = "linux", feature = "provider"))]
pub mod provider;
#[cfg(target_os = "linux")]
mod queue;
#[cfg(target_os = "linux")]
mod resource;
#[cfg(target_os = "linux")]
mod sys;
#[cfg(target_os = "linux")]
mod window;

#[cfg(target_os = "linux")]
pub use device::{Device, Limits, Link, Port, Usage};
#[cfg(target_os = "linux")]
pub use memory::{
    HostBuffer, Permissions, ReadRange, RegisteredMemory, RegisteredRange, WriteRange,
};
#[cfg(target_os = "linux")]
pub use operation::{
    Completion, Operation, Outcome, PostStatus, Reclaim, Rejected, Request, Submission, Transfer,
    TransferFailure,
};
#[cfg(target_os = "linux")]
pub use queue::{ConnectionParams, QueueConfig, QueuePair};
#[cfg(target_os = "linux")]
pub use window::{RemoteAccess, RemoteDescriptor, RemoteWindow, SharedWindow};

/// Submit work and poll completions on RC queues owned by another verbs runtime.
/// Callers retain their QP, CQ, memory registrations and DMA loans. Applications
/// using OrbitRDMA-owned memory can use the safe [`QueuePair`] API.
#[cfg(target_os = "linux")]
pub mod verbs {
    pub use crate::sys::transfer::{Opcode, PostReceipt, WorkCompletion, WorkRequest, poll, post};
}

use std::{error, ffi::OsString, fmt, io};

/// Failure to load the host's verbs function table.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadError {
    /// The dynamic loader's diagnostic for `libibverbs.so.1`.
    Unavailable(String),
    /// A required export was absent from the loaded library.
    MissingSymbol(&'static str),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(f, "could not load libibverbs.so.1: {reason}"),
            Self::MissingSymbol(name) => write!(f, "libibverbs.so.1 is missing {name}"),
        }
    }
}

impl error::Error for LoadError {}

/// Device discovery, native resource, or ownership error.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The verbs runtime could not be initialized.
    Load(LoadError),
    /// Device enumeration failed after the runtime had loaded.
    DeviceList(io::Error),
    DeviceNotFound(OsString),
    Invalid(&'static str),
    Unsupported(&'static str),
    Exhausted(&'static str),
    Busy,
    Closed,
    PermissionDenied,
    Native {
        operation: &'static str,
        code: i32,
    },
    /// A native destructor failed. Its dependent owners remain quarantined.
    Unquiesced {
        operation: &'static str,
        code: i32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(error) => error.fmt(f),
            Self::DeviceList(error) => write!(f, "could not enumerate RDMA devices: {error}"),
            Self::DeviceNotFound(name) => write!(f, "RDMA device {name:?} was not found"),
            Self::Invalid(what) => write!(f, "invalid {what}"),
            Self::Unsupported(what) => write!(f, "provider does not support {what}"),
            Self::Exhausted(what) => write!(f, "{what} limit reached"),
            Self::Busy => f.write_str("resource is still in use"),
            Self::Closed => f.write_str("resource is closed"),
            Self::PermissionDenied => f.write_str("registration does not permit this access"),
            Self::Native { operation, code } => write!(f, "{operation} failed ({code})"),
            Self::Unquiesced { operation, code } => {
                write!(f, "{operation} failed ({code}), native owners retained")
            }
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(match self {
            Self::Load(error) => error,
            Self::DeviceList(error) => error,
            _ => return None,
        })
    }
}

/// Lists device names reported by libibverbs, without opening a device.
///
/// The first call loads the runtime and caches the result for the process
/// lifetime. Each successful call takes a fresh inventory. An empty list is a
/// successful enumeration on a host with no visible RDMA devices.
///
/// ```no_run
/// for name in orbit_rdma::devices()? {
///     println!("{}", name.to_string_lossy());
/// }
/// # Ok::<(), orbit_rdma::Error>(())
/// ```
#[cfg(target_os = "linux")]
pub fn devices() -> Result<Vec<OsString>, Error> {
    sys::get()
        .map_err(Error::Load)?
        .devices()
        .map_err(Error::DeviceList)
}
