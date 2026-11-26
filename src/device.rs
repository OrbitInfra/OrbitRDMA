//! Selected port and protection domain. Descendants keep the actual domain alive.
use crate::resource::{Cleanup, Fault, Owner, Result};
use crate::{Error, sys};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex, MutexGuard};

/// Link protocol for the selected port and GID entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    InfiniBand,
    RoceV1,
    RoceV2,
}

/// Address and active MTU observed on an explicitly selected device port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Port {
    pub number: u8,
    pub gid_index: u8,
    pub gid: [u8; 16],
    pub link: Link,
    pub lid: u16,
    /// Verbs MTU encoding (1 through 5 for 256 through 4096 bytes).
    pub mtu: u8,
}
/// Admission limits for one protection domain, including quarantined resources.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Combined byte budget for all SQ and RQ payloads retained by the provider.
    pub in_flight_bytes: usize,
    pub queue_pairs: usize,
    pub regions: usize,
    pub registered_bytes: usize,
    pub windows: usize,
    pub key_indices: usize,
    pub range_loans: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            in_flight_bytes: 1 << 30,
            queue_pairs: 128,
            regions: 256,
            registered_bytes: 1 << 30,
            windows: 128,
            key_indices: 1024,
            range_loans: 1024,
        }
    }
}
/// Live and quarantined reservations in this domain. Key history lives until domain close.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub struct Usage {
    pub send_requests: usize,
    pub receive_requests: usize,
    pub in_flight_bytes: usize,
    pub queue_pairs: usize,
    pub regions: usize,
    pub registered_bytes: usize,
    pub windows: usize,
    pub key_indices: usize,
    pub range_loans: usize,
}
#[derive(Clone, Copy)]
pub(crate) enum Charge {
    Payload { bytes: usize, receive: bool },
    Queue,
    Region(usize),
    Window,
    Range,
}
#[derive(Default)]
struct Ledger {
    usage: Usage,
    // Adapted from OrbiTE's keys.rs: permanent low-eight-bit tag history per PD.
    keys: HashMap<u32, [u64; 4]>,
}
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) struct Permit {
    book: Arc<Mutex<Ledger>>,
    charge: Charge,
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut ledger = lock(&self.book);
        let u = &mut ledger.usage;
        match self.charge {
            Charge::Payload { bytes, receive } => {
                u.in_flight_bytes -= bytes;
                if receive {
                    u.receive_requests -= 1;
                } else {
                    u.send_requests -= 1;
                }
            }
            Charge::Queue => u.queue_pairs -= 1,
            Charge::Region(n) => {
                u.regions -= 1;
                u.registered_bytes -= n;
            }
            Charge::Window => u.windows -= 1,
            Charge::Range => u.range_loans -= 1,
        }
    }
}
struct DeviceData {
    api: &'static sys::Native,
    ctx: Option<sys::Context>,
    pd: Option<sys::Pd>,
}
impl Cleanup for DeviceData {
    fn cleanup(&mut self) -> std::result::Result<(), Fault> {
        sys::destroy(&mut self.pd, "ibv_dealloc_pd", self.api.ibv_dealloc_pd)?;
        sys::destroy(&mut self.ctx, "ibv_close_device", self.api.ibv_close_device)
    }
}
pub(crate) struct Domain {
    owner: Owner<DeviceData>,
    pub caps: sys::Caps,
    pub port: Port,
    ledger: Arc<Mutex<Ledger>>,
    limits: Limits,
}
impl Domain {
    pub(crate) fn native(&self) -> Result<(&'static sys::Native, sys::Context, sys::Pd)> {
        let d = self.owner.get()?;
        Ok((
            d.api,
            d.ctx.ok_or(Error::Closed)?,
            d.pd.ok_or(Error::Closed)?,
        ))
    }
    pub(crate) fn reserve(&self, charge: Charge) -> Result<Permit> {
        let mut ledger = lock(&self.ledger);
        let u = &mut ledger.usage;
        let l = self.limits;
        match charge {
            Charge::Payload { bytes, receive }
                if bytes <= l.in_flight_bytes - u.in_flight_bytes =>
            {
                u.in_flight_bytes += bytes;
                if receive {
                    u.receive_requests += 1;
                } else {
                    u.send_requests += 1;
                }
            }
            Charge::Queue if u.queue_pairs < l.queue_pairs => u.queue_pairs += 1,
            Charge::Region(n)
                if u.regions < l.regions && n <= l.registered_bytes - u.registered_bytes =>
            {
                u.regions += 1;
                u.registered_bytes += n;
            }
            Charge::Window if u.windows < l.windows => u.windows += 1,
            Charge::Range if u.range_loans < l.range_loans => u.range_loans += 1,
            _ => return Err(Error::Exhausted("protection domain resources")),
        }
        Ok(Permit {
            book: self.ledger.clone(),
            charge,
        })
    }
    pub(crate) fn burn_key(&self, seed: u32) -> Result<u32> {
        let mut ledger = lock(&self.ledger);
        let index = seed & !255;
        if !ledger.keys.contains_key(&index) {
            if ledger.keys.len() >= self.limits.key_indices {
                return Err(Error::Exhausted("window key indices"));
            }
            ledger
                .keys
                .try_reserve(1)
                .map_err(|_| Error::Exhausted("window key history"))?;
            ledger.usage.key_indices += 1;
        }
        let tags = ledger.keys.entry(index).or_default();
        for increment in 1..=255 {
            let tag = seed.wrapping_add(increment) & 255;
            let key = index | tag;
            let bit = 1u64 << (tag % 64);
            let word = &mut tags[(tag / 64) as usize];
            if key != 0 && *word & bit == 0 {
                *word |= bit;
                return Ok(key);
            }
        }
        Err(Error::Exhausted("window key tags"))
    }
}

