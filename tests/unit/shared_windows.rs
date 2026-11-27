use super::provider::{Provider, isolated};
use super::{bytes, cleanup, conn, device, external};
use orbit_rdma::*;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn case(name: &str, run: impl FnOnce(&Provider)) {
    isolated(&format!("shared_windows::{name}"), run);
}

#[test]
fn independent_ranges_survive_delayed_bind_and_invalidation() {
    case(
        "independent_ranges_survive_delayed_bind_and_invalidation",
        |p| {
            let d = device();
            let mut q = conn(&d);
            let foreign = device();
            let mut other = conn(&foreign);
            let mut mr = d.register(bytes(24), Permissions::RemoteReadWrite).unwrap();
            p.option("delay");
            let mut a = q
                .expose_window(&mr, 4..8, Permissions::RemoteWrite)
                .unwrap();
            let bw = {
                let b = q
                    .expose_window(&mr, 12..16, Permissions::RemoteReadWrite)
                    .unwrap();
                assert!(a.descriptor(&mut q, RemoteAccess::Write).unwrap().is_none());
                assert!(b.descriptor(&mut q, RemoteAccess::Read).unwrap().is_none());
                assert!(matches!(
                    a.descriptor(&mut other, RemoteAccess::Write),
                    Err(Error::Invalid(_))
                ));
                assert!(matches!(
                    a.descriptor(&mut q, RemoteAccess::Read),
                    Err(Error::PermissionDenied)
                ));
                assert!(matches!(mr.read(4..8), Err(Error::Busy)));
                assert!(matches!(mr.write(12..16), Err(Error::Busy)));
                assert!(mr.write(8..12).is_ok());
                let live = d.usage().unwrap();
                assert_eq!(
                    (
                        live.queue_pairs,
                        live.windows,
                        live.regions,
                        live.registered_bytes
                    ),
                    (1, 2, 3, 32)
                );
                assert_eq!(
                    q.wait_for_progress(Instant::now() + Duration::from_millis(25))
                        .unwrap(),
                    0
                );
                assert_eq!(p.count("arm"), 1);
                p.option("");
                let ad = a.descriptor(&mut q, RemoteAccess::Write).unwrap().unwrap();
                let br = b.descriptor(&mut q, RemoteAccess::Read).unwrap().unwrap();
                let bw = b.descriptor(&mut q, RemoteAccess::Write).unwrap().unwrap();
                assert_eq!(ad.qpn, br.qpn);
                assert_ne!(ad.rkey, br.rkey);
                assert_eq!(br.rkey, bw.rkey);
                assert!(p.allows(ad, ad.address, 4, true));
                assert!(!p.allows(ad, ad.address, 4, false));
                assert!(!p.allows(ad, br.address, 1, true));
                assert!(p.allows(br, br.address, 4, false));
                assert!(p.allows(bw, bw.address, 4, true));

                p.option("delay");
                assert!(!a.revoke(&mut q).unwrap());
                assert!(!a.revoke(&mut q).unwrap());
                // An event can wake the waiter before it can observe a matching WC.
                assert_eq!(
                    q.wait_for_progress(Instant::now() + Duration::from_millis(25))
                        .unwrap(),
                    0
                );
                assert_eq!((p.count("get_event"), p.count("ack")), (1, 1));
                assert!(!p.allows(ad, ad.address, 1, true)); // A translated WRITE is still pending.
                assert!(matches!(
                    a.descriptor(&mut q, RemoteAccess::Write),
                    Err(Error::Closed)
                ));
                assert!(matches!(mr.write(4..8), Err(Error::Busy)));
                assert!(matches!(mr.deregister(), Err(Error::Busy)));
                assert_eq!(
                    (p.count("free_mw"), p.count("free_mr"), p.count("free_qp")),
                    (0, 0, 0)
                );
                assert_eq!(b.descriptor(&mut q, RemoteAccess::Write).unwrap(), Some(bw));

                p.option("");
                assert!(a.revoke(&mut q).unwrap());
                assert!(a.revoke(&mut q).unwrap());
                assert_eq!(mr.read(4..8).unwrap()[3], 0xa5); // The fixture drains that WRITE only at deregistration.
                mr.write(4..8).unwrap().fill(0x33);
                assert!(p.allows(bw, bw.address, 4, true) && p.allows(br, br.address, 4, false));
                assert!(matches!(mr.read(12..16), Err(Error::Busy)));
                assert_eq!(
                    (p.count("free_mw"), p.count("free_mr"), p.count("free_qp")),
                    (1, 1, 0)
                );
                let live = d.usage().unwrap();
                assert_eq!(
                    (live.windows, live.regions, live.registered_bytes),
                    (1, 2, 28)
                );

                // Discarding the application token must leave the actual owner with its queue.
                bw
            };
            assert!(matches!(mr.deregister(), Err(Error::Busy)));
            q.close().unwrap();
            assert_eq!(mr.read(4..8).unwrap()[3], 0x33);
            assert!(!p.allows(bw, bw.address, 1, true));
            mr.deregister().unwrap();
            assert_eq!(
                (d.usage().unwrap().windows, d.usage().unwrap().regions),
                (0, 0)
            );
        },
    );
}

