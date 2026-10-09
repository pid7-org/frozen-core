//! NA

use crate::error::{ErrCode, FrozenError, FrozenResult};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod posix;

/// Error codes for [`MemMap`] module
#[allow(unused)]
pub(in crate::memmap) mod err {
    use super::{ErrCode, FrozenError, FrozenResult};

    /// Domain Id for [`MemMap`] is **16**
    const ERRDOMAIN: u8 = 0x10;

    /// internal fuck up (hault and catch fire)
    pub const HCF: ErrCode = ErrCode::new(0x02, "hault and catch fire");

    /// unknown error (fallback)
    pub const UNK: ErrCode = ErrCode::new(0x04, "unknown error");

    /// no more memory available
    pub const NMM: ErrCode = ErrCode::new(0x06, "not enough memory available on the device");

    /// syncing error
    pub const SYN: ErrCode = ErrCode::new(0x08, "failed to sync/flush data to storage device");

    /// no write/read perm
    pub const PRM: ErrCode = ErrCode::new(0x0A, "missing permissions for IO");

    /// flush_tx error (unable to spawn)
    pub const FXE: ErrCode = ErrCode::new(0x10, "unable to spawn flush_tx");

    /// type `T` is zero sized
    pub const ZRO: ErrCode = ErrCode::new(0x0C, "type T must not be zero sized");

    /// type `T` implements drop
    pub const DRP: ErrCode = ErrCode::new(0x12, "type T must not implement `Drop`");

    /// type `T` is not 8 bytes aligned
    pub const ALN: ErrCode = ErrCode::new(0x14, "type T must be 8-bytes aligned");

    /// `size_of::<T>()` is not multiple of 8
    pub const SZE: ErrCode = ErrCode::new(0x16, "`size_of::<T>()` must be multiple of 8 bytes");

    #[inline]
    pub(in crate::memmap) fn raw_error<R, E: std::fmt::Display>(
        code: ErrCode,
        error: E,
    ) -> FrozenResult<R> {
        let err = FrozenError::unbound_raw(ERRDOMAIN, code, error);
        Err(err)
    }

    #[inline]
    pub(in crate::memmap) fn default_error<R>(code: ErrCode) -> FrozenResult<R> {
        let err = FrozenError::unbound(ERRDOMAIN, code, "");
        Err(err)
    }

    #[inline]
    pub(in crate::memmap) fn make_error(code: ErrCode) -> FrozenError {
        FrozenError::unbound(ERRDOMAIN, code, "")
    }

    #[inline]
    pub(in crate::memmap) fn make_raw_error<E: std::fmt::Display>(
        code: ErrCode,
        error: E,
    ) -> FrozenError {
        FrozenError::unbound_raw(ERRDOMAIN, code, error)
    }
}

/// NA
pub struct MemMap<T> {
    _type: T,
}
