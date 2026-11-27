use super::provider::{Provider, isolated};
use super::{bytes, cleanup, conn, device, external};
use orbit_rdma::*;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn case(name: &str, run: impl FnOnce(&Provider)) {
    isolated(&format!("transfers::{name}"), run);
}
fn pair(a: &Device, b: &Device, interval: u32) -> (QueuePair, QueuePair) {
    let mut left = a
        .queue_pair(QueueConfig {
            signal_interval: interval,
            ..QueueConfig::new(0x1234)
        })
        .unwrap();
    let mut right = b.queue_pair(QueueConfig::new(0x7654)).unwrap();
    let (l, r) = (left.parameters(), right.parameters());
    left.connect(&r).unwrap();
    right.connect(&l).unwrap();
    (left, right)
}
fn batch(m: &RegisteredMemory, op: Transfer, offsets: &[usize]) -> Vec<Request> {
    offsets
        .iter()
        .map(|&start| Request::new(m.range(start..start + 2).unwrap(), op))
        .collect()
}
fn ready(op: Operation) -> Completion {
    match op.try_reclaim() {
        Reclaim::Ready(c) => c,
        _ => panic!("range is still held"),
    }
}
fn success(bytes: u32) -> Outcome {
    Outcome::Success {
        bytes,
        invalidated_rkey: None,
    }
}

#[test]
fn known_zero_and_partial_prefix() {
    case("known_zero_and_partial_prefix", |p| {
        p.option("manual");
        let d = device();
        let mut mr = d.register(bytes(16), Permissions::Local).unwrap();
        let mut q = conn(&d);
        let n = q.parameters().qpn;
        p.post_result(0, 0);
        let reject = q
            .submit(batch(&mr, Transfer::Send, &[0, 2, 4, 6]))
            .unwrap_err();
        assert_eq!(reject.requests.len(), 4);
        assert!(mr.write(0..8).is_ok());
        assert_eq!(d.usage().unwrap().in_flight_bytes, 0);
        p.post_result(2, 0);
        let result = q.submit(reject.requests).unwrap();
        assert_eq!(result.status, PostStatus::Partial { code: libc::ENOMEM });
        assert_eq!(
            result
                .unposted
                .iter()
                .map(|r| r.buffer.bounds())
                .collect::<Vec<_>>(),
            [4..6, 6..8]
        );
        assert_eq!(
            (
                d.usage().unwrap().send_requests,
                d.usage().unwrap().in_flight_bytes
            ),
            (2, 4)
        );
        assert!(mr.write(4..8).is_ok());
        assert!(matches!(mr.write(1..2), Err(Error::Busy)));
        assert!(matches!(
            q.submit(batch(&mr, Transfer::Send, &[8]))
                .unwrap_err()
                .error,
            Error::Closed
        ));
        let mut ops = result.operations.into_iter();
        let first = ops.next().unwrap();
        let second = ops.next().unwrap();
        assert!(second.outcome().is_none());
        p.emit(n, second.id(), n, (0, 0, 0), None);
        assert_eq!(q.progress().unwrap(), 1);
        assert!(mr.write(2..4).is_ok());
        assert!(matches!(mr.write(0..2), Err(Error::Busy)));
        assert_eq!(ready(second).outcome, success(2));
        p.emit(n, first.id(), n, (0, 0, 0), None);
        q.progress().unwrap();
        assert_eq!(ready(first).outcome, success(2));
        drop(result.unposted);
        assert_eq!(p.count("sq_wr"), 2);
        q.close().unwrap();
        // A returned suffix can alias the accepted source. Only that source's
        // loan still blocks writes to the overlapping bytes.
        let mut q = conn(&d);
        p.post_result(1, 0);
        let aliases = q.submit(batch(&mr, Transfer::Send, &[0, 0, 2])).unwrap();
        assert_eq!(d.usage().unwrap().send_requests, 1);
        assert!(matches!(
            aliases.unposted[0].buffer.write(),
            Err(Error::Busy)
        ));
        assert!(aliases.unposted[1].buffer.write().is_ok());
        q.close().unwrap();
        assert!(aliases.unposted[0].buffer.write().is_ok());
        mr.deregister().unwrap();
    });
}

