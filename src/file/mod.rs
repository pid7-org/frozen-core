//!

use crate::error::{ErrCode, FrozenError, FrozenResult};
use std::sync::atomic;

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
    current_length: atomic::AtomicUsize,

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

        Ok(Self { cfg, file, current_length: atomic::AtomicUsize::new(curr_len) })
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

        if offset.checked_add(buf.len()).map_or(true, |end| end > self.length()) {
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

        if offset.checked_add(buf.len()).map_or(true, |end| end > self.length()) {
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

    /// Get file descriptor for [`File`]
    #[inline]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn fd(&self) -> FileId {
        self.file.fd()
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
            assert_ne!(file.fd(), posix::CLOSED_FD);
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
        fn ok_file_send_and_sync() {
            fn assert_send_sync<T: Send + Sync>() {}
            assert_send_sync::<File>();
        }
    }
}