#[test]
fn uncertain_invalidation_and_failed_drain_hold_the_actual_backing() {
    case(
        "uncertain_invalidation_and_failed_drain_hold_the_actual_backing",
        |p| {
            let d = device();
            let mut q = conn(&d);
            let (buf, drops) = external(16);
            let mut mr = d.register(buf, Permissions::RemoteReadWrite).unwrap();
            let mut a = q
                .expose_window(&mr, 0..8, Permissions::RemoteWrite)
                .unwrap();
            let _b = q
                .expose_window(&mr, 8..16, Permissions::RemoteWrite)
                .unwrap();
            assert!(a.descriptor(&mut q, RemoteAccess::Write).unwrap().is_some());
            p.fault("invalidate", "free_qp");
            assert!(matches!(a.revoke(&mut q), Err(Error::Native { .. })));
            assert!(matches!(a.revoke(&mut q), Err(Error::Closed)));
            assert!(matches!(mr.read(0..8), Err(Error::Busy)));
            assert!(matches!(mr.read(8..16), Err(Error::Busy)));
            cleanup(q.close().unwrap_err(), "ibv_destroy_qp");
            p.fault("", "");
            cleanup(q.close().unwrap_err(), "ibv_destroy_qp");
            assert!(matches!(mr.deregister(), Err(Error::Busy)));
            drop(q);
            drop(mr);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert_eq!(
                (
                    p.count("invalidate"),
                    p.count("free_qp"),
                    p.count("free_mw"),
                    p.count("free_mr")
                ),
                (1, 1, 2, 0)
            );
            assert_eq!(p.count("reset"), 1);
            let u = d.usage().unwrap();
            assert_eq!((u.regions, u.windows, u.registered_bytes), (3, 2, 32));
        },
    );
}

#[test]
fn failed_window_destructors_retain_the_range_and_never_retry() {
    case(
        "failed_window_destructors_retain_the_range_and_never_retry",
        |p| {
            for (op, native) in [("free_mw", "ibv_dealloc_mw"), ("free_mr", "ibv_dereg_mr")] {
                let d = device();
                let mut q = conn(&d);
                let (buf, drops) = external(16);
                let mut mr = d.register(buf, Permissions::RemoteReadWrite).unwrap();
                let mut a = q
                    .expose_window(&mr, 0..8, Permissions::RemoteWrite)
                    .unwrap();
                let _b = q
                    .expose_window(&mr, 8..16, Permissions::RemoteWrite)
                    .unwrap();
                assert!(!a.revoke(&mut q).unwrap());
                p.fault(op, "");
                cleanup(a.revoke(&mut q).unwrap_err(), native);
                assert!(matches!(mr.write(0..8), Err(Error::Busy)));
                assert!(matches!(mr.write(8..16), Err(Error::Busy)));
                p.fault("", "");
                let before = p.count(op);
                cleanup(q.close().unwrap_err(), native);
                cleanup(q.close().unwrap_err(), native);
                assert_eq!(p.count(op), before + 1); // Only B's healthy destructor.
                assert!(matches!(mr.write(0..8), Err(Error::Busy)));
                assert!(mr.write(8..16).is_ok());
                assert!(matches!(mr.deregister(), Err(Error::Busy)));
                drop(mr);
                drop(q);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                assert_eq!(d.usage().unwrap().windows, 1);
            }
        },
    );
}

