mod connection;
mod traffic;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use orbit_rdma::{Error, LoadError, devices};

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("idle") => {}
        Some("connection") => {
            let mut args = std::env::args_os().skip(2);
            let device = args.next().expect("device name");
            let port = args
                .next()
                .expect("port")
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            let gid = args
                .next()
                .expect("GID index")
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            assert!(args.next().is_none());
            connection::exercise(&device, port, gid).unwrap();
            traffic::exercise(&device, port, gid).unwrap();
        }
        Some("host") => println!(
            "Host libibverbs reported {} device(s).",
            devices().unwrap().len()
        ),
        Some("without-runtime") => {
            assert!(matches!(
                devices(),
                Err(Error::Load(LoadError::Unavailable(_)))
            ));
        }
        Some("fixture") => {
            assert_eq!(
                devices().unwrap(),
                [OsStr::new("cpu-first"), OsStr::from_bytes(b"cpu-\xff")]
            );
        }
        Some("missing-symbol") => {
            assert!(matches!(
                devices(),
                Err(Error::Load(LoadError::MissingSymbol("ibv_destroy_qp")))
            ));
        }
        _ => panic!("expected a package test mode"),
    }
}
