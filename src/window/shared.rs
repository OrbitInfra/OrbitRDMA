//! Independent type-2B windows on a long-lived RC queue pair.
use super::{RemoteAccess, RemoteDescriptor};
use crate::device::{Charge, Domain, Permit};
use crate::memory::Loan;
use crate::operation::reserve_ids;
use crate::resource::{Cleanup, Fault, Owner, Result};
use crate::{Error, Permissions, QueuePair, RegisteredMemory, sys};
use std::{collections::HashMap, ops::Range};

struct Backing {
    api: &'static sys::Native,
    loan: Loan,
    mr: Option<sys::Mr>,
    mw: Option<sys::Mw>,
    _region: Permit,
    _window: Permit,
}

impl Backing {
    fn allocate(
        &mut self,
        domain: &Domain,
        permissions: Permissions,
    ) -> Result<(u32, sys::WindowBind)> {
        let (_, _, pd) = domain.native()?;
        let mr = self.loan.register_window(self.api, pd)?;
        self.mr = Some(mr);
        let (mw, seed) = self.api.window(pd)?;
        self.mw = Some(mw);
        let access = match permissions {
            Permissions::RemoteRead => sys::ffi::IBV_ACCESS_REMOTE_READ,
            Permissions::RemoteWrite => sys::ffi::IBV_ACCESS_REMOTE_WRITE,
            Permissions::RemoteReadWrite => sys::RC_READ_WRITE,
            Permissions::Local => return Err(Error::PermissionDenied),
        };
        Ok((
            domain.burn_key(seed)?,
            sys::WindowBind {
                mr,
                mw,
                address: self.loan.address(),
                length: self.loan.length,
                access,
            },
        ))
    }
}

impl Cleanup for Backing {
    fn cleanup(&mut self) -> std::result::Result<(), Fault> {
        self.api.free_window(&mut self.mw)?;
        // The bind target is private to this window. Deregistration also retires
        // accesses that already resolved its key before LOCAL_INV (IBA C10-59/60).
        // The loan and both reservations survive either failing destructor.
        sys::destroy(&mut self.mr, "ibv_dereg_mr", self.api.ibv_dereg_mr)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Binding,
    Ready,
    Invalidating,
    Invalidated,
}

struct Window {
    stage: Stage,
    backing: Owner<Backing>,
}

#[derive(Default)]
pub(crate) struct Windows {
    entries: HashMap<u64, Window>,
    pending: usize,
}
impl Windows {
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub(crate) fn pending(&self) -> usize {
        self.pending
    }

