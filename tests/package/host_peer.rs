//! Two-process host-memory check. TCP exchanges only connection parameters,
//! window descriptors and phase acknowledgements. Each payload is a verbs WR.
mod completion;
mod host_peer_wire;
use completion::{deadline, finish};
use host_peer_wire::{field, read_params, read_window, send_params, send_window};
use orbit_rdma::{
    Device, HostBuffer, Limits, Permissions, QueueConfig, QueuePair, RegisteredMemory,
    RemoteAccess, RemoteDescriptor, Request, SharedWindow, Transfer,
};
use std::{
    env,
    error::Error,
    ffi::OsStr,
    io::Write,
    net::{SocketAddr, TcpListener, TcpStream},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn receive_ack(control: &mut TcpStream, expected: u8) -> Result<()> {
    if field::<1>(control)? != [expected] {
        return Err("unexpected peer phase".into());
    }
    Ok(())
}

fn connect(dev: &Device, control: &mut TcpStream) -> Result<QueuePair> {
    // Both peers use this connection across every phase.
    let mut random = std::fs::File::open("/dev/urandom")?;
    let psn = u32::from_be_bytes(field(&mut random)?) & 0xff_ffff;
    let mut qp = dev.queue_pair(QueueConfig::new(psn))?;
    send_params(control, qp.parameters())?;
    qp.connect(&read_params(control)?)?;
    Ok(qp)
}

fn publish(w: &SharedWindow, q: &mut QueuePair, access: RemoteAccess) -> Result<RemoteDescriptor> {
    let end = deadline();
    loop {
        if let Some(d) = w.descriptor(q, access)? {
            return Ok(d);
        }
        if Instant::now() >= end {
            return Err("window bind timed out".into());
        }
        q.wait_for_progress(end)?;
    }
}

fn retire(w: &mut SharedWindow, q: &mut QueuePair) -> Result<()> {
    let end = deadline();
    while !w.revoke(q)? {
        if Instant::now() >= end {
            return Err("window revoke timed out".into());
        }
        q.wait_for_progress(end)?;
    }
    Ok(())
}

fn serve_windows(dev: &Device, q: &mut QueuePair, control: &mut TcpStream) -> Result<()> {
    let mut expected = [0x67; 24];
    let mut mem = dev.register(
        HostBuffer::from_vec(expected.to_vec())?,
        Permissions::RemoteReadWrite,
    )?;
    let mut a = q.expose_window(&mem, 4..8, Permissions::RemoteWrite)?;
    let mut b = q.expose_window(&mem, 12..16, Permissions::RemoteReadWrite)?;
    for desc in [
        publish(&a, q, RemoteAccess::Write)?,
        publish(&b, q, RemoteAccess::Read)?,
        publish(&b, q, RemoteAccess::Write)?,
    ] {
        send_window(control, desc)?;
    }
    receive_ack(control, b'D')?;
    retire(&mut a, q)?;
    if &*mem.read(4..8)? != b"aaaa" || !matches!(mem.write(12..16), Err(orbit_rdma::Error::Busy)) {
        return Err("unexpected window ownership after revoking A".into());
    }
    let usage = dev.usage()?;
    if usage.queue_pairs != 1 || usage.windows != 1 {
        return Err("expected the original QP and window B to stay live".into());
    }
    mem.write(4..8)?.copy_from_slice(b"free");
    control.write_all(b"V")?;
    receive_ack(control, b'D')?;
    retire(&mut b, q)?;
    expected[4..8].copy_from_slice(b"free");
    expected[12..16].copy_from_slice(b"next");
    if mem.deregister()?.as_ref() != expected {
        return Err("window payload or guard mismatch".into());
    }
    control.write_all(b"V")?;
    Ok(())
}

fn transfer(q: &mut QueuePair, mem: &RegisteredMemory, remote: RemoteDescriptor) -> Result<()> {
    let op = match remote.access {
        RemoteAccess::Read => Transfer::Read { remote, offset: 0 },
        RemoteAccess::Write => Transfer::Write { remote, offset: 0 },
    };
    let ops = q
        .submit(vec![Request::new(mem.range(2..6)?, op)])?
        .operations;
    finish(q, ops, &[4])
}

fn access_windows(dev: &Device, q: &mut QueuePair, control: &mut TcpStream) -> Result<()> {
    let (a, b_read, b_write) = (
        read_window(control)?,
        read_window(control)?,
        read_window(control)?,
    );
    if a.qpn != b_read.qpn
        || a.qpn != b_write.qpn
        || a.rkey == b_read.rkey
        || b_read.rkey != b_write.rkey
        || a.access != RemoteAccess::Write
        || b_read.access != RemoteAccess::Read
        || b_write.access != RemoteAccess::Write
    {
        return Err("expected two independent keys on one peer QP".into());
    }
    let mut expected = [0x33; 10];
    let mut mem = dev.register(HostBuffer::from_vec(expected.to_vec())?, Permissions::Local)?;
    mem.write(2..6)?.copy_from_slice(b"aaaa");
    transfer(q, &mem, a)?;
    mem.write(2..6)?.copy_from_slice(b"bbbb");
    transfer(q, &mem, b_write)?;
    control.write_all(b"D")?;
    receive_ack(control, b'V')?;
    mem.write(2..6)?.fill(0);
    transfer(q, &mem, b_read)?;
    if &*mem.read(2..6)? != b"bbbb" {
        return Err("B lost its contents when A was revoked".into());
    }
    mem.write(2..6)?.copy_from_slice(b"next");
    transfer(q, &mem, b_write)?;
    mem.write(2..6)?.fill(0);
    transfer(q, &mem, b_read)?;
    expected[2..6].copy_from_slice(b"next");
    if mem.deregister()?.as_ref() != expected {
        return Err("B readback or local guard mismatch".into());
    }
    control.write_all(b"D")?;
    receive_ack(control, b'V')
}

fn send_receive(
    dev: &Device,
    qp: &mut QueuePair,
    control: &mut TcpStream,
    server: bool,
) -> Result<()> {
    let mut expected = [0x77; 12];
    if !server {
        expected[2..6].copy_from_slice(b"send");
    }
    let mut memory = dev.register(HostBuffer::from_vec(expected.to_vec())?, Permissions::Local)?;
    let request = if server {
        Request::new(memory.range(4..10)?, Transfer::Receive)
    } else {
        receive_ack(control, b'R')?;
        Request::new(memory.range(2..6)?, Transfer::Send)
    };
    let ops = qp.submit(vec![request])?.operations;
    if server {
        control.write_all(b"R")?;
    }
    finish(qp, ops, &[4])?;
    if server {
        expected[4..8].copy_from_slice(b"send");
    }
    if memory.deregister()?.as_ref() != expected {
        return Err("SEND/RECV payload or guard mismatch".into());
    }
    control.write_all(b"V")?;
    receive_ack(control, b'V')
}

fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    let [role, address, device, port, gid] = args.as_slice() else {
        return Err("usage: host_peer listen|connect IP:PORT DEVICE PORT GID_INDEX".into());
    };
    let address: SocketAddr = address.parse()?;
    let (server, mut control) = match role.as_str() {
        "listen" => {
            let listener = TcpListener::bind(address)?;
            println!("Listening on {}", listener.local_addr()?);
            (true, listener.accept()?.0)
        }
        "connect" => (
            false,
            TcpStream::connect_timeout(&address, Duration::from_secs(10))?,
        ),
        _ => return Err("expected listen or connect".into()),
    };
    control.set_read_timeout(Some(Duration::from_secs(15)))?;
    control.set_write_timeout(Some(Duration::from_secs(15)))?;
    control.set_nodelay(true)?;
    let mut dev = Device::open(
        OsStr::new(device),
        port.parse()?,
        gid.parse()?,
        Limits::default(),
    )?;
    let mut qp = connect(&dev, &mut control)?;
    let qpn = qp.parameters().qpn;
    if server {
        serve_windows(&dev, &mut qp, &mut control)?;
    } else {
        access_windows(&dev, &mut qp, &mut control)?;
    }
    send_receive(&dev, &mut qp, &mut control, server)?;
    qp.close()?;
    dev.close()?;
    println!(
        "Verified two windows on one QP {qpn}, READ/WRITE on B after revoking A, SEND/RECV and guard bytes on {device} ({role})."
    );
    Ok(())
}
