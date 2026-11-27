//! Completion state for one type-2B window, independent of who polls its CQ.
use crate::{Error, resource::Result, sys::ffi};

#[derive(Default, PartialEq, Eq, Clone, Copy)]
pub(super) enum Stage {
    #[default]
    Unbound,
    Binding,
    Ready,
    Invalidating,
    Invalidated,
    Failed,
}

/// Tracks one grant on an externally polled RC QP using the same completion
/// checks as the owned queue API. The provider owner retains an exact-range
/// bind-only MR, its MW and its DMA loan until successful invalidation, MW
/// deallocation and MR deregistration, or a proven terminal queue teardown.
/// This object only tracks CQ receipts. Dropping it does not retire native work.
#[derive(Default)]
pub struct SharedWindowControl {
    pub(super) stage: Stage,
    qpn: u32,
    id: u64,
}

impl SharedWindowControl {
    /// Associate the signaled BIND_MW with its owning QP and a unique SQ work ID.
    pub fn bind(&mut self, qpn: u32, id: u64) -> Result<()> {
        if self.stage != Stage::Unbound || qpn == 0 || id == 0 {
            return Err(Error::Invalid("shared window bind identity or stage"));
        }
        self.qpn = qpn;
        self.id = id;
        self.stage = Stage::Binding;
        Ok(())
    }

    /// Associate LOCAL_INV with a fresh work ID on the original QP.
    /// The peer must have finished its payload and acknowledged with a plain
    /// SEND or control message. A remote invalidation is a different protocol.
    pub fn revoke(&mut self, id: u64) -> Result<()> {
        if !self.is_bound() || id == 0 || self.id == id {
            return Err(Error::Invalid("shared window invalidate identity or stage"));
        }
        self.id = id;
        self.stage = Stage::Invalidating;
        Ok(())
    }

    /// Consume the actual send-CQ entry. Error CQEs only have defined identity
    /// and status. A wrong identity, opcode or failure cannot advance the grant.
    pub fn complete(&mut self, qpn: u32, id: u64, status: u32, opcode: u32) -> Result<()> {
        let (expected, next) = match self.stage {
            Stage::Binding => (ffi::IBV_WC_BIND_MW, Stage::Ready),
            Stage::Invalidating => (ffi::IBV_WC_LOCAL_INV, Stage::Invalidated),
            _ => return Err(Error::Invalid("unexpected window completion")),
        };
        if qpn != self.qpn || id != self.id {
            return Err(Error::Invalid("foreign or stale window completion"));
        }
        if status != 0 {
            self.stage = Stage::Failed;
            return Err(Error::Native {
                operation: "window completion",
                code: status as i32,
            });
        }
        if opcode != expected {
            self.stage = Stage::Failed;
            return Err(Error::Invalid("window completion opcode"));
        }
        self.stage = next;
        Ok(())
    }

    /// The exact bind completed and local invalidation has not started.
    pub fn is_bound(&self) -> bool {
        self.stage == Stage::Ready
    }

    /// The exact local invalidation completed. Retire the MW and its private
    /// MR successfully before allowing the backing range to be used again.
    pub fn is_invalidated(&self) -> bool {
        self.stage == Stage::Invalidated
    }
}
