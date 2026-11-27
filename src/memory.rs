//! Owned host backing and checked range loans, adapted from OrbiTE's MR and DMA lease owners.
#![allow(unsafe_code)]
use crate::device::{Charge, Domain, Permit, lock};
use crate::resource::{Cleanup, Fault, Owner, Result};
use crate::{Device, Error, sys};
use std::{
    ops::{Deref, DerefMut, Range},
    ptr::NonNull,
    sync::{Arc, Mutex},
};

/// Exclusive ownership of initialized host storage at a stable address.
pub struct HostBuffer {
    address: NonNull<u8>,
    len: usize,
    _owner: Box<dyn Send + Sync>,
}
// SAFETY: construction transfers exclusive ownership of stable initialized storage.
// Shared CPU access is read-only; registered access is arbitrated by Region.
unsafe impl Send for HostBuffer {}
// SAFETY: safe mutation requires exclusivity, including for an external owner's aliases.
unsafe impl Sync for HostBuffer {}

impl HostBuffer {
    /// Adopts a nonempty allocation without copying its bytes.
    pub fn from_vec(mut bytes: Vec<u8>) -> Result<Self> {
        if bytes.is_empty() {
            return Err(Error::Invalid("empty host buffer"));
        }
        let address = NonNull::new(bytes.as_mut_ptr()).ok_or(Error::Invalid("host address"))?;
        Ok(Self {
            address,
            len: bytes.len(),
            _owner: Box::new(bytes),
        })
    }

    /// Adopts an external owner's exclusive loan of initialized host memory.
    ///
    /// # Safety
    /// The allocation at `address` must remain writable and at the same address
    /// for `len` bytes until `owner` drops, even if this object is forgotten.
    /// The caller must exclude every other CPU and DMA access to those bytes
    /// for that entire lifetime. Dropping the owner returns the loan.
    pub unsafe fn from_external<T: Send + Sync + 'static>(
        owner: T,
        address: NonNull<u8>,
        len: usize,
    ) -> Result<Self> {
        if len == 0
            || len > isize::MAX as usize
            || (address.as_ptr() as usize).checked_add(len).is_none()
        {
            return Err(Error::Invalid("external host range"));
        }
        Ok(Self {
            address,
            len,
            _owner: Box::new(owner),
        })
    }
}
impl AsRef<[u8]> for HostBuffer {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: the public buffer can only be accessed outside a live registration.
        unsafe { std::slice::from_raw_parts(self.address.as_ptr(), self.len) }
    }
}
impl AsMut<[u8]> for HostBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: the caller exclusively owns the unregistered, initialized buffer.
        unsafe { std::slice::from_raw_parts_mut(self.address.as_ptr(), self.len) }
    }
}

/// Directions a registration may subsequently grant through a memory window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permissions {
    Local,
    RemoteRead,
    RemoteWrite,
    RemoteReadWrite,
}
impl Permissions {
    pub(crate) fn allows(self, access: crate::RemoteAccess) -> bool {
        matches!(
            (self, access),
            (Self::RemoteReadWrite, _)
                | (Self::RemoteRead, crate::RemoteAccess::Read)
                | (Self::RemoteWrite, crate::RemoteAccess::Write)
        )
    }
}

struct MrData {
    domain: Arc<Domain>,
    api: &'static sys::Native,
    mr: Option<sys::Mr>,
    buffer: HostBuffer,
    _permit: Permit,
}
impl Cleanup for MrData {
    fn cleanup(&mut self) -> std::result::Result<(), Fault> {
        sys::destroy(&mut self.mr, "ibv_dereg_mr", self.api.ibv_dereg_mr)
    }
}
struct Claim {
    id: u64,
    range: Range<usize>,
    write: bool,
}
#[derive(Default)]
struct Claims {
    next: u64,
    entries: Vec<Claim>,
}
struct Region {
    state: Mutex<Registration>,
    permissions: Permissions,
}
struct Registration {
    owner: Owner<MrData>,
    claims: Claims,
}
/// A registered host buffer whose local guards and remote windows share range exclusion.
pub struct RegisteredMemory {
    inner: Option<Arc<Region>>,
}

