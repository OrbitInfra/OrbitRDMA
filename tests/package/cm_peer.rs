//! Standard CM listen/connect and message completion over the provider-selected route.
#![forbid(unsafe_code)]
use orbit_rdma::{Error, PostStatus, cm::*};
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn complete(q: &mut CmConnection) -> Result<CmMessage> {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        if let Some(m) = q.poll()? {
            return Ok(m);
        }
        std::thread::yield_now();
    }
    Err("CM completion deadline".into())
}
fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let role = args.next().ok_or("missing role")?;
    let addr: SocketAddr = args
        .next()
        .ok_or("missing numeric RDMA interface address")?
        .parse()?;
    let mut q = match role.as_str() {
        "listen" => {
            let mut l = CmListener::bind(addr, 4)?;
            let mut actual = addr;
            actual.set_port(l.local_port());
            println!("Listening on {actual}");
            l.accept()?
        }
        "connect" => CmConnection::connect(addr)?,
        _ => return Err("expected listen or connect".into()),
    };
    for (send, expected) in [(role == "connect", b"ping"), (role == "listen", b"pong")] {
        let status = if send {
            q.send(expected.to_vec())?
        } else {
            q.receive(vec![0xa5; 8])?
        };
        if status != PostStatus::Complete || !matches!(q.close(), Err(Error::Busy)) {
            return Err("CM admission".into());
        }
        let result = complete(&mut q)?;
        if result.bytes != 4
            || &result.buffer[..4] != expected
            || (!send && result.buffer[4..] != [0xa5; 4])
        {
            return Err("CM payload or guard".into());
        }
    }
    // Flush a posted receive without exposing it to another local user.
    if role == "listen" {
        q.receive(vec![0; 8])?;
        q.abort()?;
    } else {
        q.close()?;
    }
    println!("Verified RDMA-CM ping/pong ({role}).");
    Ok(())
}
