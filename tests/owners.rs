#[path = "support/provider.rs"]
mod provider;
#[path = "unit/transfers.rs"]
mod transfers;
use orbit_rdma::*;
use provider::{Provider, isolated};
use std::ptr::NonNull;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn device() -> Device {
    Device::open("cpu-first", 1, 1, Limits::default()).unwrap()
}
fn bytes(n: usize) -> HostBuffer {
    HostBuffer::from_vec(vec![0; n]).unwrap()
}
fn conn(d: &Device) -> QueuePair {
    let mut q = d.queue_pair(QueueConfig::new(0x1234)).unwrap();
    let mut peer = q.parameters();
    peer.qpn += 10;
    q.connect(&peer).unwrap();
    q
}
fn cleanup(err: Error, name: &str) {
    assert!(
        matches!(err,Error::Unquiesced{operation,code} if operation==name && code==libc::EIO),
        "{err:?}"
    );
}
fn events(p: &Provider, from: usize, want: &[&str]) {
    assert_eq!(&p.events()[from..], want);
}

#[test]
fn selection_and_rc_transitions() {
    isolated("selection_and_rc_transitions", |p| {
        assert!(matches!(
            Device::open("absent", 1, 0, Limits::default()),
            Err(Error::DeviceNotFound(_))
        ));
        assert!(matches!(
            Device::open("cpu-first", 0, 0, Limits::default()),
            Err(Error::Invalid(_))
        ));
        for (port, index, mode) in [(3, 0, ""), (1, 2, ""), (1, 1, "down"), (1, 1, "stale_gid")] {
            p.option(mode);
            let opens = p.count("open");
            let closes = p.count("close");
            assert!(matches!(
                Device::open("cpu-first", port, index, Limits::default()),
                Err(Error::Invalid(_))
            ));
            assert_eq!(p.count("open"), opens + 1);
            assert_eq!(p.count("close"), closes + 1);
        }
        p.option("");
        let mut d = Device::open("cpu-first", 2, 1, Limits::default()).unwrap();
        let port = d.port().unwrap();
        assert_eq!(
            (port.number, port.link, port.lid),
            (2, Link::InfiniBand, 17)
        );
        p.option("new_mtu");
        assert!(matches!(
            d.queue_pair(QueueConfig::new(9)),
            Err(Error::Invalid(_))
        ));
        p.option("");
        for cfg in [
            QueueConfig::new(1 << 24),
            QueueConfig {
                send_depth: u32::MAX,
                ..QueueConfig::new(1)
            },
            QueueConfig {
                send_depth: 0,
                ..QueueConfig::new(0)
            },
        ] {
            assert!(matches!(d.queue_pair(cfg), Err(Error::Invalid(_))));
        }
        let mut q = d
            .queue_pair(QueueConfig {
                read_depth: 10,
                ..QueueConfig::new(123)
            })
            .unwrap();
        let local = q.parameters();
        assert_eq!((local.responder_depth, local.initiator_depth), (3, 5));
        let mut peer = local;
        peer.qpn = 456;
        peer.psn = 78;
        peer.mtu = 2;
        peer.responder_depth = 2;
        peer.initiator_depth = 1;
        for bad in [
            ConnectionParams { qpn: 0, ..peer },
            ConnectionParams {
                psn: 1 << 24,
                ..peer
            },
            ConnectionParams {
                link: Link::RoceV1,
                ..peer
            },
            ConnectionParams {
                gid: [0xff; 16],
                ..peer
            },
            ConnectionParams { mtu: 9, ..peer },
        ] {
            assert!(matches!(q.connect(&bad), Err(Error::Invalid(_))));
        }
        assert_eq!(p.count("rtr"), 0);
        q.connect(&peer).unwrap();
        assert_eq!(
            (0..8).map(|i| p.attr(i)).collect::<Vec<_>>(),
            [456, 78, 123, 2, 1, 2, 1, 17]
        );
        assert!(q.connect(&peer).is_err());
        assert!(matches!(d.close(), Err(Error::Busy)));
        let from = p.events().len();
        q.close().unwrap();
        q.close().unwrap();
        d.close().unwrap();
        d.close().unwrap();
        events(
            p,
            from,
            &["free_qp", "free_cq", "free_channel", "free_pd", "close"],
        );
        p.option("extended_port");
        let legacy_calls = p.count("port");
        let mut extended = device();
        assert_eq!(extended.port().unwrap().link, Link::RoceV2);
        assert_eq!(p.count("port_ex"), 1);
        assert_eq!(p.count("port"), legacy_calls);
        extended.close().unwrap();
    });
}

