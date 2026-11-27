//! Bounded work identities and provider-named key allocation extracted from OrbiTE.
use super::{GpuKeyRange, KeyHistory};
use crate::{Error, resource::Result};
use std::sync::atomic::{AtomicU64, Ordering};

/// WR IDs for one CQ dispatcher are divided into namespaces of a fixed stride.
/// The dispatcher assigns one returned namespace per physical submission.
pub struct WorkNamespaces {
    stride: u64,
    next: AtomicU64,
}
impl WorkNamespaces {
    pub fn new(stride: u64) -> Result<Self> {
        if stride < 2 {
            return Err(Error::Invalid("work namespace stride"));
        }
        Ok(Self {
            stride,
            next: AtomicU64::new(1),
        })
    }
    pub fn reserve(&self) -> Result<u64> {
        Self::reserve_from(&self.next, self.stride)
    }
    /// Reserve against an external lane's monotonic counter. Initialize it to
    /// one and retain it through every QP that may return completions to the lane.
    pub fn reserve_from(counter: &AtomicU64, stride: u64) -> Result<u64> {
        if stride < 2 {
            return Err(Error::Invalid("work namespace stride"));
        }
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            let next = current
                .checked_add(1)
                .filter(|n| current != 0 && n.checked_mul(stride).is_some())
                .ok_or(Error::Exhausted("work namespaces"))?;
            match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Ok(current),
                Err(observed) => current = observed,
            }
        }
    }
    pub fn work_id(&self, namespace: u64, slot: u64) -> Result<u64> {
        Self::encode(self.stride, namespace, slot)
    }
    /// Encode a slot for an externally owned lane using the same stride as its allocator.
    pub fn encode(stride: u64, namespace: u64, slot: u64) -> Result<u64> {
        if stride < 2 || namespace == 0 || slot >= stride {
            return Err(Error::Invalid("work namespace or slot"));
        }
        namespace
            .checked_mul(stride)
            .and_then(|base| base.checked_add(slot))
            .ok_or(Error::Invalid("work ID overflow"))
    }
    pub fn split(&self, id: u64) -> Result<(u64, u64)> {
        Self::decode(self.stride, id)
    }
    /// Decode an identity observed on an externally owned CQ.
    pub fn decode(stride: u64, id: u64) -> Result<(u64, u64)> {
        if stride < 2 || id < stride {
            return Err(Error::Invalid("reserved work namespace"));
        }
        Ok((id / stride, id % stride))
    }
}

/// One monotonic cursor per native PD. A candidate is burned in the PD's
/// shared history before the caller may submit it to firmware or retry a busy index.
pub struct NamedKeyCursor {
    base: u32,
    count: u64,
    pdn: u64,
    next: u64,
}
impl NamedKeyCursor {
    pub fn new(space: GpuKeyRange, index_limit: usize) -> Result<Self> {
        let count = u64::from(space.count).min(index_limit as u64);
        if count == 0 || u64::from(space.base) + count > 1 << 24 {
            return Err(Error::Invalid("named MKey range"));
        }
        Ok(Self {
            base: space.base,
            count,
            pdn: u64::from(space.pdn),
            next: 0,
        })
    }
    /// Inspect at most `attempts` candidates, preserving the cursor across calls.
    pub fn claim(&mut self, history: &mut KeyHistory, attempts: usize) -> Result<Option<u32>> {
        for _ in 0..attempts {
            let sequence = self.next;
            if sequence >= self.count * 255 {
                break;
            }
            self.next += 1;
            let index = self.base + ((sequence + self.pdn) % self.count) as u32;
            let key = (index << 8) | (1 + (sequence / self.count) as u32);
            if history.claim_exact(key)? {
                return Ok(Some(key));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_namespaces_and_pd_key_rotation() {
        let ns = WorkNamespaces::new(16).unwrap();
        let n = ns.reserve().unwrap();
        assert_eq!(ns.split(ns.work_id(n, 15).unwrap()).unwrap(), (n, 15));
        assert!(ns.work_id(n, 16).is_err());
        assert!(ns.split(15).is_err());
        assert!(WorkNamespaces::new(u64::MAX).unwrap().reserve().is_err());
        let mut h = KeyHistory::new(2);
        let mut a = NamedKeyCursor::new(
            GpuKeyRange {
                base: 20,
                count: 10,
                pdn: 1,
            },
            2,
        )
        .unwrap();
        assert!(h.claim_exact((21 << 8) | 1).unwrap());
        assert_eq!(a.claim(&mut h, 1).unwrap(), None);
        assert_eq!(a.claim(&mut h, 1).unwrap(), Some((20 << 8) | 1));
        assert_eq!(a.claim(&mut h, 1).unwrap(), Some((21 << 8) | 2));
        let mut count = 0;
        while a.claim(&mut h, 1).unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 507);
        assert_eq!(h.len(), 2);
        assert!(
            NamedKeyCursor::new(
                GpuKeyRange {
                    base: 0xffffff,
                    count: 2,
                    pdn: 0
                },
                2
            )
            .is_err()
        );
    }
}
