//! NA

mod interface;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod posix;

#[cfg(target_os = "windows")]
mod windows;

use crate::error::{ErrCode, FrozenError, FrozenResult};
use interface::FileInterface;
use std::sync::atomic;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(in crate::file) type PlatformFile = posix::POSIXFile;

#[cfg(target_os = "windows")]
pub(in crate::file) type PlatformFile = windows::WINFile;

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
    /// Returns `Ok(())` if set successfully, or `Err(already_set_id)` if it was already initialized
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

/// File handle of [`File`]
///
/// ## NOTE
///
/// On Windows, kernel HANDLE's are pointer sized but are stored as `isize` for atomic access; while the
/// `INVALID_HANDLE_VALUE` maps to `-1isize`, which is [`windows::CLOSED_HANDLE`]
#[cfg(target_os = "windows")]
pub type FileId = isize;

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

/// Custom implementation of `std::fs::File`
#[derive(Debug)]
pub struct File {
    cfg: FileCfg,
    file: PlatformFile,
    current_length: atomic::AtomicUsize,
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

        let file = PlatformFile::create(&cfg.path)?;

        if let Err(mut e) = file.grow(0, init_len) {
            if let Err(close_err) = file.close() {
                e.add_suppressed(close_err);
            }
            return Err(e);
        }

        if let Err(mut e) = file.sync() {
            if let Err(close_err) = file.close() {
                e.add_suppressed(close_err);
            }
            return Err(e);
        }

        Ok(Self { cfg, file, current_length: atomic::AtomicUsize::new(init_len) })
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

        let file = PlatformFile::open(&cfg.path)?;

        if let Err(mut e) = file.flock() {
            if let Err(close_err) = file.close() {
                e.add_suppressed(close_err);
            }
            return Err(e);
        }