/// An owned registration and byte range. CPU and DMA access still require a loan.
#[derive(Clone)]
pub struct RegisteredRange {
    region: Arc<Region>,
    bounds: Range<usize>,
}
impl RegisteredRange {
    /// Byte offsets in the original registration.
    pub fn bounds(&self) -> Range<usize> {
        self.bounds.clone()
    }
    pub fn read(&self) -> Result<ReadRange> {
        self.region.claim(self.bounds.clone(), false).map(ReadRange)
    }
    pub fn write(&self) -> Result<WriteRange> {
        self.region.claim(self.bounds.clone(), true).map(WriteRange)
    }
    pub(crate) fn dma(&self, domain: &Arc<Domain>, write: bool) -> Result<Loan> {
        if !Arc::ptr_eq(domain, &lock(&self.region.state).owner.get()?.domain) {
            return Err(Error::Invalid("foreign protection domain"));
        }
        self.region.claim(self.bounds.clone(), write)
    }
}

impl Device {
    /// Registers initialized host memory. Remote permissions authorize future
    /// exact-range windows. The parent MR itself has no remote access flags.
    ///
    /// ```no_run
    /// # use orbit_rdma::{Device, Error, HostBuffer, Permissions};
    /// # fn fill(device: &Device) -> Result<(), Error> {
    /// let mut mr = device.register(HostBuffer::from_vec(vec![0; 4096])?, Permissions::Local)?;
    /// mr.write(16..20)?.copy_from_slice(b"page");
    /// let buffer = mr.deregister()?;
    /// assert_eq!(&buffer.as_ref()[16..20], b"page");
    /// # Ok(()) }
    /// ```
    pub fn register(
        &self,
        buffer: HostBuffer,
        permissions: Permissions,
    ) -> Result<RegisteredMemory> {
        let domain = self.domain()?;
        let (api, _, pd) = domain.native()?;
        let bind = permissions != Permissions::Local;
        if bind && !domain.caps.windows {
            return Err(Error::Unsupported("type 2B memory windows"));
        }
        let permit = domain.reserve(Charge::Region(buffer.len))?;
        let mr = api.register(pd, buffer.address, buffer.len, bind)?;
        let data = MrData {
            domain: domain.clone(),
            mr: Some(mr),
            api,
            buffer,
            _permit: permit,
        };
        Ok(RegisteredMemory {
            inner: Some(Arc::new(Region {
                state: Mutex::new(Registration {
                    owner: Owner::new(data),
                    claims: Claims::default(),
                }),
                permissions,
            })),
        })
    }
}
impl Region {
    fn check(data: &MrData, range: &Range<usize>) -> Result<()> {
        if range.start >= range.end || range.end > data.buffer.len {
            return Err(Error::Invalid("registered range"));
        }
        Ok(())
    }
    fn claim(self: &Arc<Self>, range: Range<usize>, write: bool) -> Result<Loan> {
        let mut state = lock(&self.state);
        let Registration { owner, claims } = &mut *state;
        let data = owner.get()?;
        Self::check(data, &range)?;
        if claims
            .entries
            .iter()
            .any(|c| (write || c.write) && c.range.start < range.end && range.start < c.range.end)
        {
            return Err(Error::Busy);
        }
        let permit = data.domain.reserve(Charge::Range)?;
        claims
            .entries
            .try_reserve(1)
            .map_err(|_| Error::Exhausted("range loans"))?;
        let id = claims
            .next
            .checked_add(1)
            .ok_or(Error::Exhausted("range identities"))?;
        claims.next = id;
        claims.entries.push(Claim {
            id,
            range: range.clone(),
            write,
        });
        // SAFETY: this nonempty subrange was checked against the live backing above.
        let pointer = unsafe { data.buffer.address.add(range.start) };
        Ok(Loan {
            region: self.clone(),
            id,
            pointer,
            length: range.len(),
            _permit: permit,
        })
    }
}
impl RegisteredMemory {
    /// Selects bytes for a transfer without borrowing them until submission.
    pub fn range(&self, bounds: Range<usize>) -> Result<RegisteredRange> {
        let region = self.inner.as_ref().ok_or(Error::Closed)?;
        Region::check(lock(&region.state).owner.get()?, &bounds)?;
        Ok(RegisteredRange {
            region: region.clone(),
            bounds,
        })
    }
    fn claim(&self, range: Range<usize>, write: bool) -> Result<Loan> {
        self.inner
            .as_ref()
            .ok_or(Error::Closed)?
            .claim(range, write)
    }
    /// Borrows a nonempty byte range. Overlapping readers can coexist.
    pub fn read(&self, range: Range<usize>) -> Result<ReadRange> {
        self.claim(range, false).map(ReadRange)
    }
    /// Borrows a nonempty byte range exclusively. Conflicting access returns [`Error::Busy`].
    pub fn write(&self, range: Range<usize>) -> Result<WriteRange> {
        self.claim(range, true).map(WriteRange)
    }
    pub(crate) fn remote(
        &self,
        domain: &Arc<Domain>,
        range: Range<usize>,
        access: crate::RemoteAccess,
    ) -> Result<Loan> {
        let permissions = match access {
            crate::RemoteAccess::Read => Permissions::RemoteRead,
            crate::RemoteAccess::Write => Permissions::RemoteWrite,
        };
        self.window_loan(domain, range, permissions)
    }
    pub(crate) fn window_loan(
        &self,
        domain: &Arc<Domain>,
        range: Range<usize>,
        permissions: Permissions,
    ) -> Result<Loan> {
        use crate::RemoteAccess::{Read, Write};
        let region = self.inner.as_ref().ok_or(Error::Closed)?;
        if !Arc::ptr_eq(domain, &lock(&region.state).owner.get()?.domain) {
            return Err(Error::Invalid("foreign protection domain"));
        }
        if permissions == Permissions::Local
            || [Read, Write]
                .into_iter()
                .any(|dir| permissions.allows(dir) && !region.permissions.allows(dir))
        {
            return Err(Error::PermissionDenied);
        }
        self.claim(range, permissions.allows(Write))
    }
    /// Returns the original backing after the final loan and successful deregistration.
    /// Idle range descriptors become closed, including those in an uncollected completion.
    pub fn deregister(&mut self) -> Result<HostBuffer> {
        let region = self.inner.as_ref().ok_or(Error::Closed)?;
        let mut state = lock(&region.state);
        if !state.claims.entries.is_empty() {
            return Err(Error::Busy);
        }
        let data = state.owner.release()?.ok_or(Error::Closed)?;
        drop(state);
        self.inner = None;
        Ok(data.buffer)
    }
}