    pub(crate) fn completion(&mut self, qpn: u32, wc: sys::WorkCompletion) -> Result<bool> {
        if self.entries.is_empty() {
            return Ok(false);
        }
        let (id, which) = match wc {
            sys::WorkCompletion::Success { id, qpn, .. }
            | sys::WorkCompletion::Error { id, qpn, .. } => (id, qpn),
        };
        // Each window reserves adjacent, process-unique bind and invalidate IDs.
        let (w, stage, opcode, next) = if let Some(w) = self.entries.get_mut(&id) {
            (w, Stage::Binding, sys::ffi::IBV_WC_BIND_MW, Stage::Ready)
        } else if let Some(w) = id.checked_sub(1).and_then(|key| self.entries.get_mut(&key)) {
            (
                w,
                Stage::Invalidating,
                sys::ffi::IBV_WC_LOCAL_INV,
                Stage::Invalidated,
            )
        } else {
            return Ok(false);
        };
        if which != qpn || w.stage != stage {
            return Err(Error::Invalid("foreign or stale window completion"));
        }
        match wc {
            sys::WorkCompletion::Error { status, .. } => Err(Error::Native {
                operation: "window completion",
                code: status as i32,
            }),
            sys::WorkCompletion::Success { opcode: actual, .. } if actual == opcode => {
                w.stage = next;
                self.pending -= 1;
                Ok(true)
            }
            _ => Err(Error::Invalid("window completion opcode")),
        }
    }
    /// Called only after QP RESET has removed all pending bind/invalidate WRs.
    /// Detaching a key retains its MR, range loan and reservations until QP destroy.
    pub(crate) fn detach(&mut self) -> std::result::Result<(), Fault> {
        for window in self.entries.values_mut() {
            if window.stage != Stage::Invalidated {
                window
                    .backing
                    .cleanup_step(|b| b.api.free_window(&mut b.mw))?;
            }
        }
        Ok(())
    }
    pub(crate) fn drain(&mut self) -> std::result::Result<(), Fault> {
        let mut first = None;
        for (_, mut window) in self.entries.drain() {
            if let Err(fault) = window.backing.retire() {
                first.get_or_insert(fault);
            }
        }
        self.pending = 0;
        first.map_or(Ok(()), Err)
    }
}

/// A grant on a shared RC QP. Drive its original queue to publish or revoke it.
/// The grant owner initiates revocation. The peer acknowledges its last use
/// over the control channel or with a plain SEND. It must not use SEND_WITH_INV
/// on this rkey. A RECV's invalidated_rkey belongs to a separate key protocol.
/// Dropping or forgetting this token leaves the grant with the QP until close.
#[must_use = "revoke the window or close its queue to retire the grant"]
pub struct SharedWindow {
    id: u64,
    key: u32,
    permissions: Permissions,
    revoking: bool,
    retired: bool,
}

impl QueuePair {
    /// Binds one nonempty host range on this QP. Independent windows can coexist
    /// with payload work. Each consumes one MW plus an exact-range bind-only MR
    /// charged to the device's region and registered-byte budgets.
    pub fn expose_window(
        &mut self,
        memory: &RegisteredMemory,
        range: Range<usize>,
        permissions: Permissions,
    ) -> Result<SharedWindow> {
        self.connected()?;
        let q = self.owner.get_mut()?;
        let qp = q.qp.ok_or(Error::Closed)?;
        q.send_capacity(1)?;
        q.windows
            .entries
            .try_reserve(1)
            .map_err(|_| Error::Exhausted("shared windows"))?;
        let id = reserve_ids(2)?;
        let loan = memory.window_loan(&q.domain, range, permissions)?;
        let mut backing = Owner::new(Backing {
            api: q.api,
            mr: None,
            mw: None,
            _region: q.domain.reserve(Charge::Region(loan.length))?,
            _window: q.domain.reserve(Charge::Window)?,
            loan,
        });
        let (key, bind) = backing
            .get_mut()?
            .allocate(&q.domain, permissions)
            .map_err(|e| backing.rollback(e))?;
        let receipt = sys::post_window(qp, id, key, Some(bind)).map_err(|e| backing.rollback(e))?;
        let error = Error::Native {
            operation: "ibv_post_send window bind",
            code: receipt.code,
        };
        if receipt.accepted == Some(0) {
            return Err(backing.rollback(error));
        }
        q.windows.entries.insert(
            id,
            Window {
                stage: Stage::Binding,
                backing,
            },
        );
        q.windows.pending += 1;
        if receipt.code != 0 || receipt.accepted != Some(1) {
            q.payload.fenced = true;
            return Err(error);
        }
        Ok(SharedWindow {
            id,
            key,
            permissions,
            revoking: false,
            retired: false,
        })
    }
}

impl SharedWindow {
    fn stage(&self, qp: &QueuePair) -> Result<Stage> {
        let q = qp.connected()?;
        if q.payload.fenced {
            return Err(Error::Closed);
        }
        q.windows
            .entries
            .get(&self.id)
            .map(|w| w.stage)
            .ok_or(Error::Invalid("window belongs to another queue"))
    }
    /// Returns a directional descriptor once this window's bind has completed.
    /// Read and write views of a read/write window carry the same hardware key.
    /// Give a peer a single-direction window if it should have only that right.
    pub fn descriptor(
        &self,
        qp: &mut QueuePair,
        access: RemoteAccess,
    ) -> Result<Option<RemoteDescriptor>> {
        if self.revoking {
            return Err(Error::Closed);
        }
        if !self.permissions.allows(access) {
            return Err(Error::PermissionDenied);
        }
        if self.stage(qp)? == Stage::Binding {
            qp.progress()?;
        }
        if self.stage(qp)? != Stage::Ready {
            return Ok(None);
        }
        let w = qp
            .connected()?
            .windows
            .entries
            .get(&self.id)
            .ok_or(Error::Closed)?;
        let loan = &w.backing.get()?.loan;
        Ok(Some(RemoteDescriptor {
            qpn: qp.parameters().qpn,
            address: loan.address(),
            length: loan.length as u64,
            rkey: self.key,
            access,
        }))
    }
    /// Returns true once the key is invalid, the MW was deallocated and its
    /// private MR was deregistered. Coordinate normal retirement with the peer:
    /// posting against a stale key can put the peer's entire RC QP in error.
    /// A full SQ returns false while earlier work completes.
    /// A proven unposted invalidation may be retried. On an uncertain post or
    /// CQ error, close the queue to drain it.
    pub fn revoke(&mut self, qp: &mut QueuePair) -> Result<bool> {
        if self.retired {
            return Ok(true);
        }
        let stage = self.stage(qp)?;
        self.revoking = true;
        if stage != Stage::Invalidated {
            qp.progress()?;
        }
        let stage = self.stage(qp)?;
        let q = qp.owner.get_mut()?;
        match stage {
            Stage::Binding | Stage::Invalidating => Ok(false),
            Stage::Ready => {
                match q.send_capacity(1) {
                    Ok(()) => {}
                    Err(Error::Exhausted(_)) => return Ok(false),
                    Err(e) => return Err(e),
                }
                let receipt =
                    sys::post_window(q.qp.ok_or(Error::Closed)?, self.id + 1, self.key, None)?;
                if receipt.code != 0 || receipt.accepted != Some(1) {
                    // A bad_wr pointing at this WR proves it never reached the SQ.
                    // Keep A bound and allow a retry without disrupting other grants.
                    if receipt.accepted != Some(0) {
                        q.payload.fenced = true;
                    }
                    return Err(Error::Native {
                        operation: "ibv_post_send window invalidate",
                        code: receipt.code,
                    });
                }
                q.windows
                    .entries
                    .get_mut(&self.id)
                    .ok_or(Error::Closed)?
                    .stage = Stage::Invalidating;
                q.windows.pending += 1;
                Ok(false)
            }
            Stage::Invalidated => {
                let w = q.windows.entries.get_mut(&self.id).ok_or(Error::Closed)?;
                if let Err(e) = w.backing.release() {
                    q.payload.fenced = true;
                    return Err(e);
                }
                q.windows.entries.remove(&self.id);
                self.retired = true;
                Ok(true)
            }
        }
    }
}
