use super::*;
use libloading::Library;

#[test]
fn negotiated_speed_decodes_xdr_and_leaves_unknown_values_unset() {
    for (speed, width, expected) in [
        (1, 1, Some(2500)),
        (4, 2, Some(40000)),
        (16, 4, Some(112000)),
        (32, 8, Some(300000)),
        (64, 2, Some(200000)),
        (128, 2, Some(400000)),
        (128, 16, Some(200000)),
        (256, 2, Some(800000)),
        (256, 1, Some(200000)),
        (512, 2, None),
        (0, 2, None),
        (3, 2, None),
        (128, 0, None),
        (128, 3, None),
    ] {
        let attr = PortAttributes {
            active_speed: speed,
            active_width: width,
            ..Default::default()
        };
        assert_eq!(link_speed_mbps(&attr), expected, "{speed}/{width}");
    }
}

#[test]
fn public_header_port_query_and_optional_symbol_preserve_fields_errors_and_units() {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!(
        "orbite-port-abi-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let source = dir.join("port_abi.c");
    std::fs::write(&source, include_str!("port_abi.c")).unwrap();
    let cc = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let speed_probe = dir.join("port_speed.c");
    std::fs::write(&speed_probe,
        "#include <infiniband/verbs.h>\nunsigned int speed(struct ibv_port_attr *a) { return a->active_speed_ex; }"
    ).unwrap();
    let speed_available = Command::new(&cc)
        .arg("-fsyntax-only")
        .arg(speed_probe)
        .output()
        .unwrap()
        .status
        .success();
    assert_eq!(cfg!(orbit_active_speed_ex), speed_available);
    for present in [false, true] {
        let library = dir.join(if present { "new.so" } else { "old.so" });
        let mut command = Command::new(&cc);
        command.args(["-shared", "-fPIC", "-Wall", "-Werror"]);
        if cfg!(feature = "mlx5") {
            command.arg("-DORBIT_TEST_MLX5");
        }
        if present {
            command.arg("-DORBITE_WITH_PORT_SPEED");
        }
        if cfg!(orbit_active_speed_ex) {
            command.arg("-DORBITE_HAVE_ACTIVE_SPEED_EX");
        }
        let output = command
            .arg(&source)
            .arg("-o")
            .arg(&library)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // SAFETY: The test loads its freshly compiled fixture library, whose initializer and ABI
        // it controls.
        let library = unsafe { Library::new(&library) }.unwrap();
        #[cfg(feature = "mlx5")]
        {
            type Init = unsafe extern "C" fn(*mut std::ffi::c_void, u64) -> c_int;
            // SAFETY: the C fixture exports these signatures and lives through both calls.
            unsafe {
                let has_pd = library
                    .get::<unsafe extern "C" fn() -> c_int>(b"orbit_has_mlx5_pd\0")
                    .unwrap();
                assert_eq!(has_pd() != 0, cfg!(orbit_mlx5dv_pd));
                let init = *library.get::<Init>(b"orbit_test_mlx5_init\0").unwrap();
                // The fixture checks the input PD for null and never dereferences it.
                let mut token = 0u8;
                let pd = std::ptr::from_mut(&mut token).cast();
                assert_eq!(
                    crate::provider::mlx5::pd_number(init, pd),
                    cfg!(orbit_mlx5dv_pd).then_some(0x123456)
                );
                assert_eq!(
                    crate::provider::mlx5::pd_number(init, std::ptr::null_mut()),
                    None
                );
            }
        }
        // SAFETY: The fixture exports this exact C signature, and the Library outlives its
        // symbol.
        let make_context = unsafe {
            library.get::<unsafe extern "C" fn(c_int) -> *mut ffi::ibv_context>(
                b"orbite_test_context\0",
            )
        }
        .unwrap();
        for mode in 0..3 {
            // SAFETY: The typed fixture symbol is loaded from the live library and returns its
            // own context.
            let context = unsafe { make_context(mode) };
            // SAFETY: The fixture library owns the returned verbs context and stays loaded during
            // the port query.
            let result = unsafe { query_port(context, 7) };
            if mode == 2 {
                assert!(matches!(result, Err(libc::EIO)));
                continue;
            }
            let attr = result.unwrap();
            assert_eq!((attr.state, attr.active_mtu, attr.gid_tbl_len), (4, 5, 17));
            assert_eq!((attr.lid, attr.link_layer, attr.active_width), (42, 2, 2));
            let expected = if mode == 0 && cfg!(orbit_active_speed_ex) {
                800_000
            } else {
                400_000
            };
            assert_eq!(link_speed_mbps(&attr), Some(expected));
        }
        // SAFETY: this fixture exports the legacy ABI and the library owns the callback.
        let legacy = unsafe {
            library.get::<unsafe extern "C" fn(
                *mut ffi::ibv_context,
                u8,
                *mut ffi::_compat_ibv_port_attr,
            ) -> c_int>(b"orbite_test_legacy_query\0")
        }
        .unwrap();
        // Exercise the legacy tag, an extended table with a null query callback,
        // and an older table ending one byte before that callback is available.
        for mode in [3, 4, 5] {
            // SAFETY: all three contexts are retained by the fixture library.
            let context = unsafe { make_context(mode) };
            assert_eq!(
                // SAFETY: the context, callback and output remain alive during dispatch.
                unsafe { query_with(context, 7, Some(*legacy)) }.map(|a| link_speed_mbps(&a)),
                Ok(Some(400_000))
            );
            assert_eq!(
                // SAFETY: the fixture context is live and the legacy callback is absent.
                unsafe { query_with(context, 7, None) }.err(),
                Some(libc::ENOSYS)
            );
        }
        // SAFETY: the fixture exports the exact optional rate ABI and stays loaded.
        let query = unsafe { library.get::<QueryPortSpeed>(b"ibv_query_port_speed\0") }.ok();
        assert_eq!(query.is_some(), present);
        if let Some(query) = query {
            for (port, expected) in [(1, 200_000), (4, 0)] {
                assert_eq!(
                    // SAFETY: This fixture symbol explicitly accepts a null context and remains
                    // loaded by the query owner.
                    unsafe { effective_speed(*query, std::ptr::null_mut(), port) }.unwrap(),
                    expected
                );
            }
            for (port, code) in [(2, libc::EIO), (3, libc::EOVERFLOW)] {
                assert!(
                    // SAFETY: This fixture symbol explicitly accepts a null context and remains
                    // loaded by the query owner.
                    matches!(unsafe { effective_speed(*query,std::ptr::null_mut(),port) },Err(crate::Error::Native {code:observed,..}) if observed==code)
                );
            }
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}