#[test]
fn only_the_matching_successful_invalidate_allows_reuse() {
    case(
        "only_the_matching_successful_invalidate_allows_reuse",
        |p| {
            for mode in ["foreign_id", "foreign_qp", "wrong_opcode", "failed_wc"] {
                let d = device();
                let mut q = conn(&d);
                let mut mr = d.register(bytes(8), Permissions::RemoteWrite).unwrap();
                let mut a = q
                    .expose_window(&mr, 0..8, Permissions::RemoteWrite)
                    .unwrap();
                assert!(!a.revoke(&mut q).unwrap());
                p.option(mode);
                assert!(a.revoke(&mut q).is_err());
                assert!(matches!(mr.write(0..8), Err(Error::Busy)));
                assert!(matches!(mr.deregister(), Err(Error::Busy)));
                p.option("");
                q.close().unwrap();
                mr.deregister().unwrap();
            }
        },
    );
}

#[test]
fn shared_grants_and_payload_share_the_sq_and_cq() {
    case("shared_grants_and_payload_share_the_sq_and_cq", |p| {
        let d = device();
        let mut left = d
            .queue_pair(QueueConfig {
                send_depth: 2,
                ..QueueConfig::new(19)
            })
            .unwrap();
        let mut right = d.queue_pair(QueueConfig::new(23)).unwrap();
        let (l, r) = (left.parameters(), right.parameters());
        left.connect(&r).unwrap();
        right.connect(&l).unwrap();
        let mut mr = d.register(bytes(28), Permissions::RemoteReadWrite).unwrap();
        mr.write(16..20).unwrap().copy_from_slice(b"data");
        p.option("delay");
        let mut a = left
            .expose_window(&mr, 0..4, Permissions::RemoteWrite)
            .unwrap();
        let mut b = left
            .expose_window(&mr, 8..12, Permissions::RemoteReadWrite)
            .unwrap();
        let send = Request::new(mr.range(16..20).unwrap(), Transfer::Send);
        let rejected = left.submit(vec![send]).unwrap_err();
        assert!(matches!(rejected.error, Error::Exhausted(_)));
        assert!(matches!(
            left.expose_window(&mr, 12..16, Permissions::RemoteRead),
            Err(Error::Exhausted(_))
        ));
        assert_eq!(p.count("sq_wr"), 0);
        p.option("");
        let b_write = b
            .descriptor(&mut left, RemoteAccess::Write)
            .unwrap()
            .unwrap();
        let b_read = b
            .descriptor(&mut left, RemoteAccess::Read)
            .unwrap()
            .unwrap();
        let recv = Request::new(mr.range(20..24).unwrap(), Transfer::Receive);
        let recv = right.submit(vec![recv]).unwrap().operations.pop().unwrap();
        assert!(!a.revoke(&mut left).unwrap());
        let send = left
            .submit(rejected.requests)
            .unwrap()
            .operations
            .pop()
            .unwrap();
        // Polling for A also dispatches an unrelated SEND CQE on this very CQ.
        assert!(a.revoke(&mut left).unwrap());
        assert!(matches!(
            send.outcome(),
            Some(Outcome::Success { bytes: 4, .. })
        ));
        right.progress().unwrap();
        assert!(matches!(
            recv.outcome(),
            Some(Outcome::Success { bytes: 4, .. })
        ));
        assert_eq!(&*mr.read(20..24).unwrap(), b"data");

        for (remote, range, write) in [(b_write, 16..20, true), (b_read, 24..28, false)] {
            let transfer = if write {
                Transfer::Write { remote, offset: 0 }
            } else {
                Transfer::Read { remote, offset: 0 }
            };
            let req = Request::new(mr.range(range).unwrap(), transfer);
            let op = right.submit(vec![req]).unwrap().operations.pop().unwrap();
            right.progress().unwrap();
            assert!(matches!(
                op.outcome(),
                Some(Outcome::Success { bytes: 4, .. })
            ));
        }
        assert_eq!(&*mr.read(24..28).unwrap(), b"data");
        assert!(!b.revoke(&mut left).unwrap());
        assert!(b.revoke(&mut left).unwrap());
        assert_eq!(&*mr.read(8..12).unwrap(), b"data");
        assert_eq!(p.count("free_qp"), 0);
        left.close().unwrap();
        right.close().unwrap();
        mr.deregister().unwrap();
    });
}