        let curr_len = match file.length() {
            Ok(len) => len,
            Err(mut e) => {
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }
                return Err(e);
            }
        };

        if curr_len < init_len || curr_len % cfg.buffer_size != 0 {
            let mut e = err::default_error::<Self>(err::CPT).unwrap_err();
            if let Err(close_err) = file.close() {
                e.add_suppressed(close_err);
            }
            return Err(e);
        }

        Ok(Self { cfg, file, current_length: atomic::AtomicUsize::new(curr_len) })
    }

    /// Open an existing [`File`] or create it if it does not yet exist
    ///
    /// ## Semantics
    ///
    /// Uses idempotent open-or-create (`OPEN_ALWAYS` on Windows, `O_CREAT` without `O_EXCL` on POSIX)
    ///
    /// If the file is newly created, it is grown to `buffer_size * initial_available_buffers` and synced; and if
    /// the file already exists, it is opened and its layout invariants are validated
    ///
    /// In both cases, an exclusive advisory lock is acquired on the file
    pub fn open_or_create(cfg: FileCfg) -> FrozenResult<Self> {
        let _ = err::init_mid(cfg.module_id);

        if cfg.buffer_size == 0 || cfg.initial_available_buffers == 0 {
            return err::default_error(err::INV);
        }

        let init_len = match cfg.buffer_size.checked_mul(cfg.initial_available_buffers) {
            Some(len) => len,
            None => return err::default_error(err::INV),
        };

        let file = PlatformFile::new(&cfg.path)?;

        if let Err(mut e) = file.flock() {
            if let Err(close_err) = file.close() {
                e.add_suppressed(close_err);
            }

            return Err(e);
        }

        let curr_len = match file.length() {
            Ok(len) => len,
            Err(mut e) => {
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }

                return Err(e);
            }
        };

        if curr_len == 0 {
            if let Err(mut e) = file.grow(0, init_len) {
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }

                return Err(e);
            }

            if let Err(mut e) = file.sync() {
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }

                return Err(e);
            }

            Ok(Self { cfg, file, current_length: atomic::AtomicUsize::new(init_len) })
        } else {
            if curr_len < init_len || curr_len % cfg.buffer_size != 0 {
                let mut e = err::default_error::<Self>(err::CPT).unwrap_err();
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }

                return Err(e);
            }

            Ok(Self { cfg, file, current_length: atomic::AtomicUsize::new(curr_len) })
        }
    }

    /// Read bytes starting from buffer `index` into `buf` w/ `pread` syscall
    ///
    /// ## Multiple Buffers
    ///
    /// The input `buf` can span across a single or multiple buffers in memory
    ///
    /// ## Constraints
    ///
    /// - `buf.len()` must be a non-zero multiple of `cfg.buffer_size`
    /// - Reading beyond current file length will return [`err::HCF`]
    #[inline(always)]
    pub fn read(&self, buf: &mut [u8], index: usize) -> FrozenResult<()> {
        if buf.is_empty() {
            return Ok(());
        }

        if buf.len() % self.cfg.buffer_size != 0 {
            return err::default_error(err::INV);
        }

        let offset = match index.checked_mul(self.cfg.buffer_size) {
            Some(off) => off,
            None => return err::default_error(err::INV),
        };

        if offset.checked_add(buf.len()).is_none_or(|end| end > self.length()) {
            return err::default_error(err::HCF);
        }

        self.file.pread(buf, offset)
    }

    /// Write bytes starting at buffer `index` from `buf` w/ `pwrite` syscall
    ///
    /// ## Multiple Buffers
    ///
    /// The input `buf` can span across a single or multiple buffers in memory
    ///
    /// ## Constraints
    ///
    /// - `buf.len()` must be a non-zero multiple of `cfg.buffer_size`
    /// - Writing beyond current file length will return [`err::HCF`]
    #[inline(always)]
    pub fn write(&self, buf: &[u8], index: usize) -> FrozenResult<()> {
        if buf.is_empty() {
            return Ok(());
        }

        if buf.len() % self.cfg.buffer_size != 0 {
            return err::default_error(err::INV);
        }

        let offset = match index.checked_mul(self.cfg.buffer_size) {
            Some(off) => off,
            None => return err::default_error(err::INV),
        };

        if offset.checked_add(buf.len()).is_none_or(|end| end > self.length()) {
            return err::default_error(err::HCF);
        }

        self.file.pwrite(buf, offset)
    }

    /// Grow file size of [`File`] by given `count` of buffers
    ///
    /// After successful execution, updated file length will be `current_length + (count * buffer_size)`
    pub fn grow(&self, count: usize) -> FrozenResult<()> {
        if count == 0 {
            return Ok(());
        }

        let len_to_add = match self.cfg.buffer_size.checked_mul(count) {
            Some(len) => len,
            None => return err::default_error(err::GRW),
        };

        let curr_len = self.current_length.load(atomic::Ordering::Acquire);
        self.file.grow(curr_len, len_to_add)?;
        self.current_length.fetch_add(len_to_add, atomic::Ordering::Release);

        Ok(())
    }

    /// Syncs in-mem data to the storage device
    #[inline]
    pub fn sync(&self) -> FrozenResult<()> {
        self.file.sync()
    }

    /// Best-effort call to prompt kernel to start flushing dirty pages in the specified chunk range
    #[cfg(target_os = "linux")]
    pub fn sync_range(&self, index: usize, count: usize) -> FrozenResult<()> {
        let offset = match index.checked_mul(self.cfg.buffer_size) {
            Some(off) => off,
            None => return err::default_error(err::INV),
        };
        let len_to_sync = match count.checked_mul(self.cfg.buffer_size) {
            Some(len) => len,
            None => return err::default_error(err::INV),
        };

        self.file.sync_range(offset, len_to_sync)
    }

    /// Fetch total available buffers in [`File`]
    #[inline]
    pub fn total_buffers(&self) -> FrozenResult<usize> {
        let curr_len = self.length();
        let buffer_size = self.cfg.buffer_size;

        if crate::hints::unlikely(curr_len % buffer_size != 0) {
            return err::default_error(err::CPT);
        }

        Ok(curr_len / buffer_size)
    }

    /// Get reference to configuration of [`File`]
    #[inline]
    pub fn cfg(&self) -> &FileCfg {
        &self.cfg
    }

    /// Read current length (in bytes) of [`File`]
    #[inline]
    pub fn length(&self) -> usize {
        self.current_length.load(atomic::Ordering::Acquire)
    }

    /// Check if [`File`] exists on storage device or not
    ///
    /// ## Access Semantics
    ///
    /// Uses `access(path, F_OK)` to verify the existence of the file
    #[inline]
    pub fn exists(&self) -> FrozenResult<bool> {
        PlatformFile::exists(&self.cfg.path)
    }

    /// Deletes the [`File`] entry from the storage device
    ///
    /// Consumes `self` by value to prevent any concurrent or post-deletion operations
    ///
    /// Unlinks the file at `path`, closes the underlying descriptor, and syncs the parent directory to
    /// guarantee crash-safe durability
    pub fn delete(self) -> FrozenResult<()> {
        let this = core::mem::ManuallyDrop::new(self);
        let cfg = unsafe { core::ptr::read(&this.cfg) };
        let file = unsafe { core::ptr::read(&this.file) };

        file.unlink(&cfg.path)
    }

    /// Get file descriptor or handle for [`File`]
    #[inline]
    pub fn fd(&self) -> FileId {
        self.file.fd()
    }
}

