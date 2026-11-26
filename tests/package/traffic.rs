//! Public consumer of the extracted crate. Both peers use their chosen local port.
use orbit_rdma::{
    ConnectionParams, Device, HostBuffer, Limits, Operation, Outcome, Permissions, QueueConfig,
    QueuePair, Reclaim, RemoteAccess, RemoteDescriptor, RemoteWindow, Request, Transfer,
};
use std::{
    error::Error,
    ffi::OsStr,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn connect(left: &Device, right: &Device) -> Result<(QueuePair, QueuePair)> {
    let mut a = left.queue_pair(QueueConfig {
        signal_interval: 2,
        ..QueueConfig::new(0x192ab)
    })?;
    let mut b = right.queue_pair(QueueConfig::new(0x374cd))?;
    let (l, r): (ConnectionParams, ConnectionParams) = (a.parameters(), b.parameters());
    a.connect(&r)?;
    b.connect(&l)?;
    Ok((a, b))
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
fn descriptor(w: &mut RemoteWindow) -> Result<RemoteDescriptor> {
    let limit = deadline();
    loop {
        if let Some(value) = w.descriptor()? {
            return Ok(value);
        }
        if Instant::now() >= limit {
            return Err("window bind timed out".into());
        }
        std::thread::yield_now();
    }
}
fn finish(q: &mut QueuePair, ops: Vec<Operation>, lengths: &[u32]) -> Result<()> {
    if ops.len() != lengths.len() {
        return Err("incomplete native post".into());
    }
    let limit = deadline();
    for (op, &bytes) in ops.into_iter().zip(lengths) {
        if !op.wait_until(q, limit)? {
            return Err("payload CQE timed out".into());
        }
        let Reclaim::Ready(c) = op.try_reclaim() else {
            return Err("payload range still held".into());
        };
        if c.outcome
            != (Outcome::Success {
                bytes,
                invalidated_rkey: None,
            })
        {
            return Err(format!("unexpected payload outcome {:?}", c.outcome).into());
        }
    }
    Ok(())
}
fn one_sided(left: &Device, right: &Device, access: RemoteAccess) -> Result<()> {
    let (mut q, server) = connect(left, right)?;
    let mut local = left.register(HostBuffer::from_vec(vec![0x11; 12])?, Permissions::Local)?;
    let permission = match access {
        RemoteAccess::Read => Permissions::RemoteRead,
        RemoteAccess::Write => Permissions::RemoteWrite,
    };
    let mut remote = right.register(HostBuffer::from_vec(vec![0xee; 12])?, permission)?;
    let mut expected_local = [0x11; 12];
    let mut expected_remote = [0xee; 12];
    match access {
        RemoteAccess::Read => {
            remote.write(4..8)?.copy_from_slice(b"read");
            expected_remote[4..8].copy_from_slice(b"read");
            expected_local[2..6].copy_from_slice(b"read");
        }
        RemoteAccess::Write => {
            local.write(2..6)?.copy_from_slice(b"send");
            expected_local[2..6].copy_from_slice(b"send");
            expected_remote[4..8].copy_from_slice(b"send");
        }
    }
    let mut w = server.expose(&remote, 3..9, access)?;
    let desc = descriptor(&mut w)?;
    let mut requests = Vec::new();
    for (start, offset) in [(2, 1), (4, 3)] {
        let transfer = match access {
            RemoteAccess::Read => Transfer::Read {
                remote: desc,
                offset,
            },
            RemoteAccess::Write => Transfer::Write {
                remote: desc,
                offset,
            },
        };
        requests.push(Request::new(local.range(start..start + 2)?, transfer));
    }
    let ops = q.submit(requests)?.operations;
    finish(&mut q, ops, &[2, 2])?;
    let limit = deadline();
    while !w.revoke()? {
        if Instant::now() >= limit {
            return Err("window invalidation timed out".into());
        }
        std::thread::yield_now();
    }
    q.close()?;
    if local.deregister()?.as_ref() != expected_local
        || remote.deregister()?.as_ref() != expected_remote
    {
        return Err("one-sided payload or guard mismatch".into());
    }
    Ok(())
}
fn two_sided(left: &Device, right: &Device) -> Result<()> {
    let (mut sender, mut receiver) = connect(left, right)?;
    let mut source = left.register(
        HostBuffer::from_vec(b"..onetwo..".to_vec())?,
        Permissions::Local,
    )?;
    let mut destination =
        right.register(HostBuffer::from_vec(vec![0x55; 16])?, Permissions::Local)?;
    let recv = receiver
        .submit(vec![
            Request::new(destination.range(2..7)?, Transfer::Receive),
            Request::new(destination.range(9..15)?, Transfer::Receive),
        ])?
        .operations;
    let send = sender
        .submit(vec![
            Request::new(source.range(5..8)?, Transfer::Send),
            Request::new(source.range(2..5)?, Transfer::Send),
        ])?
        .operations;
    finish(&mut receiver, recv, &[3, 3])?;
    finish(&mut sender, send, &[3, 3])?;
    let mut expected = [0x55; 16];
    expected[2..5].copy_from_slice(b"two");
    expected[9..12].copy_from_slice(b"one");
    if destination.deregister()?.as_ref() != expected
        || source.deregister()?.as_ref() != b"..onetwo.."
    {
        return Err("SEND/RECV payload, receive slot or guard mismatch".into());
    }
    receiver.close()?;
    sender.close()?;
    Ok(())
}
pub fn exercise(name: &OsStr, port: u8, gid: u8) -> Result<()> {
    let mut left = Device::open(name, port, gid, Limits::default())?;
    let mut right = Device::open(name, port, gid, Limits::default())?;
    one_sided(&left, &right, RemoteAccess::Read)?;
    one_sided(&left, &right, RemoteAccess::Write)?;
    two_sided(&left, &right)?;
    for device in [&mut left, &mut right] {
        let u = device.usage()?;
        if (
            u.queue_pairs,
            u.regions,
            u.range_loans,
            u.in_flight_bytes,
            u.send_requests,
            u.receive_requests,
        ) != (0, 0, 0, 0, 0, 0)
        {
            return Err("physical payload resources still charged".into());
        }
        device.close()?;
    }
    println!(
        "Packaged READ/WRITE and SEND/RECV passed full payload, guard and physical-credit checks."
    );
    Ok(())
}
