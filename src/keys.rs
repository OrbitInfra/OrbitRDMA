use std::collections::HashMap;

use crate::{Error, resource::Result};

/// Burned full rkeys survive MW deallocation for the entire native PD lifetime.
pub struct KeyHistory {
    indices: HashMap<u32, [u64; 4]>,
    limit: usize,
}

impl KeyHistory {
    pub fn new(limit: usize) -> Self {
        Self {
            indices: HashMap::new(),
            limit,
        }
    }

    /// Number of native key indices remembered by this protection domain.
    pub fn len(&self) -> usize {
        self.indices.len()
    }

    #[cfg(any(feature = "provider", test))]
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Whether every bindable tag on this provider index has already been used.
    #[cfg(any(feature = "provider", test))]
    pub fn exhausted(&self, native_key: u32) -> bool {
        self.indices.get(&(native_key & !255)).is_some_and(|used| {
            (1..=255).all(|increment| {
                let tag = native_key.wrapping_add(increment) & 255;
                (native_key & !255) | tag == 0
                    || used[(tag / 64) as usize] & (1u64 << (tag % 64)) != 0
            })
        })
    }

    fn tags(&mut self, native_key: u32) -> Result<&mut [u64; 4]> {
        let index = native_key & !255;
        if !self.indices.contains_key(&index) {
            if self.indices.len() >= self.limit {
                return Err(Error::Exhausted("window key indices"));
            }
            self.indices
                .try_reserve(1)
                .map_err(|_| Error::Exhausted("window key history"))?;
        }
        Ok(self.indices.entry(index).or_default())
    }
    /// Reserve a provider-named key before the firmware may make it accessible.
    #[cfg(any(feature = "provider", test))]
    pub fn claim_exact(&mut self, key: u32) -> Result<bool> {
        if key == 0 {
            return Err(Error::Invalid("zero remote key"));
        }
        let used = self.tags(key)?;
        let tag = key & 255;
        let word = &mut used[(tag / 64) as usize];
        let bit = 1u64 << (tag % 64);
        let fresh = *word & bit == 0;
        *word |= bit;
        Ok(fresh)
    }
    /// Burn one fresh nonzero full rkey, distinct from the native initial key.
    pub fn burn(&mut self, native_key: u32) -> Result<u32> {
        let index = native_key & !255;
        let used = self.tags(native_key)?;
        // ibv_inc_rkey changes only the low eight bits. The provider may hand
        // this same index back after MW destruction, even with a different tag.
        // A BIND always changes the native initial key, even if that tag has
        // never been published. Do not rely on a provider accepting equal keys.
        for increment in 1..=255 {
            let tag = native_key.wrapping_add(increment) & 255;
            let word = &mut used[(tag / 64) as usize];
            let bit = 1u64 << (tag % 64);
            if index | tag != 0 && *word & bit == 0 {
                *word |= bit;
                return Ok(index | tag);
            }
        }
        Err(Error::Exhausted("window key tags"))
    }
}

#[cfg(test)]
#[path = "../tests/unit/keys.rs"]
mod tests;
