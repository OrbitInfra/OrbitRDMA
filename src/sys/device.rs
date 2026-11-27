//! Adapted from OrbiTE's sys/device.rs. Open the selected device before releasing the list.
use super::*;
use crate::{Link, Port};

pub(crate) type Context = Handle<ffi::ibv_context>;
pub(crate) type Pd = Handle<ffi::ibv_pd>;

#[derive(Clone, Copy)]
pub(crate) struct Caps {
    pub ports: u8,
    pub max_wr: u32,
    pub max_cqe: u32,
    pub initiator: u8,
    pub responder: u8,
    pub windows: bool,
}
impl Native {
    pub(crate) fn open(&self, name: &OsStr) -> RdmaResult<Context> {
        let list = self.list().map_err(Error::DeviceList)?;
        for entry in list.entries() {
            let (device, found) = entry.map_err(Error::DeviceList)?;
            if name == found {
                return handle(
                    // SAFETY: the list retains this device through open, which acquires its own reference.
                    unsafe { (self.ibv_open_device)(device.as_ptr()) },
                    "ibv_open_device",
                );
            }
        }
        Err(Error::DeviceNotFound(name.to_owned()))
    }
    pub(crate) fn caps(&self, ctx: Context) -> RdmaResult<Caps> {
        let mut attr = ffi::ibv_device_attr::default();
        // SAFETY: the context is live; attr is writable at the system-header size.
        check("ibv_query_device", unsafe {
            (self.ibv_query_device)(ctx.0.as_ptr(), &mut attr)
        })?;
        let depth = |v: i32| v.clamp(0, i32::from(u8::MAX)) as u8;
        Ok(Caps {
            ports: attr.phys_port_cnt,
            max_wr: attr.max_qp_wr.max(0) as u32,
            max_cqe: attr.max_cqe.max(0) as u32,
            initiator: depth(attr.max_qp_init_rd_atom),
            responder: depth(attr.max_qp_rd_atom),
            windows: attr.device_cap_flags & ffi::IBV_DEVICE_MEM_WINDOW_TYPE_2B != 0,
        })
    }
    pub(crate) fn port(&self, ctx: Context, number: u8, index: u8) -> RdmaResult<Port> {
        let mut attr = ffi::ibv_port_attr::default();
        // SAFETY: the owner retains the provider and context throughout the query.
        check("ibv_query_port", unsafe {
            port::query(ctx.0.as_ptr(), number, &mut attr, Some(self.ibv_query_port))
        })?;
        if attr.state != ffi::IBV_PORT_ACTIVE || !(1..=5).contains(&attr.active_mtu) {
            return Err(Error::Invalid("inactive port or MTU"));
        }
        if i32::from(index) >= attr.gid_tbl_len {
            return Err(Error::Invalid("GID index"));
        }
        let mut gid = ffi::ibv_gid::default();
        // SAFETY: the device owns this port and the caller provides an entire writable GID.
        check("ibv_query_gid", unsafe {
            (self.ibv_query_gid)(ctx.0.as_ptr(), number, i32::from(index), &mut gid)
        })?;
        let mut entry = ffi::ibv_gid_entry::default();
        // SAFETY: this is rdma-core's size-aware exported query, using the build header's layout.
        check("ibv_query_gid_ex", unsafe {
            (self._ibv_query_gid_ex)(
                ctx.0.as_ptr(),
                number.into(),
                index.into(),
                &mut entry,
                0,
                size_of_val(&entry),
            )
        })?;
        // SAFETY: both queries initialized the 16 raw address bytes.
        let (gid, verified) = unsafe { (gid.raw, entry.gid.raw) };
        let link = match (u32::from(attr.link_layer), entry.gid_type) {
            (ffi::IBV_LINK_LAYER_INFINIBAND, ffi::IBV_GID_TYPE_IB) => Link::InfiniBand,
            (ffi::IBV_LINK_LAYER_ETHERNET, ffi::IBV_GID_TYPE_ROCE_V1) => Link::RoceV1,
            (ffi::IBV_LINK_LAYER_ETHERNET, ffi::IBV_GID_TYPE_ROCE_V2) => Link::RoceV2,
            _ => return Err(Error::Unsupported("port link and GID type")),
        };
        if entry.gid_index != u32::from(index)
            || entry.port_num != u32::from(number)
            || gid != verified
            || gid == [0; 16]
            || gid[0] == 0xff
            || (link == Link::InfiniBand && attr.lid == 0)
        {
            return Err(Error::Invalid("selected port address"));
        }
        Ok(Port {
            number,
            gid_index: index,
            gid,
            link,
            lid: attr.lid,
            mtu: attr.active_mtu as u8,
        })
    }
    pub(crate) fn pd(&self, ctx: Context) -> RdmaResult<Pd> {
        handle(
            // SAFETY: the domain will retain ctx for the entire lifetime of the new PD.
            unsafe { (self.ibv_alloc_pd)(ctx.0.as_ptr()) },
            "ibv_alloc_pd",
        )
    }
}
