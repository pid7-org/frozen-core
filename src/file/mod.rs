//! Custom implementation of `std::fs::File` with fixed-size buffer addressing, background synchronization,
//! and epoch-based durability tracking
//!
//! ## Example
//!
//! ```
//! use frozen_core::file::{File, FileCfg};
//!
//! const MID: u8 = 0;
//!
//! let dir = tempfile::tempdir().unwrap();
//! let path = dir.path().join("tmp_file");
//!
//! let cfg = FileCfg {
//!     module_id: MID,
//!     buffer_size: 0x10,
//!     path: path.clone(),
//!     initial_available_buffers: 4,
//!     sync_interval: None,
//! };
//!
//! let file = File::new(cfg.clone()).unwrap();
//! assert_eq!(file.length(), 0x10 * 4);
//!
//! let data = [1u8; 0x10];
//! let ticket = file.write(&data, 0).unwrap();
//! file.sync().unwrap();
//! assert!(ticket.is_durable());
//!
//! let mut buf = [0u8; 0x10];
//! file.read(&mut buf, 0).unwrap();
//! assert_eq!(buf, data);
//!
//! assert!(File::new(cfg.clone()).is_err());
//!
//! assert!(file.delete().is_ok());
//! assert!(!path.exists());
//! ```

// TODO: Tackle durability verification for uncommitted/non-durable reads internally in the future
// TODO: Tackle coarse-grained RwLock write lock in sync_internal which blocks concurrent pread and pwrite ops during disk flushes

mod interface;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod posix;

#[cfg(target_os = "windows")]
mod windows;

use crate::{
    ack::{AckTicket, Completion, SyncTrigger, TEpoch},
    error::{BindModule, ErrCode, FrozenError, FrozenResult},
};
use interface::FileInterface;
use std::sync::{Arc, Condvar, Mutex, RwLock, atomic};

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(in crate::file) type PlatformFile = posix::POSIXFile;

#[cfg(target_os = "windows")]
pub(in crate::file) type PlatformFile = windows::WINFile;

/// Error codes for [`File`] module
pub(in crate::file) mod err {
    use super::{ErrCode, FrozenError, FrozenResult};

    /// Domain Id for [`File`] is **8**
    const ERRDOMAIN: u8 = 0x08;

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

    #[inline]
    pub(in crate::file) fn raw_error<R, E: std::fmt::Display>(
        code: ErrCode,
        error: E,
    ) -> FrozenResult<R> {
        let err = FrozenError::unbound_raw(ERRDOMAIN, code, error);
        Err(err)
    }

    #[inline]
    pub(in crate::file) fn default_error<R>(code: ErrCode) -> FrozenResult<R> {
        let err = FrozenError::unbound(ERRDOMAIN, code, "");
        Err(err)
    }

    #[inline]
    pub(in crate::file) fn make_error(code: ErrCode) -> FrozenError {
        FrozenError::unbound(ERRDOMAIN, code, "")
    }