impl Drop for File {
    fn drop(&mut self) {
        if self.file.is_closed() {
            return;
        }

        let _ = self.sync();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    const MID: u8 = 0;
    const BUFFER_SIZE: usize = 0x10;
    const INIT_BUFFERS: usize = 4;

    fn tmp_path() -> (tempfile::TempDir, FileCfg) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmp_file");
        let cfg = FileCfg {
            module_id: MID,
            path,
            buffer_size: BUFFER_SIZE,
            initial_available_buffers: INIT_BUFFERS,
        };

        (dir, cfg)
    }

    mod file_new {
        use super::*;

        #[test]
        fn ok_new_creates_file_with_correct_length() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();

            let expected_len = BUFFER_SIZE * INIT_BUFFERS;
            assert_eq!(file.length(), expected_len);
            assert_eq!(file.cfg().buffer_size, BUFFER_SIZE);
            assert_ne!(file.fd(), PlatformFile::CLOSED_ID);
            assert!(cfg.path.exists());
        }

        #[test]
        fn err_new_when_already_exists() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();
            drop(file);

            // Attempting to create again on an existing path must fail w/ EXS
            let err = File::new(cfg).unwrap_err();
            assert_eq!(err.reason, err::EXS.reason);
        }

        #[test]
        fn err_new_invalid_cfg() {
            let (_dir, mut cfg) = tmp_path();

            cfg.buffer_size = 0;
            let err = File::new(cfg.clone()).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);

            cfg.buffer_size = BUFFER_SIZE;
            cfg.initial_available_buffers = 0;
            let err = File::new(cfg).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_new_missing_parent_dir() {
            let (_dir, mut cfg) = tmp_path();
            cfg.path = cfg.path.join("missing/sub/dir/file.db");

            let err = File::new(cfg).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }
    }

    mod file_open {
        use super::*;

        #[test]
        fn ok_open_existing_valid_file() {
            let (_dir, cfg) = tmp_path();

            {
                let file = File::new(cfg.clone()).unwrap();
                assert_eq!(file.length(), BUFFER_SIZE * INIT_BUFFERS);
            }

            let file = File::open(cfg).unwrap();
            assert_eq!(file.length(), BUFFER_SIZE * INIT_BUFFERS);
        }

        #[test]
        fn err_open_missing_file() {
            let (_dir, cfg) = tmp_path();

            let err = File::open(cfg).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_open_invalid_cfg() {
            let (_dir, mut cfg) = tmp_path();

            cfg.buffer_size = 0;
            let err = File::open(cfg.clone()).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);

            cfg.buffer_size = BUFFER_SIZE;
            cfg.initial_available_buffers = 0;
            let err = File::open(cfg).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_open_when_file_smaller_than_init_len() {
            let (_dir, cfg) = tmp_path();

            // Create an empty file (size 0 < init_len)
            std::fs::write(&cfg.path, []).unwrap();

            let err = File::open(cfg).unwrap_err();
            assert_eq!(err.reason, err::CPT.reason);
        }

        #[test]
        fn err_open_when_file_not_buffer_multiple() {
            let (_dir, cfg) = tmp_path();

            // Create a file with size larger than init_len, but not a multiple of buffer_size
            let non_aligned_len = (BUFFER_SIZE * INIT_BUFFERS) + 3;
            std::fs::write(&cfg.path, vec![0u8; non_aligned_len]).unwrap();

            let err = File::open(cfg).unwrap_err();
            assert_eq!(err.reason, err::CPT.reason);
        }
    }

    mod file_open_or_create {
        use super::*;

        #[test]
        fn ok_creates_when_missing() {
            let (_dir, cfg) = tmp_path();
            assert!(!cfg.path.exists());

            let file = File::open_or_create(cfg.clone()).unwrap();
            assert!(cfg.path.exists());
            assert_eq!(file.length(), BUFFER_SIZE * INIT_BUFFERS);
            assert!(file.exists().unwrap());
        }

        #[test]
        fn ok_opens_when_existing() {
            let (_dir, cfg) = tmp_path();
            // First create and populate
            let file = File::new(cfg.clone()).unwrap();
            let data = [0x5Au8; BUFFER_SIZE];
            file.write(&data, 0).unwrap();
            file.sync().unwrap();
            drop(file);

            // Now open_or_create should open the existing file
            let opened = File::open_or_create(cfg.clone()).unwrap();
            assert_eq!(opened.length(), BUFFER_SIZE * INIT_BUFFERS);
            let mut buf = [0u8; BUFFER_SIZE];
            opened.read(&mut buf, 0).unwrap();
            assert_eq!(buf, data);
        }

        #[test]
        fn err_open_or_create_invalid_cfg() {
            let (_dir, mut cfg) = tmp_path();
            cfg.buffer_size = 0;
            let err = File::open_or_create(cfg.clone()).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);

            cfg.buffer_size = BUFFER_SIZE;
            cfg.initial_available_buffers = 0;
            let err = File::open_or_create(cfg).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_open_or_create_corrupt_existing() {
            let (_dir, cfg) = tmp_path();
            // Write a corrupt non-aligned size file
            let non_aligned = (BUFFER_SIZE * INIT_BUFFERS) + 1;
            std::fs::write(&cfg.path, vec![0u8; non_aligned]).unwrap();

            let err = File::open_or_create(cfg).unwrap_err();
            assert_eq!(err.reason, err::CPT.reason);
        }

        #[test]
        fn err_open_or_create_when_locked() {
            let (_dir, cfg) = tmp_path();
            let file = File::open_or_create(cfg.clone()).unwrap();

            // Opening another instance while locked fails with LCK
            let err = File::open_or_create(cfg).unwrap_err();
            assert_eq!(err.reason, err::LCK.reason);
            drop(file);
        }
    }

    mod file_toctou_and_concurrency {
        use super::*;

        #[test]
        fn err_concurrent_new_exact_same_path() {
            let (_dir, cfg) = tmp_path();
            let barrier = Arc::new(Barrier::new(2));

            let t1 = {
                let cfg = cfg.clone();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    File::new(cfg)
                })
            };

            let t2 = {
                let cfg = cfg;
                let b = barrier;
                std::thread::spawn(move || {
                    b.wait();
                    File::new(cfg)
                })
            };

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();

            // Due to atomic O_CREAT | O_EXCL, exactly one MUST succeed and the other MUST fail with EXS
            match (r1, r2) {
                (Ok(f), Err(e)) | (Err(e), Ok(f)) => {
                    assert_eq!(e.reason, err::EXS.reason);
                    assert_eq!(f.length(), BUFFER_SIZE * INIT_BUFFERS);
                }
                (Ok(_), Ok(_)) => {
                    panic!("TOCTOU violation: both concurrent File::new succeeded on same path!");
                }
                (Err(e1), Err(e2)) => {
                    panic!("Unexpected failure of both threads: {:?}, {:?}", e1, e2);
                }
            }
        }

        #[test]
        fn err_concurrent_new_and_open() {
            let (_dir, cfg) = tmp_path();
            let barrier = Arc::new(Barrier::new(2));

            let t_new = {
                let cfg = cfg.clone();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    File::new(cfg)
                })
            };

            let t_open = {
                let cfg = cfg;
                let b = barrier;
                std::thread::spawn(move || {
                    b.wait();
                    File::open(cfg)
                })
            };

            let r_new = t_new.join().unwrap();
            let r_open = t_open.join().unwrap();

            // File::new must always succeed
            let _file = r_new.expect("File::new should succeed");

            // File::open either ran before creation (INV) or while lock was held (LCK)
            if let Err(e) = r_open {
                assert!(
                    e.reason == err::INV.reason || e.reason == err::LCK.reason,
                    "unexpected error: {:?}",
                    e
                );
            }
        }

        #[test]
        fn err_concurrent_open_same_file() {
            let (_dir, cfg) = tmp_path();

            // First create the valid file
            {
                let file = File::new(cfg.clone()).unwrap();
                assert_eq!(file.length(), BUFFER_SIZE * INIT_BUFFERS);
            }

            let barrier = Arc::new(Barrier::new(2));

            let t1 = {
                let cfg = cfg.clone();
                let b = barrier.clone();
                std::thread::spawn(move || {
                    b.wait();
                    File::open(cfg)
                })
            };

            let t2 = {
                let cfg = cfg;
                let b = barrier;
                std::thread::spawn(move || {
                    b.wait();
                    File::open(cfg)
                })
            };

            let r1 = t1.join().unwrap();
            let r2 = t2.join().unwrap();

            // Exactly one must acquire exclusive flock and succeed, the other fails with LCK
            match (r1, r2) {
                (Ok(_), Err(e)) | (Err(e), Ok(_)) => {
                    assert_eq!(e.reason, err::LCK.reason, "error must be LCK: {:?}", e);
                }
                (Ok(_), Ok(_)) => {
                    panic!(
                        "Locking violation: both concurrent File::open acquired exclusive lock!"
                    );
                }
                (Err(e1), Err(e2)) => {
                    panic!("Unexpected failure of both threads: {:?}, {:?}", e1, e2);
                }
            }
        }

        #[test]
        fn err_new_while_file_open() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();

            // Calling File::new while open must fail with EXS
            let err = File::new(cfg).unwrap_err();
            assert_eq!(err.reason, err::EXS.reason);

            drop(file);
        }

        #[test]
        fn ok_reopen_after_drop() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();
            drop(file);

            // Once dropped, File::open must succeed
            let reopened = File::open(cfg).unwrap();
            assert_eq!(reopened.length(), BUFFER_SIZE * INIT_BUFFERS);
        }

        #[test]
        fn err_concurrent_open_during_and_after_delete() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();
            file.grow(2).unwrap();

            let running = Arc::new(atomic::AtomicBool::new(true));

            // Thread 2 constantly attempts File::open
            let opener = {
                let cfg = cfg.clone();
                let running = running.clone();
                std::thread::spawn(move || {
                    let mut attempts = 0;
                    while running.load(atomic::Ordering::Relaxed) || attempts < 50 {
                        attempts += 1;
                        let res = File::open(cfg.clone());
                        // While the file is open, open fails with LCK.
                        // On Windows, opening a delete-pending file yields PRM.
                        // Once the file is deleted, open fails with INV.
                        // On POSIX, a descriptor opened right before unlink may momentarily succeed after
                        // deletion releases the writer's lock; if so, dropping it is completely safe.
                        if let Err(err) = res {
                            assert!(
                                err.reason == err::LCK.reason
                                    || err.reason == err::INV.reason
                                    || err.reason == err::PRM.reason,
                                "Unexpected error reason: {:?}",
                                err
                            );
                        }
                        std::thread::yield_now();
                    }
                })
            };

            // Thread 1 deletes the file
            std::thread::sleep(std::time::Duration::from_millis(5));
            file.delete().unwrap();
            running.store(false, atomic::Ordering::Relaxed);

            opener.join().unwrap();

            // After delete, opening must consistently fail with INV
            let err = File::open(cfg).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn ok_concurrent_new_fails_before_delete_and_succeeds_after() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();
            file.grow(2).unwrap();

            let deleted = Arc::new(atomic::AtomicBool::new(false));

            let creator = {
                let cfg = cfg.clone();
                let deleted = deleted.clone();
                std::thread::spawn(move || {
                    let mut created = None;
                    // Before delete occurs, File::new must fail with EXS (or PRM on Windows while delete is pending).
                    // Once delete completes, File::new must eventually succeed.
                    while created.is_none() {
                        match File::new(cfg.clone()) {
                            Ok(f) => {
                                assert!(
                                    deleted.load(atomic::Ordering::Acquire),
                                    "File::new succeeded before file.delete completed!"
                                );
                                created = Some(f);
                            }
                            Err(e) => {
                                assert!(
                                    e.reason == err::EXS.reason || e.reason == err::PRM.reason,
                                    "Unexpected error reason during race: {:?}",
                                    e
                                );
                                std::thread::yield_now();
                            }
                        }
                    }
                    created.unwrap()
                })
            };

            std::thread::sleep(std::time::Duration::from_millis(10));
            deleted.store(true, atomic::Ordering::Release);
            file.delete().unwrap();

            let new_file = creator.join().unwrap();
            assert!(new_file.exists().unwrap());
            new_file.delete().unwrap();
        }

        #[test]
        fn ok_file_send_and_sync() {
            fn assert_send_sync<T: Send + Sync>() {}
            assert_send_sync::<File>();
        }
    }

    mod file_write_read {
        use super::*;

        #[test]
        fn ok_single_buffer_write_read() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();

            let data = [0xABu8; BUFFER_SIZE];
            file.write(&data, 2).unwrap();
            file.sync().unwrap();

            let mut buf = [0u8; BUFFER_SIZE];
            file.read(&mut buf, 2).unwrap();
            assert_eq!(buf, data);
        }

        #[test]
        fn ok_multi_buffer_write_read() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();

            // Span across 2 buffers in a single write and read call
            let multi_buf_data = [0x77u8; BUFFER_SIZE * 2];
            file.write(&multi_buf_data, 1).unwrap();
            file.sync().unwrap();

            let mut read_buf = [0u8; BUFFER_SIZE * 2];
            file.read(&mut read_buf, 1).unwrap();
            assert_eq!(read_buf, multi_buf_data);

            // Verify individual buffers also match
            let mut chunk1 = [0u8; BUFFER_SIZE];
            let mut chunk2 = [0u8; BUFFER_SIZE];
            file.read(&mut chunk1, 1).unwrap();
            file.read(&mut chunk2, 2).unwrap();
            assert_eq!(chunk1, [0x77u8; BUFFER_SIZE]);
            assert_eq!(chunk2, [0x77u8; BUFFER_SIZE]);
        }

        #[test]
        fn ok_empty_buffer_noop() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();

            assert!(file.write(&[], 0).is_ok());
            assert!(file.read(&mut [], 0).is_ok());
        }

        #[test]
        fn err_write_unaligned_buffer_size() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();

            let data = [0u8; BUFFER_SIZE - 1];
            let err = file.write(&data, 0).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);

            let data_multi_unaligned = [0u8; (BUFFER_SIZE * 2) + 1];
            let err = file.write(&data_multi_unaligned, 0).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_read_unaligned_buffer_size() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();

            let mut buf = [0u8; BUFFER_SIZE - 1];
            let err = file.read(&mut buf, 0).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);

            let mut buf_multi_unaligned = [0u8; (BUFFER_SIZE * 2) + 3];
            let err = file.read(&mut buf_multi_unaligned, 0).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_read_past_eof() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();

            let mut buf = [0u8; BUFFER_SIZE];
            let err = file.read(&mut buf, INIT_BUFFERS).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);

            // Multi-buffer read exceeding available chunks
            let mut multi_buf = [0u8; BUFFER_SIZE * 2];
            let err = file.read(&mut multi_buf, INIT_BUFFERS - 1).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
        }

        #[test]
        fn err_write_past_eof() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();

            let data = [1u8; BUFFER_SIZE];
            let err = file.write(&data, INIT_BUFFERS).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);

            // Multi-buffer write exceeding available chunks
            let multi_data = [1u8; BUFFER_SIZE * 2];
            let err = file.write(&multi_data, INIT_BUFFERS - 1).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
        }

        #[test]
        fn ok_concurrent_non_overlapping_writes() {
            let (_dir, mut cfg) = tmp_path();
            cfg.initial_available_buffers = 16;
            let file = Arc::new(File::new(cfg).unwrap());

            let mut handles = vec![];
            for i in 0..4 {
                let f = file.clone();
                handles.push(std::thread::spawn(move || {
                    let data = [i as u8 + 10; BUFFER_SIZE * 2];
                    f.write(&data, i * 2).unwrap();
                }));
            }

            for h in handles {
                h.join().unwrap();
            }

            file.sync().unwrap();

            for i in 0..4 {
                let mut buf = [0u8; BUFFER_SIZE * 2];
                file.read(&mut buf, i * 2).unwrap();
                assert_eq!(buf, [i as u8 + 10; BUFFER_SIZE * 2]);
            }
        }

        #[test]
        fn ok_concurrent_readers() {
            let (_dir, cfg) = tmp_path();
            let file = Arc::new(File::new(cfg).unwrap());

            let data = [0x42u8; BUFFER_SIZE * 2];
            file.write(&data, 0).unwrap();
            file.sync().unwrap();

            let mut handles = vec![];
            for _ in 0..8 {
                let f = file.clone();
                let expected = data;
                handles.push(std::thread::spawn(move || {
                    let mut buf = [0u8; BUFFER_SIZE * 2];
                    f.read(&mut buf, 0).unwrap();
                    assert_eq!(buf, expected);
                }));
            }

            for h in handles {
                h.join().unwrap();
            }
        }
    }

    mod file_grow {
        use super::*;

        #[test]
        fn ok_grow_updates_length() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();
            assert_eq!(file.length(), BUFFER_SIZE * INIT_BUFFERS);
            assert_eq!(file.total_buffers().unwrap(), INIT_BUFFERS);

            file.grow(0x20).unwrap();
            assert_eq!(file.length(), BUFFER_SIZE * (INIT_BUFFERS + 0x20));
            assert_eq!(file.total_buffers().unwrap(), INIT_BUFFERS + 0x20);
        }

        #[test]
        fn ok_grow_zero_count() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();
            file.grow(0).unwrap();
            assert_eq!(file.length(), BUFFER_SIZE * INIT_BUFFERS);
        }

        #[test]
        fn ok_grow_and_multi_buffer_write_read() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();

            file.grow(4).unwrap();
            let new_index = INIT_BUFFERS;

            let data = [0x55u8; BUFFER_SIZE * 3];
            file.write(&data, new_index).unwrap();
            file.sync().unwrap();

            let mut buf = [0u8; BUFFER_SIZE * 3];
            file.read(&mut buf, new_index).unwrap();
            assert_eq!(buf, data);
        }

        #[test]
        fn ok_concurrent_grow_and_write() {
            let (_dir, cfg) = tmp_path();
            let file = Arc::new(File::new(cfg).unwrap());

            let writer = {
                let f = file.clone();
                std::thread::spawn(move || {
                    for i in 0..INIT_BUFFERS {
                        let data = [i as u8; BUFFER_SIZE];
                        f.write(&data, i).unwrap();
                    }
                })
            };

            let chunks_to_grow = 0x20;
            let grower = {
                let f = file.clone();
                std::thread::spawn(move || {
                    f.grow(chunks_to_grow).unwrap();
                })
            };

            writer.join().unwrap();
            grower.join().unwrap();

            file.sync().unwrap();
            assert_eq!(file.length(), BUFFER_SIZE * (INIT_BUFFERS + chunks_to_grow));

            for i in 0..INIT_BUFFERS {
                let mut buf = [0u8; BUFFER_SIZE];
                file.read(&mut buf, i).unwrap();
                assert_eq!(buf, [i as u8; BUFFER_SIZE]);
            }
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn ok_sync_range() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();

            let data = [0x33u8; BUFFER_SIZE * 2];
            file.write(&data, 0).unwrap();
            file.sync_range(0, 2).unwrap();
            file.sync().unwrap();

            let mut buf = [0u8; BUFFER_SIZE * 2];
            file.read(&mut buf, 0).unwrap();
            assert_eq!(buf, data);
        }
    }

    mod file_delete_exists {
        use super::*;

        #[test]
        fn ok_exists_true_on_created_file() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();

            assert!(file.exists().unwrap());
            assert!(cfg.path.exists());
        }

        #[test]
        fn ok_delete_removes_file() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();
            let path = cfg.path.clone();

            assert!(file.exists().unwrap());
            file.delete().unwrap();

            assert!(!path.exists());
            // Re-creating the file at the unlinked path should succeed
            assert!(File::new(cfg).is_ok());
        }

        #[test]
        fn ok_open_and_delete() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();
            drop(file);

            let opened = File::open(cfg.clone()).unwrap();
            assert!(opened.exists().unwrap());
            opened.delete().unwrap();

            assert!(!cfg.path.exists());
        }

        #[test]
        fn err_open_after_delete_grown_file() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();

            // Grow the file
            file.grow(2).unwrap();
            assert_eq!(file.length(), (INIT_BUFFERS + 2) * BUFFER_SIZE);

            // Delete the file
            file.delete().unwrap();
            assert!(!cfg.path.exists());

            // Open must fail with err::INV (file not found)
            let err = File::open(cfg).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        #[cfg(unix)]
        fn ok_exists_false_after_external_removal() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();
            assert!(file.exists().unwrap());

            std::fs::remove_file(&cfg.path).unwrap();
            assert!(!file.exists().unwrap());
        }

        #[test]
        #[cfg(windows)]
        fn ok_exists_false_after_external_removal() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();
            assert!(file.exists().unwrap());
            drop(file);

            std::fs::remove_file(&cfg.path).unwrap();
            assert!(!PlatformFile::exists(&cfg.path).unwrap());
        }

        #[test]
        #[cfg(unix)]
        fn err_delete_when_unlinked_externally() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();

            std::fs::remove_file(&cfg.path).unwrap();
            let err = file.delete().unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        #[cfg(windows)]
        fn err_delete_when_unlinked_externally() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();

            // With FILE_SHARE_DELETE enabled, external deletion is permitted while open.
            // After external deletion, calling file.delete() fails with err::INV or err::PRM (delete-pending).
            std::fs::remove_file(&cfg.path).unwrap();
            let err = file.delete().unwrap_err();
            assert!(
                err.reason == err::INV.reason || err.reason == err::PRM.reason,
                "Unexpected error reason: {:?}",
                err
            );
        }

        #[test]
        #[cfg(unix)]
        fn err_delete_permission_denied() {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let sub_dir = dir.path().join("readonly_dir");
            std::fs::create_dir(&sub_dir).unwrap();

            let path = sub_dir.join("victim.db");
            let cfg = FileCfg {
                module_id: MID,
                path,
                buffer_size: BUFFER_SIZE,
                initial_available_buffers: INIT_BUFFERS,
            };

            let file = File::new(cfg).unwrap();
            std::fs::set_permissions(&sub_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

            let err = file.delete().unwrap_err();
            std::fs::set_permissions(&sub_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

            assert_eq!(err.reason, err::PRM.reason);
        }

        #[test]
        #[cfg(windows)]
        fn err_delete_permission_denied() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();

            let mut perms = std::fs::metadata(&cfg.path).unwrap().permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&cfg.path, perms).unwrap();

            let err = file.delete().unwrap_err();

            let mut perms = std::fs::metadata(&cfg.path).unwrap().permissions();
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(&cfg.path, perms);

            assert_eq!(err.reason, err::PRM.reason);
        }

        #[test]
        fn ok_delete_and_recreate_cycle() {
            let (_dir, cfg) = tmp_path();

            for i in 0..3 {
                let file = File::new(cfg.clone()).unwrap();
                let data = [i as u8; BUFFER_SIZE];
                file.write(&data, 0).unwrap();
                assert!(file.exists().unwrap());

                file.delete().unwrap();
                assert!(!cfg.path.exists());
            }
        }
    }

    mod file_drop {
        use super::*;

        #[test]
        fn ok_drop_persists_written_data() {
            let (_dir, cfg) = tmp_path();
            let data = [0x7Au8; BUFFER_SIZE * 2];

            {
                let file = File::new(cfg.clone()).unwrap();
                file.write(&data, 0).unwrap();
                // drop file without explicit file.sync()
                drop(file);
            }

            {
                let opened = File::open(cfg).unwrap();
                let mut buf = [0u8; BUFFER_SIZE * 2];
                opened.read(&mut buf, 0).unwrap();
                assert_eq!(buf, data);
            }
        }

        #[test]
        fn ok_drop_releases_exclusive_lock() {
            let (_dir, cfg) = tmp_path();

            let file = File::new(cfg.clone()).unwrap();
            // Opening another instance while locked fails with LCK
            let err = File::open(cfg.clone()).unwrap_err();
            assert_eq!(err.reason, err::LCK.reason);

            drop(file);

            // Once dropped, the exclusive lock is released and open succeeds
            let opened = File::open(cfg);
            assert!(opened.is_ok());
        }
    }
}
