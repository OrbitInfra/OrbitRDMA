#!/usr/bin/env bash
# Run both processes on one Linux host with the selected RC provider.
set -euo pipefail
if (( $# < 4 || $# > 5 )); then
    echo "usage: $0 HOST_PEER_BINARY DEVICE PORT GID_INDEX [REPETITIONS]" >&2
    exit 2
fi
binary=$1
device=$2
port=$3
gid=$4
count=${5:-1}
[[ $count =~ ^[1-9][0-9]*$ ]] || exit 2
work=$(mktemp -d)
server=
cleanup() {
    if [[ -n "$server" ]]; then
        kill "$server" 2>/dev/null || true
        wait "$server" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT
for ((i=1; i<=count; i++)); do
    # Port zero lets the kernel choose a free port. The listener prints the
    # actual bound address before accepting its single control connection.
    timeout 30 "$binary" listen 127.0.0.1:0 "$device" "$port" "$gid" >"$work/server" 2>&1 &
    server=$!
    address=
    for ((n=0; n<200; n++)); do
        address=$(sed -n 's/^Listening on \(127\.0\.0\.1:[0-9]*\)$/\1/p' "$work/server")
        [[ -n "$address" ]] && break
        kill -0 "$server" 2>/dev/null || break
        sleep 0.02
    done
    if [[ -z "$address" ]] || ! timeout 30 "$binary" connect "$address" "$device" "$port" "$gid" >"$work/client" 2>&1; then
        cat "$work/server" >&2
        if [[ -f "$work/client" ]]; then cat "$work/client" >&2; fi
        exit 1
    fi
    if ! wait "$server"; then
        server=
        cat "$work/server" "$work/client" >&2
        exit 1
    fi
    server=
    if (( i == 1 )); then cat "$work/server" "$work/client"; fi
done
echo "$count two-process runs passed on $device port $port GID $gid"
