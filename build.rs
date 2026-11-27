use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    println!("cargo:rerun-if-changed=src/sys/port.c");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let mut native = cc::Build::new();
    if env::var_os("CARGO_FEATURE_MLX5").is_some() {
        native.define("ORBIT_MLX5", None);
    }
    let probe = out.join("port_speed_ex.c");
    std::fs::write(&probe, "#include <infiniband/verbs.h>\nint orbit_probe(struct ibv_port_attr *a) { return a->active_speed_ex; }\n").expect("write port ABI probe");
    let extended = native
        .get_compiler()
        .to_command()
        .arg("-c")
        .arg(&probe)
        .arg("-o")
        .arg(out.join("port_speed_ex.o"))
        .output()
        .expect("probe port speed layout")
        .status
        .success();
    if extended {
        native.define("ORBIT_HAVE_ACTIVE_SPEED_EX", None);
    }
    println!("cargo:rustc-env=ORBIT_RDMA_ACTIVE_SPEED_EX={extended}");
    native.file("src/sys/port.c").compile("orbit_port");
    bindgen::Builder::default()
        .header("src/verbs.h")
        .allowlist_type("ibv_.*")
        .allowlist_type("_compat_ibv_port_attr")
        .allowlist_function("orbit_query_port|orbit_port_speed_bits")
        .allowlist_var("IBV_.*")
        .prepend_enum_name(false)
        .derive_default(true)
        .layout_tests(false)
        .generate_comments(false)
        .formatter(bindgen::Formatter::None)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("generate private verbs types from <infiniband/verbs.h>")
        .write_to_file(out.join("verbs.rs"))
        .expect("write private verbs types");
}