#[test]
fn known_rejections_roll_back_and_uncertain_binds_are_drained() {
    case(
        "known_rejections_roll_back_and_uncertain_binds_are_drained",
        |p| {
            let d = Device::open(
                "cpu-first",
                1,
                1,
                Limits {
                    regions: 2,
                    registered_bytes: 20,
                    ..Limits::default()
                },
            )
            .unwrap();
            let mut q = conn(&d);
            let mut mr = d.register(bytes(16), Permissions::RemoteWrite).unwrap();
            assert!(matches!(
                q.expose_window(&mr, 0..4, Permissions::RemoteReadWrite),
                Err(Error::PermissionDenied)
            ));
            assert!(matches!(
                q.expose_window(&mr, 0..8, Permissions::RemoteWrite),
                Err(Error::Exhausted(_))
            ));
            assert_eq!(d.usage().unwrap().range_loans, 0);

            for failure in ["mr", "alloc_mw"] {
                p.fault(failure, "");
                assert!(
                    q.expose_window(&mr, 0..4, Permissions::RemoteWrite)
                        .is_err()
                );
                assert!(mr.write(0..4).is_ok());
                assert_eq!(d.usage().unwrap().regions, 1);
            }
            p.fault("", "");
            p.option("reject_bind");
            assert!(
                q.expose_window(&mr, 0..4, Permissions::RemoteWrite)
                    .is_err()
            );
            assert!(mr.write(0..4).is_ok());
            assert_eq!(d.usage().unwrap().windows, 0);

            p.option("");
            p.fault("bind", "");
            assert!(
                q.expose_window(&mr, 0..4, Permissions::RemoteWrite)
                    .is_err()
            );
            assert!(matches!(mr.read(0..4), Err(Error::Busy)));
            assert!(matches!(
                q.expose_window(&mr, 8..12, Permissions::RemoteWrite),
                Err(Error::Closed)
            ));
            q.close().unwrap();
            assert_eq!(mr.read(0..4).unwrap()[3], 0xa5);
            mr.deregister().unwrap();
        },
    );
}

#[test]
fn unposted_revoke_keeps_both_windows_live_and_can_be_retried() {
    case(
        "unposted_revoke_keeps_both_windows_live_and_can_be_retried",
        |p| {
            let d = device();
            let mut q = conn(&d);
            let mr = d.register(bytes(8), Permissions::RemoteReadWrite).unwrap();
            let mut a = q
                .expose_window(&mr, 0..4, Permissions::RemoteWrite)
                .unwrap();
            let b = q.expose_window(&mr, 4..8, Permissions::RemoteRead).unwrap();
            let ad = a.descriptor(&mut q, RemoteAccess::Write).unwrap().unwrap();
            p.option("reject_invalidate");
            assert!(matches!(
                a.revoke(&mut q),
                Err(Error::Native {
                    code: libc::ENOMEM,
                    ..
                })
            ));
            assert!(p.allows(ad, ad.address, 4, true));
            assert!(matches!(mr.write(0..4), Err(Error::Busy)));
            assert!(b.descriptor(&mut q, RemoteAccess::Read).unwrap().is_some());
            assert_eq!(d.usage().unwrap().windows, 2);
            p.option("");
            assert!(!a.revoke(&mut q).unwrap());
            assert!(a.revoke(&mut q).unwrap());
            assert!(mr.write(0..4).is_ok());
            assert!(matches!(mr.write(4..8), Err(Error::Busy)));
            assert_eq!(p.count("free_qp"), 0);
        },
    );
}

