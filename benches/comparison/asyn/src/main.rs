#![forbid(unsafe_code)]
#[path = "../../common.rs"]
mod common;
use async_rdma::{ConnectionType, LocalMrReadAccess, LocalMrWriteAccess, MTU, RdmaBuilder};
use common::*;
use std::{
    alloc::Layout,
    io::Write,
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
async fn run() -> Result<()> {
    let (server, work, mut s) = connection()?;
    if work != "rw" && work != "send_async" {
        return Err("expected rw".into());
    }
    let mut d = RdmaBuilder::default()
        .set_dev("orbit_rxe")
        .set_raw(work == "send_async")
        .set_port_num(1)
        .set_gid_index(1)
        .set_mtu(MTU::MTU1024)
        .set_max_rd_atomic(1)
        .set_max_dest_rd_atomic(1)
        .set_conn_type(ConnectionType::RCIBV)
        .build()?;
    bincode::serialize_into(&mut s, &d.get_qp_endpoint())?;
    s.flush()?;
    let remote_ep = bincode::deserialize_from(&mut s)?;
    d.ibv_connect(remote_ep).await?;
    if work == "send_async" {
        return raw_send(server, d, s).await;
    }
    s.set_nonblocking(true)?;
    let mut s = tokio::net::TcpStream::from_std(s)?;
    s.write_all(b"R").await?;
    if s.read_u8().await? != b'R' {
        return Err("setup".into());
    }
    let layout = Layout::from_size_align(N + 2, 64)?;
    if server {
        let m = d.receive_local_mr().await?;
        check(&m.as_slice(), TOTAL - 1)?;
    } else {
        let mut m = d.alloc_local_mr(layout)?;
        let mut remote = d
            .request_remote_mr_with_timeout(layout, Duration::from_secs(120))
            .await?;
        m.as_mut_slice().fill(GUARD);
        d.write(&m, &mut remote).await?;
        for write in [true, false] {
            let mut samples = Vec::with_capacity(COUNT);
            for i in 0..TOTAL {
                if write {
                    m.as_mut_slice()[1..=N].copy_from_slice(&pattern(i));
                } else {
                    m.as_mut_slice()[1..=N].fill(0);
                }
                let start = Instant::now();
                if write {
                    let l = m.get(1..N + 1).ok_or("local slice")?;
                    let mut r = remote.get_mut(1..N + 1)?;
                    d.write(&l, &mut r).await?;
                } else {
                    let mut l = m.get_mut(1..N + 1).ok_or("local slice")?;
                    let r = remote.get(1..N + 1)?;
                    d.read(&mut l, &r).await?;
                }
                sample(&mut samples, i, start);
                if !write {
                    check(&m.as_slice(), TOTAL - 1)?;
                }
            }
            report(if write { "write" } else { "read" }, samples);
        }
        check(&m.as_slice(), TOTAL - 1)?;
        d.send_remote_mr(remote).await?;
    }
    s.write_all(b"D").await?;
    if s.read_u8().await? != b'D' {
        return Err("peer validation".into());
    }
    Ok(())
}
async fn raw_send(
    server: bool,
    d: async_rdma::Rdma,
    mut socket: std::net::TcpStream,
) -> Result<()> {
    let layout = Layout::from_size_align(N + 2, 64)?;
    let mut samples = Vec::with_capacity(COUNT);
    let mut src = d.alloc_local_mr(layout)?;
    src.as_mut_slice().fill(GUARD);
    barrier(&mut socket)?;
    for i in 0..TOTAL {
        if server {
            let mut sent = Ok(());
            // The public experimental callback runs after posting the receive,
            // so the client never spends a measured SEND waiting for an RQ slot.
            let recv = d
                .receive_raw_fn(layout, || {
                    sent = signal(&mut socket);
                })
                .await?;
            sent?;
            let bytes = recv.as_slice();
            if bytes[..N] != pattern(i) || bytes[N..] != [0, 0] {
                return Err("receive payload or guards".into());
            }
        } else {
            wait(&mut socket)?;
            src.as_mut_slice()[1..=N].copy_from_slice(&pattern(i));
            let start = Instant::now();
            let slice = src.get(1..N + 1).ok_or("send range")?;
            d.send_raw(&slice).await?;
            sample(&mut samples, i, start);
        }
    }
    barrier(&mut socket)?;
    if !server {
        check(&src.as_slice(), TOTAL - 1)?;
        report("send", samples);
    }
    Ok(())
}
fn main() -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run())
}
