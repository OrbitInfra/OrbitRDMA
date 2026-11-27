# OrbitRDMA v0.1 development plan

The first release should let two Linux processes register host buffers, connect an RC queue pair (QP), transfer bytes in either direction, and recover their buffers after success or failure. OrbiTE and an independent two-peer program should both use the packaged public API. Stable Rust and Rust 1.85.0 are the target toolchains.

Milestones 1–3 and the two-process consumer are implemented in the standalone crate. CPU tests exercise discovery, RC setup, registered host memory, windows, payload submission and physical completion. OrbiTE uses the provider for device setup, registration, queueing, completion, optional mlx5 keys and native retirement in milestone 4. Milestone 5 covers installed qualification. The source baseline is OrbiTE commit `e4a12a20b4f47201c48f9e562aa5d95ea1db83e4`. Its historical hardware results apply to the artifacts named in those reports. [Pinned source][donor] · [OrbiTE qualification record][qualification] · [Project overview](../README.md)

The client API on the development branch adds a fixed connection exchange and an optional Tokio waiter for applications using the owned QP. Both blocking and async waits share the same CQ and arm state. Dropping a wait future leaves the queue's physical ownership intact. Cross-runtime and sync/async handoff tests verify who may reuse a buffer. The engine provider continues to accept externally owned CQs. [Client examples](../README.md#connect-peers-and-register-a-buffer) · [Reactor tests](../tests/unit/async_progress.rs)

## Extraction map

Source paths in this table are relative to OrbiTE's `src/transport/rdma/` unless they begin with `src/` or refer to the root build script. The proposed Rust modules in the first column are private implementation boundaries.

| OrbitRDMA owner | Source to extract or split | Work required |
| --- | --- | --- |
| `sys` | Root `build.rs`, `sys.rs`, `sys/{native,ffi,port,device,qp,cq,memory}.rs` and `sys/port.{c,h}` | Generate Rust layouts from the system headers. The production crate dispatches both port-query ABIs and optional mlx5 PD lookup in Rust. CPU fixtures compare this dispatch against the installed C headers. Resolve the host library on explicit device discovery or open. Keep its code available through the last cleanup callback. Remove engine configuration, topology and GPU types from the host table. The owned API keeps generated layouts private. The opt-in provider exposes the ABI types used by caller-owned resources and native fault fixtures. The provider documents the lifetimes and partial results of caller-owned native operations. [Loader][loader] · [Build bridge][build] |
| `device` and `qp` | `context.rs`, `discovery.rs`, `qp.rs`, `cq.rs` and the device-event parts of `events.rs` and `monitor.rs` | Extract device and port facts, protection domains (PDs), CQ/event channels, RC state transitions and device read-depth limits. Split `Descriptor` into connection parameters and remote memory authority. Give the caller the connection parameters to exchange over its own channel. [Mixed descriptor][descriptor] · [Discovery][discovery] |
| `memory` | `buffers.rs`, `sys/memory.rs`, `keys.rs`, `context/key_domain.rs` and the native-resource parts of `grant.rs` | Extract MR ownership, exact-range memory-window (MW) bind/invalidate, and bounded per-PD key history. Verify range, direction and connection lifetime. Keep the parent MR local-only when a window grants a subrange. An unsupported revocation mechanism returns a capability error. Preserve the backing through deregistration failures. [MR owners][buffers] · [Window operations][window] · [Key history][keys] |
| `operation/queue` and `sys/transfer` (SQ) | `qp/send_queue.rs`, `slice.rs`, `sys/qp.rs` and the posting/completion parts of `cq_lane/{submit,completion,poll}.rs` | Retain the exact work-request (WR) identities, accepted-prefix accounting and selective signaling. A signaled completion can settle earlier unsignaled WRs only within its proven ordered group. Extract physical queue and byte limits. Leave rail load samples and request priorities with OrbiTE. [Native post result][nativepost] · [Send accounting][sq] |
| `operation/queue` and `sys/transfer` (RQ) | `qp/receive_queue.rs`, `receive.rs`, `sys/cq.rs` and `sys/qp.rs` | Return the slot actually consumed, the received length and any hardware-reported invalidated key. OrbitRDMA supplies general-purpose SEND/RECV with caller-sized buffers. Today's completion SEND and receive slots carry an OrbiTE grant message. Keep binding checks, message decoding and `DONE/ACK/READY` in the OrbiTE adapter. [Receive dispatch][rq] · [Grant handshake][handshake] |
| `cq` progress and teardown | `cq.rs`, `cq_lane.rs`, `cq_lane/retirement.rs`, physical QP records in `qp_store.rs` and cleanup in `context.rs` | Poll or wait on the same physical completion owners. Fence admission before shutdown and preserve the dependencies of a failed teardown. A timed-out close may wait again. An ambiguous native destroy must execute only once. [Grant-coupled CQ record][record] · [Cleanup dependencies][cleanup] |

