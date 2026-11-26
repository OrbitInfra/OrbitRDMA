use orbit_rdma::{Operation, Outcome, QueuePair, Reclaim, RemoteDescriptor, RemoteWindow};
use std::{
    error::Error,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;
pub(super) fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
pub(super) fn descriptor(w: &mut RemoteWindow) -> Result<RemoteDescriptor> {
    let limit = deadline();
    loop {
        if let Some(value) = w.descriptor()? {
            return Ok(value);
        }
        if Instant::now() >= limit {
            return Err("window bind timed out".into());
        }
        std::thread::yield_now();
    }
}
pub(super) fn finish(q: &mut QueuePair, ops: Vec<Operation>, lengths: &[u32]) -> Result<()> {
    if ops.len() != lengths.len() {
        return Err("incomplete native post".into());
    }
    let limit = deadline();
    for (op, &bytes) in ops.into_iter().zip(lengths) {
        if !op.wait_until(q, limit)? {
            return Err("payload CQE timed out".into());
        }
        let Reclaim::Ready(c) = op.try_reclaim() else {
            return Err("payload range still held".into());
        };
        if c.outcome
            != (Outcome::Success {
                bytes,
                invalidated_rkey: None,
            })
        {
            return Err(format!("unexpected payload outcome {:?}", c.outcome).into());
        }
    }
    Ok(())
}
