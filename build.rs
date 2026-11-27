use bindgen::callbacks::{FieldInfo, ParseCallbacks};
use std::{env, path::PathBuf};

#[derive(Debug)]
struct HeaderFeatures;

impl ParseCallbacks for HeaderFeatures {
    fn field_visibility(&self, field: FieldInfo<'_>) -> Option<bindgen::FieldVisibilityKind> {
        match (field.type_name, field.field_name) {
            ("ibv_port_attr", "active_speed_ex") => {
                println!("cargo:rustc-cfg=orbit_active_speed_ex")
            }
            ("mlx5dv_obj", "pd") => println!("cargo:rustc-cfg=orbit_mlx5dv_pd"),
            _ => {}
        }
        None
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(orbit_active_speed_ex)");
    println!("cargo:rustc-check-cfg=cfg(orbit_mlx5dv_pd)");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let headers = if env::var_os("CARGO_FEATURE_MLX5").is_some() {
        "#include <infiniband/verbs.h>\n#if __has_include(<infiniband/mlx5dv.h>)\n#include <infiniband/mlx5dv.h>\n#endif"
    } else {
        "#include <infiniband/verbs.h>"
    };
    let headers = if env::var_os("CARGO_FEATURE_CM").is_some() {
        format!("{headers}\n#include <rdma/rdma_cma.h>")
    } else {
        headers.to_owned()
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    bindgen::Builder::default()
        .header_contents("orbit-rdma-abi.h", &headers)
        .allowlist_type(
            "ibv_.*|_compat_ibv_port_attr|verbs_context|rdma_cm_id|rdma_addrinfo|rdma_conn_param|rdma_port_space|mlx5dv_obj|mlx5dv_pd|mlx5dv_obj_type",
        )
        .allowlist_var("IBV_.*|RAI_.*|MLX5DV_OBJ_PD")
        .prepend_enum_name(false)
        .derive_default(true)
        .layout_tests(false)
        .generate_comments(false)
        .formatter(bindgen::Formatter::None)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .parse_callbacks(Box::new(HeaderFeatures))
        .generate()
        .expect("generate verbs ABI definitions from the installed headers")
        .write_to_file(out.join("verbs.rs"))
        .expect("write private verbs types");
}