**Keep the shared memory loan in OrbiTE.** Its `RegisteredMemory::dma_lease` arbitrates access across RDMA, io_uring and GDS. The OrbiTE grant retains the loan while OrbitRDMA submits its registered range and decodes receipts. The standalone buffer API must likewise keep the address stable and exclude conflicting CPU or DMA access, including through aliases. The in-flight table holds that loan independently of the user's operation handle. [OrbiTE DMA loan][dmaloan] · [io_uring caller][uring] · [GDS caller][gds]

**Keep application policy with its caller.** OrbiTE retains `protocol.rs`, `rdma_transport/{handshake,setup,finish}.rs`, peer and epoch lookup in `endpoint.rs` and `endpoint_store.rs`, `rail_monitor.rs`, QoS, worker scheduling, CUDA readiness/staging and cross-transport failover. OrbitRDMA exposes port and device capabilities, CQ events and physical resource usage. An application decides whom to authorize, how to encode descriptors, and when to switch transports. [Peer protocol][protocol] · [Retry decision][failover]

During the switch, give each live QP and CQ exactly one owner. Migrate host-only connections first. If a host/CUDA session shares a QP or CQ, move that entire resource group together when its dependencies are ready. Two completion pollers could otherwise consume each other's receipts. Exercise both OrbiTE feature combinations, `rdma` and `rdma,cuda`, through their normal entry points.

### What a transport engine can delegate

