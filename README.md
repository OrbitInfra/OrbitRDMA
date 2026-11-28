# OrbitRDMA

OrbitRDMA is a Rust library for moving bytes between registered host buffers over Linux RDMA. It supports one-sided READ/WRITE and two-sided SEND/RECV on reliable connected (RC) queue pairs. The application chooses its control channel and decides which ranges to expose.

The standalone Cargo package is `orbit-rdma`, imported as `orbit_rdma`. Version `0.2.1` adds `QueuePair::send_and_poll` for busy-polling RC senders and speeds up single SENDs through the existing `submit_one` API. [Development plan](docs/development-plan.md#delivery-order)

## Try device discovery

Use stable Rust or Rust 1.85.0 on Linux. On Debian or Ubuntu, install `build-essential`, `libclang-dev` and `libibverbs-dev`. Add the dependency with `cargo add orbit-rdma@0.2`.

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

Open a device by name, port and GID index. Supply a 24-bit initial packet sequence number for each QP. Both peers can use a connected TCP or Unix stream to exchange the two fixed 30-byte records and bring their local RC queues to RTS.

```rust
#![forbid(unsafe_code)]
use orbit_rdma::{Device, Error, QueueConfig, QueuePair};
use std::net::TcpStream;

fn connect_peer(device: &Device, psn: u32, control: &mut TcpStream) -> Result<QueuePair, Error> {
    let mut qp = device.queue_pair(QueueConfig::new(psn))?;
    qp.connect_over(control)?;
    Ok(qp)
}
```

The application establishes peer identity and sets stream deadlines. After a failed or interrupted exchange, start a handshake on a fresh stream. To keep an existing control protocol, exchange `qp.parameters()` and call `qp.connect(&peer)` (also available in 0.1.2). `orbit-rdma` also provides `RemoteDescriptor::to_bytes` and `from_bytes` for the 25-byte window records used by the 0.1.2 example. Publish a descriptor only after bind completion and coordinate remote use before revocation. [Two-process consumer](tests/package/host_peer.rs)

`Device::register` accepts an owned `HostBuffer`. Use `HostBuffer::from_vec` to transfer a vector without copying, or `unsafe HostBuffer::from_external` to hand the crate an exclusive, stable host-memory loan. A local `read` or `write` guard keeps its registration alive. Overlapping reads can coexist. An overlapping writer or early deregistration returns `Error::Busy`.

## Keep a connection open across windows

`QueuePair::expose_window` borrows a connected QP and grants access to one range of registered **host** memory. Two disjoint windows can be open while the QP also sends or receives data. The provider must support RC and type-2B memory windows.

```rust
let mut a = qp.expose_window(&region, 0..4096, Permissions::RemoteWrite)?;
let b = qp.expose_window(&region, 4096..8192, Permissions::RemoteReadWrite)?;
```

Poll `window.descriptor(&mut qp, RemoteAccess::Write)` or request a read descriptor. Publish it to the connected peer when it returns `Some`. Each window has its own rkey. Both directional descriptors of a read/write window carry the same rkey, which grants both rights in hardware. Create a single-direction window to enforce narrower access.

The grant owner initiates revocation. The peer acknowledges its last use over the control channel or with a plain SEND. Reserve SEND_WITH_INV for keys managed by a separate protocol. Its `invalidated_rkey` on a receive completion does not retire a `SharedWindow`.

Once the peer finishes using A, poll `a.revoke(&mut qp)`. The call returns `true` after A's invalidate completion, MW deallocation and private MR deregistration. A's bytes can then be borrowed locally, and the peer can still READ and WRITE B on the original QP. Coordinate retirement with the peer because submitting work with a stale rkey can put the RC connection into error. Poll or use `qp.wait_for_progress(deadline)` between attempts. A deadline or a discarded window token leaves its loan with the QP. When closing with windows still attached, the queue enters RESET to remove outstanding local work, detaches the keys and destroys the QP before returning the affected registrations and buffers. A failed reset, detach or destroy retains the dependent resources.

Each shared window consumes one MW and one additional registration for precisely the granted bytes. `Device::usage()` includes both in the configured limits. A provably unposted invalidation can be retried. An ambiguous post or an invalid completion fences new work until the queue is closed. A failed destructor returns `Error::Unquiesced` and retains the affected backing and budget. [Two-process example](tests/package/host_peer.rs) · [CPU lifetime tests](tests/unit/shared_windows.rs)

The dedicated-window API `qp.expose(&region, range, access)` dedicates one QP to one window. Its `revoke()` also drains that QP. [Dedicated-window example](tests/package/connection.rs)

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

For a busy-polling RC sender, `qp.send_and_poll(range)` posts one signaled SEND. It polls once when the QP owns its CQ, has no earlier payload requests, and has neither a shared receive queue nor attached windows. Otherwise it returns a tracked submission after posting. `PolledSend::Completed(c)` returns the range on that SEND's successful local completion. A write to the range still requires every overlapping reader, including SENDs on other QPs, to finish. The peer can still be processing the message. `PolledSend::Tracked { submission, progress_error }` preserves the operation on a delayed CQE, an ambiguous post, or a polling error. Inspect the status and drive the queue or close it to drain. After retirement, the next SEND can take the same `Completion::buffer` again. Each post acquires a fresh DMA loan for the bytes.

## Wait with Tokio

The optional `tokio` Cargo feature supports asynchronous clients. It adds `connect_over_async` for an existing asynchronous stream, `QueuePair::wait_for_progress_async` for CQ progress (including window changes), and `Operation::wait_async` for one posted transfer. Enable I/O on the Tokio runtime.

```rust
use orbit_rdma::{Error, Operation, QueuePair, Reclaim};

async fn reclaim(op: Operation, qp: &mut QueuePair) -> Result<Reclaim, Error> {
    op.wait_async(qp).await?;
    Ok(op.try_reclaim())
}
```

A waiter polls the CQ, arms it and polls again before sleeping. The first wait that needs to sleep registers the completion channel with the Tokio reactor. Subsequent waits on that QP and runtime reuse the registration. Readiness wakes the task. Only a matching completion or successful teardown releases a DMA loan. A dropped future or an expired `tokio::time::timeout` leaves the operation on the QP. Resume with either the blocking or asynchronous progress API. The default feature set has no Tokio dependency. [Asynchronous peer](tests/package/async_peer.rs) · [Cancellation and race tests](tests/unit/async_progress.rs)

## RC, datagrams and shared queues

| Workload | Public API and completion contract |
| --- | --- |
| One RC request or a list | `submit_one` returns one tracked operation. `submit` returns the accepted or possible prefix and any proven unposted suffix. Polling drains up to 32 CQEs per provider call. |
| One WR spanning several host buffers | Request the device limit with `queue_pair_with_sge`, then call `submit_vectored`. All local ranges stay loaned through the same physical receipt. |
| Notify a peer | `submit_with_immediate` sends a 32-bit value with SEND or WRITE. `Operation::immediate` distinguishes message bytes from bytes written to a remote window. A WRITE notification consumes one receive credit and writes zero bytes into that receive buffer. |
| Remote atomic updates | Enable `atomic_queue_pair`, expose an `AtomicWindow`, and call `submit_atomic` with fetch-add or compare/swap. The result range holds the old 64-bit value. Hardware must support atomics. A failed or uncertain atomic must not be replayed automatically. |
| Multiple QPs | `completion_group` reserves bounded capacity on one CQ and dispatches completions by QPN and WR identity. `shared_receive_queue` posts a common receive pool that several RC QPs can consume. The two mechanisms can be used independently. |
| Unreliable datagrams | `datagram_queue` sets a destination address handle and QKey. Receive buffers reserve 40 bytes for the GRH. `Operation::datagram` reports the source QPN and payload length. Send completion permits local reuse. Application protocols supply any required delivery confirmation. |
| Standard connection management | The optional `cm` feature uses the system `librdmacm.so.1` for address resolution, listen, accept and connect. `CmConnection` offers a simple, one-outstanding-message SEND/RECV API with owned buffers. Install the rdma-core CM headers when building that feature. The owned RC hot path reuses registered memory. |

The [two-process advanced example](tests/package/advanced_peer.rs) checks scatter/gather, immediates, atomic return values, independent QPs sharing a CQ or SRQ, and datagram payloads. The [CM example](tests/package/cm_peer.rs) establishes a connection over an RDMA-capable interface and checks a ping and pong. The [comparison harness](benches/comparison/README.md) measures QD1 and QD32 on the chosen host.

## Platform and project boundary

OrbitRDMA uses Rust edition 2024 with an MSRV of 1.85. The crate compiles its production code entirely from Rust, including port-query dispatch and mlx5 protection-domain lookup. At build time bindgen reads the installed headers with libclang. At runtime the loader calls the host's libibverbs and NIC provider. The owned-buffer, connection and completion APIs can be used from a crate with `#![forbid(unsafe_code)]`. Native calls, DMA-backed memory access and caller-owned raw handles have explicit `unsafe` boundaries. CPU tests compile C fixtures against the public headers to check those boundaries. Each supported RoCE or InfiniBand configuration will have its own two-host qualification record. [Linux RDMA userspace stack](https://github.com/linux-rdma/rdma-core)

The optional `provider` feature provides Linux port and GID inventory and bounded per-PD key history. It lets engines manage shared completion queues,
protection domains, RC transitions, memory registrations and asynchronous device
events. Native resources may be present after a failed create. The caller must
retain those slots, their parents and the backing until successful retirement.
The optional `mlx5` feature currently gates the standard rdma-core dma-buf
registration entry point as well as mlx5-specific DevX keys. Each RNIC driver
must support importing the exported GPU allocation for the dma-buf route to work.
The application retains the allocation and export FD through deregistration.

Engines with an existing CQ poller can call `provider::prepare_shared_window` for a grant's exact host range. Feed that grant's signaled BIND and LOCAL_INV completions to `provider::SharedWindowControl`. It uses the same QP, work ID, opcode and status checks as `QueuePair::expose_window`. After successful LOCAL_INV, retire the MW and private MR before releasing the external DMA loan.

OrbiTE uses these provider operations and `verbs::{post,poll}`. Its progress workers
own scheduling, peer grants, cross-transport memory loans and CUDA exports.
[OrbiTE integration](https://github.com/OrbitInfra/OrbiTE/pull/227)

The host API selects a device, an active port and a GID index. Standard verbs
handle RC, CQ, MR and MW operations through the installed driver. For RoCE, select
a matching Ethernet GID type at both ends. For InfiniBand, the subnet manager
configures the fabric and assigns LIDs. The peers exchange QP and path parameters
over their control channel, then use the same Rust transfer API.
[rdma-core providers](https://github.com/linux-rdma/rdma-core/tree/master/providers) · [InfiniBand subnet management](https://networking-docs.nvidia.com/nvidiainfinibandsecurityoverviewandguidelines/security-in-infiniband)

OrbiTE's direct CUDA route exports a GPU allocation as dma-buf or selects a
supported peer-memory driver. It owns CUDA ordering, GPU memory lifetime and
the choice of direct or pinned-host staging. Its optional reusable GPU-source
keys use mlx5 DevX. The safe multi-window API above accepts registered host memory.
[OrbiTE GPU registration routes](https://github.com/OrbitInfra/OrbiTE/blob/main/docs/rdma-cuda.md#gpu-direct-registration-routes) · [NVIDIA GPUDirect RDMA](https://docs.nvidia.com/cuda/gpudirect-rdma/)

## Check two peers

Build `cargo build --locked --release --example host_peer` on Linux. On a pair of hosts with an RC provider supporting type-2B memory windows, run the listener first. Each host selects its own device, port and GID index.

```sh
# Listener
target/release/examples/host_peer listen 0.0.0.0:7400 mlx5_0 1 0
# Requester (substitute the listener's reachable address)
target/release/examples/host_peer connect 192.0.2.1:7400 mlx5_0 1 0
```

Both processes use one RC QP apiece. The requester writes A and B, waits for the owner to revoke and reuse A, then reads B, overwrites it and reads it back. The same QP carries a final SEND/RECV with B still bound. The owner closes the QP, recovers the backing and verifies the final bytes and guards. TCP carries connection parameters, window descriptors and acknowledgements. [Two-peer consumer](tests/package/host_peer.rs)

For a single-host Soft-RoCE run, create an RXE device on the Linux host and select its active GID index. Give the Docker container the host network namespace, its `/dev/infiniband/uverbsN` device and sufficient locked memory. Inside the container, run the following for each installed toolchain. The runner starts two separate processes per repetition. Physical two-host RNIC and GPU profiles require their own installed runs. [Linux RXE setup](https://github.com/linux-rdma/rdma-core#usage) · [Verification plan](docs/development-plan.md#release-decision)

```sh
for tc in stable 1.85.0; do
    cargo +"$tc" build --locked --example host_peer
    bash tests/peer-loopback.sh target/debug/examples/host_peer orbit_rxe 1 1 50
    cargo +"$tc" build --locked --features tokio --example async_peer
    bash tests/peer-loopback.sh target/debug/examples/async_peer orbit_rxe 1 1 50
done
```

## License and provenance

OrbitRDMA is licensed under Apache-2.0. Its loader, port-query ABI handling, RC transitions, host MR/window operations, key-history rules, WR prefix accounting and CQ retirement were adapted from OrbiTE commit `e4a12a20b4f47201c48f9e562aa5d95ea1db83e4`. The 25 required host exports and optional `ibv_query_port_speed` are resolved at runtime. [License](LICENSE) · [Extraction map](docs/development-plan.md#extraction-map) · [Original verbs code](https://github.com/OrbitInfra/OrbiTE/tree/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/sys)
