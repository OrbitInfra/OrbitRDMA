//! Two-process host-memory check. TCP exchanges only connection parameters,
//! window descriptors and phase acknowledgements. Each payload is a verbs WR.
mod completion;
mod host_peer_wire;
use completion::{deadline, descriptor, finish};
use host_peer_wire::{field, read_params, read_window, send_params, send_window};
use orbit_rdma::{
    Device, HostBuffer, Limits, Permissions, QueueConfig, QueuePair, RemoteAccess, Request,
    Transfer,
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
    // A new QP is dedicated to each phase; both peers supply their own initial PSN.
    let mut random = std::fs::File::open("/dev/urandom")?;
    let psn = u32::from_be_bytes(field(&mut random)?) & 0xff_ffff;
    let mut qp = dev.queue_pair(QueueConfig::new(psn))?;
    send_params(control, qp.parameters())?;
    qp.connect(&read_params(control)?)?;
    Ok(qp)
}

fn one_sided(
    dev: &Device,
    control: &mut TcpStream,
    server: bool,
    access: RemoteAccess,
) -> Result<()> {
    let mut qp = connect(dev, control)?;
    let permission = match (server, access) {
        (true, RemoteAccess::Read) => Permissions::RemoteRead,
        (true, RemoteAccess::Write) => Permissions::RemoteWrite,
        (false, _) => Permissions::Local,
    };
    let mut expected = [if server { 0xdd } else { 0x33 }; 14];
    let payload = match access {
        RemoteAccess::Read => b"read",
        RemoteAccess::Write => b"wr!t",
    };
    let range = if server { 5..9 } else { 2..6 };
    let source = server == (access == RemoteAccess::Read);
    if source {
        expected[range.clone()].copy_from_slice(payload);
    }
    let mut memory = dev.register(HostBuffer::from_vec(expected.to_vec())?, permission)?;
    if server {
        let mut window = qp.expose(&memory, 4..10, access)?;
        send_window(control, descriptor(&mut window)?)?;
        receive_ack(control, b'D')?;
        let end = deadline();
        while !window.revoke()? {
            if Instant::now() >= end {
                return Err("window revoke timed out".into());
            }
            std::thread::yield_now();
        }
    } else {
        let remote = read_window(control)?;
        let transfer = match access {
            RemoteAccess::Read => Transfer::Read { remote, offset: 1 },
            RemoteAccess::Write => Transfer::Write { remote, offset: 1 },
        };
        let ops = qp
            .submit(vec![Request::new(memory.range(range.clone())?, transfer)])?
            .operations;
        finish(&mut qp, ops, &[4])?;
        control.write_all(b"D")?;
        receive_ack(control, b'V')?;
        qp.close()?;
    }
    expected[range].copy_from_slice(payload);
    if memory.deregister()?.as_ref() != expected {
        return Err("one-sided payload or guard mismatch".into());
    }
    if server {
        control.write_all(b"V")?;
    }
    Ok(())
}

fn send_receive(dev: &Device, control: &mut TcpStream, server: bool) -> Result<()> {
    let mut qp = connect(dev, control)?;
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
    finish(&mut qp, ops, &[4])?;
    if server {
        expected[4..8].copy_from_slice(b"send");
    }
    if memory.deregister()?.as_ref() != expected {
        return Err("SEND/RECV payload or guard mismatch".into());
    }
    control.write_all(b"V")?;
    receive_ack(control, b'V')?;
    qp.close()?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    let [role, address, device, port, gid] = args.as_slice() else {
        return Err("usage: host_peer listen|connect IP:PORT DEVICE PORT GID_INDEX".into());
    };
    let address: SocketAddr = address.parse()?;
    let (server, mut control) = match role.as_str() {
        "listen" => (true, TcpListener::bind(address)?.accept()?.0),
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
    one_sided(&dev, &mut control, server, RemoteAccess::Read)?;
    one_sided(&dev, &mut control, server, RemoteAccess::Write)?;
    send_receive(&dev, &mut control, server)?;
    dev.close()?;
    println!("Verified READ, WRITE, SEND/RECV and guard bytes on {device} ({role}).");
    Ok(())
}