#[test]
fn ambiguous_write_local_drain_preserves_unknown_remote_effect() {
    case(
        "ambiguous_write_local_drain_preserves_unknown_remote_effect",
        |p| {
            for style in [1, 2, 3] {
                p.post_result(-1, 0);
                let d = device();
                let target = device();
                let (mut q, other) = pair(&d, &target, 1);
                let source = d
                    .register(
                        HostBuffer::from_vec(b"..abcdef..".to_vec()).unwrap(),
                        Permissions::Local,
                    )
                    .unwrap();
                let destination = target
                    .register(
                        HostBuffer::from_vec(vec![0x33; 10]).unwrap(),
                        Permissions::RemoteWrite,
                    )
                    .unwrap();
                let mut w = other
                    .expose(&destination, 2..8, RemoteAccess::Write)
                    .unwrap();
                let desc = w.descriptor().unwrap().unwrap();
                let requests = (0..3)
                    .map(|i| {
                        Request::new(
                            source.range(2 + 2 * i..4 + 2 * i).unwrap(),
                            Transfer::Write {
                                remote: desc,
                                offset: 2 * i as u64,
                            },
                        )
                    })
                    .collect();
                let before = (p.count("post_send"), p.count("sq_wr"));
                p.post_result(1, style);
                let posted = q.submit(requests).unwrap();
                assert!(matches!(posted.status, PostStatus::Uncertain { .. }));
                assert_eq!(posted.operations.len(), 3);
                assert!(posted.unposted.is_empty());
                assert!(matches!(source.write(2..8), Err(Error::Busy)));
                assert_eq!(d.usage().unwrap().in_flight_bytes, 6);
                q.close().unwrap();
                for op in posted.operations {
                    let c = ready(op);
                    assert!(matches!(
                        c.outcome,
                        Outcome::Failed {
                            reason: TransferFailure::Post { .. },
                            remote_effect_unknown: true
                        }
                    ));
                    assert!(c.buffer.write().is_ok());
                }
                assert_eq!(
                    (p.count("post_send") - before.0, p.count("sq_wr") - before.1),
                    (1, 1)
                );
                assert!(w.revoke().unwrap());
                assert_eq!(
                    &*destination.read(0..10).unwrap(),
                    &[0x33, 0x33, b'a', b'b', 0x33, 0x33, 0x33, 0x33, 0x33, 0x33]
                );
            }
        },
    );
}

#[test]
fn selective_receipts_do_not_cross_batches_or_reads() {
    case("selective_receipts_do_not_cross_batches_or_reads", |p| {
        p.option("manual");
        let d = device();
        let other = device();
        let (mut q, peer) = pair(&d, &other, 2);
        let n = q.parameters().qpn;
        let mr = d.register(bytes(24), Permissions::Local).unwrap();
        let a = q
            .submit(batch(&mr, Transfer::Send, &[0, 2, 4, 6]))
            .unwrap()
            .operations;
        let b = q
            .submit(batch(&mr, Transfer::Send, &[8, 10]))
            .unwrap()
            .operations;
        assert_eq!(p.count("signaled"), 3);
        p.emit(n, b[1].id(), n, (0, 999, 0), None);
        q.progress().unwrap();
        assert!(b.iter().all(Operation::is_reusable));
        assert!(a.iter().all(|op| !op.is_reusable()));
        // Even a duplicate valid tail cannot pay another group's debt.
        p.emit(n, b[1].id(), n, (0, 0, 0), None);
        assert!(q.progress().is_err());
        p.emit(n, a[3].id(), n, (0, 0, 0), None);
        q.progress().unwrap();
        assert!(a[2].is_reusable() && a[3].is_reusable());
        assert!(matches!(mr.write(0..2), Err(Error::Busy)));
        // Error/flush receipts define only identity, status and vendor_err.
        p.emit(n, a[1].id(), n, (u32::MAX, u32::MAX, 5), Some(99));
        q.progress().unwrap();
        assert!(matches!(
            a[1].outcome(),
            Some(Outcome::Failed {
                reason: TransferFailure::Completion {
                    status: 5,
                    vendor_error: 0x21
                },
                ..
            })
        ));
        assert!(!a[0].is_reusable());
        q.close().unwrap();
        assert!(matches!(
            a[0].outcome(),
            Some(Outcome::Failed {
                reason: TransferFailure::QueueClosed,
                ..
            })
        ));
        drop(peer);
        let (mut q, peer) = pair(&d, &other, 4);
        let n = q.parameters().qpn;
        let remote = RemoteDescriptor {
            qpn: peer.parameters().qpn,
            address: 16,
            length: 16,
            rkey: 1,
            access: RemoteAccess::Read,
        };
        let mut requests = batch(&mr, Transfer::Send, &[0]);
        requests.extend(batch(&mr, Transfer::Read { remote, offset: 0 }, &[2]));
        requests.extend(batch(&mr, Transfer::Send, &[4, 6]));
        let ops = q.submit(requests).unwrap().operations;
        assert_eq!(p.count("fence"), 1);
        p.emit(n, ops[3].id(), n, (0, 0, 0), None);
        q.progress().unwrap();
        assert!(!ops[0].is_reusable() && !ops[1].is_reusable());
        assert!(ops[2].is_reusable() && ops[3].is_reusable());
        p.emit(n, ops[1].id(), n, (2, 1, 0), None);
        assert!(q.progress().is_err());
        assert!(!ops[1].is_reusable());
        q.close().unwrap();
    });
}

