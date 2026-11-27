#![forbid(unsafe_code)]
#[path = "../../common.rs"]
mod common;
use common::*;
use orbit_rdma::{
    Device, HostBuffer, Limits, Operation, Outcome, Permissions, QueueConfig, QueuePair, Reclaim,
    RegisteredMemory, RemoteAccess, RemoteDescriptor, Request, Transfer,
};
use std::{
    io::{Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

fn post(q: &mut QueuePair, m: &RegisteredMemory, x: Transfer) -> Result<Operation> {
    #[cfg(feature = "single")]
    return Ok(q.submit_one(Request::new(m.range(1..N + 1)?, x))?.operation);
    #[cfg(not(feature = "single"))]
    q.submit(vec![Request::new(m.range(1..N + 1)?, x)])?
        .operations
        .pop()
        .ok_or_else(|| "unposted".into())
}
fn done(op: Operation) -> Result<()> {
    match op.try_reclaim() {
        Reclaim::Ready(c)
            if c.outcome
                == (Outcome::Success {
                    bytes: N as u32,
                    invalidated_rkey: None,
                }) =>
        {
            Ok(())
        }
        _ => Err("CQE error or pending DMA".into()),
    }
}
async fn sender(
    async_wait: bool,
    server: bool,
    q: &mut QueuePair,
    m: &RegisteredMemory,
    s: &mut TcpStream,
) -> Result<()> {
    let mut samples = Vec::with_capacity(COUNT);
    for i in 0..TOTAL {
        let start;
        let op = if server {
            m.write(1..N + 1)?.fill(0);
            let op = post(q, m, Transfer::Receive)?;
            signal(s)?;
            start = Instant::now();
            op
        } else {
            wait(s)?;
            m.write(1..N + 1)?.copy_from_slice(&pattern(i));
            start = Instant::now();
            post(q, m, Transfer::Send)?
        };
        if async_wait {
            op.wait_async(q).await?;
        } else {
            loop {
                if q.progress()? != 0 {
                    break;
                }
                std::hint::spin_loop();
            }
        }
        done(op)?;
        if server {
            check(&*m.read(0..N + 2)?, i)?;
        } else {
            sample(&mut samples, i, start);
        }
    }
    check(&*m.read(0..N + 2)?, TOTAL - 1)?;
    barrier(s)?;
    if !server {
        report("send", samples);
    }
    Ok(())
}

async fn rw(
    server: bool,
    q: &mut QueuePair,
    m: &RegisteredMemory,
    s: &mut TcpStream,
) -> Result<()> {
    if server {
        let mut w = q.expose_window(m, 1..N + 1, Permissions::RemoteReadWrite)?;
        for access in [RemoteAccess::Write, RemoteAccess::Read] {
            loop {
                if let Some(d) = w.descriptor(q, access)? {
                    s.write_all(&d.to_bytes())?;
                    break;
                }
                q.wait_for_progress(Instant::now() + Duration::from_secs(3))?;
            }
        }
        wait(s)?;
        while !w.revoke(q)? {
            q.wait_for_progress(Instant::now() + Duration::from_secs(3))?;
        }
        check(&*m.read(0..N + 2)?, TOTAL - 1)?;
        signal(s)?;
    } else {
        for write in [true, false] {
            let mut record = [0; 25];
            s.read_exact(&mut record)?;
            let d = RemoteDescriptor::from_bytes(record)?;
            let mut samples = Vec::with_capacity(COUNT);
            for i in 0..TOTAL {
                if write {
                    m.write(1..N + 1)?.copy_from_slice(&pattern(i));
                } else {
                    m.write(1..N + 1)?.fill(0);
                }
                let start = Instant::now();
                let x = if write {
                    Transfer::Write {
                        remote: d,
                        offset: 0,
                    }
                } else {
                    Transfer::Read {
                        remote: d,
                        offset: 0,
                    }
                };
                let op = post(q, m, x)?;
                op.wait_async(q).await?;
                done(op)?;
                sample(&mut samples, i, start);
                if !write {
                    check(&*m.read(0..N + 2)?, TOTAL - 1)?;
                }
            }
            report(if write { "write" } else { "read" }, samples);
        }
        signal(s)?;
        wait(s)?;
        check(&*m.read(0..N + 2)?, TOTAL - 1)?;
    }
    Ok(())
}
fn burst(server: bool, q: &mut QueuePair, m: &RegisteredMemory, s: &mut TcpStream) -> Result<()> {
    const DEPTH: usize = 32;
    const ROUNDS: usize = 500;
    if server {
        let mut window = q.expose_window(m, 1..N + 1, Permissions::RemoteWrite)?;
        loop {
            if let Some(d) = window.descriptor(q, RemoteAccess::Write)? {
                s.write_all(&d.to_bytes())?;
                break;
            }
            q.progress()?;
        }
        wait(s)?;
        while !window.revoke(q)? {
            q.progress()?;
        }
        check(&*m.read(0..N + 2)?, ROUNDS - 1)?;
        signal(s)?;
    } else {
        let mut record = [0; 25];
        s.read_exact(&mut record)?;
        let remote = RemoteDescriptor::from_bytes(record)?;
        let mut timings = Vec::with_capacity(ROUNDS);
        for i in 0..ROUNDS {
            m.write(1..N + 1)?.copy_from_slice(&pattern(i));
            let start = Instant::now();
            let mut requests = Vec::with_capacity(DEPTH);
            for _ in 0..DEPTH {
                requests.push(Request::new(
                    m.range(1..N + 1)?,
                    Transfer::Write { remote, offset: 0 },
                ));
            }
            let sub = q.submit(requests)?;
            if sub.operations.len() != DEPTH {
                return Err("partial burst".into());
            }
            while sub.operations.iter().any(|op| !op.is_reusable()) {
                q.progress()?;
            }
            for op in sub.operations {
                done(op)?;
            }
            if i >= 50 {
                timings.push(start.elapsed().as_nanos() as u64);
            }
        }
        timings.sort_unstable();
        let pct = |p: usize| timings[(timings.len() - 1) * p / 100];
        println!("SAMPLE burst {} {}", pct(50), pct(99));
        signal(s)?;
        wait(s)?;
    }
    Ok(())
}
async fn run() -> Result<()> {
    let (server, work, mut s) = connection()?;
    let mut d = Device::open("orbit_rxe", 1, 1, Limits::default())?;
    let mut config = QueueConfig::new(std::process::id() & 0xffffff);
    if work == "burst" {
        config.send_depth = 64;
    }
    let mut q = d.queue_pair(config)?;
    q.connect_over(&mut s)?;
    let mut mr = d.register(
        HostBuffer::from_vec(vec![GUARD; N + 2])?,
        if server && (work == "rw" || work == "burst") {
            Permissions::RemoteReadWrite
        } else {
            Permissions::Local
        },
    )?;
    barrier(&mut s)?;
    if work == "send" || work == "send_async" {
        sender(work == "send_async", server, &mut q, &mr, &mut s).await?;
    } else if work == "burst" {
        burst(server, &mut q, &mr, &mut s)?;
    } else {
        rw(server, &mut q, &mr, &mut s).await?;
    }
    q.close()?;
    mr.deregister()?;
    d.close()?;
    Ok(())
}
fn main() -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run())
}
