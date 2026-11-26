//! External consumer of the extracted .crate; both QPs use the selected port.
use orbit_rdma::{Device, HostBuffer, Limits, Permissions, QueueConfig, RemoteAccess};
use std::{
    ffi::OsStr,
    time::{Duration, Instant},
};

pub fn exercise(name: &OsStr, port: u8, gid_index: u8) -> Result<(), Box<dyn std::error::Error>> {
    let mut first = Device::open(name, port, gid_index, Limits::default())?;
    let mut second = Device::open(name, port, gid_index, Limits::default())?;
    let mut a = first.queue_pair(QueueConfig::new(0x124))?;
    let mut b = second.queue_pair(QueueConfig::new(0x547))?;
    let (left, right) = (a.parameters(), b.parameters());
    a.connect(&right)?;
    b.connect(&left)?;

    let mut region = first.register(
        HostBuffer::from_vec(vec![0x31; 32])?,
        Permissions::RemoteRead,
    )?;
    region.write(8..12)?.copy_from_slice(b"rust");
    let mut window = a.expose(&region, 8..12, RemoteAccess::Read)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let descriptor = loop {
        if let Some(descriptor) = window.descriptor()? {
            break descriptor;
        }
        if Instant::now() >= deadline {
            return Err("bind CQE timed out".into());
        }
        std::thread::yield_now();
    };
    if descriptor.length != 4 || descriptor.qpn != left.qpn {
        return Err("invalid window descriptor".into());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !window.revoke()? {
        if Instant::now() >= deadline {
            return Err("invalidation CQE timed out".into());
        }
        std::thread::yield_now();
    }
    b.close()?;
    let bytes = region.deregister()?;
    let (prefix, rest) = bytes.as_ref().split_at(8);
    let (value, suffix) = rest.split_at(4);
    if prefix != [0x31; 8] || value != b"rust" || suffix != [0x31; 20] {
        return Err("host buffer or guard changed".into());
    }
    let usage = first.usage()?;
    if usage.queue_pairs != 0
        || usage.regions != 0
        || usage.windows != 0
        || usage.range_loans != 0
        || usage.registered_bytes != 0
    {
        return Err("healthy close retained a live resource".into());
    }
    first.close()?;
    second.close()?;
    println!("RC connection, host registration, type-2 window and teardown passed");
    Ok(())
}
