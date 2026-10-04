//!

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod posix;

/// Error codes for [`File`] module
pub(in crate::file) mod err {
    use crate::error::{ErrCode, FrozenError, FrozenResult};

    /// Domain Id for [`File`] is **8**
    const ERRDOMAIN: u8 = 0x08;

    /// module id used for [`FrozenError`]
    pub static MID: std::sync::OnceLock<u8> = std::sync::OnceLock::new();

    /// internal fuck up (hault and catch fire)
    pub const HCF: ErrCode = ErrCode::new(0x02, "hault and catch fire");

    /// unknown error (fallback)
    pub const UNK: ErrCode = ErrCode::new(0x04, "unknown error");

    /// no more space available
    pub const NSP: ErrCode = ErrCode::new(0x08, "not enough space available on the storage device");

    /// syncing error
    pub const SYN: ErrCode = ErrCode::new(0x0A, "failed to sync/flush data to storage device");

    /// no write perm
    pub const WRT: ErrCode = ErrCode::new(0x0C, "missing permissions for write");

    /// no read perm
    pub const RED: ErrCode = ErrCode::new(0x0E, "missing permissions for read");

    /// invalid file path
    pub const INV: ErrCode = ErrCode::new(0x10, "invalid path to file");

    /// corrupted file
    pub const CPT: ErrCode = ErrCode::new(0x12, "file is either invalid or corrupted");

    /// unable to grow
    pub const GRW: ErrCode = ErrCode::new(0x14, "unable to zero-extend file");

    /// locks exhausted (can happen on systems w/ NFS)
    pub const LEX: ErrCode =
        ErrCode::new(0x18, "failed to obtain lock, as no more locks available");

    /// no write/read permission
    pub const PRM: ErrCode = ErrCode::new(0x1A, "missing permissions for IO");

    /// unable to obtain exclusive lock
    pub const LCK: ErrCode =
        ErrCode::new(0x1C, "failed to obtain exclusive lock as file may already opened");

    /// Default module id used when [`MID`] is not explicitly initialized
    const DEFAULT_MID: u8 = 0x00;

    #[inline(always)]
    fn mid() -> u8 {
        *MID.get_or_init(|| DEFAULT_MID)
    }

    #[inline]
    pub(in crate::file) fn raw_error<R, E: std::fmt::Display>(
        code: ErrCode,
        error: E,
    ) -> FrozenResult<R> {
        let err = FrozenError::new_raw(mid(), ERRDOMAIN, code, error);
        Err(err)
    }

    #[inline]
    pub(in crate::file) fn default_error<R>(code: ErrCode) -> FrozenResult<R> {
        let err = FrozenError::new(mid(), ERRDOMAIN, code, "");
        Err(err)
    }
}

/// Initialize the module identifier used for [`File`] error propagation.
///
/// Returns `Ok(())` if set successfully, or `Err(already_set_id)` if it was already initialized.
pub fn init_mid(id: u8) -> Result<(), u8> {
    err::MID.set(id)
}

/// File descriptor of [`File`]
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub type FileId = libc::c_int;

///
pub struct File {}
