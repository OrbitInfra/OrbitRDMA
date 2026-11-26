use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    println!("cargo:rerun-if-changed=src/sys/port.c");
    cc::Build::new()
        .file("src/sys/port.c")
        .compile("orbit_port");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    bindgen::Builder::default()
        .header("src/verbs.h")
        .allowlist_type("ibv_.*")
        .allowlist_type("_compat_ibv_port_attr")
        .allowlist_function("orbit_query_port")
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
