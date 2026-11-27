# RC latency comparison

The three binaries use the same 4,096-byte payload and guard bytes. Each pair of peers uses one long-lived RC QP on the RXE device named `orbit_rxe`, port 1, GID index 1. The runner pins the two processes to different CPUs and alternates execution order across rounds. Use a quiet machine. QD1 measures local completion latency, including the respective public submission and completion APIs. The receiver checks the payload separately. The TCP readiness byte happens before the timed interval. Busy-poll SEND and Tokio SEND use different waiting strategies.

From the repository root on Linux with rdma-core, CMake, libclang and librdmacm development files installed:

```bash
export CARGO_TARGET_DIR=/tmp/orbit-compare
for peer in verbs asyn orbit; do
    cargo build --release --locked --manifest-path "benches/comparison/$peer/Cargo.toml"
done
BENCH_BIN_DIR="$CARGO_TARGET_DIR/release" python3 benches/comparison/run.py 16
```

Build the Orbit caller with `--features single` to select `submit_one`. Its default calls the existing list API even at QD1. To compare revisions, build the same caller against each checkout, save each executable with its own `bench-NAME` basename, then supply pairs such as `old:send new:send old:burst new:burst` after the round count. QD1 has 500 warmups and 5,000 timed operations per process pair. The QD32 burst posts 32 individually signaled WRITEs in one native list and drains all receipts before starting the next batch. It reports time per batch over 450 measured batches after 50 warmups. All 32 writes target the same 4 KiB range on a single RC QP. This measures queue overhead and is not a streaming link-bandwidth test. The summary is the median of the individual round percentiles.

The lockfiles pin ibverbs 0.9.2 and async-rdma 0.5.0, including the previously published transitive dependency used by async-rdma. Soft-RoCE timings characterize the software provider and this workload. Run on two physical hosts to publish NIC throughput or cross-host tail latency.