#[test]
fn receive_identity_length_and_optional_invalidation() {
    case("receive_identity_length_and_optional_invalidation", |p| {
        let d = device();
        let mut q = conn(&d);
        let n = q.parameters().qpn;
        let m = d.register(bytes(12), Permissions::Local).unwrap();
        let ops = q
            .submit(batch(&m, Transfer::Receive, &[2, 6, 8, 10]))
            .unwrap()
            .operations;
        // The second posted slot receives its own receipt first.
        p.emit(n, ops[1].id(), n, (128, 1, 0), Some(0xabcd));
        q.progress().unwrap();
        assert_eq!(
            ops[1].outcome(),
            Some(Outcome::Success {
                bytes: 1,
                invalidated_rkey: Some(0xabcd)
            })
        );
        assert!(matches!(m.read(2..4), Err(Error::Busy)));
        p.emit(n, ops[0].id(), n, (128, 0, 0), None);
        q.progress().unwrap();
        assert_eq!(ops[0].outcome(), Some(success(0)));
        p.emit(n, ops[2].id(), n + 1, (128, 2, 0), None);
        assert!(q.progress().is_err());
        p.emit(n, ops[2].id(), n, (128, 3, 0), None);
        assert!(q.progress().is_err());
        p.emit(n, ops[2].id(), n, (0, 2, 0), None);
        assert!(q.progress().is_err());
        assert!(matches!(m.write(8..10), Err(Error::Busy)));
        p.emit(n, ops[2].id(), n, (u32::MAX, u32::MAX, 7), Some(11));
        q.progress().unwrap();
        assert!(matches!(
            ops[2].outcome(),
            Some(Outcome::Failed {
                reason: TransferFailure::Completion { status: 7, .. },
                remote_effect_unknown: false
            })
        ));
        assert!(!ops[3].is_reusable());
        q.close().unwrap();
    });
}

#[test]
fn delayed_receive_cancel_drop_and_sticky_destroy() {
    case("delayed_receive_cancel_drop_and_sticky_destroy", |p| {
        for fault in ["", "free_qp", "free_cq"] {
            p.fault("", "");
            p.post_result(-1, 0);
            p.option("late_dma");
            let d = device();
            let mut q = conn(&d);
            let (buffer, drops) = external(12);
            let mut mr = d.register(buffer, Permissions::Local).unwrap();
            p.post_result(2, 0);
            let result = q.submit(batch(&mr, Transfer::Receive, &[2, 5, 8])).unwrap();
            assert_eq!(result.unposted.len(), 1);
            assert!(mr.write(8..10).is_ok());
            drop(result.unposted);
            let mut ops = result.operations;
            let op = ops.pop().unwrap();
            drop(ops);
            op.cancel();
            assert!(
                !op.wait_until(&mut q, Instant::now() + Duration::from_millis(2))
                    .unwrap()
            );
            let Reclaim::Pending(op) = op.try_reclaim() else {
                panic!("early release")
            };
            assert!(matches!(mr.read(2..7), Err(Error::Busy)));
            p.fault(fault, "");
            let start = p.count("free_qp");
            if fault.is_empty() {
                q.close().unwrap();
            } else {
                assert!(matches!(q.close(), Err(Error::Unquiesced { .. })));
            }
            if fault == "free_qp" {
                let Reclaim::Unquiesced { error, .. } = op.try_reclaim() else {
                    panic!("missing retained receipt")
                };
                cleanup(error, "ibv_destroy_qp");
                p.fault("", "");
                assert!(q.close().is_err());
                assert_eq!(d.usage().unwrap().in_flight_bytes, 4);
                assert!(matches!(mr.deregister(), Err(Error::Busy)));
                drop(mr);
                drop(q);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
            } else {
                assert!(matches!(
                    ready(op).outcome,
                    Outcome::Failed {
                        reason: TransferFailure::Cancelled,
                        ..
                    }
                ));
                assert_eq!(
                    &*mr.read(0..12).unwrap(),
                    &[17, 17, 17, 0xb6, 17, 17, 0xb6, 17, 17, 17, 17, 17]
                );
                drop(mr.deregister().unwrap());
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert_eq!(d.usage().unwrap().in_flight_bytes, 0);
            }
            assert_eq!(p.count("free_qp"), start + 1);
        }
    });
}

