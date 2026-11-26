//! Payload WR chains and CQ events using the retained verbs function table.
use super::*;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataOpcode {
    Read,
    Write,
    Send,
    Receive,
}
impl DataOpcode {
    pub(crate) fn wc(self) -> u32 {
        match self {
            Self::Read => ffi::IBV_WC_RDMA_READ,
            Self::Write => ffi::IBV_WC_RDMA_WRITE,
            Self::Send => ffi::IBV_WC_SEND,
            Self::Receive => ffi::IBV_WC_RECV,
        }
    }
}
pub(crate) struct DataWork {
    pub id: u64,
    pub opcode: DataOpcode,
    pub sge: ffi::ibv_sge,
    pub remote: Option<(u64, u32)>,
    pub signaled: bool,
    pub fence: bool,
}
pub(crate) fn payload_sge(mr: Mr, addr: u64, length: u32) -> ffi::ibv_sge {
    // SAFETY: the in-flight loan keeps the live MR and its immutable local key through retirement.
    let lkey = unsafe { (*mr.0.as_ptr()).lkey };
    ffi::ibv_sge { addr, length, lkey }
}
pub(crate) struct Post {
    pub accepted: Option<usize>,
    pub code: i32,
}
impl Post {
    fn receipt<T>(code: i32, bad: *mut T, chain: &[T]) -> Self {
        if code == 0 && bad.is_null() {
            return Self {
                accepted: Some(chain.len()),
                code,
            };
        }
        if code == 0 {
            return Self {
                accepted: None,
                code: libc::EPROTO,
            };
        }
        // Compare against each actual WR. The provider may return an unrelated pointer.
        Self {
            accepted: chain.iter().position(|w| std::ptr::eq(w, bad)),
            code,
        }
    }
}
pub(crate) enum WorkCompletion {
    Success {
        id: u64,
        qpn: u32,
        opcode: u32,
        bytes: u32,
        invalidated: Option<u32>,
    },
    Error {
        id: u64,
        qpn: u32,
        status: u32,
        vendor: u32,
    },
}
pub(crate) fn storage<T>(capacity: usize) -> RdmaResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(capacity)
        .map_err(|_| Error::Exhausted("work request storage"))?;
    Ok(v)
}
impl Native {
    pub(crate) fn post_payload(
        &self,
        qp: Qp,
        entries: &mut [DataWork],
        receive: bool,
    ) -> RdmaResult<Post> {
        // SAFETY: the QP and its context are held by the submitting owner.
        let ops = unsafe { &(*(*qp.0.as_ptr()).context).ops };
        if receive {
            let post = ops.post_recv.ok_or(Error::Unsupported("ibv_post_recv"))?;
            let mut wrs = storage(entries.len())?;
            wrs.extend(entries.iter_mut().map(|w| ffi::ibv_recv_wr {
                wr_id: w.id,
                sg_list: &mut w.sge,
                num_sge: 1,
                ..Default::default()
            }));
            let mut next = std::ptr::null_mut();
            for wr in wrs.iter_mut().rev() {
                wr.next = next;
                next = wr;
            }
            let mut bad = std::ptr::null_mut();
            // SAFETY: the nonempty linked array and SGEs remain stationary until post returns.
            let code = unsafe { post(qp.0.as_ptr(), next, &mut bad) };
            Ok(Post::receipt(code, bad, &wrs))
        } else {
            let post = ops.post_send.ok_or(Error::Unsupported("ibv_post_send"))?;
            let mut wrs = storage(entries.len())?;
            for w in entries {
                let opcode = match w.opcode {
                    DataOpcode::Read => ffi::IBV_WR_RDMA_READ,
                    DataOpcode::Write => ffi::IBV_WR_RDMA_WRITE,
                    DataOpcode::Send => ffi::IBV_WR_SEND,
                    DataOpcode::Receive => return Err(Error::Invalid("receive on send queue")),
                };
                let mut wr = ffi::ibv_send_wr {
                    wr_id: w.id,
                    opcode,
                    num_sge: 1,
                    sg_list: &mut w.sge,
                    ..Default::default()
                };
                if w.signaled {
                    wr.send_flags |= ffi::IBV_SEND_SIGNALED;
                }
                if w.fence {
                    wr.send_flags |= ffi::IBV_SEND_FENCE;
                }
                if let Some((addr, key)) = w.remote {
                    wr.wr.rdma.remote_addr = addr;
                    wr.wr.rdma.rkey = key;
                }
                wrs.push(wr);
            }
            let mut next = std::ptr::null_mut();
            for wr in wrs.iter_mut().rev() {
                wr.next = next;
                next = wr;
            }
            let mut bad = std::ptr::null_mut();
            // SAFETY: all WRs and SGEs live through this single native post. Loans outlive possible DMA.
            let code = unsafe { post(qp.0.as_ptr(), next, &mut bad) };
            Ok(Post::receipt(code, bad, &wrs))
        }
    }

