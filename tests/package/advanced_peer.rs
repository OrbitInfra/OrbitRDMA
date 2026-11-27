//! Two-process, byte-checked RC scatter/gather, immediate and atomic example.
#![forbid(unsafe_code)]
use orbit_rdma::*;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    ops::Range,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn word<const N: usize>(s: &mut TcpStream) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    s.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn wait(q: &mut QueuePair, op: &Operation, bytes: u32) -> Result<()> {
    if !op.wait_until(q, Instant::now() + Duration::from_secs(5))?
        || op.outcome()
            != Some(Outcome::Success {
                bytes,
                invalidated_rkey: None,
            })
    {
        return Err(format!("unexpected completion {:?}", op.outcome()).into());
    }
    Ok(())
}
fn span(m: &RegisteredMemory, r: Range<usize>, expected: &[u8]) -> Result<()> {
    if &*m.read(r)? != expected {
        return Err("payload or guard mismatch".into());
    }
    Ok(())
}

fn server(q: &mut QueuePair, m: &RegisteredMemory, s: &mut TcpStream) -> Result<()> {
    let ranges = [m.range(1..3)?, m.range(6..9)?];
    let rx = q.submit_vectored(&ranges, Transfer::Receive)?;
    s.write_all(&[1])?;
    wait(q, rx.receipt(), 5)?;
    span(m, 0..10, b".he...llo.")?;
    let one = q.submit_one(Request::new(m.range(10..15)?, Transfer::Receive))?;
    s.write_all(&[2])?;
    wait(q, &one.operation, 5)?;
    if one.operation.immediate() != Some(Immediate::Message(0x12345678)) {
        return Err("SEND immediate".into());
    }
    span(m, 10..15, b"hello")?;
    let mut w = q.expose_window(m, 16..24, Permissions::RemoteWrite)?;
    let desc = loop {
        if let Some(d) = w.descriptor(q, RemoteAccess::Write)? {
            break d;
        }
    };
    let note = q.submit_one(Request::new(m.range(15..16)?, Transfer::Receive))?;
    s.write_all(&desc.to_bytes())?;
    wait(q, &note.operation, 0)?;
    if note.operation.immediate()
        != Some(Immediate::RemoteWrite {
            value: 0x10203040,
            written: 5,
        })
    {
        return Err("WRITE immediate".into());
    }
    while !w.revoke(q)? {
        q.progress()?;
    }
    span(m, 15..24, b".hello...")?;
    m.write(24..32)?.copy_from_slice(&10u64.to_ne_bytes());
    let mut a = q.expose_atomic_window(m, 24..32)?;
    let t = loop {
        if let Some(t) = a.descriptor(q)? {
            break t;
        }
    };
    s.write_all(&t.qpn.to_be_bytes())?;
    s.write_all(&t.address.to_be_bytes())?;
    s.write_all(&t.length.to_be_bytes())?;
    s.write_all(&t.rkey.to_be_bytes())?;
    if word::<1>(s)? != [3] {
        return Err("atomic confirmation".into());
    }
    while !a.revoke(q)? {
        q.progress()?;
    }
    span(m, 24..32, &123u64.to_ne_bytes())?;
    s.write_all(&[4])?;
    Ok(())
}
fn client(q: &mut QueuePair, m: &RegisteredMemory, s: &mut TcpStream) -> Result<()> {
    if word::<1>(s)? != [1] {
        return Err("scatter phase".into());
    }
    m.write(1..4)?.copy_from_slice(b"hel");
    m.write(6..8)?.copy_from_slice(b"lo");
    let tx = q.submit_vectored(&[m.range(1..4)?, m.range(6..8)?], Transfer::Send)?;
    wait(q, tx.receipt(), 5)?;
    if word::<1>(s)? != [2] {
        return Err("immediate phase".into());
    }
    m.write(8..13)?.copy_from_slice(b"hello");
    let op = q.submit_with_immediate(Request::new(m.range(8..13)?, Transfer::Send), 0x12345678)?;
    wait(q, &op.operation, 5)?;
    let remote = RemoteDescriptor::from_bytes(word(s)?)?;
    let op = q.submit_with_immediate(
        Request::new(m.range(8..13)?, Transfer::Write { remote, offset: 0 }),
        0x10203040,
    )?;
    wait(q, &op.operation, 5)?;
    let target = AtomicTarget {
        qpn: u32::from_be_bytes(word(s)?),
        address: u64::from_be_bytes(word(s)?),
        length: u64::from_be_bytes(word(s)?),
        rkey: u32::from_be_bytes(word(s)?),
    };
    for (op, previous) in [
        (AtomicOperation::FetchAdd(7), 10u64),
        (
            AtomicOperation::CompareSwap {
                compare: 11,
                swap: 42,
            },
            17,
        ),
        (
            AtomicOperation::CompareSwap {
                compare: 17,
                swap: 123,
            },
            17,
        ),
    ] {
        let a = q.submit_atomic(&m.range(24..32)?, target, 0, op)?;
        wait(q, &a.operation, 8)?;
        span(m, 24..32, &previous.to_ne_bytes())?;
    }
    s.write_all(&[3])?;
    if word::<1>(s)? != [4] {
        return Err("peer verification".into());
    }
    Ok(())
}
fn shared_receive(d: &Device, m: &RegisteredMemory, s: &mut TcpStream, server: bool) -> Result<()> {
    let mut pool = d.shared_receive_queue(4)?;
    let cfg = QueueConfig::new(std::process::id() & 0xffffff);
    let mut first = d.queue_pair_with_srq(cfg, 1, &pool)?;
    first.connect_over(s)?;
    let mut second = d.queue_pair_with_srq(cfg, 1, &pool)?;
    second.connect_over(s)?;
    if server {
        let a = pool.post(&m.range(0..3)?)?;
        let b = pool.post(&m.range(8..11)?)?;
        if !matches!(pool.close(), Err(orbit_rdma::Error::Busy)) {
            return Err("attached SRQ closed".into());
        }
        s.write_all(&[1])?;
        wait(&mut second, &a.operation, 3)?;
        span(m, 0..3, b"two")?;
        s.write_all(&[2])?;
        wait(&mut first, &b.operation, 3)?;
        span(m, 8..11, b"one")?;
        s.write_all(&[3])?;
    } else {
        if word::<1>(s)? != [1] {
            return Err("SRQ phase".into());
        }
        for (q, data, ack) in [(&mut second, b"two", 2), (&mut first, b"one", 3)] {
            m.write(0..3)?.copy_from_slice(data);
            let op = q.submit_one(Request::new(m.range(0..3)?, Transfer::Send))?;
            wait(q, &op.operation, 3)?;
            if word::<1>(s)? != [ack] {
                return Err("SRQ CQ dispatch".into());
            }
        }
    }
    first.close()?;
    second.close()?;
    pool.close()?;
    Ok(())
}
fn grouped(d: &Device, m: &RegisteredMemory, s: &mut TcpStream, server: bool) -> Result<()> {
    let mut g = d.completion_group(16)?;
    let config = QueueConfig {
        send_depth: 4,
        receive_depth: 4,
        ..QueueConfig::new(std::process::id() & 0xffffff)
    };
    let ids = [g.add(config)?, g.add(config)?];
    for &id in &ids {
        g.connect_over(id, s)?;
    }
    let ranges = [0..2, 8..10];
    if !server {
        for (r, b) in ranges.iter().zip([0x33, 0xaa]) {
            m.write(r.clone())?.fill(b);
        }
    }
    let mut ops = Vec::new();
    if !server && word::<1>(s)? != [5] {
        return Err("shared CQ readiness".into());
    }
    for i in (0..2).rev() {
        ops.push(
            g.submit_one(
                ids[i],
                Request::new(
                    m.range(ranges[i].clone())?,
                    if server {
                        Transfer::Receive
                    } else {
                        Transfer::Send
                    },
                ),
            )?
            .operation,
        );
    }
    if server {
        s.write_all(&[5])?;
    }
    let end = Instant::now() + Duration::from_secs(5);
    while ops.iter().any(|op| !op.is_reusable()) {
        if Instant::now() >= end {
            return Err("shared CQ deadline".into());
        }
        g.progress()?;
    }
    for op in ops {
        if op.outcome()
            != Some(Outcome::Success {
                bytes: 2,
                invalidated_rkey: None,
            })
        {
            return Err("shared CQ receipt".into());
        }
    }
    span(m, ranges[0].clone(), &[0x33; 2])?;
    span(m, ranges[1].clone(), &[0xaa; 2])?;
    s.write_all(&[6])?;
    if word::<1>(s)? != [6] {
        return Err("shared CQ verification".into());
    }
    g.close()?;
    Ok(())
}
fn datagram(d: &Device, s: &mut TcpStream, server: bool) -> Result<()> {
    let mut q = d.datagram_queue(QueueConfig::new(std::process::id() & 0xffffff), 0x1217)?;
    let a = q.address();
    s.write_all(&a.qpn.to_be_bytes())?;
    s.write_all(&a.qkey.to_be_bytes())?;
    s.write_all(&a.lid.to_be_bytes())?;
    s.write_all(&a.gid)?;
    s.write_all(&[a.mtu])?;
    let peer = DatagramAddress {
        qpn: u32::from_be_bytes(word(s)?),
        qkey: u32::from_be_bytes(word(s)?),
        lid: u16::from_be_bytes(word(s)?),
        gid: word(s)?,
        mtu: word::<1>(s)?[0],
        link: a.link,
    };
    q.set_destination(peer)?;
    let mut mem = d.register(HostBuffer::from_vec(vec![0x57; 64])?, Permissions::Local)?;
    for receiver in [true, false] {
        let (op, n) = if server == receiver {
            let op = q.receive(&mem.range(1..56)?)?;
            s.write_all(&[1])?;
            (op, 45)
        } else {
            if word::<1>(s)? != [1] {
                return Err("UD receive readiness".into());
            }
            mem.write(41..46)?.copy_from_slice(b"orbit");
            (q.send(&mem.range(41..46)?)?, 5)
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !op.operation.is_reusable() && Instant::now() < deadline {
            q.wait_for_progress(deadline)?;
        }
        if op.operation.outcome()
            != Some(Outcome::Success {
                bytes: n,
                invalidated_rkey: None,
            })
        {
            return Err(format!("UD completion {:?}", op.operation.outcome()).into());
        }
        if server == receiver
            && !op
                .operation
                .datagram()
                .is_some_and(|r| r.source_qp == peer.qpn && r.payload_bytes == 5)
        {
            return Err("UD source or payload length".into());
        }
        span(&mem, 41..46, b"orbit")?;
        span(&mem, 0..1, &[0x57])?;
        span(&mem, 56..64, &[0x57; 8])?;
    }
    q.close()?;
    mem.deregister()?;
    Ok(())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [role, addr, dev, port, gid] = args.as_slice() else {
        return Err("expected listen|connect ADDRESS DEVICE PORT GID".into());
    };
    let mut s = match role.as_str() {
        "listen" => {
            let l = TcpListener::bind(addr)?;
            println!("Listening on {}", l.local_addr()?);
            l.accept()?.0
        }
        "connect" => TcpStream::connect(addr)?,
        _ => return Err("unknown role".into()),
    };
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut d = Device::open(dev, port.parse()?, gid.parse()?, Limits::default())?;
    let mut q = d.atomic_queue_pair(QueueConfig::new(std::process::id() & 0xffffff), 2)?;
    q.connect_over(&mut s)?;
    let mut m = d.register(
        HostBuffer::from_vec(vec![b'.'; 40])?,
        Permissions::RemoteReadWrite,
    )?;
    if role == "listen" {
        server(&mut q, &m, &mut s)?;
    } else {
        client(&mut q, &m, &mut s)?;
    }
    q.close()?;
    shared_receive(&d, &m, &mut s, role == "listen")?;
    grouped(&d, &m, &mut s, role == "listen")?;
    datagram(&d, &mut s, role == "listen")?;
    m.deregister()?;
    d.close()?;
    println!(
        "Verified scatter/gather, immediate SEND and WRITE, fetch-add, compare/swap, two QPs sharing one SRQ and UD on {dev} ({role})."
    );
    Ok(())
}
