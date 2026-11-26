# OrbitRDMA

OrbitRDMA is a Rust library for moving bytes between registered host buffers over Linux RDMA. It supports one-sided READ/WRITE and two-sided SEND/RECV on reliable connected (RC) queue pairs. The application chooses its control channel and decides which ranges to expose.

The standalone Cargo package is `orbit-rdma`, imported as `orbit_rdma`. Version `0.1.0` supports batch submission, polling and completion-channel waits with a deadline. [Development plan](docs/development-plan.md#delivery-order)

## Try device discovery

Use stable Rust or Rust 1.85.0 on Linux. On Debian or Ubuntu, install `build-essential`, `libclang-dev` and `libibverbs-dev`. Add the dependency with `cargo add orbit-rdma@0.1`.

```rust
fn main() -> Result<(), orbit_rdma::Error> {
    for device in orbit_rdma::devices()? {
        println!("{}", device.to_string_lossy());
    }
    Ok(())
}
```

The first call loads `libibverbs.so.1` and checks its host verbs exports. The process caches that load result, including a failure. Once it succeeds, each call enumerates devices again and returns owned names. A host with a working runtime and no visible device returns an empty list. A loader failure is `Error::Load(LoadError::Unavailable(_))` or `Error::Load(LoadError::MissingSymbol(name))`. Enumeration errors retain the underlying I/O error in `Error::DeviceList`. [rdma-core device list contract](https://github.com/linux-rdma/rdma-core/blob/master/libibverbs/man/ibv_get_device_list.3.md)

## Connect peers and register a buffer

Open a device by its exact name, physical port and GID index. Supply a 24-bit initial packet sequence number for each queue pair. Both peers call `connect` with the other's parameters. The application chooses its control channel and serialization.

```rust
use orbit_rdma::{ConnectionParams, Device, Error, QueueConfig, QueuePair};

fn connect_peer(
    device: &Device,
    psn: u32,
    exchange: impl FnOnce(ConnectionParams) -> Result<ConnectionParams, Error>,
) -> Result<QueuePair, Error> {
    let mut qp = device.queue_pair(QueueConfig::new(psn))?;
    let peer = exchange(qp.parameters())?;
    qp.connect(&peer)?;
    Ok(qp)
}
```

`Device::register` accepts an owned `HostBuffer`. Use `HostBuffer::from_vec` to transfer a vector without copying, or `unsafe HostBuffer::from_external` to hand the crate an exclusive, stable host-memory loan. A local `read` or `write` guard keeps its registration alive. Overlapping reads can coexist. An overlapping writer or early deregistration returns `Error::Busy`.

To expose a range, call `qp.expose(&region, start..end, RemoteAccess::Read)` or request write access. Each window owns one connected QP. Poll `descriptor()` until the bind completion arrives, then send the descriptor to the connected peer. Poll `revoke()` until it returns `true`. At that point the window has retired, its QP has drained and the range can be borrowed again. A failed native destroy reports `Error::Unquiesced` and retains the affected resources and their charges. [Runnable packaged example](tests/package/connection.rs) · [CPU fault tests](tests/owners.rs)

## Submit work and reuse a buffer

Select local bytes with `RegisteredMemory::range`. Submit a list of READ, WRITE or SEND requests on the send queue, or a list of RECV requests on the receive queue. Each tracked request has its own completion and owns an independent DMA loan. A receive reports how many bytes arrived in its particular slot and any key the peer invalidated.

```rust
use orbit_rdma::{Operation, QueuePair, RegisteredMemory, RemoteDescriptor, Request, Transfer};

fn post_page(qp: &mut QueuePair, source: &RegisteredMemory, target: RemoteDescriptor)
    -> Result<Operation, Box<dyn std::error::Error>>
{
    let write = Request::new(source.range(0..4096)?, Transfer::Write { remote: target, offset: 0 });
    let mut receipt = qp.submit(vec![write])?;
    receipt.operations.pop().ok_or_else(|| "missing tracked write".into())
}
```

| Native post result | What the application owns |
| --- | --- |
| Zero accepted | `Err(Rejected)` returns every original request. All reservations from this submission have been released. |
| Known prefix accepted | `PostStatus::Partial` returns an operation for each accepted request and the unposted suffix in order. The suffix gives up its loans immediately. Bytes overlapping an accepted request remain subject to that request's loan. |
| First failure cannot be identified | `PostStatus::Uncertain` returns an operation for every possible WR and fences new submissions on that QP. A WRITE can have changed the peer even after local resources are recovered. The library issues it once. |

Drive the QP with `progress()`, or call `op.wait_until(&mut qp, deadline)`. A timeout, logical cancellation or dropped operation handle leaves the loan with the QP. A matching CQE can free an individual range or an ordered SEND/WRITE group. Successful QP destruction frees the ranges still held by that QP. A failed QP destroy retains them and reports the native error on subsequent attempts to reclaim.

`op.try_reclaim()` returns the original range and its outcome when ready, or a tracked handle while it is pending or unquiesced. Receive and READ destinations have exclusive loans. A WRITE or SEND source permits other readers. The checked-out range can safely be modified again after its last conflicting loan retires. [Packaged four-operation example](tests/package/traffic.rs) · [Failure and reuse tests](tests/unit/transfers.rs)

## Platform and project boundary

OrbitRDMA uses Rust edition 2024 with an MSRV of 1.85. The loader calls the host's rdma-core runtime through a private function table. Each supported RoCE or InfiniBand configuration will have its own two-host qualification record. [Linux RDMA userspace stack](https://github.com/linux-rdma/rdma-core)

OrbiTE uses the same WR posting and CQ decoding as the standalone library. Its progress workers pass their live queue handles to `verbs::post` and `verbs::poll`. OrbiTE owns each QP, shared CQ, cross-transport memory loan, peer grant and retry decision. The safe `QueuePair` API owns those resources for standalone applications. [OrbiTE RDMA implementation](https://github.com/OrbitInfra/OrbiTE/tree/main/src/transport/rdma) · [Integration boundary](docs/development-plan.md#extraction-map)

## Check two peers

Build `cargo build --locked --release --example host_peer` on Linux. On a pair of hosts with an RC provider supporting type-2B memory windows, run the listener first. Each host selects its own device, port and GID index.

```sh
# Listener
target/release/examples/host_peer listen 0.0.0.0:7400 mlx5_0 1 0
# Requester (substitute the listener's reachable address)
target/release/examples/host_peer connect 192.0.2.1:7400 mlx5_0 1 0
```

Both processes check each transferred byte and the surrounding guards. TCP carries QP parameters, window descriptors and acknowledgements. READ, WRITE, SEND and RECV use the verbs provider. Linux CPU tests cover partial posts, delayed completions, failed cleanup and packaged consumers. Two-host RNIC qualification and performance measurements for this version are pending. [Two-peer consumer](tests/package/host_peer.rs) · [Verification plan](docs/development-plan.md#release-decision)

## License and provenance

OrbitRDMA is licensed under Apache-2.0. Its loader, port-query ABI bridge, RC transitions, host MR/window operations, key-history rules, WR prefix accounting and CQ retirement were adapted from OrbiTE commit `e4a12a20b4f47201c48f9e562aa5d95ea1db83e4`. The 25 required host exports and optional `ibv_query_port_speed` are resolved at runtime. [License](LICENSE) · [Extraction map](docs/development-plan.md#extraction-map) · [Original verbs code](https://github.com/OrbitInfra/OrbiTE/tree/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/sys)