#[test]
fn revoke_waits_for_an_sq_slot_without_retiring_either_range() {
    case(
        "revoke_waits_for_an_sq_slot_without_retiring_either_range",
        |p| {
            let d = device();
            let mut q = d
                .queue_pair(QueueConfig {
                    send_depth: 1,
                    ..QueueConfig::new(19)
                })
                .unwrap();
            let mut peer = q.parameters();
            peer.qpn += 10;
            q.connect(&peer).unwrap();
            let mr = d.register(bytes(8), Permissions::RemoteWrite).unwrap();
            let mut a = q
                .expose_window(&mr, 0..4, Permissions::RemoteWrite)
                .unwrap();
            assert!(a.descriptor(&mut q, RemoteAccess::Write).unwrap().is_some());
            p.option("delay");
            let b = q
                .expose_window(&mr, 4..8, Permissions::RemoteWrite)
                .unwrap();
            assert!(!a.revoke(&mut q).unwrap());
            assert_eq!(p.count("invalidate"), 0);
            assert!(matches!(mr.write(0..8), Err(Error::Busy)));
            p.option("");
            assert!(!a.revoke(&mut q).unwrap());
            assert!(a.revoke(&mut q).unwrap());
            assert!(b.descriptor(&mut q, RemoteAccess::Write).unwrap().is_some());
            assert!(mr.write(0..4).is_ok());
            assert!(matches!(mr.write(4..8), Err(Error::Busy)));
        },
    );
}

#[test]
fn overlapping_read_only_windows_retire_independently() {
    case("overlapping_read_only_windows_retire_independently", |p| {
        let d = device();
        let mut q = conn(&d);
        let mr = d.register(bytes(12), Permissions::RemoteReadWrite).unwrap();
        let mut a = q.expose_window(&mr, 0..8, Permissions::RemoteRead).unwrap();
        let mut b = q
            .expose_window(&mr, 4..12, Permissions::RemoteRead)
            .unwrap();
        assert!(mr.read(0..12).is_ok());
        assert!(matches!(
            q.expose_window(&mr, 2..5, Permissions::RemoteWrite),
            Err(Error::Busy)
        ));
        let bd = b.descriptor(&mut q, RemoteAccess::Read).unwrap().unwrap();
        assert!(!p.allows(bd, bd.address, 8, true));
        assert!(!a.revoke(&mut q).unwrap());
        assert!(a.revoke(&mut q).unwrap());
        assert!(mr.write(0..4).is_ok());
        assert!(matches!(mr.write(4..8), Err(Error::Busy)));
        assert!(p.allows(bd, bd.address, 8, false));
        assert!(!b.revoke(&mut q).unwrap());
        assert!(b.revoke(&mut q).unwrap());
        assert!(mr.write(0..12).is_ok());
    });
}

#[test]
fn close_removes_pending_control_work_before_detaching_each_key() {
    case(
        "close_removes_pending_control_work_before_detaching_each_key",
        |p| {
            let d = device();
            let mut q = conn(&d);
            let mut mr = d.register(bytes(16), Permissions::RemoteWrite).unwrap();
            let mut a = q
                .expose_window(&mr, 0..8, Permissions::RemoteWrite)
                .unwrap();
            assert!(a.descriptor(&mut q, RemoteAccess::Write).unwrap().is_some());
            p.option("delay");
            let _b = q
                .expose_window(&mr, 8..16, Permissions::RemoteWrite)
                .unwrap();
            assert!(!a.revoke(&mut q).unwrap());
            let before = p.events().len();
            q.close().unwrap(); // BIND B and LOCAL_INV A have no observable completions.
            super::events(
                p,
                before,
                &[
                    "reset",
                    "free_mw",
                    "free_mw",
                    "free_qp",
                    "free_mr",
                    "free_mr",
                    "free_cq",
                    "free_channel",
                ],
            );
            let bytes = mr.deregister().unwrap();
            assert_eq!((bytes.as_ref()[7], bytes.as_ref()[15]), (0xa5, 0xa5));
            let u = d.usage().unwrap();
            assert_eq!(
                (u.queue_pairs, u.regions, u.windows, u.range_loans),
                (0, 0, 0, 0)
            );
        },
    );
}