#[test]
fn partial_creation_unwinds_in_dependency_order() {
    isolated("partial_creation_unwinds_in_dependency_order", |p| {
        for (fault, trail) in [
            ("open", vec!["open"]),
            ("caps", vec!["caps", "close"]),
            ("port", vec!["port", "close"]),
            ("gid", vec!["gid", "close"]),
            ("gid_ex", vec!["gid_ex", "close"]),
            ("pd", vec!["pd", "close"]),
        ] {
            p.fault(fault, "");
            assert!(matches!(
                Device::open("cpu-first", 1, 0, Limits::default()),
                Err(Error::Native { .. })
            ));
            assert!(
                p.events()
                    .ends_with(&trail.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            );
        }
        p.fault("caps", "close");
        cleanup(
            Device::open("cpu-first", 1, 1, Limits::default())
                .err()
                .unwrap(),
            "ibv_close_device",
        );
        p.fault("", "");
        let mut d = device();
        for (fault, trail) in [
            ("channel", vec!["channel"]),
            ("cq", vec!["cq", "free_channel"]),
            ("qp", vec!["qp", "free_cq", "free_channel"]),
            ("init", vec!["init", "free_qp", "free_cq", "free_channel"]),
        ] {
            p.fault(fault, "");
            assert!(matches!(
                d.queue_pair(QueueConfig::new(0)),
                Err(Error::Native { .. })
            ));
            assert!(
                p.events()
                    .ends_with(&trail.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            );
            assert_eq!(d.usage().unwrap().queue_pairs, 0);
        }
        p.fault("", "");
        p.option("undersized");
        assert!(matches!(
            d.queue_pair(QueueConfig::new(1)),
            Err(Error::Unsupported(_))
        ));
        p.option("");
        for fault in ["rtr", "rts"] {
            p.fault("", "");
            let mut q = d.queue_pair(QueueConfig::new(3)).unwrap();
            p.fault(fault, "");
            assert!(matches!(
                q.connect(&q.parameters()),
                Err(Error::Native { .. })
            ));
            assert!(matches!(q.connect(&q.parameters()), Err(Error::Closed)));
            assert_eq!(d.usage().unwrap().queue_pairs, 0);
        }
        p.fault("mr", "");
        assert!(matches!(
            d.register(bytes(8), Permissions::Local),
            Err(Error::Native { .. })
        ));
        assert_eq!(d.usage().unwrap().registered_bytes, 0);
        p.fault("", "");
        d.close().unwrap();
    });
}

#[test]
fn ambiguous_destruction_is_sticky_and_keeps_dependencies_charged() {
    isolated(
        "ambiguous_destruction_is_sticky_and_keeps_dependencies_charged",
        |p| {
            for (fault, op, tail) in [
                ("free_qp", "ibv_destroy_qp", vec!["free_qp"]),
                ("free_cq", "ibv_destroy_cq", vec!["free_qp", "free_cq"]),
                (
                    "free_channel",
                    "ibv_destroy_comp_channel",
                    vec!["free_qp", "free_cq", "free_channel"],
                ),
            ] {
                p.fault("", "");
                let mut d = Device::open(
                    "cpu-first",
                    1,
                    0,
                    Limits {
                        queue_pairs: 1,
                        ..Default::default()
                    },
                )
                .unwrap();
                let mut q = conn(&d);
                let from = p.events().len();
                p.fault(fault, "");
                cleanup(q.close().unwrap_err(), op);
                p.fault("", "");
                cleanup(q.close().unwrap_err(), op);
                drop(q);
                events(p, from, &tail);
                assert_eq!(d.usage().unwrap().queue_pairs, 1);
                assert!(matches!(d.close(), Err(Error::Busy)));
                assert!(matches!(
                    d.queue_pair(QueueConfig::new(0)),
                    Err(Error::Exhausted(_))
                ));
                let mut mr = d.register(bytes(1), Permissions::Local).unwrap();
                mr.deregister().unwrap();
            }
            for (fault, op, tail) in [
                ("free_pd", "ibv_dealloc_pd", vec!["free_pd"]),
                ("close", "ibv_close_device", vec!["free_pd", "close"]),
            ] {
                p.fault("", "");
                let mut d = device();
                let from = p.events().len();
                p.fault(fault, "");
                cleanup(d.close().unwrap_err(), op);
                p.fault("", "");
                cleanup(d.close().unwrap_err(), op);
                assert!(matches!(
                    d.register(bytes(1), Permissions::Local),
                    Err(Error::Unquiesced { .. })
                ));
                drop(d);
                events(p, from, &tail);
            }
            p.fault("", "");
            let d = device();
            p.fault("init", "free_qp");
            cleanup(
                d.queue_pair(QueueConfig::new(1)).err().unwrap(),
                "ibv_destroy_qp",
            );
            assert_eq!(d.usage().unwrap().queue_pairs, 1);
        },
    );
}

struct Watched {
    bytes: Vec<u8>,
    drops: Arc<AtomicUsize>,
}
impl Drop for Watched {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
#[allow(unsafe_code)]
fn external(n: usize) -> (HostBuffer, Arc<AtomicUsize>) {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut backing = Watched {
        bytes: vec![17; n],
        drops: drops.clone(),
    };
    let ptr = NonNull::new(backing.bytes.as_mut_ptr()).unwrap();
    // SAFETY: the transferred owner exclusively owns this initialized Vec allocation and never resizes it.
    let buffer = unsafe { HostBuffer::from_external(backing, ptr, n) }.unwrap();
    (buffer, drops)
}

#[test]
fn external_backing_range_conflicts_and_registration_cleanup() {
    isolated(
        "external_backing_range_conflicts_and_registration_cleanup",
        |p| {
            let mut d = Device::open(
                "cpu-first",
                1,
                0,
                Limits {
                    regions: 1,
                    registered_bytes: 8,
                    range_loans: 3,
                    ..Default::default()
                },
            )
            .unwrap();
            let (buf, drops) = external(8);
            let mut mr = d.register(buf, Permissions::Local).unwrap();
            assert!(matches!(
                d.register(bytes(1), Permissions::Local),
                Err(Error::Exhausted(_))
            ));
            for r in [8..9, 0..usize::MAX, 0..0] {
                assert!(matches!(mr.read(r), Err(Error::Invalid(_))));
            }
            for _ in 0..50 {
                let a = mr.read(0..4).unwrap();
                let b = mr.read(1..4).unwrap();
                let mut c = mr.write(4..8).unwrap();
                c[0] = 29;
                assert!(matches!(mr.read(0..1), Err(Error::Exhausted(_))));
                std::thread::scope(|s| {
                    s.spawn(|| assert!(matches!(mr.write(3..5), Err(Error::Busy))))
                        .join()
                        .unwrap();
                });
                assert!(matches!(mr.deregister(), Err(Error::Busy)));
                assert_eq!(a[0], b[0]);
                drop((a, b, c));
            }
            let mut original = mr.deregister().unwrap();
            assert_eq!(original.as_ref()[4], 29);
            original.as_mut()[0] = 31;
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(original);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(d.usage().unwrap(), Usage::default());
            let (buf, kept) = external(8);
            let mut mr = d.register(buf, Permissions::Local).unwrap();
            p.fault("free_mr", "");
            let before = p.count("free_mr");
            cleanup(mr.deregister().err().unwrap(), "ibv_dereg_mr");
            p.fault("", "");
            cleanup(mr.deregister().err().unwrap(), "ibv_dereg_mr");
            drop(mr);
            assert_eq!(p.count("free_mr"), before + 1);
            assert_eq!(kept.load(Ordering::SeqCst), 0);
            assert_eq!(d.usage().unwrap().registered_bytes, 8);
            assert!(matches!(d.close(), Err(Error::Busy)));
        },
    );
}

#[test]
fn window_publishes_after_bind_and_returns_range_after_physical_drain() {
    isolated(
        "window_publishes_after_bind_and_returns_range_after_physical_drain",
        |p| {
            let mut d = device();
            let mut mr = d.register(bytes(12), Permissions::RemoteWrite).unwrap();
            assert!(matches!(
                conn(&d).expose(&mr, 2..6, RemoteAccess::Read),
                Err(Error::PermissionDenied)
            ));
            let other = device();
            assert!(matches!(
                conn(&other).expose(&mr, 2..6, RemoteAccess::Write),
                Err(Error::Invalid(_))
            ));
            assert!(matches!(
                conn(&d).expose(&mr, 3..usize::MAX, RemoteAccess::Write),
                Err(Error::Invalid(_))
            ));
            let q = conn(&d);
            let qpn = q.parameters().qpn;
            let mut w = q.expose(&mr, 2..6, RemoteAccess::Write).unwrap();
            p.option("delay");
            assert!(w.descriptor().unwrap().is_none());
            assert!(matches!(mr.read(5..8), Err(Error::Busy)));
            assert!(mr.write(6..12).is_ok());
            p.option("");
            let desc = w.descriptor().unwrap().unwrap();
            assert_eq!((desc.qpn, desc.length), (qpn, 4));
            assert!(p.allows(desc, desc.address, 4, true));
            assert!(!p.allows(desc, desc.address, 4, false));
            assert_ne!(desc.rkey, 0xdead);
            assert_eq!(desc.rkey, 0x5401);
            assert_eq!(
                desc.checked_address(3, 1, RemoteAccess::Write).unwrap(),
                desc.address + 3
            );
            for (off, n) in [(4, 1), (u64::MAX, 2), (0, 0)] {
                assert!(desc.checked_address(off, n, RemoteAccess::Write).is_err());
            }
            assert!(matches!(
                desc.checked_address(0, 1, RemoteAccess::Read),
                Err(Error::PermissionDenied)
            ));
            let from = p.events().len();
            p.option("delay");
            assert!(!w.revoke().unwrap());
            assert!(matches!(w.descriptor(), Err(Error::Closed)));
            assert!(!w.revoke().unwrap());
            assert!(matches!(mr.deregister(), Err(Error::Busy)));
            assert_eq!(p.count("invalidate"), 1);
            p.option("");
            assert!(w.revoke().unwrap());
            assert!(w.revoke().unwrap());
            drop(w);
            events(
                p,
                from,
                &[
                    "invalidate",
                    "poll",
                    "poll",
                    "poll",
                    "free_qp",
                    "free_cq",
                    "free_channel",
                    "free_mw",
                ],
            );
            assert_eq!(
                &*mr.read(0..12).unwrap(),
                &[0, 0, 0, 0, 0, 0xa5, 0, 0, 0, 0, 0, 0]
            );
            mr.deregister().unwrap();
            assert_eq!(
                d.usage().unwrap(),
                Usage {
                    key_indices: 1,
                    ..Default::default()
                }
            );
            d.close().unwrap();
        },
    );
}

#[test]
fn window_faults_preserve_only_unretired_dependencies() {
    isolated("window_faults_preserve_only_unretired_dependencies", |p| {
        for (fault, op, retains_range) in [
            ("free_qp", "ibv_destroy_qp", true),
            ("free_cq", "ibv_destroy_cq", false),
            ("free_channel", "ibv_destroy_comp_channel", false),
            ("free_mw", "ibv_dealloc_mw", true),
        ] {
            p.fault("", "");
            let mut d = device();
            let (buf, drops) = external(8);
            let mut mr = d.register(buf, Permissions::RemoteWrite).unwrap();
            let mut w = conn(&d).expose(&mr, 2..4, RemoteAccess::Write).unwrap();
            w.descriptor().unwrap().unwrap();
            let before = p.count(fault);
            p.fault(fault, "");
            cleanup(w.revoke().unwrap_err(), op);
            p.fault("", "");
            cleanup(w.close().unwrap_err(), op);
            drop(w);
            assert_eq!(p.count(fault), before + 1);
            let u = d.usage().unwrap();
            assert_eq!(
                (u.windows, u.range_loans),
                (usize::from(retains_range), usize::from(retains_range))
            );
            if retains_range {
                assert!(matches!(mr.deregister(), Err(Error::Busy)));
                drop(mr);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
            } else {
                assert_eq!(mr.read(0..8).unwrap()[3], 0xa5);
                drop(mr.deregister().unwrap());
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            }
            assert!(matches!(d.close(), Err(Error::Busy)));
        }
        p.fault("", "");
        let d = device();
        let mr = d.register(bytes(8), Permissions::RemoteReadWrite).unwrap();
        for fault in ["alloc_mw", "bind"] {
            p.fault(fault, "");
            assert!(matches!(
                conn(&d).expose(&mr, 1..3, RemoteAccess::Write),
                Err(Error::Native { .. })
            ));
            assert!(mr.write(1..3).is_ok());
            assert_eq!(d.usage().unwrap().windows, 0);
        }
        // A window allocation failure preceded the bind, even if the unrelated QP cannot close.
        p.fault("alloc_mw", "free_qp");
        cleanup(
            conn(&d)
                .expose(&mr, 1..3, RemoteAccess::Write)
                .err()
                .unwrap(),
            "ibv_destroy_qp",
        );
        assert!(mr.write(1..3).is_ok());
        p.fault("bind", "free_qp");
        cleanup(
            conn(&d)
                .expose(&mr, 1..3, RemoteAccess::Write)
                .err()
                .unwrap(),
            "ibv_destroy_qp",
        );
        assert!(matches!(mr.read(1..3), Err(Error::Busy)));
    });
}

#[test]
fn control_cqe_identity_and_dropped_handles() {
    isolated("control_cqe_identity_and_dropped_handles", |p| {
        let d = device();
        for mode in ["foreign_id", "foreign_qp", "wrong_opcode", "failed_wc"] {
            p.option("");
            let mut mr = d.register(bytes(8), Permissions::RemoteWrite).unwrap();
            let mut w = conn(&d).expose(&mr, 1..4, RemoteAccess::Write).unwrap();
            p.option(mode);
            assert!(w.descriptor().is_err());
            assert!(matches!(w.descriptor(), Err(Error::Closed)));
            mr.deregister().unwrap();
        }
        p.option("");
        let mut mr = d.register(bytes(6), Permissions::RemoteReadWrite).unwrap();
        for fault in ["invalidate", "poll"] {
            p.fault("", "");
            let mut w = conn(&d).expose(&mr, 0..6, RemoteAccess::Read).unwrap();
            w.descriptor().unwrap().unwrap();
            p.fault(fault, "");
            assert!(w.revoke().is_err());
            assert!(mr.write(0..6).is_ok());
        }
        p.fault("", "");
        let (buf, drops) = external(6);
        let held = d.register(buf, Permissions::RemoteWrite).unwrap();
        let mut w = conn(&d).expose(&held, 1..3, RemoteAccess::Write).unwrap();
        drop(held);
        p.option("delay");
        assert!(w.descriptor().unwrap().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(w);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let (buf, drops) = external(5);
        let held = d.register(buf, Permissions::Local).unwrap();
        let guard = held.read(1..4).unwrap();
        drop(held);
        assert_eq!(&*guard, &[17; 3]);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(guard);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        mr.deregister().unwrap();
    });
}

#[test]
fn provider_recycled_key_indices_never_reissue_a_pd_key() {
    isolated(
        "provider_recycled_key_indices_never_reissue_a_pd_key",
        |p| {
            let mut d = Device::open(
                "cpu-first",
                1,
                0,
                Limits {
                    key_indices: 1,
                    ..Default::default()
                },
            )
            .unwrap();
            let mut mr = d.register(bytes(8), Permissions::RemoteRead).unwrap();
            let mut seen = std::collections::HashSet::new();
            for _ in 0..255 {
                let mut w = conn(&d).expose(&mr, 0..4, RemoteAccess::Read).unwrap();
                let key = w.descriptor().unwrap().unwrap().rkey;
                assert!(seen.insert(key));
                assert!(mr.read(0..4).is_ok());
                assert!(matches!(mr.write(1..2), Err(Error::Busy)));
                assert!(w.revoke().unwrap());
            }
            let posts = p.count("bind");
            assert!(matches!(
                conn(&d).expose(&mr, 0..4, RemoteAccess::Read),
                Err(Error::Exhausted("window key tags"))
            ));
            p.option("other_index");
            assert!(matches!(
                conn(&d).expose(&mr, 0..4, RemoteAccess::Read),
                Err(Error::Exhausted("window key indices"))
            ));
            assert_eq!(p.count("bind"), posts);
            assert_eq!(d.usage().unwrap().key_indices, 1);
            mr.deregister().unwrap();
            d.close().unwrap();
            p.option("no_windows");
            let d = device();
            assert!(matches!(
                d.register(bytes(8), Permissions::RemoteRead),
                Err(Error::Unsupported(_))
            ));
            assert!(d.register(bytes(8), Permissions::Local).is_ok());
        },
    );
}

#[test]
fn recycled_qpn_cannot_restore_a_revoked_range_and_caps_fail_before_admission() {
    isolated(
        "recycled_qpn_cannot_restore_a_revoked_range_and_caps_fail_before_admission",
        |p| {
            let mut d = Device::open(
                "cpu-first",
                2,
                0,
                Limits {
                    registered_bytes: 12,
                    windows: 1,
                    ..Default::default()
                },
            )
            .unwrap();
            p.option("small_cq");
            assert!(matches!(
                d.queue_pair(QueueConfig::new(1)),
                Err(Error::Unsupported(_))
            ));
            assert_eq!(d.usage().unwrap().queue_pairs, 0);
            assert_eq!(p.count("qp"), 0);
            p.option("lid_only");
            let mut q = d.queue_pair(QueueConfig::new(1)).unwrap();
            q.connect(&ConnectionParams {
                gid: [0; 16],
                ..q.parameters()
            })
            .unwrap();
            q.close().unwrap();
            assert!(matches!(
                d.register(bytes(13), Permissions::Local),
                Err(Error::Exhausted(_))
            ));
            let mut mr = d.register(bytes(12), Permissions::RemoteRead).unwrap();
            p.option("reuse_qpn");
            let mut w = conn(&d).expose(&mr, 2..7, RemoteAccess::Read).unwrap();
            let old = w.descriptor().unwrap().unwrap();
            assert!(p.allows(old, old.address, 5, false));
            assert!(!p.allows(old, old.address - 1, 1, false));
            assert!(!p.allows(old, old.address + 4, 2, false));
            assert!(!p.allows(old, old.address, 1, true));
            assert!(matches!(
                conn(&d).expose(&mr, 8..9, RemoteAccess::Read),
                Err(Error::Exhausted(_))
            ));
            assert!(w.revoke().unwrap());
            assert!(!p.allows(old, old.address, 1, false));
            let mut fresh = conn(&d).expose(&mr, 2..7, RemoteAccess::Read).unwrap();
            let new = fresh.descriptor().unwrap().unwrap();
            assert_eq!((old.qpn, old.address), (new.qpn, new.address));
            assert_ne!(old.rkey, new.rkey);
            assert!(!p.allows(old, old.address, 1, false));
            assert!(p.allows(new, new.address, 5, false));
            fresh.close().unwrap();
            mr.deregister().unwrap();
            d.close().unwrap();
        },
    );
}

#[cfg(feature = "provider")]
#[test]
#[allow(unsafe_code)]
fn shared_completion_queues_and_partial_rc_creation() {
    use orbit_rdma::provider as verbs;
    isolated(
        "shared_completion_queues_and_partial_rc_creation",
        |fixture| {
            fixture.option("shared_cqs");
            let mut device = verbs::DeviceHandles::default();
            let mut send = verbs::CqHandles::default();
            let mut recv = verbs::CqHandles::default();
            // SAFETY: this isolated test exclusively owns every native handle from the fixture.
            // It retires live and partially created QPs before their CQs, then the PD and device.
            unsafe {
                assert_eq!(verbs::open_device(&mut device, c"cpu-first".as_ptr()), 0);
                assert_eq!(
                    verbs::create_cq(&mut send, &device, 32, std::ptr::null_mut()),
                    0
                );
                assert_eq!(
                    verbs::create_cq(&mut recv, &device, 16, std::ptr::null_mut()),
                    0
                );
                let mut queue = verbs::ConnectionHandles {
                    cq: send.cq,
                    receive_cq: recv.cq,
                    read_depth: verbs::ReadDepth {
                        initiator: 2,
                        responder: 3,
                    },
                    ..Default::default()
                };
                let qpn = verbs::open_rc(&mut queue, device.pd, 1, 1, 8, 4).unwrap();
                let endpoint = verbs::Endpoint {
                    qpn,
                    psn: 3,
                    gid: [1; 16],
                    link_layer: 2,
                    mtu: 4,
                    ..Default::default()
                };
                assert_eq!(
                    verbs::connect(&mut queue, &endpoint, &endpoint, 1, 1, 5, 17, 32),
                    0
                );
                assert_eq!(
                    [
                        fixture.attr(4),
                        fixture.attr(5),
                        fixture.attr(8),
                        fixture.attr(9),
                        fixture.attr(10)
                    ],
                    [3, 2, 5, 17, 32]
                );
                assert_eq!(verbs::destroy_qp(&mut queue), 0);
                fixture.fault("init", "");
                assert_eq!(
                    verbs::open_rc(&mut queue, device.pd, 1, 1, 8, 4),
                    Err(libc::EIO)
                );
                assert!(!queue.qp.is_null());
                assert_eq!(fixture.count("free_qp"), 1);
                assert_eq!(verbs::destroy_qp(&mut queue), 0);
                assert!(queue.qp.is_null());
                assert_eq!(verbs::destroy_cq(&mut recv), 0);
                assert_eq!(verbs::destroy_cq(&mut send), 0);
                assert_eq!(verbs::dealloc_pd(&mut device), 0);
                assert_eq!(verbs::close_device(&mut device), 0);
            }
        },
    );
}
