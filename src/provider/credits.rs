//! Physical work credits for a transport that owns its QPs, CQs and DMA backing.
use crate::{Error, resource::Result, verbs::PostReceipt};

/// One ordered work request in an externally owned registered range.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct WorkSlice {
    pub id: u64,
    pub offset: u32,
    pub length: u32,
    pub signaled: bool,
}

struct Entry {
    id: u64,
    length: u32,
    posted: bool,
}

/// A bounded physical SQ shared by any number of application-level grants.
pub struct SendQueueLedger {
    entries: Vec<Entry>,
    depth: usize,
    fenced: bool,
    drained: bool,
}
impl SendQueueLedger {
    pub fn new(depth: usize) -> Result<Self> {
        if !(1..=64).contains(&depth) {
            return Err(Error::Invalid("send credit depth"));
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(depth)
            .map_err(|_| Error::Exhausted("send credits"))?;
        Ok(Self {
            entries,
            depth,
            fenced: false,
            drained: false,
        })
    }
    pub fn outstanding(&self) -> usize {
        self.entries.len()
    }
    pub fn drained(&self) -> bool {
        self.drained
    }
    pub fn fenced(&self) -> bool {
        self.fenced
    }
    /// Reserve up to the available lane credits. Each admitted list ends in a signaled WR.
    pub fn reserve(
        &mut self,
        work: &mut [WorkSlice],
        available: usize,
        signal_interval: usize,
    ) -> Result<usize> {
        if self.fenced || self.drained {
            return Err(Error::Closed);
        }
        if work.is_empty() {
            return Err(Error::Invalid("empty send reservation"));
        }
        let count = work
            .len()
            .min(self.depth - self.entries.len())
            .min(available);
        if count == 0 {
            return Err(Error::Exhausted("send credits"));
        }
        let work = &mut work[..count];
        for (i, wr) in work.iter().enumerate() {
            if wr.id == 0
                || work[..i].iter().any(|other| other.id == wr.id)
                || self.entries.iter().any(|e| e.id == wr.id)
            {
                return Err(Error::Invalid("duplicate send identity"));
            }
        }
        let interval = signal_interval.clamp(1, (self.depth / 2).max(1));
        for (i, wr) in work.iter_mut().enumerate() {
            wr.signaled = (i + 1) % interval == 0 || i + 1 == count;
            self.entries.push(Entry {
                id: wr.id,
                length: wr.length,
                posted: false,
            });
        }
        Ok(count)
    }
    /// Keep the whole possible prefix. Validate every reservation before changing credits.
    pub fn posted(&mut self, work: &[WorkSlice], receipt: PostReceipt) -> Result<(usize, bool)> {
        self.validate(work, false)?;
        let (owed, uncertain) = receipt.possible_prefix(work.len());
        self.entries
            .retain_mut(|entry| match work.iter().position(|wr| wr.id == entry.id) {
                Some(i) if i < owed => {
                    entry.posted = true;
                    true
                }
                Some(_) => false,
                None => true,
            });
        self.fenced |= receipt.code != 0 || uncertain;
        Ok((owed, uncertain))
    }
    fn validate(&self, work: &[WorkSlice], posted: bool) -> Result<()> {
        if work.is_empty()
            || work.iter().enumerate().any(|(i, w)| {
                work[..i].iter().any(|other| other.id == w.id)
                    || !self
                        .entries
                        .iter()
                        .any(|e| e.id == w.id && e.length == w.length && e.posted == posted)
            })
        {
            return Err(Error::Invalid("send credit identity or state"));
        }
        Ok(())
    }
    pub fn complete(&mut self, work: &[WorkSlice], successful: usize) -> Result<()> {
        if successful > work.len() {
            return Err(Error::Invalid("send completion count"));
        }
        self.validate(work, true)?;
        self.entries
            .retain(|e| !work.iter().any(|wr| e.id == wr.id));
        self.fenced |= successful < work.len();
        Ok(())
    }
    /// Stop admission. Return whether a previously healthy SQ had posted payload bytes.
    pub fn fence(&mut self) -> bool {
        let active = !self.fenced && self.entries.iter().any(|e| e.posted && e.length != 0);
        self.fenced = true;
        active
    }
    /// Call after native drain. The transport retains backing independently of this ledger.
    pub fn quiesce(&mut self) -> (usize, u64) {
        let count = self.entries.len();
        let bytes = self
            .entries
            .drain(..)
            .filter(|e| e.posted)
            .map(|e| u64::from(e.length))
            .sum();
        self.drained = true;
        (count, bytes)
    }
}

/// CQ retirement for one ordered send list. A flush proves no payload bytes.
#[derive(Clone, Copy, Default)]
pub struct PostedWindow(u64);
impl PostedWindow {
    pub fn new(count: usize) -> Result<Self> {
        match count {
            0..=63 => Ok(Self((1u64 << count) - 1)),
            64 => Ok(Self(u64::MAX)),
            _ => Err(Error::Invalid("send completion window length")),
        }
    }
    pub fn outstanding(&self) -> usize {
        self.0.count_ones() as usize
    }
    /// Returns the covered prefix start, successful WR count and proposed next
    /// state. Commit it only after validating the CQE's operation and SQ owners.
    pub fn completion(
        self,
        work: &[WorkSlice],
        index: usize,
        status: u32,
    ) -> Result<(usize, usize, Self)> {
        if work.len() > 64 || index >= work.len() || self.0 & (1u64 << index) == 0 {
            return Err(Error::Invalid("send completion identity"));
        }
        if status == 0 && !work[index].signaled {
            return Err(Error::Invalid("unsignaled send success"));
        }
        let mut first = index;
        while first > 0 && !work[first - 1].signaled && self.0 & (1u64 << (first - 1)) != 0 {
            first -= 1;
        }
        let count = index - first + 1;
        let successful = match status {
            0 => count,
            super::abi::IBV_WC_WR_FLUSH_ERR => 0,
            _ => count - 1,
        };
        let mask = (u64::MAX >> (64 - count)) << first;
        Ok((first, successful, Self(self.0 & !mask)))
    }
}

/// Physical RQ reservation. Logical grant acknowledgement lives with the caller.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum ReceiveSlot {
    #[default]
    Idle,
    Reserved,
    Posted,
    Retired,
}
impl ReceiveSlot {
    pub fn pending(self) -> bool {
        matches!(self, Self::Reserved | Self::Posted)
    }
    pub fn reserve(&mut self) -> Result<()> {
        if *self != Self::Idle {
            return Err(Error::Invalid("receive already reserved"));
        }
        *self = Self::Reserved;
        Ok(())
    }
    /// An uncertain result keeps the credit until a physical receipt or teardown.
    pub fn posted(&mut self, receipt: PostReceipt) -> Result<(usize, bool)> {
        if *self != Self::Reserved {
            return Err(Error::Invalid("receive lacks reservation"));
        }
        let prefix = receipt.possible_prefix(1);
        if prefix == (0, false) {
            *self = Self::Idle;
        }
        if prefix == (1, false) && receipt.code == 0 {
            *self = Self::Posted;
        }
        Ok(prefix)
    }
    /// Returns whether a physical credit was previously outstanding.
    pub fn retire(&mut self) -> bool {
        let pending = self.pending();
        *self = Self::Retired;
        pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wr(id: u64) -> WorkSlice {
        WorkSlice {
            id,
            length: 4,
            offset: 0,
            signaled: false,
        }
    }
    #[test]
    fn shared_sq_returns_only_the_known_suffix_and_matching_receipts() {
        let mut q = SendQueueLedger::new(6).unwrap();
        let mut a = [wr(1), wr(3), wr(5)];
        let mut b = [wr(2), wr(4)];
        q.reserve(&mut a, 6, 2).unwrap();
        q.reserve(&mut b, 3, 2).unwrap();
        q.posted(
            &b,
            PostReceipt {
                accepted: Some(2),
                code: 0,
            },
        )
        .unwrap();
        assert_eq!(
            q.posted(
                &a,
                PostReceipt {
                    accepted: Some(2),
                    code: libc::ENOMEM
                }
            )
            .unwrap(),
            (2, false)
        );
        assert!(q.complete(&[wr(1), wr(1)], 2).is_err());
        assert_eq!(q.outstanding(), 4);
        let (start, bytes_ok, next) = PostedWindow::new(2).unwrap().completion(&a, 1, 0).unwrap();
        assert_eq!((start, bytes_ok, next.outstanding()), (0, 2, 0));
        q.complete(&a[..2], 2).unwrap();
        assert_eq!(q.outstanding(), 2);
        assert_eq!(q.quiesce(), (2, 8));
    }
    #[test]
    fn uncertain_receive_and_tail_flush_require_physical_retirement() {
        let mut slot = ReceiveSlot::default();
        slot.reserve().unwrap();
        assert_eq!(
            slot.posted(PostReceipt {
                accepted: None,
                code: libc::EIO
            })
            .unwrap(),
            (1, true)
        );
        assert!(slot.pending());
        assert!(slot.retire());
        assert!(!slot.retire());
        let window = PostedWindow::new(64).unwrap();
        let mut wrs = [wr(1); 64];
        wrs[63].signaled = true;
        let (start, ok, retired) = window
            .completion(&wrs, 63, super::super::abi::IBV_WC_WR_FLUSH_ERR)
            .unwrap();
        assert_eq!((start, ok, retired.outstanding()), (0, 0, 0));
        assert!(retired.completion(&wrs, 63, 0).is_err());
    }
}
