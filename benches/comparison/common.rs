//! Shared workload and validation for the standalone comparison executables.
#![allow(dead_code)]
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub const N: usize = 4096;
pub const WARM: usize = 500;
pub const COUNT: usize = 5000;
pub const TOTAL: usize = WARM + COUNT;
pub const GUARD: u8 = 0xba;

pub fn connection() -> Result<(bool, String, TcpStream)> {
    let args: Vec<_> = std::env::args().collect();
    let [_, role, addr, work] = args.as_slice() else {
        return Err("expected listen|connect addr send|rw".into());
    };
    let server = role == "listen";
    let s = if server {
        let l = TcpListener::bind(addr)?;
        println!("LISTEN {}", l.local_addr()?);
        l.accept()?.0
    } else {
        TcpStream::connect(addr)?
    };
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(Duration::from_secs(15)))?;
    s.set_write_timeout(Some(Duration::from_secs(15)))?;
    Ok((server, work.clone(), s))
}

pub fn signal(s: &mut TcpStream) -> Result<()> {
    s.write_all(b"R")?;
    Ok(())
}
pub fn wait(s: &mut TcpStream) -> Result<()> {
    let mut b = [0];
    s.read_exact(&mut b)?;
    if b != *b"R" {
        return Err("control byte".into());
    }
    Ok(())
}
pub fn barrier(s: &mut TcpStream) -> Result<()> {
    signal(s)?;
    wait(s)
}

pub fn pattern(iter: usize) -> [u8; N] {
    let mut value = [0; N];
    for (i, b) in value.iter_mut().enumerate() {
        *b = (i.wrapping_mul(73) ^ (i >> 4)) as u8;
    }
    value[..8].copy_from_slice(&(iter as u64).to_le_bytes());
    value
}
pub fn check(buf: &[u8], iter: usize) -> Result<()> {
    if buf.len() != N + 2 || buf[0] != GUARD || buf[N + 1] != GUARD || buf[1..=N] != pattern(iter) {
        return Err(format!("payload/guard mismatch {iter}").into());
    }
    Ok(())
}
pub fn sample(out: &mut Vec<u64>, i: usize, start: Instant) {
    let ns = start.elapsed().as_nanos() as u64;
    if i >= WARM {
        out.push(ns);
    }
}
pub fn report(op: &str, mut out: Vec<u64>) {
    assert_eq!(out.len(), COUNT);
    out.sort_unstable();
    println!(
        "SAMPLE {op} {} {}",
        out[COUNT / 2 - 1],
        out[COUNT * 99 / 100 - 1]
    );
}