    #[inline]
    pub(in crate::file) fn make_raw_error<E: std::fmt::Display>(
        code: ErrCode,
        error: E,
    ) -> FrozenError {
        FrozenError::unbound_raw(ERRDOMAIN, code, error)
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

/// Configurations for [`File`]
///
/// ## Example
///
/// ```
/// use frozen_core::file::FileCfg;
/// use std::time::Duration;
///
/// let cfg = FileCfg {
///     module_id: 1,
///     path: std::path::PathBuf::from("test.db"),
///     buffer_size: 0x100,
///     initial_available_buffers: 0x10,
///     sync_interval: Some(Duration::from_millis(0x64)),
/// };
///
/// assert_eq!(cfg.buffer_size, 0x100);
/// assert_eq!(cfg.initial_available_buffers, 0x10);
/// assert_eq!(cfg.sync_interval, Some(Duration::from_millis(0x64)));
/// ```
#[derive(Debug, Clone)]
pub struct FileCfg {
    /// Identifier used while error propagation
    pub module_id: u8,

    /// Absolute path for/of the file
    ///
    /// *NOTE:* The caller must make sure that the path represents a file and all the parent directories included
    /// in the path do exists
    pub path: std::path::PathBuf,

    /// Size (in bytes) of a single chunk in file
    ///
    /// A chunk is a small fixed size allocation and addressing unit used by [`File`] for all the write/read ops
    ///
    /// These ops are operated by index of the chunk and not the offset of the byte
    ///
    /// *NOTE:* Chunk size when power of 2, is cache efficient and good for performance
    pub buffer_size: usize,

    /// Number of chunks to pre-allocate on fs when [`File`] is initialized
    ///
    /// Initial file length will be `buffer_size * initial_available_buffers` (bytes)
    pub initial_available_buffers: usize,

    /// Optional interval for the background sync thread
    ///
    /// If `Some(duration)`, a background worker thread is spawned which flushes dirty pages at this interval; and
    /// if `None`, background sync is disabled and durability advances via manual sync or forced tickets
    pub sync_interval: Option<std::time::Duration>,
}

#[derive(Debug)]
struct FileInner {
    cfg: FileCfg,
    file: RwLock<Option<PlatformFile>>,
    current_length: atomic::AtomicUsize,
    completion: Arc<Completion>,
    sync_condvar: Condvar,
    sync_mutex: Mutex<bool>,
    shutdown: atomic::AtomicBool,
}

impl SyncTrigger for FileInner {
    fn trigger_sync(&self) -> FrozenResult<()> {
        if self.cfg.sync_interval.is_some() {
            {
                let mut guard = self.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());
                *guard = true;
            }

            self.sync_condvar.notify_one();
            Ok(())
        } else {
            self.sync_internal()
        }
    }
}

impl FileInner {
    fn sync_internal(&self) -> FrozenResult<()> {
        let guard = self.file.write().unwrap_or_else(|e| e.into_inner());
        let file = match guard.as_ref() {
            Some(f) => f,
            None => return Ok(()),
        };

        let target_epoch = self.completion.read_current_epoch();
        let durable_epoch = self.completion.read_durable_epoch();

        if target_epoch == durable_epoch {
            return Ok(());
        }

        match file.sync() {
            Ok(()) => {
                self.completion.mark_epoch_as_durable(target_epoch);
                self.completion.del_err();
                self.completion.notify_all_listeners();

                Ok(())
            }
            Err(e) => {
                let e = e.with_module(self.cfg.module_id);
                self.completion.set_err(e.clone());
                self.completion.notify_all_listeners();

                Err(e)
            }
        }
    }
}

/// Custom implementation of `std::fs::File`
#[derive(Debug)]
pub struct File {
    cfg: FileCfg,
    inner: Arc<FileInner>,
    bg_thread: Option<std::thread::JoinHandle<()>>,
}

unsafe impl Send for File {}
unsafe impl Sync for File {}

impl File {
    fn from_platform_file(
        cfg: FileCfg,
        file: PlatformFile,
        current_length: usize,
    ) -> FrozenResult<Self> {
        let completion = Arc::new(Completion::default());
        let sync_interval = cfg.sync_interval;

        let inner = Arc::new(FileInner {
            cfg: cfg.clone(),
            file: RwLock::new(Some(file)),
            current_length: atomic::AtomicUsize::new(current_length),
            completion: completion.clone(),
            sync_condvar: Condvar::new(),
            sync_mutex: Mutex::new(false),
            shutdown: atomic::AtomicBool::new(false),
        });

        let weak_inner: std::sync::Weak<dyn SyncTrigger> =
            Arc::downgrade(&inner) as std::sync::Weak<dyn SyncTrigger>;
        completion.set_sync_trigger(weak_inner);

        let bg_thread = if let Some(interval) = sync_interval {
            let worker_inner = Arc::clone(&inner);
            let handle =
                std::thread::Builder::new().name("frozen-sync-worker".into()).spawn(move || {
                    let mut guard =
                        worker_inner.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());

                    while !worker_inner.shutdown.load(atomic::Ordering::Acquire) {
                        if *guard {
                            *guard = false;
                            drop(guard);

                            let _ = worker_inner.sync_internal();
                            guard =
                                worker_inner.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());

                            continue;
                        }

                        let (new_guard, _) = worker_inner
                            .sync_condvar
                            .wait_timeout(guard, interval)
                            .unwrap_or_else(|e| e.into_inner());

                        guard = new_guard;
                        if worker_inner.shutdown.load(atomic::Ordering::Acquire) {
                            break;
                        }

                        *guard = false;
                        drop(guard);

                        let _ = worker_inner.sync_internal();
                        guard = worker_inner.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());
                    }
                });

            let handle = match handle {
                Ok(h) => h,
                Err(spawn_err) => {
                    let mut file_guard = inner.file.write().unwrap_or_else(|e| e.into_inner());
                    if let Some(f) = file_guard.take() {
                        let mut err = err::make_raw_error(err::HCF, spawn_err);
                        if let Err(close_err) = f.close() {
                            err.add_suppressed(close_err);
                        }

                        return Err(err.with_module(cfg.module_id));
                    }

                    return err::raw_error(err::HCF, spawn_err).with_module(cfg.module_id);
                }
            };