#[test]
fn event_arming_race_spurious_wakeup_and_poll_errors() {
    case("event_arming_race_spurious_wakeup_and_poll_errors", |p| {
        let d = device();
        let m = d.register(bytes(4), Permissions::Local).unwrap();
        for mode in [
            "arm_race",
            "on_event",
            "spurious",
            "timeout",
            "poll",
            "arm",
            "get_event",
        ] {
            let repetitions = if ["arm_race", "on_event", "spurious"].contains(&mode) {
                50
            } else {
                1
            };
            for _ in 0..repetitions {
                p.fault("", "");
                let mut q = conn(&d);
                let op = q
                    .submit(batch(&m, Transfer::Receive, &[1]))
                    .unwrap()
                    .operations
                    .pop()
                    .unwrap();
                p.option(if mode == "get_event" {
                    "on_event"
                } else {
                    mode
                });
                let before = p.count("ack");
                if ["poll", "arm", "get_event"].contains(&mode) {
                    p.fault(mode, "");
                    assert!(
                        op.wait_until(&mut q, Instant::now() + Duration::from_secs(5))
                            .is_err()
                    );
                    assert!(!op.is_reusable());
                    assert!(matches!(m.read(1..3), Err(Error::Busy)));
                    p.fault("", "");
                } else {
                    let timeout = if mode == "timeout" {
                        Duration::from_millis(5)
                    } else {
                        Duration::from_secs(5)
                    };
                    assert_eq!(
                        op.wait_until(&mut q, Instant::now() + timeout).unwrap(),
                        mode != "timeout"
                    );
                    let expected_acks = match mode {
                        "spurious" => 2,
                        "on_event" => 1,
                        _ => 0,
                    };
                    assert_eq!(p.count("ack") - before, expected_acks);
                    if mode == "timeout" {
                        assert!(matches!(m.read(1..3), Err(Error::Busy)));
                    }
                }
                q.close().unwrap();
                drop(ready(op));
                p.option("");
            }
        }
    });
}

#[test]
fn forgotten_handle_and_unknown_receive_own_the_buffer_until_drain() {
    case(
        "forgotten_handle_and_unknown_receive_own_the_buffer_until_drain",
        |p| {
            for style in [1, 2, 3] {
                p.option("late_dma");
                p.post_result(1, style);
                let d = device();
                let (buffer, drops) = external(10);
                let mut mr = d.register(buffer, Permissions::Local).unwrap();
                let stale = mr.range(2..4).unwrap();
                let mut q = conn(&d);
                let mut posted = q.submit(batch(&mr, Transfer::Receive, &[2, 6])).unwrap();
                assert!(matches!(posted.status, PostStatus::Uncertain { .. }));
                let forgotten = posted.operations.pop().unwrap();
                std::mem::forget(forgotten);
                drop(posted);
                assert_eq!(d.usage().unwrap().receive_requests, 2);
                assert!(matches!(mr.deregister(), Err(Error::Busy)));
                drop(q);
                let original = mr.deregister().unwrap();
                assert_eq!(
                    original.as_ref(),
                    &[17, 17, 17, 0xb6, 17, 17, 17, 17, 17, 17]
                );
                assert!(matches!(stale.write(), Err(Error::Closed)));
                assert_eq!(d.usage().unwrap(), Usage::default());
                drop(original);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            }
        },
    );
}

