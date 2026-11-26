#!/usr/bin/env bash
# Run inside a Linux environment with a C compiler and rdma-core headers.
set -euo pipefail
cd "$(dirname "$0")/.."
rustc -vV
cargo fmt --all -- --check
rustfmt --edition 2024 --check tests/package/probe.rs
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo test --locked --doc
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
bash tests/package/check.sh