Against [OrbiTE main at 65e4d5b](https://github.com/OrbitInfra/OrbiTE/tree/65e4d5bf98c4e753549129719c646feb824b7f39/src/transport/rdma), the production verbs entry points already call OrbitRDMA. Three groups of engine-side ownership and accounting remain as consumer migration work. Each row includes the corresponding public library mechanism. The engine still owns its control protocol, registered CUDA backing and decisions about retrying or changing rails.

| OrbiTE call sites | Reusable OrbitRDMA work | OrbiTE responsibility |
| --- | --- | --- |
| `sys/cq.rs`, `cq_lane/{poll,completion}.rs` | One native poll for up to 32 CQEs, completion decoding in provider order, QP/WR dispatch and bounded work namespaces. `CompletionGroup` owns a shared CQ for library-owned QPs. An externally owned engine CQ uses `verbs::poll_batch`. | Lane threads, wake policy, per-engine capacity, queue generation and scheduling. |
| `qp/{send_queue,receive_queue}.rs`, `slice.rs` | Known or possible post prefix, selective-signaling receipts, posted range and credit retirement, receive ownership and SRQs. The owned API retains its own registrations. | Mapping receipts to transfer aliases, grant messages, byte and bandwidth statistics, CUDA or staging loans, retry policy. An SRQ and OrbiTE's grant acknowledgement serve different roles. |
| `context/{gpu_keys,key_domain}.rs` | A bounded native PD key cursor burns index/tag candidates in the same `KeyHistory` before firmware creation. Existing provider operations allocate and inspect named mlx5 objects. | Authorization of a CUDA range, GPU readiness, allocation budgets, choice of direct/staged path and the decision to retry a busy index. |

The client also supports vectored RC requests, SEND/WRITE immediate values, atomic-only windows, UD, shared receive queues and optional standard RDMA-CM. Each mode has a two-process public example. Device acceptance depends on the provider, advertised capabilities and installed runs. RXE exercises the Linux software RoCE stack. Native IB, physical RNIC throughput and GPU Direct RDMA require separate measurements. [Advanced peer](../tests/package/advanced_peer.rs) · [CM peer](../tests/package/cm_peer.rs) · [Performance methodology](../benches/comparison/README.md)

## Delivery order

| Milestone | Deliverable | Exit check |
| --- | --- | --- |
| 1. Linux provider | Create the independent crate and private ABI boundary. Record the source revision, carry over the Apache-2.0 license and applicable notices, and port the native-loader shim. | On both toolchains the packaged library builds with system headers. A clean consumer starts without a verbs runtime. Explicit discovery with a missing library or required symbol returns a typed error. The installed consumer's ELF dependencies show no link-time `libibverbs` dependency. [Loader controls][loadtest] |
| 2. Connection and host memory | Implemented in `device`, `queue`, `memory` and `window`. Explicit port/GID selection, RC parameters, owned or externally loaned host buffers, checked ranges and revocable type-2B windows are public. | CPU controls drive the production PD, QP, MR and MW owners. They cover rollback, uncertain teardown, overflow, access direction, overlapping loans, stale keys, and a prior WRITE held pending until QP destruction. The installed physical profile is unqualified. [Standalone controls](../tests/owners.rs) · [Donor key history][keytest] · [Donor MR loans][mrtest] |
| 3. Transfers and physical completion | Implemented. Each physical QP owns outstanding READ, WRITE, SEND and RECV requests, its private completion queue, queue-depth reservations and a share of the protection domain's in-flight byte budget. Applications poll or wait for completion-channel events. | CPU controls cover known and unknown native prefixes, selective signaling, reversed receive receipts, malformed and delayed completions, cancellation, operation detachment and failed native destruction. The packaged consumer verifies all four operations and their guard bytes. Installed hardware remains unqualified. [Transfer tests](../tests/unit/transfers.rs) · [Packaged consumer](../tests/package/traffic.rs) |
| 4. Two independent callers | The packaged `host_peer` example uses TCP to exchange descriptors and verifies RDMA payloads and guards in two processes. OrbiTE uses the provider to construct and retire the queues, handle memory and device events, submit SQ/RQ work and poll completions. | Build the independent consumer from the archive. Run OrbiTE's real submission adapter with the packaged dependency and CPU provider on both toolchains. Use a registry version in OrbiTE's manifest. Installed RNIC checks follow in milestone 5. [Two-peer caller](../tests/package/host_peer.rs) · [Installed OrbiTE builder][builder] |
| 5. Installed qualification | Use the packaged consumer and the physical fault harness on a named pair of RNIC hosts. Write the runners and profiles during implementation so they are ready when hardware is available. | A reviewer checks the artifact hashes, reproduces the relevant gates and records the exact passing profile. Track RoCEv2 and InfiniBand separately. While hardware is unavailable, record source implementation and the outstanding installed runs. |

## Fault cases and pass conditions

The CPU shim must call the production owners. Capture the submitted WRs, completion queue entries (CQEs), native cleanup calls, range loans and remaining budget. Use guard bytes before and after each transfer range.

| Fault or race | Required observation | OrbiTE control to port or adapt |
| --- | --- | --- |
| Queue full, partial submission, missing or unrelated `bad_wr` | Proven zero submission returns the original buffer. A valid first failure releases only the unposted suffix's reservations. An unidentifiable prefix retains all possible work and fences the queue. An uncertain WRITE is issued once and its remote effect stays unknown after local drain. | [Partial and inconsistent submissions][partialtest] |
| Selective signaling, flush, duplicate, delayed or foreign CQE | Match WR ID and QP incarnation. Count only proven successful bytes. On error use only fields the provider defines. A flush or stale CQE cannot settle another group or return its credit. | [Signal-group controls][grouptest] · [Wrong QP or stale CQE][staletest] · [Verbs CQ contract][pollman] |
| Two receives posted, messages arrive in reverse application order | Return the actual consumed receive buffer exactly once. Validate received length before exposing initialized bytes. Report an invalidated key only when its success flag is present. OrbiTE separately checks the named grant. | [Reversed grants][recvtest] |
| Wait expires, cancel requested, handle dropped or forgotten, unregister races close | Hold a CQE while submitting overlapping work. The backing, registration and charges survive dropping the last user handle. New conflicting access fails or waits. Endpoint progress or close can still drive drain. Return each eligible resource once. | [Retirement under load][retiretest] · [Capacity after detachment][storetest] |
| Native creation fails partway, deregistration or destruction fails | Make MR, MW, QP, CQ, channel and PD failure cases distinct. Retain the affected dependency chain and its charge, allow independent cleanup, and block admission at the configured limit. Repeated close or Drop issues no second ambiguous destroy. | [CQ/channel dependency failures][cqtest] · [Installed cleanup cases][installedcleanup] |
| Peer dies during WRITE or reconnects at the same address | Record guarded bytes and the actual remote effect when observable. Preserve uncertainty when it cannot be proved. A descriptor from the old connection cannot authorize fresh work. An idle restart and an interrupted in-flight transfer get separate results. | [Posted-work interruption][installedfault] · [Restart with a survivor][restart] |

Run the known-prefix case with the first WR rejected, a middle WR rejected and an invalid failure pointer. The provider may accept earlier WRs before returning an error. Validate the pointer against the submitted list before attributing an index. [Native verbs specification][postman]

## Validation runs

**Milestone 3 CPU results (2026-11-26).** The Debian 12 Docker suite runs four loader tests, nine device/window lifecycle tests, ten transfer tests and four public API doctests. The extracted archive reruns all 19 resource and transfer tests. A separate public caller posts batches of all four opcodes between two local QPs on the CPU provider, compares both full buffers and their guards, checks receive lengths, and verifies that all live payload credits return to zero. The system libibverbs (44.0-2) enumerated zero devices. [Test entry point](../tests/cpu.sh) · [Public caller](../tests/package/traffic.rs)

| Milestone 3 Docker userspace | Rust stable 1.100.0 | Rust 1.85.0 |
| --- | --- | --- |
| aarch64 on a native Linux VM | Passed | Passed |
| x86_64 compilers and libraries under QEMU in the same VM | Passed | Passed |

The partial-post controls reject the first or a middle WR. A valid failure pointer releases only the proven suffix. An alias in that suffix still respects an overlapping accepted loan. A null or foreign pointer, or a success return paired with a failure pointer, retains every possible loan. One test lets a WRITE modify two peer bytes before returning an ambiguous post result. It checks the entire guarded destination, the single native submission, and the unknown remote outcome after successful local drain. A receive control defers its last write until QP destruction and verifies that failed destruction keeps the external backing and byte charges. Another control forgets a public handle and checks that, after successful drain, the caller can still deregister and recover its original allocation. [Prefix, completion and drain tests](../tests/unit/transfers.rs)

OrbitRDMA assigns process-unique payload WR IDs and matches each receipt against the physical QP and WR. Selective signaling covers consecutive SEND/WRITE requests in one native list. Every READ has its own receipt and the following SQ request carries a fence. Every RECV has its own slot and actual byte count. An error CQE retires only its identified WR. Earlier unsignaled work without a successful covering tail keeps its loan until independently retired or the QP drains. Notification waits poll before and after arming, acknowledge each consumed event and repoll the CQ. The arm race, event delivery and spurious-notification cases each run 50 times. Timeout and native CQ errors also have separate controls. [Queue owner](../src/operation/queue.rs) · [Verbs CQ contract][pollman]

**Installed milestone 3 check (written, hardware run pending).** With an active local port and type-2B MW support, the commands below exercise the packaged caller on the selected device. Supply its actual device name, port and GID index. The single-host RXE run exercises the software provider. The installed two-host physical profile still needs its own run.

```sh
RUSTUP_TOOLCHAIN=stable bash tests/package/check.sh mlx5_0 1 0
RUSTUP_TOOLCHAIN=1.85.0 bash tests/package/check.sh mlx5_0 1 0
```

**Milestone 2 CPU results (2026-11-26).** The four Linux Docker configurations below exercise the same public API through a shared-library CPU provider. Each runs four loader tests, nine resource lifecycle tests and three doctests. The package check reruns the nine lifecycle tests from the extracted archive, then builds a separate application that connects two QPs on separate PDs, registers a guarded buffer, exposes a four-byte read window, revokes it, and verifies every byte after deregistration. The Debian 12 system runtime (libibverbs 44.0-2) enumerated zero RDMA devices. [CPU suite](../tests/cpu.sh) · [Packaged application](../tests/package/connection.rs)

| Milestone 2 Linux Docker userspace | Rust stable 1.100.0 | Rust 1.85.0 |
| --- | --- | --- |
| aarch64, native in a Linux VM on Apple Silicon | Passed | Passed |
| x86_64, x86_64 compilers and libraries under QEMU in that VM | Passed | Passed |

The concurrency control repeats conflicting cross-thread borrows 50 times. The key-reuse control exhausts all 255 available tags for the fixture's recycled index. A separate control reuses both the QP number and the address and checks that the revoked key is rejected. For the dedicated-window API, each window consumes its own connected QP. Successful invalidation stops future access, and successful QP destruction fences previously accepted work. A failed destroy records its operation and return code, retains dependent backing and budget, and is never retried. The per-PD key history has a configured upper bound. Exhausting an index returns `Error::Exhausted` before posting another bind. [Window owner](../src/window.rs) · [Key allocation](../src/device.rs) · [Fault injection](../tests/unit/resources.c)

**Independent windows on a long-lived QP.** Each shared grant binds a type-2B MW to its own exact-range, bind-only MR. The PD charges the extra registration and its bytes. A shared CQ routes bind, invalidate and payload completions by QP, unique WR ID and opcode. The owner releases a range only after the matching invalidate CQE and successful MW and MR destruction, or after successful QP teardown and cleanup. For terminal close with grants, the QP enters RESET to discard pending local bind and invalidate WRs. The owner then detaches possibly bound MWs before QP destruction. Payloads, underlying MRs, loans and credits survive until their teardown receipts. This ordering supports [RXE providers that require MWs to detach before QP destruction](https://github.com/gregkh/linux/blob/v6.1.193/drivers/infiniband/sw/rxe/rxe_qp.c#L777-L793). This also covers a remote operation that resolved a key before invalidation. The CPU provider models its final byte becoming visible as the grant-specific MR retires. A failed destructor retains the backing and is attempted once. The tests also cover a proven rejection, an ambiguous post, corrupt completions, SQ saturation and B's READ/WRITE after A retires. [Window owner](../src/window/shared.rs) · [Fault and concurrency tests](../tests/unit/shared_windows.rs) · [InfiniBand Architecture, volume 1, C10-59/60 and C10-69/70](https://www.afs.enea.it/asantoro/V1r1_2_1.Release_12062007.pdf)

The packaged multi-window example passed 50 two-process runs per toolchain on 2026-11-27, using stable 1.100.0 and Rust 1.85.0 in a Debian 12 aarch64 Docker container. Both processes used the kernel RXE device on one host (Linux 6.1.0-56-arm64, rdma-core 44.0-2, Ethernet, port 1, GID index 1). After A's revocation and local reuse, the requester read B, wrote B and read it back, then used the same QP for SEND/RECV. The review regression additionally keeps B bound through queue close and checks the returned bytes. Physical RNIC, InfiniBand fabric and GPU-memory profiles remain pending for this API. [Two-process runner](../tests/peer-loopback.sh) · [Reproduction commands](../README.md#check-two-peers)

The milestone 2 setup and teardown gate also runs as the first part of the installed milestone 3 check above.

**Milestone 1 CPU results (2026-11-26).** All four Debian 12 runs used libibverbs 44.0-2 and passed formatting, Clippy, four loader/discovery tests, one public API doctest and the package check. The installed system runtime enumerated zero devices. The fixture supplied two device names and the failure cases.

| Linux environment | Rust stable 1.100.0 | Rust 1.85.0 |
| --- | --- | --- |
| aarch64 virtual machine on Apple Silicon | Passed | Passed |
| x86_64 Debian userspace under QEMU, with x86_64 compilers and libraries | Passed | Passed |

For x86_64 discovery, the VM loaded `ib_uverbs` and exposed its sysfs read-only to the emulated userspace. CI is configured with a native runner for each architecture. Physical RDMA qualification starts with the two-peer runs below.

The donor's known-prefix and unknown-post baseline is already runnable on Linux from the pinned OrbiTE checkout. With verbs development headers and libclang installed, run `cargo +stable test --locked --features rdma --lib partial_unknown_and_inconsistent_posts_keep_only_proven_or_possible_debt_until_quiescence` and check that the output shows one executed test.

The Linux CI matrix covers x86_64 and aarch64 with stable and 1.85.0. Install a C toolchain, libclang, verbs development headers, binutils and Python 3. The package check runs in a disposable Linux environment with root or passwordless `sudo chroot`. It builds a separate caller from the extracted archive, runs discovery against the host and fixture runtimes, inspects the ELF dependencies and runs the binary in a minimal filesystem without libibverbs. The unit suite also removes each required export in turn and exercises the optional port-speed symbol. [Loader tests](../tests/unit/native.rs) · [Package check](../tests/package/check.sh)

```sh
for tc in stable 1.85.0; do
    RUSTUP_TOOLCHAIN="$tc" bash tests/cpu.sh
done
```

To reproduce the CPU suite in Docker, run this from the checkout. The Docker host or its Linux VM needs the `ib_uverbs` kernel interface for the system-library enumeration check. No RDMA device needs to be passed into the container. Set `--platform linux/arm64` or `--platform linux/amd64` on a host with native or emulated support for that architecture. [Official Rust images](https://github.com/docker-library/official-images/blob/master/library/rust)

```sh
docker run --rm -v "$PWD:/src:ro" -w /src -e CARGO_TARGET_DIR=/tmp/orbit-target \
  rust:bookworm bash -ec '
    apt-get update -qq
    apt-get install -y --no-install-recommends build-essential libclang-dev libibverbs-dev binutils python3
    for tc in stable 1.85.0; do
      rustup toolchain install "$tc" --profile minimal --component clippy,rustfmt
      RUSTUP_TOOLCHAIN="$tc" bash tests/cpu.sh
    done'
```

As ownership and close-race cases land, repeat timing-sensitive tests at least 50 times on Linux, keeping higher donor counts. The following installed runs are planned for milestones 4 and 5. Build the external consumer against files extracted from the package in a clean directory. Record the archive SHA-256, lockfile, toolchain and binary digest. `ART` and `RUN` are fresh output directories outside Git. `PROFILES` contains the host and device settings for each run. Repeat with `--toolchain 1.85.0` and distinct output paths.

```sh
python3 tests/installed/build.py --toolchain stable --output "$ART/stable"
ORBITRDMA_REQUIRE_RXE=1 python3 tests/installed/run.py \
  --artifact "$ART/stable/manifest.json" --profile "$PROFILES/rxe.json" --output "$RUN/rxe"
python3 tests/installed/run.py --artifact "$ART/stable/manifest.json" \
  --profile "$PROFILES/rocev2.json" --output "$RUN/rocev2"
python3 tests/installed/run.py --artifact "$ART/stable/manifest.json" \
  --profile "$PROFILES/infiniband.json" --output "$RUN/infiniband"
```

| Evidence | What a passing run establishes |
| --- | --- |
| Linux software RDMA (RXE), two processes | Compare every byte for READ/WRITE and SEND/RECV, verify both guard regions and bounded close. The required RXE job fails if its device or a mandatory capability is missing. This result covers the software provider. |
| Two distinct hosts and physical RNICs | Use the same packaged consumer on both hosts. Exercise both directions, polling and event progress, single and batched requests, mid-transfer peer or QP interruption, revoke/reconnect and cleanup failure. Compare complete buffers and guards. Healthy shutdown has zero live resources. A retained case reports the exact owners and charges. Record host boot IDs, link layer, NIC/firmware, kernel, provider, queue depths, negotiated MTU and limits of the injected faults. |
| OrbiTE as a consumer | Run its original CPU-shim and packaged host controls against the candidate crate. Check reversed grant receipts, source epoch on restart, remote WRITE uncertainty and existing feature isolation. Report these results independently of the standalone tests. [OrbiTE installed host procedure][installed] |
| Performance | Run two OrbitRDMA peers, then two native verbs `perftest` peers on the same hosts and NICs. Match CPU placement, MTU, QP count, depth, signaling and message sizes. Include 64 B, 4 KiB and 1 MiB, disclose warm-up and repetitions, and report bandwidth, measured latency percentiles and CPU use. Investigate reproducible regressions before publishing a performance claim. [Native comparison tools][perf] |

## Release decision

Version 0.1.1 extends the original consumer integration to all native verbs. The opt-in provider operates shared queues and device events, and the mlx5 extension registers exported dma-bufs and verifies exact-range DevX keys. Its release checks use the packaged library from OrbiTE and an independent caller on stable and Rust 1.85.0. OrbiTE's native READ/WRITE, fenced SEND (including invalidation), RECV and CQ dispatch all use the crate. OrbiTE retains physical queue and registration ownership for shared host/CUDA paths. Installed hardware qualification is tracked separately.

Version 0.1.2 supports independent type-2B host windows on a long-lived RC QP. Owned queues and external CQ pollers share the grant completion checks. The owner waits for its LOCAL_INV completion, deallocates the window and deregisters its private MR before returning the bytes. The packaged two-process control exercises B after A retires; the CPU controls delay CQEs and inject failed native cleanup.

For a hardware-supported v0.1, require both callers to use the same packaged crate and at least one named physical profile to pass the byte, authority, fault and cleanup cases. A QP-level injection establishes behavior for that fault. Claims about physical port or device loss require an observed run of that failure.

Include a bounded soak with 1,000 register, transfer, revoke and close cycles. Run at the configured admission limit, then submit one more operation. Check backpressure before any extra post, zero outstanding owners after a healthy close, and exact retained usage after an uncertain close. A wrong byte, premature reuse, stale access, duplicate destroy, replay of an uncertain WRITE, unexplained resource growth or silent test skip blocks qualification. Archive raw logs outside Git. Publish the artifact digest, commands, results, reviewed profiles and their limits.

For 1.0, exercise an upgrade with OrbiTE and a maintained independent caller. Check source compatibility and the behavior each relies on for rejection, completion, buffer return, revocation and shutdown. A change to any of those guarantees is a compatibility change even when the code still compiles.

[donor]: https://github.com/OrbitInfra/OrbiTE/tree/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4
[qualification]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/docs/qualification.md
[loader]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/sys/native.rs#L100
[build]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/build.rs#L94
[descriptor]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/qp.rs#L173
[discovery]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/discovery.rs
[buffers]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/buffers.rs
[window]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/sys/memory.rs#L188
[keys]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/context/key_domain.rs
[nativepost]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/sys/qp.rs#L255
[sq]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/qp/send_queue.rs#L106
[rq]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/qp/receive_queue.rs#L190
[handshake]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/rdma_transport/handshake.rs#L121
[record]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/cq_lane.rs#L67
[cleanup]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/docs/native-cleanup.md#owners-and-receipts
[dmaloan]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/memory.rs#L723
[uring]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/io_uring/task.rs#L104
[gds]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/gds/batch.rs#L107
[protocol]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/transport/rdma/protocol.rs#L1
[failover]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/src/engine/failover.rs#L284
[loadtest]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/tests/unit/transport/rdma/sys/native.rs#L51
[keytest]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/tests/unit/transport/rdma/context/key_domain.rs#L4
[mrtest]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/tests/unit/transport/rdma/buffers.rs#L337
[sharedtest]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/tests/unit/transport/rdma/slice.rs#L541
[builder]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/tests/hardware/build_installed.py
[partialtest]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/tests/unit/transport/rdma/slice.rs#L159
[grouptest]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/tests/unit/transport/rdma/slice.rs#L631
[staletest]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/tests/unit/transport/rdma/cq_lane.rs#L471
[recvtest]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/tests/unit/transport/rdma/receive_queue.rs#L117
[retiretest]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/tests/unit/transport/rdma/cq_lane/retirement.rs#L61
[storetest]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/tests/unit/transport/rdma/qp_store.rs#L7
[cqtest]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/tests/unit/transport/rdma/cq.rs#L641
[installedcleanup]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/docs/installed-host.md#cq-completion-channel-and-pd-dependency-failures
[installedfault]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/docs/installed-host.md#in-flight-dma-interruption
[restart]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/docs/installed-host.md#peer-restart-with-a-surviving-engine
[installed]: https://github.com/OrbitInfra/OrbiTE/blob/e4a12a20b4f47201c48f9e562aa5d95ea1db83e4/docs/installed-host.md
[cargo]: https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#multiple-locations
[pollman]: https://github.com/linux-rdma/rdma-core/blob/master/libibverbs/man/ibv_poll_cq.3
[postman]: https://github.com/linux-rdma/rdma-core/blob/master/libibverbs/man/ibv_post_send.3
[perf]: https://github.com/linux-rdma/perftest