#[test]
fn unsignaled_partial_prefix_and_independent_queue_limits() {
    case(
        "unsignaled_partial_prefix_and_independent_queue_limits",
        |p| {
            let d = device();
            let peer = device();
            let mut q = d
                .queue_pair(QueueConfig {
                    send_depth: 8,
                    receive_depth: 1,
                    signal_interval: 4,
                    ..QueueConfig::new(100)
                })
                .unwrap();
            let mut other = peer.queue_pair(QueueConfig::new(123)).unwrap();
            let (a, b) = (q.parameters(), other.parameters());
            q.connect(&b).unwrap();
            other.connect(&a).unwrap();
            let m = d.register(bytes(32), Permissions::Local).unwrap();
            p.option("manual");
            p.post_result(-1, 0);
            let rec = q
                .submit(batch(&m, Transfer::Receive, &[20]))
                .unwrap()
                .operations
                .pop()
                .unwrap();
            assert!(matches!(
                q.submit(batch(&m, Transfer::Receive, &[22]))
                    .unwrap_err()
                    .error,
                Error::Exhausted(_)
            ));
            let old = q
                .submit(batch(&m, Transfer::Send, &[12]))
                .unwrap()
                .operations
                .pop()
                .unwrap();
            p.post_result(2, 0);
            let part = q.submit(batch(&m, Transfer::Send, &[0, 2, 4, 6])).unwrap();
            assert_eq!(part.operations.len(), 2);
            assert_eq!(p.count("signaled"), 1);
            assert_eq!(d.usage().unwrap().send_requests, 3);
            assert!(m.write(4..8).is_ok());
            p.emit(a.qpn, old.id(), a.qpn, (0, 0, 0), None);
            q.progress().unwrap();
            assert!(part.operations.iter().all(|o| !o.is_reusable()));
            // A success CQE for an unsignaled WR is malformed.
            p.emit(a.qpn, part.operations[0].id(), a.qpn, (0, 2, 0), None);
            assert!(q.progress().is_err());
            assert!(matches!(m.write(0..4), Err(Error::Busy)));
            assert!(!rec.is_reusable());
            q.close().unwrap();
            assert_eq!(d.usage().unwrap().in_flight_bytes, 0);
        },
    );
}

#[test]
fn mismatched_remote_authority_and_read_depth_fail_before_post() {
    case(
        "mismatched_remote_authority_and_read_depth_fail_before_post",
        |p| {
            let d = device();
            let m = d.register(bytes(8), Permissions::Local).unwrap();
            let mut q = d.queue_pair(QueueConfig::new(16)).unwrap();
            let mut peer = q.parameters();
            peer.qpn += 1;
            peer.responder_depth = 0;
            q.connect(&peer).unwrap();
            let good = RemoteDescriptor {
                qpn: peer.qpn,
                address: 64,
                length: 4,
                rkey: 3,
                access: RemoteAccess::Write,
            };
            for (desc, offset) in [
                (
                    RemoteDescriptor {
                        qpn: peer.qpn + 1,
                        ..good
                    },
                    0,
                ),
                (RemoteDescriptor { rkey: 0, ..good }, 0),
                (
                    RemoteDescriptor {
                        access: RemoteAccess::Read,
                        ..good
                    },
                    0,
                ),
                (
                    RemoteDescriptor {
                        address: u64::MAX,
                        ..good
                    },
                    0,
                ),
                (good, 3),
                (good, u64::MAX),
            ] {
                let mut work = batch(&m, Transfer::Send, &[0]);
                work.extend(batch(
                    &m,
                    Transfer::Write {
                        remote: desc,
                        offset,
                    },
                    &[2],
                ));
                assert_eq!(q.submit(work).unwrap_err().requests.len(), 2);
                assert_eq!(d.usage().unwrap().in_flight_bytes, 0);
                assert!(m.write(0..4).is_ok());
            }
            let read = RemoteDescriptor {
                access: RemoteAccess::Read,
                ..good
            };
            assert!(matches!(
                q.submit(batch(
                    &m,
                    Transfer::Read {
                        remote: read,
                        offset: 0
                    },
                    &[0]
                ))
                .unwrap_err()
                .error,
                Error::Unsupported(_)
            ));
            assert_eq!(p.count("post_send"), 0);
        },
    );
}

