//! Tokio readiness for a queue's existing completion channel.
#![forbid(unsafe_code)]
use crate::queue::QueueData;
use crate::resource::Result;
use crate::{Error, Operation, QueuePair, sys};
use std::os::fd::OwnedFd;
use tokio::io::{Interest, unix::AsyncFd};
use tokio::runtime::{Handle, Id};

const CHANNEL_READY: Interest = Interest::READABLE.add(Interest::ERROR);

pub(crate) struct Notifier {
    runtime: Id,
    channel: AsyncFd<OwnedFd>,
}

fn reactor_error(error: std::io::Error) -> Error {
    Error::Native {
        operation: "Tokio completion channel",
        code: error.raw_os_error().unwrap_or(libc::EIO),
    }
}

impl QueueData {
    async fn wait_async(&mut self, qpn: u32) -> Result<usize> {
        let ch = self.channel.ok_or(Error::Closed)?;
        let cq = self.cq.ok_or(Error::Closed)?;
        loop {
            match self.poll_or_arm(qpn, None) {
                Ok(Some(n)) => return Ok(n),
                Ok(None) => {}
                Err(err) => {
                    self.payload.fenced = true;
                    return Err(err);
                }
            }
            let runtime = Handle::try_current()
                .map_err(|_| Error::Invalid("Tokio I/O runtime required"))?
                .id();
            let notifier = match &mut self.notifier {
                Some(notifier) if notifier.runtime == runtime => notifier,
                slot => {
                    let fd = sys::Native::notification_fd(ch)?;
                    let channel =
                        AsyncFd::with_interest(fd, CHANNEL_READY).map_err(reactor_error)?;
                    slot.insert(Notifier { runtime, channel })
                }
            };
            let mut ready = notifier
                .channel
                .ready(CHANNEL_READY)
                .await
                .map_err(reactor_error)?;
            match self.api.consume_event(ch, cq) {
                Ok(true) => self.payload.armed = false,
                // A disassociated uverbs channel can report only EPOLLERR. Some
                // kernels return EAGAIN on the final nonblocking event read.
                Ok(false) if ready.ready().is_error() || ready.ready().is_read_closed() => {
                    self.payload.fenced = true;
                    return Err(Error::Native {
                        operation: "completion channel closed",
                        code: libc::EIO,
                    });
                }
                Ok(false) => ready.clear_ready(),
                Err(err) => {
                    self.payload.fenced = true;
                    return Err(err);
                }
            }
        }
    }
}

impl QueuePair {
    /// Drives the existing CQ. Returns after one or more CQEs, or immediately with
    /// zero if no work is pending. Use a Tokio runtime with I/O enabled.
    /// Dropping this future releases only the waiter. The queue still owns its
    /// posted operations and can be driven by either progress interface.
    ///
    /// # Panics
    /// Tokio panics if the active runtime was built without an I/O driver
    /// and a pending operation needs a new reactor registration.
    pub async fn wait_for_progress_async(&mut self) -> Result<usize> {
        let qpn = self.parameters().qpn;
        self.owner.get_mut()?.wait_async(qpn).await
    }
}

impl Operation {
    /// Waits for this operation to retire on its own QP. Inspect the outcome or
    /// reclaim it after the wait. A timeout or dropped future leaves DMA loans
    /// with the QP. See [QueuePair::wait_for_progress_async] for runtime requirements.
    pub async fn wait_async(&self, queue: &mut QueuePair) -> Result<()> {
        while !self.retired_on(queue)? {
            queue.wait_for_progress_async().await?;
        }
        Ok(())
    }
}
