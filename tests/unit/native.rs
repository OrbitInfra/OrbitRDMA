//! Adapted from OrbiTE's native loader controls for OrbitRDMA's standalone discovery API.
use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        // create_dir is exclusive, including after a prior process with the
        // same PID exited before it could remove its temporary directory.
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("orbit-rdma-{}-{n}", std::process::id()));
            match fs::create_dir(&dir) {
                Ok(()) => return Self(dir),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create fixture directory: {e}"),
            }
        }
    }

    fn build(&self, optional: bool, missing: Option<&str>) -> PathBuf {
        let file = self
            .0
            .join(format!("{}-{}.so", optional, missing.unwrap_or("all")));
        // Rename only the definition. The unchanged system header checks the
        // ABI of the complete fixture on each runner.
        let mut source = include_str!("native.c").to_owned();
        if let Some(name) = missing {
            let needle = format!("{name}(");
            assert_eq!(source.matches(&needle).count(), 1);
            source = source.replace(&needle, "orbit_missing_export(");
        }
        let input = file.with_extension("c");
        fs::write(&input, source).unwrap();
        let mut cc = Command::new("cc");
        cc.args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"]);
        if optional {
            cc.arg("-DORBITRDMA_OPTIONAL_VERBS");
        }
        let output = cc.arg(&input).arg("-o").arg(&file).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        file
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn missing_library_and_each_missing_export_return_typed_errors() {
    let fixture = Fixture::new();
    assert_eq!(
        REQUIRED.len(),
        25,
        "host exports in the pinned OrbiTE table"
    );
    assert!(matches!(
        Native::load(fixture.0.join("absent.so").as_os_str()),
        Err(LoadError::Unavailable(_))
    ));
    for &name in REQUIRED {
        let file = fixture.build(false, Some(name));
        assert_eq!(
            Native::load(file.as_os_str()).err(),
            Some(LoadError::MissingSymbol(name))
        );
    }
}

#[test]
fn table_retains_loaded_code_with_and_without_optional_port_speed() {
    let fixture = Fixture::new();
    for optional in [false, true] {
        let file = fixture.build(optional, None);
        let native = Native::load(file.as_os_str()).unwrap();
        fs::remove_file(file).unwrap();
        assert_eq!(native.devices().unwrap().len(), 2);
        // SAFETY: this fixture's close callback accepts null and returns EIO.
        let close_status = unsafe { (native.ibv_close_device)(std::ptr::null_mut()) };
        assert_eq!(close_status, libc::EIO);
        assert_eq!(native.port_speed.is_some(), optional);
        if let Some(speed) = native.port_speed {
            let mut value = 0;
            // SAFETY: our fixture ignores the context pointer and writes one u64.
            assert_eq!(unsafe { speed(std::ptr::null_mut(), 1, &mut value) }, 0);
            assert_eq!(value, 2000);
        }
    }
}

#[test]
fn discovery_copies_names_and_frees_every_acquired_list_once() {
    let fixture = Fixture::new();
    let file = fixture.build(false, None);
    let native = Native::load(file.as_os_str()).unwrap();
    // SAFETY: this test owns this DSO instance and uses the fixture's C signatures.
    let mode = unsafe {
        native
            ._library
            .get::<unsafe extern "C" fn(c_int)>(b"orbit_fixture_mode\0")
    }
    .unwrap();
    // SAFETY: the counter accessor belongs to the same retained fixture instance.
    let frees = unsafe {
        native
            ._library
            .get::<unsafe extern "C" fn() -> c_int>(b"orbit_fixture_frees\0")
    }
    .unwrap();

    let names = native.devices().unwrap();
    assert_eq!(
        names,
        [OsStr::new("cpu-first"), OsStr::from_bytes(b"cpu-\xff")]
    );
    // SAFETY: both test functions access only this instance's counters and mode.
    unsafe {
        assert_eq!(frees(), 1);
        mode(1);
        assert!(native.devices().unwrap().is_empty());
        assert_eq!(frees(), 2);
        mode(2);
        assert_eq!(
            native.devices().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(frees(), 2);
        mode(3);
        assert_eq!(
            native.devices().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(frees(), 3);
    }
}

#[test]
fn public_discovery_is_lazy_and_caches_the_first_load_result() {
    const CASE: &str = "ORBITRDMA_FIXTURE_CASE";
    if let Ok(case) = std::env::var(CASE) {
        let maps = || fs::read_to_string("/proc/self/maps").unwrap();
        assert!(!maps().contains("libibverbs.so"));
        let path = Path::new(&std::env::var_os("LD_LIBRARY_PATH").unwrap()).join("libibverbs.so.1");
        match case.as_str() {
            "complete" => {
                assert_eq!(crate::devices().unwrap().len(), 2);
                assert!(maps().contains("libibverbs.so.1"));
                fs::remove_file(&path).unwrap();
                assert_eq!(crate::devices().unwrap().len(), 2);
            }
            "incomplete" => {
                let check = || {
                    assert!(matches!(
                        crate::devices(),
                        Err(crate::Error::Load(LoadError::MissingSymbol(
                            "ibv_destroy_qp"
                        )))
                    ))
                };
                check();
                fs::rename(path.with_extension("ready"), path).unwrap();
                check();
            }
            _ => panic!("unknown fixture case {case}"),
        }
        return;
    }
    for missing in [None, Some("ibv_destroy_qp")] {
        let fixture = Fixture::new();
        let path = fixture.0.join("libibverbs.so.1");
        fs::rename(fixture.build(false, missing), &path).unwrap();
        if missing.is_some() {
            fs::rename(fixture.build(true, None), path.with_extension("ready")).unwrap();
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sys::tests::public_discovery_is_lazy_and_caches_the_first_load_result",
            ])
            .env("LD_LIBRARY_PATH", &fixture.0)
            .env(
                CASE,
                if missing.is_some() {
                    "incomplete"
                } else {
                    "complete"
                },
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }
}