/// An open verbs context and its protection domain on the selected port.
pub struct Device {
    pub(crate) inner: Option<Arc<Domain>>,
}
impl Device {
    /// Opens an exact device, active physical port and GID table entry.
    pub fn open(
        name: impl AsRef<OsStr>,
        number: u8,
        gid_index: u8,
        limits: Limits,
    ) -> Result<Self> {
        if number == 0 {
            return Err(Error::Invalid("port number"));
        }
        let api = sys::get().map_err(Error::Load)?;
        let ctx = api.open(name.as_ref())?;
        let mut owner = Owner::new(DeviceData {
            api,
            ctx: Some(ctx),
            pd: None,
        });
        let selection = (|| {
            let caps = api.caps(ctx)?;
            if number > caps.ports {
                return Err(Error::Invalid("port number"));
            }
            let port = api.port(ctx, number, gid_index)?;
            owner.get_mut()?.pd = Some(api.pd(ctx)?);
            Ok((caps, port))
        })();
        let (caps, port) = selection.map_err(|err| owner.rollback(err))?;
        Ok(Self {
            inner: Some(Arc::new(Domain {
                owner,
                caps,
                port,
                limits,
                ledger: Arc::default(),
            })),
        })
    }
    pub(crate) fn domain(&self) -> Result<&Arc<Domain>> {
        self.inner.as_ref().ok_or(Error::Closed)
    }
    pub fn port(&self) -> Result<Port> {
        Ok(self.domain()?.port)
    }
    /// Returns current charges, including key indices ever used in this domain.
    pub fn usage(&self) -> Result<Usage> {
        Ok(lock(&self.domain()?.ledger).usage)
    }
    /// Returns Busy while any live or quarantined child retains this PD.
    pub fn close(&mut self) -> Result<()> {
        if let Some(domain) = self.inner.as_mut() {
            Arc::get_mut(domain).ok_or(Error::Busy)?.owner.release()?;
            self.inner = None;
        }
        Ok(())
    }
}
