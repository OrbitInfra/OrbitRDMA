//! Native owners follow OrbiTE's terminal cleanup rule. A failed destroy may
//! already have consumed the handle, so retain dependencies and never retry it.
use crate::Error;

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy)]
pub(crate) struct Fault(pub &'static str, pub i32);
impl Fault {
    pub(crate) fn unquiesced(self) -> Error {
        Error::Unquiesced {
            operation: self.0,
            code: self.1,
        }
    }
}
pub(crate) trait Cleanup {
    fn cleanup(&mut self) -> std::result::Result<(), Fault>;
}

pub(crate) struct Owner<T: Cleanup> {
    data: Option<T>,
    failed: Option<Fault>,
}

impl<T: Cleanup> Owner<T> {
    pub(crate) fn new(data: T) -> Self {
        Self {
            data: Some(data),
            failed: None,
        }
    }
    pub(crate) fn get(&self) -> Result<&T> {
        if let Some(fault) = self.failed {
            return Err(fault.unquiesced());
        }
        self.data.as_ref().ok_or(Error::Closed)
    }
    pub(crate) fn get_mut(&mut self) -> Result<&mut T> {
        if let Some(fault) = self.failed {
            return Err(fault.unquiesced());
        }
        self.data.as_mut().ok_or(Error::Closed)
    }
    /// Inspection for dependency-specific teardown receipts, including after an error.
    pub(crate) fn retained(&self) -> Option<&T> {
        self.data.as_ref()
    }
    pub(crate) fn release(&mut self) -> Result<Option<T>> {
        self.retire().map_err(Fault::unquiesced)
    }
    pub(crate) fn retire(&mut self) -> std::result::Result<Option<T>, Fault> {
        self.cleanup_step(T::cleanup)?;
        Ok(self.data.take())
    }
    /// Run one teardown step while keeping all owners and any failure sticky.
    pub(crate) fn cleanup_step(
        &mut self,
        step: impl FnOnce(&mut T) -> std::result::Result<(), Fault>,
    ) -> std::result::Result<(), Fault> {
        if let Some(fault) = self.failed {
            return Err(fault);
        }
        if let Some(data) = &mut self.data {
            if let Err(fault) = step(data) {
                self.failed = Some(fault);
                return Err(fault);
            }
        }
        Ok(())
    }
    pub(crate) fn rollback(&mut self, cause: Error) -> Error {
        self.release().err().unwrap_or(cause)
    }
}

impl<T: Cleanup> Drop for Owner<T> {
    fn drop(&mut self) {
        if self.release().is_err() {
            // A provider may still access these objects after the last Rust handle.
            // Keep the backing, ancestors and their capacity reservations together.
            std::mem::forget(self.data.take());
        }
    }
}
