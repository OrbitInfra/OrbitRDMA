//! A type-2B MW is scoped to its RC QP. Dedicated windows retire the QP.
//! Shared windows retire their own MW and exact-range MR.
pub(crate) mod control;
mod shared;
use crate::device::{Charge, Permit};
use crate::memory::Loan;
use crate::resource::{Cleanup, Fault, Owner, Result};
use crate::{Error, QueuePair, RegisteredMemory, sys};
pub use shared::SharedWindow;
pub(crate) use shared::Windows;
use std::ops::Range;

/// Direction of the peer's access to this window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteAccess {
    Read,
    Write,
}
/// Capability to exchange with the connected peer after a successful bind CQE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteDescriptor {
    pub qpn: u32,
    pub address: u64,
    pub length: u64,
    pub rkey: u32,
    pub access: RemoteAccess,
}
impl RemoteDescriptor {
    /// Checks direction and bounds before computing a remote address.
    pub fn checked_address(&self, offset: u64, length: u64, access: RemoteAccess) -> Result<u64> {
        if self.access != access {
            return Err(Error::PermissionDenied);
        }
        if length == 0
            || offset
                .checked_add(length)
                .is_none_or(|end| end > self.length)
            || self.address.checked_add(self.length).is_none()
        {
            return Err(Error::Invalid("remote range"));
        }
        self.address
            .checked_add(offset)
            .ok_or(Error::Invalid("remote address"))
    }
}
enum Binding {
    Unposted,
    MayBeBound,
    Invalidated,
}
struct WindowData {
    qp: QueuePair,
    _domain: std::sync::Arc<crate::device::Domain>,
    binding: Binding,
    api: &'static sys::Native,
    mw: Option<sys::Mw>,
    loan: Option<Loan>,
    permit: Option<Permit>,
}
impl Cleanup for WindowData {
    fn cleanup(&mut self) -> std::result::Result<(), Fault> {
        if matches!(self.binding, Binding::MayBeBound) {
            let qp = self
                .qp
                .owner
                .retained()
                .and_then(|q| q.qp)
                .ok_or(Fault("reset dedicated-window QP", libc::EBADF))?;
            self.api.reset(qp)?;
            self.api.free_window(&mut self.mw)?;
        }
        let qp_fault = self.qp.close().err().map(|err| match err {
            Error::Unquiesced { operation, code } => Fault(operation, code),
            _ => Fault("QP retirement", libc::EIO),
        });
        if matches!(self.binding, Binding::Unposted) {
            self.loan.take();
        } else if !self.qp.drained() {
            return Err(qp_fault.unwrap_or(Fault("QP drain", libc::EBUSY)));
        }
        // Successful QP destruction fences even previously accepted incoming
        // work. An invalidation CQE alone cannot discharge this range loan.
        self.api.free_window(&mut self.mw)?;
        self.loan.take();
        self.permit.take();
        // A later CQ/channel failure keeps those ancestors charged independently.
        qp_fault.map_or(Ok(()), Err)
    }
}
enum State {
    Binding,
    Ready,
    Invalidating,
}
/// A type-2B window that owns its connected QP and range loan through physical drain.
pub struct RemoteWindow {
    owner: Owner<WindowData>,
    state: State,
    desc: RemoteDescriptor,
}

impl QueuePair {
    /// Consumes this connected QP for one exact-range window. Any failed
    /// construction retires the QP, including an ambiguous native post.
    pub fn expose(
        mut self,
        memory: &RegisteredMemory,
        range: Range<usize>,
        access: RemoteAccess,
    ) -> Result<RemoteWindow> {
        let prepare = (|| {
            let queue = self.connected()?;
            if queue.payload.used() || !queue.windows.is_empty() {
                return Err(Error::Busy);
            }
            let loan = memory.remote(&queue.domain, range, access)?;
            let permit = queue.domain.reserve(Charge::Window)?;
            let (_, _, pd) = queue.domain.native()?;
            Ok((queue.api, queue.domain.clone(), pd, loan, permit))
        })();
        let (api, domain, pd, loan, permit) = match prepare {
            Ok(value) => value,
            Err(err) => return Err(self.owner.rollback(err)),
        };
        let desc = RemoteDescriptor {
            qpn: self.parameters().qpn,
            address: loan.address(),
            length: loan.length as u64,
            rkey: 0,
            access,
        };
        let mut owner = Owner::new(WindowData {
            qp: self,
            _domain: domain,
            binding: Binding::Unposted,
            api,
            mw: None,
            loan: Some(loan),
            permit: Some(permit),
        });
        let bind = (|| {
            let r = owner.get_mut()?;
            let q = r.qp.connected()?;
            let (mw, seed) = api.window(pd)?;
            r.mw = Some(mw);
            let key = q.domain.burn_key(seed)?;
            let loan = r.loan.as_ref().ok_or(Error::Closed)?;
            r.binding = Binding::MayBeBound;
            api.post_window(
                q.qp.ok_or(Error::Closed)?,
                key,
                Some(sys::Bind {
                    mw,
                    mr: loan.mr(),
                    address: loan.address(),
                    length: loan.length,
                    access,
                }),
            )?;
            Ok(key)
        })();
        let rkey = bind.map_err(|e| owner.rollback(e))?;
        Ok(RemoteWindow {
            owner,
            state: State::Binding,
            desc: RemoteDescriptor { rkey, ..desc },
        })
    }
}
impl RemoteWindow {
    fn poll(&mut self, bind: bool) -> Result<bool> {
        let data = self.owner.get()?;
        let q = data.qp.connected()?;
        data.api
            .completion(q.cq.ok_or(Error::Closed)?, self.desc.qpn, bind)
            .map_err(|e| self.owner.rollback(e))
    }
    /// Returns None until this window's bind completion has been observed.
    pub fn descriptor(&mut self) -> Result<Option<RemoteDescriptor>> {
        self.owner.get()?;
        if matches!(self.state, State::Invalidating) {
            return Err(Error::Closed);
        }
        if matches!(self.state, State::Binding) && !self.poll(true)? {
            return Ok(None);
        }
        self.state = State::Ready;
        Ok(Some(self.desc))
    }
    /// Starts or polls signaled invalidation. True also requires QP drain and
    /// MW deallocation. A not-yet-published bind can retire by QP destruction.
    pub fn revoke(&mut self) -> Result<bool> {
        if self.owner.retained().is_none() {
            return Ok(true);
        }
        let data = self.owner.get()?;
        match self.state {
            State::Binding => {
                self.close()?;
                return Ok(true);
            }
            State::Ready => {
                let q = data.qp.connected()?;
                data.api
                    .post_window(q.qp.ok_or(Error::Closed)?, self.desc.rkey, None)
                    .map_err(|e| self.owner.rollback(e))?;
                self.state = State::Invalidating;
            }
            State::Invalidating => {}
        }
        if !self.poll(false)? {
            return Ok(false);
        }
        self.owner.get_mut()?.binding = Binding::Invalidated;
        self.close()?;
        Ok(true)
    }
    /// Terminal retirement, including after a dropped or pending control WR.
    pub fn close(&mut self) -> Result<()> {
        self.owner.release().map(drop)
    }
}
