#![forbid(unsafe_code)]
use super::*;
use std::future::{Future, poll_fn};
use std::task::Poll;
use std::time::{Duration, Instant};
use tokio::runtime::{Builder, Runtime};
use tokio::time::timeout;

fn runtime() -> Runtime {
    Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap()
}
fn receive(q: &mut QueuePair, m: &RegisteredMemory) -> Operation {
    q.submit(vec![Request::new(
        m.range(0..4).unwrap(),
        Transfer::Receive,
    )])
    .unwrap()
    .operations
    .pop()
    .unwrap()
}

#[test]
fn cancelled_wait_keeps_only_the_accepted_prefix_and_reuses_the_arm() {
    isolated(
        "async_progress::cancelled_wait_keeps_only_the_accepted_prefix_and_reuses_the_arm",
        |p| {
            let d = device();
            let m = d.register(bytes(8), Permissions::Local).unwrap();
            let mut q = conn(&d);
            let n = q.parameters().qpn;
            p.post_result(1, 0);
            let mut post = q
                .submit(vec![
                    Request::new(m.range(0..4).unwrap(), Transfer::Receive),
                    Request::new(m.range(4..8).unwrap(), Transfer::Receive),
                ])
                .unwrap();
            assert!(matches!(post.status, PostStatus::Partial { .. }));
            assert_eq!(post.unposted.len(), 1);
            let op = post.operations.pop().unwrap();
            let id = op.id();
            let first = runtime();
            first.block_on(async {
                assert!(
                    timeout(Duration::from_millis(5), op.wait_async(&mut q))
                        .await
                        .is_err()
                );
            });
            assert_eq!(p.count("arm"), 1);
            assert!(!op.is_reusable());
            drop(op);
            drop(first);
            assert!(matches!(m.write(0..4), Err(Error::Busy)));
            m.write(4..8).unwrap().fill(7);
            assert_eq!(d.usage().unwrap().receive_requests, 1);

            // Register on a new reactor, then inject the CQE only after its waiter sleeps.
            runtime().block_on(async {
                let mut wait = Box::pin(q.wait_for_progress_async());
                let mut emitted = false;
                let completed = timeout(
                    Duration::from_secs(3),
                    poll_fn(|cx| match wait.as_mut().poll(cx) {
                        Poll::Pending if !emitted => {
                            emitted = true;
                            p.emit(n, id, n, (128, 4, 0), None);
                            Poll::Pending
                        }
                        polled => polled,
                    }),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(emitted);
                assert_eq!(completed, 1);
            });
            assert_eq!(p.count("ack"), 1);
            assert_eq!(p.count("arm"), 1);
            assert_eq!(d.usage().unwrap().receive_requests, 0);
            m.write(0..4).unwrap().fill(9);
        },
    );
}

#[test]
fn async_event_races_errors_and_blocking_handoff() {
    isolated(
        "async_progress::async_event_races_errors_and_blocking_handoff",
        |p| {
            let rt = runtime();
            let d = device();
            let m = d.register(bytes(4), Permissions::Local).unwrap();
            for mode in [
                "arm_race",
                "on_event",
                "spurious",
                "get_event",
                "poll",
                "arm",
            ] {
                p.fault("", "");
                p.option(if mode == "get_event" {
                    "on_event"
                } else {
                    mode
                });
                let mut q = conn(&d);
                let op = receive(&mut q, &m);
                let before = p.count("ack");
                if ["get_event", "poll", "arm"].contains(&mode) {
                    p.fault(mode, "");
                    assert!(rt.block_on(op.wait_async(&mut q)).is_err());
                    assert!(matches!(m.write(0..4), Err(Error::Busy)));
                    assert!(!op.is_reusable());
                    assert!(
                        q.submit(vec![Request::new(
                            m.range(0..4).unwrap(),
                            Transfer::Receive
                        )])
                        .is_err()
                    );
                    p.fault("", "");
                    q.close().unwrap();
                    assert!(op.is_reusable());
                } else {
                    rt.block_on(async {
                        timeout(Duration::from_secs(3), op.wait_async(&mut q))
                            .await
                            .unwrap()
                            .unwrap()
                    });
                    assert!(matches!(
                        op.try_reclaim(),
                        Reclaim::Ready(Completion {
                            outcome: Outcome::Success { bytes: 4, .. },
                            ..
                        })
                    ));
                    assert_eq!(
                        p.count("ack") - before,
                        match mode {
                            "spurious" => 2,
                            "on_event" => 1,
                            _ => 0,
                        }
                    );
                }
            }
            p.option("");
            let mut q = conn(&d);
            let op = receive(&mut q, &m);
            let mut other = conn(&d);
            assert!(matches!(
                rt.block_on(op.wait_async(&mut other)),
                Err(Error::Invalid(_))
            ));
            rt.block_on(async {
                assert!(
                    timeout(Duration::from_millis(5), op.wait_async(&mut q))
                        .await
                        .is_err()
                );
            });
            let n = q.parameters().qpn;
            p.emit(n, op.id(), n, (128, 0, 5), None);
            assert!(
                op.wait_until(&mut q, Instant::now() + Duration::from_secs(2))
                    .unwrap()
            );
            assert!(matches!(
                op.try_reclaim(),
                Reclaim::Ready(Completion {
                    outcome: Outcome::Failed {
                        reason: TransferFailure::Completion { status: 5, .. },
                        ..
                    },
                    ..
                })
            ));
            assert!(m.write(0..4).is_ok());
            assert_eq!(rt.block_on(q.wait_for_progress_async()).unwrap(), 0);
        },
    );
}

#[test]
fn error_only_channel_wakes_and_keeps_dma_until_destroy() {
    isolated(
        "async_progress::error_only_channel_wakes_and_keeps_dma_until_destroy",
        |p| {
            let d = device();
            let m = d.register(bytes(8), Permissions::Local).unwrap();
            for mode in ["event_error_only", "event_error_again"] {
                p.option(mode);
                let mut q = conn(&d);
                let op = receive(&mut q, &m);
                if mode == "event_error_only" {
                    p.fault("get_event", "");
                }
                let error = runtime().block_on(async {
                    timeout(Duration::from_secs(2), op.wait_async(&mut q))
                        .await
                        .unwrap()
                        .unwrap_err()
                });
                assert!(matches!(
                    error,
                    Error::Native {
                        code: libc::EIO,
                        ..
                    }
                ));
                assert!(matches!(m.write(0..4), Err(Error::Busy)));
                assert!(
                    q.submit(vec![Request::new(
                        m.range(4..8).unwrap(),
                        Transfer::Receive
                    )])
                    .is_err()
                );
                assert!(!op.is_reusable());
                p.fault("", "");
                q.close().unwrap();
                assert!(op.is_reusable());
            }
        },
    );
}

#[test]
fn interrupted_handshake_uses_a_fresh_stream_and_validates_the_peer() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    isolated(
        "async_progress::interrupted_handshake_uses_a_fresh_stream_and_validates_the_peer",
        |_| {
            let d = device();
            let mut q = d.queue_pair(QueueConfig::new(7)).unwrap();
            runtime().block_on(async {
            let (mut stream, mut peer) = tokio::io::duplex(64);
            peer.write_all(&[0; 7]).await.unwrap();
            peer.shutdown().await.unwrap();
            assert!(matches!(q.connect_over_async(&mut stream).await, Err(Error::ControlIo(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof));
            let (mut stream, _peer) = tokio::io::duplex(64);
            assert!(timeout(Duration::from_millis(5),q.connect_over_async(&mut stream)).await.is_err());
            let (mut stream,mut peer) = tokio::io::duplex(64);
            let other = tokio::spawn(async move {
                let mut frame = [0u8;30];
                peer.read_exact(&mut frame).await.unwrap();
                frame[3] ^= 4;
                for piece in frame.chunks(5) { peer.write_all(piece).await.unwrap(); }
            });
            q.connect_over_async(&mut stream).await.unwrap();
            other.await.unwrap();
            assert!(matches!(q.connect_over_async(&mut stream).await,Err(Error::Invalid(_))));
        });
            q.close().unwrap();
        },
    );
}