#[test]
fn failed_close_reset_or_window_detach_keeps_all_backing_and_payload() {
    case(
        "failed_close_reset_or_window_detach_keeps_all_backing_and_payload",
        |p| {
            for (step, name) in [
                ("reset", "ibv_modify_qp RESET"),
                ("free_mw", "ibv_dealloc_mw"),
            ] {
                let d = device();
                let mut q = conn(&d);
                let (buf, dropped) = external(24);
                let mut mr = d.register(buf, Permissions::RemoteWrite).unwrap();
                let a = q
                    .expose_window(&mr, 0..8, Permissions::RemoteWrite)
                    .unwrap();
                assert!(a.descriptor(&mut q, RemoteAccess::Write).unwrap().is_some());
                let _b = q
                    .expose_window(&mr, 8..16, Permissions::RemoteWrite)
                    .unwrap();
                let req = Request::new(mr.range(16..24).unwrap(), Transfer::Receive);
                let op = q.submit(vec![req]).unwrap().operations.pop().unwrap();
                let qp_destroys = p.count("free_qp");
                let mr_destroys = p.count("free_mr");
                let attempts = p.count(step);
                p.fault(step, "");
                cleanup(q.close().unwrap_err(), name);
                p.fault("", "");
                cleanup(q.close().unwrap_err(), name);
                assert_eq!(p.count(step), attempts + 1);
                assert_eq!(
                    (p.count("free_qp"), p.count("free_mr")),
                    (qp_destroys, mr_destroys)
                );
                assert!(matches!(op.try_reclaim(), Reclaim::Unquiesced { .. }));
                for r in [0..8, 8..16, 16..24] {
                    assert!(matches!(mr.write(r), Err(Error::Busy)));
                }
                assert!(matches!(mr.deregister(), Err(Error::Busy)));
                drop(q);
                drop(mr);
                assert_eq!(dropped.load(Ordering::SeqCst), 0);
                let u = d.usage().unwrap();
                assert_eq!(
                    (u.windows, u.regions, u.registered_bytes, u.in_flight_bytes),
                    (2, 3, 40, 8)
                );
            }
        },
    );
}

#[cfg(feature = "provider")]
#[test]
fn external_poller_routes_each_window_and_retains_failed_invalidation() {
    use orbit_rdma::provider::SharedWindowControl;
    let (mut a, mut b) = (
        SharedWindowControl::default(),
        SharedWindowControl::default(),
    );
    a.bind(9, 4).unwrap();
    b.bind(9, 20).unwrap();
    assert!(a.revoke(5).is_err());
    assert!(a.complete(8, 4, 0, 5).is_err());
    assert!(b.complete(9, 4, 0, 5).is_err());
    b.complete(9, 20, 0, 5).unwrap();
    a.complete(9, 4, 0, 5).unwrap();
    a.revoke(7).unwrap();
    assert!(!a.is_invalidated() && b.is_bound());
    assert!(a.complete(9, 4, 0, 5).is_err());
    a.complete(9, 7, 0, 6).unwrap();
    assert!(a.is_invalidated() && b.is_bound());
    b.revoke(23).unwrap();
    assert!(b.complete(9, 23, 5, 0).is_err());
    assert!(b.complete(9, 23, 0, 6).is_err());
    assert!(!b.is_bound() && !b.is_invalidated());
}
