//! Linux RDMA port and GID inventory in the caller's sysfs and devfs namespaces.
use crate::resource::Result;
use crate::{Error, Link};
use std::{ffi::CStr, fs, net::Ipv6Addr, os::unix::fs::FileTypeExt, path::Path};

/// Port state and Linux network identity of a selectable unicast GID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PortAddress {
    pub active: bool,
    pub link_layer: u8,
    pub gid: [u8; 16],
    pub gid_index: u8,
    pub link: Link,
    pub net_device: Option<String>,
    pub net_device_index: u32,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !matches!(name, "." | "..")
}

/// Physical port numbers exported for this device, in increasing order.
pub fn ports(sys: &Path, device: &str) -> Vec<u8> {
    if !valid_name(device) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(sys.join("class/infiniband").join(device).join("ports")) else {
        return Vec::new();
    };
    let mut ports: Vec<_> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u8>().ok())
        .filter(|port| *port != 0)
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// Whether sysfs classifies this interface as a virtual netdev.
pub fn virtual_netdev(sys: &Path, name: &str) -> bool {
    valid_name(name) && sys.join("devices/virtual/net").join(name).exists()
}

/// Recheck the interface name against the current network namespace.
pub fn netdev_matches(index: u32, expected: Option<&str>) -> Result<bool> {
    let mut name = [0; libc::IF_NAMESIZE];
    // SAFETY: the buffer has IF_NAMESIZE writable bytes for if_indextoname.
    let value = unsafe { libc::if_indextoname(index, name.as_mut_ptr()) };
    if value.is_null() {
        return Err(Error::Native {
            operation: "if_indextoname",
            code: super::failure(),
        });
    }
    // SAFETY: on success the returned name is NUL terminated in our live buffer.
    Ok(unsafe { CStr::from_ptr(value) }.to_str().ok() == expected)
}

fn text(path: &Path) -> Result<String> {
    Ok(fs::read_to_string(path)
        .map_err(Error::DeviceList)?
        .trim()
        .to_owned())
}

/// Read the link protocol, associated netdev and ifindex for an exact GID entry.
pub fn gid_metadata(
    sys: &Path,
    device: &str,
    port: u8,
    index: u8,
    link_layer: u8,
) -> Result<(Link, Option<String>, u32)> {
    if !valid_name(device) || port == 0 {
        return Err(Error::Invalid("RDMA port or device name"));
    }
    let attrs = sys
        .join("class/infiniband")
        .join(device)
        .join("ports")
        .join(port.to_string())
        .join("gid_attrs");
    let kind = match text(&attrs.join("types").join(index.to_string())) {
        Ok(kind) => kind,
        Err(Error::DeviceList(error))
            if link_layer == 1
                && (error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::EINVAL)) =>
        {
            "IB/RoCE v1".into()
        }
        Err(error) => return Err(error),
    };
    let kind = match (link_layer, kind.as_str()) {
        (1, "IB/RoCE v1") => Link::InfiniBand,
        (2, "IB/RoCE v1") => Link::RoceV1,
        (2, "RoCE v2") => Link::RoceV2,
        _ => return Err(Error::Unsupported("unsupported RDMA GID type")),
    };
    let net_device = match text(&attrs.join("ndevs").join(index.to_string())) {
        Ok(value) => value,
        Err(Error::DeviceList(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            String::new()
        }
        Err(error) => return Err(error),
    };
    let index = if net_device.is_empty() {
        0
    } else {
        if !valid_name(&net_device) {
            return Err(Error::Unsupported("invalid RDMA network device"));
        }
        text(&sys.join("class/net").join(&net_device).join("ifindex"))?
            .parse::<u32>()
            .ok()
            .filter(|index| *index != 0)
            .ok_or(Error::Unsupported("invalid RDMA network device index"))?
    };
    Ok((kind, (!net_device.is_empty()).then_some(net_device), index))
}

/// Whether the uverbs character device is readable and writable under this devfs root.
pub fn accessible(dev: &Path, name: &str) -> bool {
    if !valid_name(name) {
        return false;
    }
    let path = dev.join("infiniband").join(name);
    if !fs::metadata(&path).is_ok_and(|meta| meta.file_type().is_char_device()) {
        return false;
    }
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: access only checks the existing character node; it never opens an RNIC.
    unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
}

/// Enumerate usable unicast GIDs for a port. A pinned index filters the result.
pub fn port_addresses(
    sys: &Path,
    device: &str,
    port: u8,
    gid_index: Option<u8>,
) -> Vec<PortAddress> {
    if !valid_name(device) || port == 0 {
        return Vec::new();
    }
    let path = sys
        .join("class/infiniband")
        .join(device)
        .join("ports")
        .join(port.to_string());
    let active = match text(&path.join("state"))
        .ok()
        .and_then(|value| value.split(':').next()?.trim().parse::<u8>().ok())
    {
        Some(state @ 1..=5) => state == 4,
        _ => return Vec::new(),
    };
    let link_layer = match text(&path.join("link_layer")).as_deref() {
        Ok("InfiniBand") => 1,
        Ok("Ethernet") => 2,
        _ => return Vec::new(),
    };
    let Ok(gids) = fs::read_dir(path.join("gids")) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for index in gids
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u8>().ok())
    {
        if gid_index.is_some_and(|pin| pin != index) {
            continue;
        }
        let Some(gid) = text(&path.join("gids").join(index.to_string()))
            .ok()
            .and_then(|value| value.parse::<Ipv6Addr>().ok())
            .filter(|gid| !gid.is_unspecified() && !gid.is_multicast())
        else {
            continue;
        };
        let Ok((gid_type, net_device, net_device_index)) =
            gid_metadata(sys, device, port, index, link_layer)
        else {
            continue;
        };
        candidates.push(PortAddress {
            active,
            link_layer,
            gid: gid.octets(),
            gid_index: index,
            link: gid_type,
            net_device,
            net_device_index,
        });
    }
    candidates.sort_by_key(|address| address.gid_index);
    candidates
}

#[cfg(test)]
#[path = "../../tests/unit/inventory.rs"]
mod tests;