#[test]
fn admission_limits_stale_incarnations_and_cross_thread_loans() {
    case(
        "admission_limits_stale_incarnations_and_cross_thread_loans",
        |p| {
            let d = Device::open(
                "cpu-first",
                1,
                0,
                Limits {
                    in_flight_bytes: 4,
                    ..Limits::default()
                },
            )
            .unwrap();
            let m = d.register(bytes(16), Permissions::Local).unwrap();
            let other = device();
            let foreign = other.register(bytes(8), Permissions::Local).unwrap();
            p.option("reuse_qpn");
            let mut q = conn(&d);
            let n = q.parameters().qpn;
            p.option("manual");
            assert!(matches!(
                q.submit(batch(&foreign, Transfer::Receive, &[0]))
                    .unwrap_err()
                    .error,
                Error::Invalid(_)
            ));
            let mut mixed = batch(&m, Transfer::Receive, &[0]);
            mixed.extend(batch(&m, Transfer::Send, &[2]));
            assert!(q.submit(mixed).is_err());
            assert!(q.submit(batch(&m, Transfer::Receive, &[0, 1])).is_err());
            assert!(q.submit(batch(&m, Transfer::Send, &[0, 2, 4])).is_err());
            assert_eq!(p.count("post_recv") + p.count("post_send"), 0);
            assert_eq!(d.usage().unwrap().in_flight_bytes, 0);
            let mut prev = 0;
            for _ in 0..50 {
                let op = q
                    .submit(batch(&m, Transfer::Receive, &[0]))
                    .unwrap()
                    .operations
                    .pop()
                    .unwrap();
                let id = op.id();
                std::thread::scope(|s| {
                    s.spawn(|| assert!(matches!(m.read(0..2), Err(Error::Busy))))
                        .join()
                        .unwrap();
                });
                drop(op);
                p.emit(n, id, n, (128, 2, 0), None);
                q.progress().unwrap();
                assert!(m.write(0..2).is_ok());
                prev = id;
            }
            q.close().unwrap();
            p.option("reuse_qpn");
            let mut q = conn(&d);
            assert_eq!(q.parameters().qpn, n);
            let op = q
                .submit(batch(&m, Transfer::Receive, &[0]))
                .unwrap()
                .operations
                .pop()
                .unwrap();
            p.emit(n, prev, n, (128, 2, 0), None);
            assert!(q.progress().is_err());
            assert!(!op.is_reusable());
            q.close().unwrap();
        },
    );
}

#[test]
fn single_post_preserves_range_and_charges_across_native_failures() {
    case(
        "single_post_preserves_range_and_charges_across_native_failures",
        |p| {
            p.option("manual");
            let d = device();
            let m = d.register(bytes(8), Permissions::Local).unwrap();
            let mut q = conn(&d);
            let request = Request::new(m.range(1..7).unwrap(), Transfer::Receive);
            p.post_result(0, 0);
            let rejected = q.submit_one(request).err().unwrap();
            assert_eq!(rejected.request.buffer.bounds(), 1..7);
            assert_eq!(d.usage().unwrap().range_loans, 0);
            p.post_result(0, 2);
            let posted = q.submit_one(rejected.request).unwrap();
            assert_eq!(posted.status, PostStatus::Uncertain { code: libc::ENOMEM });
            assert!(!posted.operation.is_reusable());
            drop(posted.operation);
            assert!(matches!(m.read(1..7), Err(Error::Busy)));
            assert_eq!(q.progress().unwrap(), 0);
            let used = d.usage().unwrap();
            assert_eq!(
                (
                    used.receive_requests,
                    used.range_loans,
                    used.in_flight_bytes
                ),
                (1, 1, 6)
            );
            q.close().unwrap();
            let used = d.usage().unwrap();
            assert_eq!(
                (
                    used.receive_requests,
                    used.range_loans,
                    used.in_flight_bytes
                ),
                (0, 0, 0)
            );
            assert!(m.write(1..7).is_ok());
        },
    );
}

#[test]
fn batch_dispatch_after_malformed_and_impossible_counts() {
    case(
        "batch_dispatch_after_malformed_and_impossible_counts",
        |p| {
            p.option("manual");
            let d = device();
            let m = d.register(bytes(16), Permissions::Local).unwrap();
            let mut q = conn(&d);
            let n = q.parameters().qpn;
            let ops = q
                .submit(batch(&m, Transfer::Receive, &[0, 2, 4]))
                .unwrap()
                .operations;
            p.emit(n, ops[0].id(), n + 1, (128, 2, 0), None);
            p.emit(n, ops[1].id(), n, (128, 2, 0), None);
            p.emit(n, ops[2].id(), n, (u32::MAX, u32::MAX, 7), Some(42));
            p.option("overreported_poll");
            assert!(q.progress().is_err());
            assert!(ops.iter().all(|op| !op.is_reusable()));
            p.option("manual");
            let polls = p.count("poll");
            assert!(q.progress().is_err());
            assert_eq!(p.count("poll"), polls + 1);
            assert!(!ops[0].is_reusable());
            assert_eq!(ops[1].outcome(), Some(success(2)));
            assert!(matches!(ops[2].outcome(), Some(Outcome::Failed { .. })));
            assert!(m.write(2..6).is_ok());
            assert!(matches!(m.read(0..2), Err(Error::Busy)));
            p.emit(n, ops[0].id(), n, (128, 2, 0), None);
            let polls = p.count("poll");
            assert_eq!(q.progress().unwrap(), 1);
            assert_eq!(p.count("poll"), polls + 1);
            assert_eq!(d.usage().unwrap().range_loans, 0);
        },
    );
}