    pub(crate) fn poll_one(&self, cq: Cq) -> RdmaResult<Option<WorkCompletion>> {
        // SAFETY: this queue has a single CQ consumer and retains the native context.
        let poll = unsafe { (*(*cq.0.as_ptr()).context).ops.poll_cq }
            .ok_or(Error::Unsupported("ibv_poll_cq"))?;
        let mut wc = ffi::ibv_wc {
            status: ffi::IBV_WC_GENERAL_ERR,
            ..Default::default()
        };
        // SAFETY: exactly one complete header-sized WC is writable.
        let count = unsafe { poll(cq.0.as_ptr(), 1, &mut wc) };
        match count {
            0 => Ok(None),
            1 if wc.status != ffi::IBV_WC_SUCCESS => Ok(Some(WorkCompletion::Error {
                id: wc.wr_id,
                qpn: wc.qp_num,
                status: wc.status,
                vendor: wc.vendor_err,
            })),
            1 => {
                let invalidated =
                    if wc.opcode == ffi::IBV_WC_RECV && wc.wc_flags & ffi::IBV_WC_WITH_INV != 0 {
                        // SAFETY: on successful RECV the flag selects this host-order union member.
                        Some(unsafe { wc.__bindgen_anon_1.invalidated_rkey })
                    } else {
                        None
                    };
                Ok(Some(WorkCompletion::Success {
                    id: wc.wr_id,
                    qpn: wc.qp_num,
                    opcode: wc.opcode,
                    bytes: wc.byte_len,
                    invalidated,
                }))
            }
            _ => Err(Error::Native {
                operation: "ibv_poll_cq",
                code: count,
            }),
        }
    }

    pub(crate) fn arm(&self, cq: Cq) -> RdmaResult<()> {
        // SAFETY: the CQ owner serializes its one-shot arm and polling.
        let notify = unsafe { (*(*cq.0.as_ptr()).context).ops.req_notify_cq }
            .ok_or(Error::Unsupported("ibv_req_notify_cq"))?;
        // SAFETY: the CQ remains live and zero requests notification for any completion.
        check("ibv_req_notify_cq", unsafe { notify(cq.0.as_ptr(), 0) })
    }

    pub(crate) fn wait_event(&self, ch: Channel, cq: Cq, deadline: Instant) -> RdmaResult<bool> {
        // SAFETY: this dedicated channel retains the fd for the duration of the wait.
        let fd = unsafe { (*ch.0.as_ptr()).fd };
        if fd < 0 {
            return Err(Error::Invalid("completion channel fd"));
        }
        // A readable fd may race with a spurious wakeup. The event read must honor the deadline too.
        // SAFETY: both fcntl commands target the live privately owned channel fd.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(last_error("completion channel flags"));
        }
        if flags & libc::O_NONBLOCK == 0 {
            // SAFETY: preserve all existing flags on the private fd.
            if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(last_error("nonblocking completion channel"));
            }
        }
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            let ms = remaining
                .as_nanos()
                .div_ceil(1_000_000)
                .min(i32::MAX as u128) as i32;
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives exactly one writable pollfd with the retained channel fd.
            let n = unsafe { libc::poll(&mut pfd, 1, ms) };
            if n < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(last_error("poll completion channel"));
            }
            if n == 0 {
                continue;
            }
            if pfd.revents & libc::POLLIN == 0 {
                return Err(Error::Native {
                    operation: "completion channel",
                    code: libc::EIO,
                });
            }
            let (mut got, mut context) = (std::ptr::null_mut(), std::ptr::null_mut());
            // SAFETY: both outputs are writable, and no other poller consumes this channel.
            if unsafe { (self.ibv_get_cq_event)(ch.0.as_ptr(), &mut got, &mut context) } != 0 {
                match io::Error::last_os_error().kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => continue,
                    _ => return Err(last_error("ibv_get_cq_event")),
                }
            }
            if got.is_null() {
                return Err(Error::Invalid("null completion event CQ"));
            }
            // SAFETY: a successful get returned a live provider CQ; acknowledge exactly that event.
            unsafe { (self.ibv_ack_cq_events)(got, 1) };
            if got != cq.0.as_ptr() {
                return Err(Error::Invalid("foreign completion event CQ"));
            }
            return Ok(true);
        }
    }
}
fn last_error(operation: &'static str) -> Error {
    Error::Native {
        operation,
        code: io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO),
    }
}
