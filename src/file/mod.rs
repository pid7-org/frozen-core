//!

use crate::error::{ErrCode, FrozenError, FrozenResult};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod posix;

/// Error codes for [`File`] module
pub(in crate::file) mod err {
    use super::{ErrCode, FrozenError, FrozenResult};

    /// Domain Id for [`File`] is **8**
    const ERRDOMAIN: u8 = 0x08;

    /// module id used for [`FrozenError`]
    static MID: std::sync::OnceLock<u8> = std::sync::OnceLock::new();

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

    /// file already exists
    pub const EXS: ErrCode = ErrCode::new(0x1E, "file already exists");

    /// Default module id used when [`MID`] is not explicitly initialized
    const DEFAULT_MID: u8 = 0x00;

    #[inline(always)]
    fn mid() -> u8 {
        *MID.get_or_init(|| DEFAULT_MID)
    }

    /// Initialize the module identifier used for [`File`] error propagation.
    ///
    /// Returns `Ok(())` if set successfully, or `Err(already_set_id)` if it was already initialized.
    pub(in crate::file) fn init_mid(id: u8) -> Result<(), u8> {
        MID.set(id)
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

/// File descriptor of [`File`]
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub type FileId = libc::c_int;

/// Configurations for [`frozen_core::file::File`]
#[derive(Debug, Clone)]
pub struct FileCfg {
    /// Identifier used while error propagation
    pub module_id: u8,

    /// Absolute path for/of the file
    ///
    /// *NOTE:* The caller must make sure that the path represents a file and all the parent
    /// directories included in the path do exists
    pub path: std::path::PathBuf,

    /// Size (in bytes) of a single chunk in file
    ///
    /// A chunk is a small fixed size allocation and addressing unit used by [`File`] for all
    /// the write/read ops. These ops are operated by index of the chunk and not the offset of the
    /// byte.
    ///
    /// *NOTE:* Chunk size when power of 2, is cache efficient and good for performance
    pub buffer_size: usize,

    /// Number of chunks to pre-allocate on fs when [`File`] is initialized
    ///
    /// Initial file length will be `buffer_size * initial_available_buffers` (bytes).
    pub initial_available_buffers: usize,
}

/// Custom implementation of file handle for [`frozen_core`]
#[derive(Debug)]
pub struct File {
    cfg: FileCfg,
    current_length: usize,

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    file: posix::POSIXFile,
}

unsafe impl Send for File {}
unsafe impl Sync for File {}

impl File {
    /// Creates and pre-allocates a new [`File`] at `cfg.path`
    ///
    /// ## TOCTAU Safe
    ///
    /// If the file already exists, an error w/ [`err::EXS`] is returned to guard against TOCTOU overwrites
    ///
    /// ## Exclusive Lock
    ///
    /// Acquires an exclusive advisory lock via `flock(LOCK_EX | LOCK_NB)` immediately after descriptor creation
    pub fn new(cfg: FileCfg) -> FrozenResult<Self> {
        let _ = err::init_mid(cfg.module_id);

        if cfg.buffer_size == 0 || cfg.initial_available_buffers == 0 {
            return err::default_error(err::INV);
        }

        let init_len = match cfg.buffer_size.checked_mul(cfg.initial_available_buffers) {
            Some(len) => len,
            None => return err::default_error(err::GRW),
        };

        let file = posix::POSIXFile::create(&cfg.path)?;

        if let Err(e) = file.flock() {
            let _ = file.close();
            return Err(e);
        }

        if let Err(e) = file.grow(0, init_len) {
            let _ = file.close();
            return Err(e);
        }

        if let Err(e) = file.sync() {
            let _ = file.close();
            return Err(e);
        }

        Ok(Self { cfg, file, current_length: init_len })
    }

    /// Open an existing [`File`] while validating its layout invariants
    ///
    /// ## Invariants
    ///
    /// Following invariants are checked,
    ///
    /// - Current file length is at least `buffer_size * initial_available_buffers`
    /// - Current file length is a multiple of `buffer_size`
    ///
    /// If any invariant is violated, the file is closed and [`err::CPT`] is returned
    pub fn open(cfg: FileCfg) -> FrozenResult<Self> {
        let _ = err::init_mid(cfg.module_id);

        if cfg.buffer_size == 0 || cfg.initial_available_buffers == 0 {
            return err::default_error(err::INV);
        }

        let init_len = match cfg.buffer_size.checked_mul(cfg.initial_available_buffers) {
            Some(len) => len,
            None => return err::default_error(err::CPT),
        };

        let file = posix::POSIXFile::open(&cfg.path)?;

        if let Err(e) = file.flock() {
            let _ = file.close();
            return Err(e);
        }

        let curr_len = match file.length() {
            Ok(len) => len,
            Err(e) => {
                let _ = file.close();
                return Err(e);
            }
        };

        if curr_len < init_len || curr_len % cfg.buffer_size != 0 {
            let _ = file.close();
            return err::default_error(err::CPT);
        }

        Ok(Self { cfg, file, current_length: curr_len })
    }

    /// Get reference to configuration of [`File`]
    #[inline]
    pub fn cfg(&self) -> &FileCfg {
        &self.cfg
    }

    /// Read current length (in bytes) of [`File`]
    #[inline]
    pub fn length(&self) -> usize {
        self.current_length
    }

    /// Get file descriptor for [`File`]
    #[inline]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn fd(&self) -> FileId {
        self.file.fd()
    }
}
