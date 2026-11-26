//! Controls for the test-owned, dynamically loaded CPU provider.
#![allow(unsafe_code)]
use libloading::Library;
use std::ffi::{CStr, CString, c_char};
use std::{fs, path::PathBuf, process::Command};

pub struct Provider(Library);
impl Provider {
    pub fn post_result(&self, accepted: i32, bad_style: i32) {
        // SAFETY: the test DSO copies these two integers during the call.
        unsafe {
            self.0
                .get::<unsafe extern "C" fn(i32, i32)>(b"orbit_post_result\0")
                .unwrap()(accepted, bad_style)
        }
    }
    pub fn emit(
        &self,
        queue: u32,
        id: u64,
        reported_qp: u32,
        fields: (u32, u32, u32),
        key: Option<u32>,
    ) {
        let (opcode, bytes, status) = fields;
        // SAFETY: only numeric WC fields enter the isolated test provider's queue.
        let rc = unsafe {
            self.0
                .get::<unsafe extern "C" fn(u32, u64, u32, u32, u32, u32, u32, u32) -> i32>(
                    b"orbit_cqe\0",
                )
                .unwrap()(
                queue,
                id,
                reported_qp,
                opcode,
                bytes,
                status,
                if key.is_some() { 8 } else { 0 },
                key.unwrap_or(0),
            )
        };
        assert_eq!(rc, 0);
    }
    pub fn allows(
        &self,
        desc: orbit_rdma::RemoteDescriptor,
        address: u64,
        len: u64,
        write: bool,
    ) -> bool {
        // SAFETY: the CPU provider observes its modeled QP and range, without dereferencing the address.
        unsafe {
            self.0
                .get::<unsafe extern "C" fn(u32, u32, u64, u64, i32) -> i32>(b"orbit_allows\0")
                .unwrap()(desc.qpn, desc.rkey, address, len, i32::from(write))
                != 0
        }
    }
    pub fn fault(&self, first: &str, second: &str) {
        let a = CString::new(first).unwrap();
        let b = CString::new(second).unwrap();
        // SAFETY: these are the fixture's exact signatures; both strings live through the call.
        unsafe {
            self.0
                .get::<unsafe extern "C" fn(*const c_char, *const c_char)>(b"orbit_fault\0")
                .unwrap()(a.as_ptr(), b.as_ptr())
        }
    }
    pub fn option(&self, mode: &str) {
        let c = CString::new(mode).unwrap();
        // SAFETY: the retained fixture copies the string before returning.
        unsafe {
            self.0
                .get::<unsafe extern "C" fn(*const c_char)>(b"orbit_option\0")
                .unwrap()(c.as_ptr())
        }
    }
    pub fn events(&self) -> Vec<String> {
        // SAFETY: this isolated process queries the fixture's static log between native calls.
        unsafe {
            let get = self
                .0
                .get::<unsafe extern "C" fn() -> *const c_char>(b"orbit_history\0")
                .unwrap();
            CStr::from_ptr(get())
                .to_str()
                .unwrap()
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        }
    }
    pub fn attr(&self, index: i32) -> u32 {
        // SAFETY: this reads a recorded attribute from this fixture instance.
        unsafe {
            self.0
                .get::<unsafe extern "C" fn(i32) -> u32>(b"orbit_attr\0")
                .unwrap()(index)
        }
    }
    pub fn count(&self, op: &str) -> usize {
        self.events().iter().filter(|e| *e == op).count()
    }
}
struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

pub fn isolated(name: &str, run: impl FnOnce(&Provider)) {
    const CHILD: &str = "ORBITRDMA_OWNER_TEST";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        // SAFETY: the parent selected only its own freshly built fixture on the loader path.
        let library = unsafe { Library::new("libibverbs.so.1") }.unwrap();
        run(&Provider(library));
        return;
    }
    let mk = Command::new("mktemp").arg("-d").output().unwrap();
    assert!(mk.status.success());
    let dir = Dir(PathBuf::from(String::from_utf8(mk.stdout).unwrap().trim()));
    let output = Command::new("cc")
        .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/unit/resources.c"
        ))
        .arg("-o")
        .arg(dir.0.join("libibverbs.so.1"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .env("LD_LIBRARY_PATH", &dir.0)
        .output()
        .unwrap();
    assert!(
        child.status.success() && String::from_utf8_lossy(&child.stdout).contains("1 passed"),
        "{}\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}