#[test]
fn scatter_retains_all_ranges_and_one_work_credit() {
    case("scatter_retains_all_ranges_and_one_work_credit", |p| {
        p.option("manual");
        let d = device();
        let m = d.register(bytes(24), Permissions::Local).unwrap();
        let mut q = d.queue_pair_with_sge(QueueConfig::new(42), 3).unwrap();
        let mut peer = q.parameters();
        peer.qpn += 1;
        q.connect(&peer).unwrap();
        let ranges = [
            m.range(0..4).unwrap(),
            m.range(8..12).unwrap(),
            m.range(16..20).unwrap(),
        ];
        let bad = [ranges[0].clone(), ranges[0].clone()];
        assert!(matches!(
            q.submit_vectored(&bad, Transfer::Receive),
            Err(Error::Busy)
        ));
        assert_eq!(d.usage().unwrap().range_loans, 0);
        let pending = q.submit_vectored(&ranges, Transfer::Receive).unwrap();
        let usage = d.usage().unwrap();
        assert_eq!(
            (
                usage.receive_requests,
                usage.range_loans,
                usage.in_flight_bytes
            ),
            (1, 3, 12)
        );
        pending.receipt().cancel();
        drop(pending);
        assert!(ranges.iter().all(|r| matches!(r.read(), Err(Error::Busy))));
        assert!(m.write(4..8).is_ok());
        q.close().unwrap();
        assert!(ranges.iter().all(|r| r.write().is_ok()));
        assert_eq!(d.usage().unwrap().receive_requests, 0);
    });
}

#[test]
fn atomic_unknown_post_and_window_permissions() {
    case("atomic_unknown_post_and_window_permissions", |p| {
        let d = device();
        let m = d.register(bytes(32), Permissions::Local).unwrap();
        let mut q = d.atomic_queue_pair(QueueConfig::new(24), 1).unwrap();
        let mut peer = q.parameters();
        peer.qpn += 1;
        q.connect(&peer).unwrap();
        assert!(matches!(
            q.expose_atomic_window(&m, 0..8),
            Err(Error::PermissionDenied)
        ));
        let target = AtomicTarget {
            qpn: peer.qpn,
            address: 4096,
            length: 16,
            rkey: 7,
        };
        let slot = m.range(8..16).unwrap();
        p.option("manual");
        assert!(
            q.submit_atomic(&slot, target, 1, AtomicOperation::FetchAdd(1))
                .is_err()
        );
        assert_eq!(p.count("post_send"), 0);
        p.post_result(0, 2);
        let one = q
            .submit_atomic(
                &slot,
                target,
                8,
                AtomicOperation::CompareSwap {
                    compare: 1,
                    swap: 3,
                },
            )
            .unwrap();
        assert!(matches!(
            one.operation.outcome(),
            Some(Outcome::Failed {
                remote_effect_unknown: true,
                ..
            })
        ));
        assert!(matches!(slot.read(), Err(Error::Busy)));
        q.close().unwrap();
        assert!(slot.write().is_ok());
    });
}

#[test]
fn retained_receipt_never_observes_reused_state() {
    case("retained_receipt_never_observes_reused_state", |p| {
        p.option("manual");
        let d = device();
        let m = d.register(bytes(8), Permissions::Local).unwrap();
        let mut q = conn(&d);
        let n = q.parameters().qpn;
        let post = |q: &mut QueuePair| {
            q.submit_one(Request::new(m.range(0..2).unwrap(), Transfer::Send))
                .unwrap()
                .operation
        };
        let old = post(&mut q);
        p.emit(n, old.id(), n, (0, 0, 0), None);
        q.progress().unwrap();
        let next = post(&mut q);
        next.cancel();
        assert_eq!(old.outcome(), Some(success(2)));
        assert!(old.is_reusable());
        assert!(!next.is_reusable());
        drop(old);
        q.close().unwrap();
        assert!(next.is_reusable());
    });
}

