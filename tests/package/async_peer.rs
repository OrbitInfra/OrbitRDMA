//! A Tokio control stream and one SEND/RECV on a private RC completion channel.
#![forbid(unsafe_code)]
use orbit_rdma::{
    Device, HostBuffer, Limits, Outcome, Permissions, QueueConfig, Reclaim, Request, Transfer,
};
use std::{env, error::Error, io::Read, net::SocketAddr, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

async fn run() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    let [role, address, device, port, gid] = args.as_slice() else {
        return Err("usage: async_peer listen|connect IP:PORT DEVICE PORT GID_INDEX".into());
    };
    let address: SocketAddr = address.parse()?;
    let (server, mut control) = match role.as_str() {
        "listen" => {
            let listener = TcpListener::bind(address).await?;
            println!("Listening on {}", listener.local_addr()?);
            (true, listener.accept().await?.0)
        }
        "connect" => (false, TcpStream::connect(address).await?),
        _ => return Err("expected listen or connect".into()),
    };
    control.set_nodelay(true)?;
    let mut d = Device::open(device, port.parse()?, gid.parse()?, Limits::default())?;
    let mut random = [0; 4];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let mut qp = d.queue_pair(QueueConfig::new(u32::from_be_bytes(random) & 0xff_ffff))?;
    qp.connect_over_async(&mut control).await?;
    let mut memory = d.register(HostBuffer::from_vec(vec![0x67; 8])?, Permissions::Local)?;
    let transfer = if server {
        Transfer::Receive
    } else {
        if control.read_u8().await? != b'R' {
            return Err("peer did not post receive".into());
        }
        memory.write(2..6)?.copy_from_slice(b"rust");
        Transfer::Send
    };
    let op = qp
        .submit(vec![Request::new(memory.range(2..6)?, transfer)])?
        .operations
        .pop()
        .ok_or("request was not posted")?;
    if server {
        control.write_all(b"R").await?;
    }
    op.wait_async(&mut qp).await?;
    let Reclaim::Ready(c) = op.try_reclaim() else {
        return Err("DMA still pending".into());
    };
    if !matches!(
        c.outcome,
        Outcome::Success {
            bytes: 4,
            invalidated_rkey: None
        }
    ) {
        return Err(format!("unexpected completion {:?}", c.outcome).into());
    }
    drop(c);
    if memory.deregister()?.as_ref() != b"ggrustgg" {
        return Err("SEND/RECV payload or guard mismatch".into());
    }
    control.write_all(b"D").await?;
    if control.read_u8().await? != b'D' {
        return Err("missing peer completion".into());
    }
    qp.close()?;
    d.close()?;
    println!("Verified async SEND/RECV payload and guard bytes on {device} ({role}).");
    Ok(())
}

fn main() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async { tokio::time::timeout(Duration::from_secs(25), run()).await? })
}
