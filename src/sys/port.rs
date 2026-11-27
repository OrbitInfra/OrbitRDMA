use super::{c_int, ffi};
use std::mem::{offset_of, size_of};

pub(crate) type LegacyQuery =
    unsafe extern "C" fn(*mut ffi::ibv_context, u8, *mut ffi::_compat_ibv_port_attr) -> c_int;
type ExtendedQuery =
    unsafe extern "C" fn(*mut ffi::ibv_context, u8, *mut ffi::ibv_port_attr, usize) -> c_int;

/// Follow verbs_get_ctx_op from the installed rdma-core ABI. The context is at
/// the end of an extensible table. Older providers may allocate a shorter table.
unsafe fn extended_query(context: *mut ffi::ibv_context) -> Option<ExtendedQuery> {
    // SAFETY: the caller retains a live base context, which always has abi_compat.
    if unsafe { (*context).abi_compat as usize } != usize::MAX {
        return None;
    }
    const BASE: usize = offset_of!(ffi::verbs_context, context);
    const SIZE: usize = offset_of!(ffi::verbs_context, sz);
    const QUERY: usize = offset_of!(ffi::verbs_context, query_port);
    let end = context.cast::<u8>();
    // SAFETY: the extended ABI tag guarantees the size field immediately before the base context.
    let size = unsafe { end.sub(BASE - SIZE).cast::<usize>().read() };
    if size < size_of::<ffi::verbs_context>() - QUERY {
        return None;
    }
    // SAFETY: the reported size covers the aligned query_port slot. A null slot is None.
    unsafe { end.sub(BASE - QUERY).cast::<Option<ExtendedQuery>>().read() }
}

/// The context and its provider callbacks must remain live during the query.
pub(crate) unsafe fn query(
    context: *mut ffi::ibv_context,
    port: u8,
    out: &mut ffi::ibv_port_attr,
    legacy: Option<LegacyQuery>,
) -> c_int {
    // SAFETY: the owner retains the provider and context throughout dispatch.
    if let Some(query) = unsafe { extended_query(context) } {
        // SAFETY: the output and its explicit size match the extended header ABI.
        return unsafe { query(context, port, out, size_of::<ffi::ibv_port_attr>()) };
    }
    match legacy {
        // SAFETY: the legacy ABI writes the common prefix of the initialized full output.
        Some(query) => unsafe { query(context, port, std::ptr::from_mut(out).cast()) },
        None => libc::ENOSYS,
    }
}