#[test]
fn shared_cq_routes_interleaved_qps_and_rejects_stale_receipts() {
    case(
        "shared_cq_routes_interleaved_qps_and_rejects_stale_receipts",
        |p| {
            p.option("manual");
            let d = device();
            let m = d.register(bytes(8), Permissions::Local).unwrap();
            let config = QueueConfig {
                send_depth: 2,
                receive_depth: 2,
                ..QueueConfig::new(1)
            };
            let mut group = d.completion_group(8).unwrap();
            let a = group.add(config).unwrap();
            let b = group.add(config).unwrap();
            assert!(group.add(config).is_err());
            let (pa, pb) = (group.parameters(a).unwrap(), group.parameters(b).unwrap());
            group.connect(a, &pb).unwrap();
            group.connect(b, &pa).unwrap();
            let post = |g: &mut CompletionGroup, id, r| {
                g.submit_one(id, Request::new(m.range(r).unwrap(), Transfer::Receive))
                    .unwrap()
                    .operation
            };
            let x = post(&mut group, a, 0..2);
            let y = post(&mut group, b, 2..4);
            p.emit(pb.qpn, x.id(), pb.qpn, (128, 1, 0), None);
            p.emit(pb.qpn, y.id(), pb.qpn, (128, 2, 0), None);
            assert!(group.progress().is_err());
            assert!(!x.is_reusable());
            assert!(y.is_reusable());
            assert!(matches!(m.read(0..2), Err(Error::Busy)));
            group.close_queue(b).unwrap();
            assert!(group.parameters(b).is_err());
            p.emit(pa.qpn, x.id(), pa.qpn, (128, 1, 0), None);
            assert_eq!(group.progress().unwrap(), 1);
            assert_eq!(x.outcome(), Some(success(1)));
            group.close().unwrap();
            assert_eq!(p.count("free_cq"), 1);
        },
    );
}

#[test]
fn uncertain_srq_retains_receives_through_last_qp_and_failed_destroy() {
    case(
        "uncertain_srq_retains_receives_through_last_qp_and_failed_destroy",
        |p| {
            let d = device();
            let other = device();
            let m = d.register(bytes(8), Permissions::Local).unwrap();
            let mut srq = d.shared_receive_queue(3).unwrap();
            let config = QueueConfig::new(12);
            assert!(other.queue_pair_with_srq(config, 1, &srq).is_err());
            let mut q = d.queue_pair_with_srq(config, 1, &srq).unwrap();
            assert!(matches!(srq.close(), Err(Error::Busy)));
            p.option("uncertain_srq");
            let op = srq.post(&m.range(0..8).unwrap()).unwrap();
            assert!(matches!(op.status, PostStatus::Uncertain { .. }));
            q.close().unwrap();
            assert!(!op.operation.is_reusable());
            assert!(m.read(0..8).is_err());
            p.fault("destroy_srq", "");
            assert!(matches!(srq.close(), Err(Error::Unquiesced { .. })));
            assert!(matches!(
                op.operation.try_reclaim(),
                Reclaim::Unquiesced { .. }
            ));
            drop(srq);
            assert_eq!(p.count("destroy_srq"), 1);
            assert_eq!(d.shared_queue_usage().unwrap().receive_queues, 1);
        },
    );
}

#[test]
fn ud_address_outlives_delayed_send_and_failed_qp_destructor() {
    case(
        "ud_address_outlives_delayed_send_and_failed_qp_destructor",
        |p| {
            p.option("manual");
            let d = device();
            let m = d.register(bytes(64), Permissions::Local).unwrap();
            let mut q = d.datagram_queue(QueueConfig::new(1), 3).unwrap();
            let dest = q.address();
            q.set_destination(dest).unwrap();
            assert!(q.receive(&m.range(0..39).unwrap()).is_err());
            let op = q.send(&m.range(0..3).unwrap()).unwrap();
            assert!(matches!(q.set_destination(dest), Err(Error::Busy)));
            p.fault("free_qp", "");
            assert!(matches!(q.close(), Err(Error::Unquiesced { .. })));
            assert!(!op.operation.is_reusable());
            drop(q);
            assert_eq!(p.count("destroy_ah"), 0);
            assert_eq!(p.count("free_qp"), 1);
            assert!(matches!(m.write(0..1), Err(Error::Busy)));
        },
    );
}