            Some(handle)
        } else {
            None
        };

        Ok(Self { cfg, inner, bg_thread })
    }

    /// Creates and pre-allocates a new [`File`] at `cfg.path`
    ///
    /// ## TOCTAU Safe
    ///
    /// If the file already exists, an error w/ `err::EXS` is returned to guard against TOCTOU overwrites
    ///
    /// ## Exclusive Lock
    ///
    /// Acquires an exclusive advisory lock via `flock(LOCK_EX | LOCK_NB)` immediately after descriptor creation
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("new_file.bin"),
    ///     buffer_size: 64,
    ///     initial_available_buffers: 4,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg.clone()).unwrap();
    /// assert_eq!(file.length(), 256);
    ///
    /// // Creating another file at the same path fails
    /// assert!(File::new(cfg).is_err());
    /// ```
    pub fn new(cfg: FileCfg) -> FrozenResult<Self> {
        let mid = cfg.module_id;
        if cfg.buffer_size == 0 || cfg.initial_available_buffers == 0 {
            return err::default_error(err::INV).with_module(mid);
        }

        let init_len = match cfg.buffer_size.checked_mul(cfg.initial_available_buffers) {
            Some(len) => len,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let file = PlatformFile::create(&cfg.path).with_module(mid)?;
        if let Err(mut e) = file.grow(0, init_len) {
            if let Err(unlink_err) = file.unlink(&cfg.path) {
                e.add_suppressed(unlink_err);
            }

            return Err(e.with_module(mid));
        }

        if let Err(mut e) = file.sync() {
            if let Err(unlink_err) = file.unlink(&cfg.path) {
                e.add_suppressed(unlink_err);
            }

            return Err(e.with_module(mid));
        }

        Self::from_platform_file(cfg, file, init_len)
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
    /// If any invariant is violated, the file is closed and `err::CPT` is returned
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("open_file.bin"),
    ///     buffer_size: 64,
    ///     initial_available_buffers: 4,
    ///     sync_interval: None,
    /// };
    ///
    /// {
    ///     let file = File::new(cfg.clone()).unwrap();
    ///     assert_eq!(file.length(), 256);
    /// }
    ///
    /// let opened = File::open(cfg).unwrap();
    /// assert_eq!(opened.length(), 256);
    /// ```
    pub fn open(cfg: FileCfg) -> FrozenResult<Self> {
        let mid = cfg.module_id;

        if cfg.buffer_size == 0 || cfg.initial_available_buffers == 0 {
            return err::default_error(err::INV).with_module(mid);
        }

        let init_len = match cfg.buffer_size.checked_mul(cfg.initial_available_buffers) {
            Some(len) => len,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let file = PlatformFile::open(&cfg.path).with_module(mid)?;

        if let Err(mut e) = file.flock() {
            if let Err(close_err) = file.close() {
                e.add_suppressed(close_err);
            }

            return Err(e.with_module(mid));
        }

        let curr_len = match file.length() {
            Ok(len) => len,
            Err(mut e) => {
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }

                return Err(e.with_module(mid));
            }
        };

        if curr_len < init_len || curr_len % cfg.buffer_size != 0 {
            let mut e = err::make_error(err::CPT);
            if let Err(close_err) = file.close() {
                e.add_suppressed(close_err);
            }

            return Err(e.with_module(mid));
        }

        Self::from_platform_file(cfg, file, curr_len)
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
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("idempotent.bin"),
    ///     buffer_size: 64,
    ///     initial_available_buffers: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// // First invocation creates and initializes the file
    /// {
    ///     let file = File::open_or_create(cfg.clone()).unwrap();
    ///     assert_eq!(file.length(), 128);
    /// }
    ///
    /// // Subsequent invocation opens the existing file safely
    /// let reopened = File::open_or_create(cfg).unwrap();
    /// assert_eq!(reopened.length(), 128);
    /// ```
    pub fn open_or_create(cfg: FileCfg) -> FrozenResult<Self> {
        let mid = cfg.module_id;

        if cfg.buffer_size == 0 || cfg.initial_available_buffers == 0 {
            return err::default_error(err::INV).with_module(mid);
        }

        let init_len = match cfg.buffer_size.checked_mul(cfg.initial_available_buffers) {
            Some(len) => len,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let file = PlatformFile::new(&cfg.path).with_module(mid)?;
        if let Err(mut e) = file.flock() {
            if let Err(close_err) = file.close() {
                e.add_suppressed(close_err);
            }

            return Err(e.with_module(mid));
        }

        let curr_len = match file.length() {
            Ok(len) => len,
            Err(mut e) => {
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }

                return Err(e.with_module(mid));
            }
        };

        if curr_len == 0 {
            if let Err(mut e) = file.grow(0, init_len) {
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }

                return Err(e.with_module(mid));
            }

            if let Err(mut e) = file.sync() {
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }

                return Err(e.with_module(mid));
            }

            Self::from_platform_file(cfg, file, init_len)
        } else {
            if curr_len < init_len || curr_len % cfg.buffer_size != 0 {
                let mut e = err::make_error(err::CPT);
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }

                return Err(e.with_module(mid));
            }

            Self::from_platform_file(cfg, file, curr_len)
        }
    }

    /// Read bytes starting from buffer `index` into `buf` w/ `pread` syscall
    ///
    /// ## Durability Guarantee & Caller Responsibility
    ///
    /// It is 100% the caller's responsibility to ensure that they do not read newly written data unless the
    /// older data has been confirmed durable (e.g. by checking or waiting on the write's [`AckTicket`])
    ///
    /// ## Multiple Buffers
    ///
    /// The input `buf` can span across a single or multiple buffers in memory
    ///
    /// ## Constraints
    ///
    /// - `buf.len()` must be a non-zero multiple of `cfg.buffer_size`
    /// - Reading beyond current file length will return `err::HCF`
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("read_test.bin"),
    ///     buffer_size: 16,
    ///     initial_available_buffers: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// let data = [7u8; 16];
    /// let ticket = file.write(&data, 0).unwrap();
    /// ticket.force().unwrap();
    ///
    /// let mut out = [0u8; 16];
    /// file.read(&mut out, 0).unwrap();
    /// assert_eq!(out, data);
    /// ```
    #[inline(always)]
    pub fn read(&self, buf: &mut [u8], index: usize) -> FrozenResult<()> {
        let mid = self.cfg.module_id;
        if buf.is_empty() {
            return Ok(());
        }

        if buf.len() % self.cfg.buffer_size != 0 {
            return err::default_error(err::INV).with_module(mid);
        }

        let offset = match index.checked_mul(self.cfg.buffer_size) {
            Some(off) => off,
            None => return err::default_error(err::INV).with_module(mid),
        };

        if offset.checked_add(buf.len()).is_none_or(|end| end > self.length()) {
            return err::default_error(err::HCF).with_module(mid);
        }

        let guard = self.inner.file.read().unwrap_or_else(|e| e.into_inner());
        let file = match guard.as_ref() {
            Some(f) => f,
            None => return err::default_error(err::INV).with_module(mid),
        };

        file.pread(buf, offset).with_module(mid)
    }

    /// Write bytes starting at buffer `index` from `buf` w/ `pwrite` syscall
    ///
    /// Returns an [`AckTicket`] representing the durability acknowledgement of this write operation
    ///
    /// ## Multiple Buffers
    ///
    /// The input `buf` can span across a single or multiple buffers in memory
    ///
    /// ## Constraints
    ///
    /// - `buf.len()` must be a non-zero multiple of `cfg.buffer_size`
    /// - Writing beyond current file length will return `err::HCF`
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("write_test.bin"),
    ///     buffer_size: 16,
    ///     initial_available_buffers: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// let data = [42u8; 16];
    /// let ticket = file.write(&data, 1).unwrap();
    /// assert_eq!(ticket.is_durable(), false);
    ///
    /// file.sync().unwrap();
    /// assert!(ticket.is_durable());
    /// ```
    #[inline(always)]
    pub fn write(&self, buf: &[u8], index: usize) -> FrozenResult<AckTicket> {
        let mid = self.cfg.module_id;
        if buf.is_empty() {
            let current = self.inner.completion.read_current_epoch();
            return Ok(AckTicket::new(current, self.inner.completion.clone()));
        }

        if buf.len() % self.cfg.buffer_size != 0 {
            return err::default_error(err::INV).with_module(mid);
        }

        let offset = match index.checked_mul(self.cfg.buffer_size) {
            Some(off) => off,
            None => return err::default_error(err::INV).with_module(mid),
        };

        if offset.checked_add(buf.len()).is_none_or(|end| end > self.length()) {
            return err::default_error(err::HCF).with_module(mid);
        }

        let guard = self.inner.file.read().unwrap_or_else(|e| e.into_inner());
        let file = match guard.as_ref() {
            Some(f) => f,
            None => return err::default_error(err::INV).with_module(mid),
        };

        file.pwrite(buf, offset).with_module(mid)?;

        let epoch = self.inner.completion.increment_current_epoch();
        Ok(AckTicket::new(epoch, self.inner.completion.clone()))
    }

    /// Grow file size of [`File`] by given `count` of buffers
    ///
    /// After successful execution, updated file length will be `current_length + (count * buffer_size)`
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("grow_test.bin"),
    ///     buffer_size: 32,
    ///     initial_available_buffers: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// assert_eq!(file.length(), 64);
    ///
    /// file.grow(3).unwrap();
    /// assert_eq!(file.length(), 160);
    /// assert_eq!(file.total_buffers().unwrap(), 5);
    /// ```
    pub fn grow(&self, count: usize) -> FrozenResult<()> {
        let mid = self.cfg.module_id;
        if count == 0 {
            return Ok(());
        }

        let len_to_add = match self.cfg.buffer_size.checked_mul(count) {
            Some(len) => len,
            None => return err::default_error(err::GRW).with_module(mid),
        };

        let guard = self.inner.file.write().unwrap_or_else(|e| e.into_inner());
        let file = match guard.as_ref() {
            Some(f) => f,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let curr_len = self.inner.current_length.load(atomic::Ordering::Acquire);
        file.grow(curr_len, len_to_add).with_module(mid)?;

        self.inner.current_length.fetch_add(len_to_add, atomic::Ordering::Release);
        self.inner.completion.increment_current_epoch();

        Ok(())
    }

    /// Syncs in-mem data to the storage device
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("sync_test.bin"),
    ///     buffer_size: 16,
    ///     initial_available_buffers: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// let ticket = file.write(&[9u8; 16], 0).unwrap();
    /// assert!(!ticket.is_durable());
    ///
    /// file.sync().unwrap();
    /// assert!(ticket.is_durable());
    /// ```
    #[inline]
    pub fn sync(&self) -> FrozenResult<()> {
        self.inner.sync_internal()
    }

    /// Best-effort call to prompt kernel to start flushing dirty pages in the specified chunk range
    ///
    /// If `count == 0`, this call is a no-op and returns `Ok(())`
    ///
    /// ## Example
    ///
    /// ```
    /// # #[cfg(target_os = "linux")]
    /// # {
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("sync_range.bin"),
    ///     buffer_size: 64,
    ///     initial_available_buffers: 4,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// file.write(&[1u8; 64], 0).unwrap();
    /// assert!(file.sync_range(0, 1).is_ok());
    /// assert!(file.sync_range(0, 0).is_ok());
    /// # }
    /// ```
    #[cfg(target_os = "linux")]
    pub fn sync_range(&self, index: usize, count: usize) -> FrozenResult<()> {
        if count == 0 {
            return Ok(());
        }

        let mid = self.cfg.module_id;
        let offset = match index.checked_mul(self.cfg.buffer_size) {
            Some(off) => off,
            None => return err::default_error(err::INV).with_module(mid),
        };
        let len_to_sync = match count.checked_mul(self.cfg.buffer_size) {
            Some(len) => len,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let guard = self.inner.file.read().unwrap_or_else(|e| e.into_inner());
        let file = match guard.as_ref() {
            Some(f) => f,
            None => return err::default_error(err::INV).with_module(mid),
        };

        file.sync_range(offset, len_to_sync).with_module(mid)
    }

    /// Fetch total available buffers in [`File`]
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("buffers.bin"),
    ///     buffer_size: 32,
    ///     initial_available_buffers: 8,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// assert_eq!(file.total_buffers().unwrap(), 8);
    /// ```
    #[inline]
    pub fn total_buffers(&self) -> FrozenResult<usize> {
        let curr_len = self.length();
        let buffer_size = self.cfg.buffer_size;

        if crate::hints::unlikely(curr_len % buffer_size != 0) {
            return err::default_error(err::CPT).with_module(self.cfg.module_id);
        }

        Ok(curr_len / buffer_size)
    }

    /// Get reference to configuration of [`File`]
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("cfg.bin"),
    ///     buffer_size: 64,
    ///     initial_available_buffers: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// assert_eq!(file.cfg().buffer_size, 64);
    /// assert_eq!(file.cfg().initial_available_buffers, 2);
    /// ```
    #[inline]
    pub fn cfg(&self) -> &FileCfg {
        &self.cfg
    }

    /// Read current length (in bytes) of [`File`]
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("len.bin"),
    ///     buffer_size: 128,
    ///     initial_available_buffers: 4,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// assert_eq!(file.length(), 512);
    /// ```
    #[inline]
    pub fn length(&self) -> usize {
        self.inner.current_length.load(atomic::Ordering::Acquire)
    }

    /// Fetch the latest assigned durability epoch
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("epoch.bin"),
    ///     buffer_size: 16,
    ///     initial_available_buffers: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// assert_eq!(file.current_epoch(), 0);
    ///
    /// file.write(&[1u8; 16], 0).unwrap();
    /// assert_eq!(file.current_epoch(), 1);
    /// ```
    #[inline]
    pub fn current_epoch(&self) -> TEpoch {
        self.inner.completion.read_current_epoch()
    }

    /// Fetch the latest durable epoch
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("durable_epoch.bin"),
    ///     buffer_size: 16,
    ///     initial_available_buffers: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// file.write(&[1u8; 16], 0).unwrap();
    /// assert_eq!(file.durable_epoch(), 0);
    ///
    /// file.sync().unwrap();
    /// assert_eq!(file.durable_epoch(), 1);
    /// ```
    #[inline]
    pub fn durable_epoch(&self) -> TEpoch {
        self.inner.completion.read_durable_epoch()
    }

    /// Fetch reference to underlying durability [`Completion`]
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("completion.bin"),
    ///     buffer_size: 16,
    ///     initial_available_buffers: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// let completion = file.completion();
    /// assert_eq!(completion.read_current_epoch(), 0);
    /// ```
    #[inline]
    pub fn completion(&self) -> &Arc<Completion> {
        &self.inner.completion
    }

    /// Check if [`File`] exists on storage device or not
    ///
    /// ## Access Semantics
    ///
    /// Uses `access(path, F_OK)` to verify the existence of the file
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("exists.bin"),
    ///     buffer_size: 16,
    ///     initial_available_buffers: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// assert!(file.exists().unwrap());
    /// ```
    #[inline]
    pub fn exists(&self) -> FrozenResult<bool> {
        PlatformFile::exists(&self.cfg.path).with_module(self.cfg.module_id)
    }

    /// Deletes the [`File`] entry from the storage device
    ///
    /// Consumes `self` by value to prevent any concurrent or post-deletion operations
    ///
    /// Unlinks the file at `path`, closes the underlying descriptor, and syncs the parent directory to
    /// guarantee crash-safe durability
    ///
    /// ## Caller Responsibilities
    ///
    /// The caller must ensure that when calling `delete`, no write operations are pending for durability
    /// and no write operations are invoked during or after `delete` is called
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let path = dir.path().join("delete_test.bin");
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: path.clone(),
    ///     buffer_size: 16,
    ///     initial_available_buffers: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// assert!(path.exists());
    ///
    /// file.delete().unwrap();
    /// assert!(!path.exists());
    /// ```
    pub fn delete(mut self) -> FrozenResult<()> {
        let mid = self.cfg.module_id;
        self.inner.shutdown.store(true, atomic::Ordering::Release);
        {
            let mut guard = self.inner.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());
            *guard = true;
        }
        self.inner.sync_condvar.notify_all();

        if let Some(handle) = self.bg_thread.take() {
            let _ = handle.join();
        }

        let mut guard = self.inner.file.write().unwrap_or_else(|e| e.into_inner());
        let file = match guard.take() {
            Some(f) => f,
            None => return err::default_error(err::INV).with_module(mid),
        };

        file.unlink(&self.cfg.path).with_module(mid)
    }

    /// Get file descriptor or handle for [`File`]
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::file::{File, FileCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = FileCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("fd_test.bin"),
    ///     buffer_size: 16,
    ///     initial_available_buffers: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// let file = File::new(cfg).unwrap();
    /// #[cfg(unix)]
    /// assert!(file.fd() >= 0);
    /// #[cfg(windows)]
    /// assert_ne!(file.fd(), -1isize);
    /// ```
    #[inline]
    pub fn fd(&self) -> FileId {
        let guard = self.inner.file.read().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(f) => f.fd(),
            None => PlatformFile::CLOSED_ID,
        }
    }
}

