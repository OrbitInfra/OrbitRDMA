#!/usr/bin/env bash
# Build the public consumer from the archive, then exercise it in an empty Linux rootfs.
set -euo pipefail
if (( $# != 0 && $# != 3 )); then
    echo "usage: bash tests/package/check.sh [device port gid-index]" >&2
    exit 2
fi

cd "$(dirname "$0")/../.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cargo package --locked
archive=$(cargo metadata --locked --format-version 1 --no-deps | python3 -c '
import json, pathlib, sys
m = json.load(sys.stdin)
p = m["packages"][0]
print(pathlib.Path(m["target_directory"], "package", "{}-{}.crate".format(p["name"], p["version"])))
')
mkdir -p "$work/pkg" "$work/consumer/src" "$work/fixture" "$work/root"
tar -xf "$archive" --strip-components=1 -C "$work/pkg"
cp "$work/pkg/tests/package/probe.rs" "$work/consumer/src/main.rs"
cp "$work/pkg/tests/package/connection.rs" "$work/consumer/src/"
cp "$work/pkg/tests/package/traffic.rs" "$work/consumer/src/"
cp "$work/pkg/Cargo.lock" "$work/consumer/"
cargo test --offline --locked --manifest-path "$work/pkg/Cargo.toml" --test owners
cat > "$work/consumer/Cargo.toml" <<'TOML'
[package]
name = "orbit-rdma-consumer"
version = "0.0.0"
edition = "2024"

[dependencies]
orbit-rdma = { path = "../pkg" }
TOML

# Adding this consumer changes the root package in the copied lockfile.
(cd "$work/consumer" && cargo metadata --offline --format-version 1 \
    --filter-platform "$(rustc -vV | sed -n 's/^host: //p')" >/dev/null)
cargo build --manifest-path "$work/consumer/Cargo.toml" --offline --locked --target-dir "$work/build"
probe="$work/build/debug/orbit-rdma-consumer"
dynamic=$(LC_ALL=C readelf -d "$probe")
if [[ "$dynamic" == *libibverbs* ]]; then
    echo "consumer has a link-time dependency on libibverbs" >&2
    exit 1
fi
awk '/\(NEEDED\)/ { print }' <<<"$dynamic"
cc -shared -fPIC -Wall -Wextra -Werror "$work/pkg/tests/unit/native.c" -o "$work/fixture/libibverbs.so.1"
LD_LIBRARY_PATH="$work/fixture" "$probe" fixture
"$probe" host
sed 's/ibv_destroy_qp(/omitted_destroy_qp(/' "$work/pkg/tests/unit/native.c" > "$work/incomplete.c"
cc -shared -fPIC -Wall -Wextra -Werror "$work/incomplete.c" -o "$work/fixture/libibverbs.so.1"
LD_LIBRARY_PATH="$work/fixture" "$probe" missing-symbol
cc -shared -fPIC -Wall -Wextra -Werror "$work/pkg/tests/unit/resources.c" -o "$work/fixture/libibverbs.so.1"
LD_LIBRARY_PATH="$work/fixture" "$probe" connection cpu-first 1 0
if (( $# == 3 )); then
    "$probe" connection "$@"
fi

# Populate only the interpreter and shared objects the consumer already needs.
# Include the interpreter's exact path; glibc uses multiarch paths on some hosts.
cp "$probe" "$work/root/probe"
deps=$(LC_ALL=C ldd "$probe")
if [[ "$deps" == *libibverbs* || "$deps" == *"not found"* ]]; then
    echo "unexpected consumer dependency: $deps" >&2
    exit 1
fi
while read -r lib; do
    cp -L --parents "$lib" "$work/root"
done < <(awk '/=> \// { print $3 } /^[[:space:]]*\// { print $1 }' <<<"$deps")
elevate=()
if (( EUID != 0 )); then elevate=(sudo -n); fi
"${elevate[@]}" chroot "$work/root" /probe idle
"${elevate[@]}" chroot "$work/root" /probe without-runtime
sha256sum "$archive" "$probe"
echo "Packaged consumer passed: discovery, RC/MR/window lifecycle, READ/WRITE and SEND/RECV on the CPU provider, ELF dependencies and missing-runtime controls."
