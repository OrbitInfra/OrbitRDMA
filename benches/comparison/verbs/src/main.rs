#[path = "../../common.rs"]
mod common;
#[cfg(feature = "send-stages")]
#[path = "../../send_stages.rs"]
mod send_stages;
use common::*;
use std::{io::Write, time::Instant};

fn run() -> Result<()> {
    let (server, work, mut socket) = connection()?;
    if work != "send" {
        return Err("ibverbs 0.9.2 safe wrapper exposes only send/receive posting".into());
    }
    let list = ibverbs::devices()?;
    let device = list
        .iter()
        .find(|d| d.name().is_some_and(|s| s.to_bytes() == b"orbit_rxe"))
        .ok_or("orbit_rxe missing")?;
    let ctx = device.open()?;
    let cq = ctx.create_cq(32, 0)?;
    let pd = ctx.alloc_pd()?;
    let pre = pd
        .create_qp(&cq, &cq, ibverbs::ibv_qp_type::IBV_QPT_RC)
        .set_gid_index(1)
        .set_path_mtu(3)
        .set_max_send_wr(16)
        .set_max_recv_wr(16)
        .build()?;
    bincode::serialize_into(&mut socket, &pre.endpoint())?;
    socket.flush()?;
    let mut q = pre.handshake(bincode::deserialize_from(&mut socket)?)?;
    let mut mr = pd.allocate::<u8>(N + 2)?;
    mr.fill(GUARD);
    barrier(&mut socket)?;
    let mut wc = [ibverbs::ibv_wc::default(); 1];
    #[cfg(feature = "send-stages")]
    let mut stages = send_stages::Stages::new();
    let mut samples = Vec::with_capacity(COUNT);
    for i in 0..TOTAL {
        let id = i as u64 + 1;
        let start;
        if server {
            mr[1..=N].fill(0);
            // SAFETY: the MR and QP stay live. This thread waits for this exact CQE
            // before inspecting the bytes, reusing the slot, or dropping the registration.
            unsafe {
                q.post_receive(&mut mr, 1..N + 1, id)?;
            }
            signal(&mut socket)?;
            start = Instant::now();
        } else {
            wait(&mut socket)?;
            mr[1..=N].copy_from_slice(&pattern(i));
            start = Instant::now();
            // SAFETY: the registered source stays live and unchanged through its matching CQE.
            unsafe {
                q.post_send(&mut mr, 1..N + 1, id)?;
            }
        }
        #[cfg(feature = "send-stages")]
        let posted = (!server).then(Instant::now);
        loop {
            let completed = cq.poll(&mut wc)?;
            if let Some(w) = completed.first() {
                #[cfg(feature = "send-stages")]
                let observed = posted.map(|_| Instant::now());
                if !w.is_valid() || w.wr_id() != id || (server && w.len() != N) {
                    return Err(format!(
                        "unexpected CQE {:?} {} {}",
                        w.error(),
                        w.wr_id(),
                        w.len()
                    )
                    .into());
                }
                #[cfg(feature = "send-stages")]
                if let (Some(post), Some(cq)) = (posted, observed) {
                    stages.record(i, start, post, cq, Instant::now());
                }
                break;
            }
            std::hint::spin_loop();
        }
        if server {
            check(&mr, i)?;
        } else {
            sample(&mut samples, i, start);
        }
    }
    check(&mr, TOTAL - 1)?;
    barrier(&mut socket)?;
    if !server {
        report("send", samples);
        #[cfg(feature = "send-stages")]
        stages.report();
    }
    Ok(())
}
fn main() -> Result<()> {
    run()
}