impl Drop for File {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, atomic::Ordering::Release);
        {
            let mut guard = self.inner.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());
            *guard = true;
        }
        self.inner.sync_condvar.notify_all();

        if let Some(handle) = self.bg_thread.take() {
            let _ = handle.join();
        }

        let is_closed = {
            let guard = self.inner.file.read().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(f) => f.is_closed(),
                None => true,
            }
        };

        if is_closed {
            return;
        }

        let _ = self.inner.sync_internal();
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
            sync_interval: None,
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
        fn err_new_cfg_overflow() {
            let (_dir, mut cfg) = tmp_path();
            cfg.buffer_size = usize::MAX;
            cfg.initial_available_buffers = 2;
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

        #[test]
        fn err_new_cleans_up_on_grow_failure() {
            let (_dir, mut cfg) = tmp_path();
            // On 64-bit systems, setting buffer_size such that buffer_size * initial_buffers exceeds
            // off_t::MAX / i64::MAX triggers grow failure after successful creation
            cfg.buffer_size = 1 << 63;
            cfg.initial_available_buffers = 1;

            let err = File::new(cfg.clone()).unwrap_err();
            assert_eq!(err.reason, err::GRW.reason);

            // File must be unlinked and not left as an orphan on disk
            assert!(!cfg.path.exists());

            // A subsequent File::new with valid config must succeed and not fail with EXS
            cfg.buffer_size = BUFFER_SIZE;
            cfg.initial_available_buffers = INIT_BUFFERS;
            let file = File::new(cfg);
            assert!(file.is_ok());
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
        fn err_open_cfg_overflow() {
            let (_dir, mut cfg) = tmp_path();
            cfg.buffer_size = usize::MAX;
            cfg.initial_available_buffers = 2;
            let err = File::open(cfg).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_open_when_file_smaller_than_init_len() {
            let (_dir, cfg) = tmp_path();

            std::fs::write(&cfg.path, []).unwrap();

            let err = File::open(cfg).unwrap_err();
            assert_eq!(err.reason, err::CPT.reason);
        }

        #[test]
        fn err_open_when_file_not_buffer_multiple() {
            let (_dir, cfg) = tmp_path();

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
            let file = File::new(cfg.clone()).unwrap();
            let data = [0x5Au8; BUFFER_SIZE];
            file.write(&data, 0).unwrap();
            file.sync().unwrap();
            drop(file);

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
        fn err_open_or_create_cfg_overflow() {
            let (_dir, mut cfg) = tmp_path();
            cfg.buffer_size = usize::MAX;
            cfg.initial_available_buffers = 2;
            let err = File::open_or_create(cfg).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_open_or_create_corrupt_existing() {
            let (_dir, cfg) = tmp_path();
            let non_aligned = (BUFFER_SIZE * INIT_BUFFERS) + 1;
            std::fs::write(&cfg.path, vec![0u8; non_aligned]).unwrap();

            let err = File::open_or_create(cfg).unwrap_err();
            assert_eq!(err.reason, err::CPT.reason);
        }

        #[test]
        fn err_open_or_create_when_locked() {
            let (_dir, cfg) = tmp_path();
            let file = File::open_or_create(cfg.clone()).unwrap();

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

            // NOTE:
            //
            // Due to atomic `O_CREAT | O_EXCL`, exactly one MUST succeed and the other MUST fail with EXS
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

            // NOTE:
            //
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

            let err = File::new(cfg).unwrap_err();
            assert_eq!(err.reason, err::EXS.reason);

            drop(file);
        }

        #[test]
        fn ok_reopen_after_drop() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();
            drop(file);

            let reopened = File::open(cfg).unwrap();
            assert_eq!(reopened.length(), BUFFER_SIZE * INIT_BUFFERS);
        }

        #[test]
        fn err_concurrent_open_during_and_after_delete() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg.clone()).unwrap();
            file.grow(2).unwrap();

            let running = Arc::new(atomic::AtomicBool::new(true));

            // Thread 2: constantly attempts File::open
            let opener = {
                let cfg = cfg.clone();
                let running = running.clone();
                std::thread::spawn(move || {
                    let mut attempts = 0;
                    while running.load(atomic::Ordering::Relaxed) || attempts < 50 {
                        attempts += 1;
                        let res = File::open(cfg.clone());
                        // While the file is open, open fails with LCK
                        // On Windows, opening a delete-pending file yields PRM
                        // Once the file is deleted, open fails with INV
                        //
                        // On POSIX, a descriptor opened right before unlink may momentarily succeed after
                        // deletion releases the writer's lock; if so, dropping it is completely safe
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

            // Thread 1: deletes the file
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
                    // Before delete occurs, File::new must fail with EXS (or PRM on Windows while delete is pending)
                    // Once delete completes, File::new must eventually succeed
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

            let multi_buf_data = [0x77u8; BUFFER_SIZE * 2];
            file.write(&multi_buf_data, 1).unwrap();
            file.sync().unwrap();

            let mut read_buf = [0u8; BUFFER_SIZE * 2];
            file.read(&mut read_buf, 1).unwrap();
            assert_eq!(read_buf, multi_buf_data);

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

        #[test]
        #[cfg(target_os = "linux")]
        fn ok_sync_range_zero_count() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();
            assert!(file.sync_range(0, 0).is_ok());
            assert!(file.sync_range(5, 0).is_ok());
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

            file.grow(2).unwrap();
            assert_eq!(file.length(), (INIT_BUFFERS + 2) * BUFFER_SIZE);

            file.delete().unwrap();
            assert!(!cfg.path.exists());

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

            // NOTE:
            //
            // With `FILE_SHARE_DELETE` enabled, external deletion is permitted while open
            //
            // After external deletion, calling `file.delete()` fails with `err::INV` or `err::PRM` (delete-pending)
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
                sync_interval: None,
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
            let err = File::open(cfg.clone()).unwrap_err();
            assert_eq!(err.reason, err::LCK.reason);

            drop(file);

            let opened = File::open(cfg);
            assert!(opened.is_ok());
        }
    }

    mod file_sync_and_durability {
        use super::*;
        use std::time::{Duration, Instant};

        #[test]
        fn ok_manual_sync_advances_epoch_and_tickets() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();
            let data = [0x55u8; BUFFER_SIZE];

            assert_eq!(file.current_epoch(), 0);
            assert_eq!(file.durable_epoch(), 0);

            let t1 = file.write(&data, 0).unwrap();
            assert_eq!(t1.epoch(), 1);
            assert!(!t1.is_durable());
            assert_eq!(file.current_epoch(), 1);
            assert_eq!(file.durable_epoch(), 0);

            let t2 = file.write(&data, 1).unwrap();
            assert_eq!(t2.epoch(), 2);
            assert!(!t2.is_durable());
            assert_eq!(file.current_epoch(), 2);
            assert_eq!(file.durable_epoch(), 0);

            file.sync().unwrap();
            assert!(t1.is_durable());
            assert!(t2.is_durable());
            assert_eq!(file.durable_epoch(), 2);
        }

        #[test]
        fn ok_ticket_force_without_background_thread() {
            let (_dir, cfg) = tmp_path();
            let file = File::new(cfg).unwrap();
            let data = [0xAAu8; BUFFER_SIZE];

            let ticket = file.write(&data, 0).unwrap();
            assert!(!ticket.is_durable());

            let durable = ticket.force().unwrap();
            assert_eq!(durable, ticket.epoch());
            assert!(ticket.is_durable());
            assert_eq!(file.durable_epoch(), ticket.epoch());
        }

        #[test]
        fn ok_background_sync_advances_epoch() {
            let (_dir, mut cfg) = tmp_path();
            cfg.sync_interval = Some(Duration::from_millis(25));

            let file = File::new(cfg).unwrap();
            let data = [0x33u8; BUFFER_SIZE];

            let ticket = file.write(&data, 0).unwrap();
            let epoch = ticket.wait().unwrap();
            assert_eq!(epoch, ticket.epoch());
            assert!(ticket.is_durable());
            assert!(file.durable_epoch() >= ticket.epoch());
        }

        #[test]
        fn ok_ticket_force_with_background_thread() {
            let (_dir, mut cfg) = tmp_path();
            // Long interval to verify force wakes up immediately
            cfg.sync_interval = Some(Duration::from_secs(10));

            let file = File::new(cfg).unwrap();
            let data = [0x44u8; BUFFER_SIZE];

            let ticket = file.write(&data, 0).unwrap();
            assert!(!ticket.is_durable());

            let start = Instant::now();
            let epoch = ticket.force().unwrap();
            let elapsed = start.elapsed();

            assert_eq!(epoch, ticket.epoch());
            assert!(ticket.is_durable());
            assert!(elapsed < Duration::from_secs(2));
        }

        #[test]
        fn ok_drop_with_background_thread_persists_data() {
            let (_dir, mut cfg) = tmp_path();
            cfg.sync_interval = Some(Duration::from_secs(30));
            let data = [0x88u8; BUFFER_SIZE];

            {
                let file = File::new(cfg.clone()).unwrap();
                let _ticket = file.write(&data, 0).unwrap();
                // Drop without waiting for interval to expire
                drop(file);
            }

            {
                let opened = File::open(cfg).unwrap();
                let mut buf = [0u8; BUFFER_SIZE];
                opened.read(&mut buf, 0).unwrap();
                assert_eq!(buf, data);
            }
        }

        #[test]
        fn ok_delete_with_background_thread() {
            let (_dir, mut cfg) = tmp_path();
            cfg.sync_interval = Some(Duration::from_millis(20));

            let file = File::new(cfg.clone()).unwrap();
            let data = [0x99u8; BUFFER_SIZE];
            let _ = file.write(&data, 0).unwrap();

            assert!(file.exists().unwrap());
            file.delete().unwrap();
            assert!(!cfg.path.exists());
        }

        #[test]
        fn ok_concurrent_writes_distinct_epochs() {
            let (_dir, cfg) = tmp_path();
            let file = Arc::new(File::new(cfg).unwrap());
            let num_threads = 4;
            let mut handles = Vec::new();

            for i in 0..num_threads {
                let f = file.clone();
                handles.push(std::thread::spawn(move || {
                    let data = [i as u8; BUFFER_SIZE];
                    f.write(&data, i).unwrap()
                }));
            }

            let mut epochs = Vec::new();
            for h in handles {
                let ticket = h.join().unwrap();
                epochs.push(ticket.epoch());
            }

            epochs.sort();
            epochs.dedup();
            assert_eq!(epochs.len(), num_threads);
        }
    }

    mod module_binding {
        use super::*;

        #[test]
        fn ok_multiple_files_distinct_module_ids() {
            let dir = tempfile::tempdir().unwrap();
            let cfg1 = FileCfg {
                module_id: 0x11,
                path: dir.path().join("file1.db"),
                buffer_size: BUFFER_SIZE,
                initial_available_buffers: INIT_BUFFERS,
                sync_interval: None,
            };
            let cfg2 = FileCfg {
                module_id: 0x22,
                path: dir.path().join("file2.db"),
                buffer_size: BUFFER_SIZE,
                initial_available_buffers: INIT_BUFFERS,
                sync_interval: None,
            };

            let file1 = File::new(cfg1.clone()).unwrap();
            let file2 = File::new(cfg2.clone()).unwrap();

            // Operations causing errors on file1 must carry module_id 0x11
            let mut invalid_buf = [0u8; BUFFER_SIZE - 1];
            let err1 = file1.read(&mut invalid_buf, 0).unwrap_err();
            assert_eq!(err1.module, 0x11);
            assert_eq!(err1.reason, err::INV.reason);

            let err1_oob = file1.read(&mut [0u8; BUFFER_SIZE], 999).unwrap_err();
            assert_eq!(err1_oob.module, 0x11);
            assert_eq!(err1_oob.reason, err::HCF.reason);

            // Operations causing errors on file2 must carry module_id 0x22
            let err2 = file2.read(&mut invalid_buf, 0).unwrap_err();
            assert_eq!(err2.module, 0x22);
            assert_eq!(err2.reason, err::INV.reason);

            let err2_oob = file2.write(&[0u8; BUFFER_SIZE], 999).unwrap_err();
            assert_eq!(err2_oob.module, 0x22);
            assert_eq!(err2_oob.reason, err::HCF.reason);

            // Existing file error on File::new carries respective module_id
            let err1_exs = File::new(cfg1).unwrap_err();
            assert_eq!(err1_exs.module, 0x11);
            assert_eq!(err1_exs.reason, err::EXS.reason);

            let err2_exs = File::new(cfg2).unwrap_err();
            assert_eq!(err2_exs.module, 0x22);
            assert_eq!(err2_exs.reason, err::EXS.reason);
        }
    }
}