pub(crate) struct Loan {
    region: Arc<Region>,
    id: u64,
    pointer: NonNull<u8>,
    pub length: usize,
    _permit: Permit,
}
// SAFETY: the range claim moves with the loan; its mutex excludes incompatible CPU/DMA access.
unsafe impl Send for Loan {}
// SAFETY: sharing a loan permits at most read access. Mutation requires an exclusive guard.
unsafe impl Sync for Loan {}
impl Loan {
    pub(crate) fn register_window(&self, api: &sys::Native, pd: sys::Pd) -> Result<sys::Mr> {
        api.register(pd, self.pointer, self.length, true)
    }
    pub(crate) fn address(&self) -> u64 {
        self.pointer.as_ptr() as usize as u64
    }
    pub(crate) fn mr(&self) -> Result<sys::Mr> {
        lock(&self.region.state)
            .owner
            .get()?
            .mr
            .ok_or(Error::Closed)
    }
    fn bytes(&self) -> &[u8] {
        // SAFETY: only local guards call this; the range claim excludes every conflicting writer.
        unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), self.length) }
    }
}
impl Drop for Loan {
    fn drop(&mut self) {
        lock(&self.region.state)
            .claims
            .entries
            .retain(|claim| claim.id != self.id);
    }
}
/// Shared access to initialized registered bytes for the lifetime of this guard.
pub struct ReadRange(Loan);
impl Deref for ReadRange {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.0.bytes()
    }
}
/// Exclusive access to initialized registered bytes for the lifetime of this guard.
pub struct WriteRange(Loan);
impl Deref for WriteRange {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.0.bytes()
    }
}
impl DerefMut for WriteRange {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: the write guard holds the only overlapping access to this range.
        unsafe { std::slice::from_raw_parts_mut(self.0.pointer.as_ptr(), self.0.length) }
    }
}
