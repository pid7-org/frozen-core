//! NA

#![allow(unused)]

use crate::{
    ack::{AckTicket, Completion, SyncTrigger, TEpoch},
    error::{BindModule, ErrCode, FrozenError, FrozenResult},
    file::{File, FileCfg},
    hints,
};
use core::marker::PhantomData;
use std::sync::{Arc, Condvar, Mutex, RwLock, atomic};

mod interface;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod posix;

#[cfg(target_os = "windows")]
mod windows;

pub(in crate::memmap) use interface::MemMapInterface;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(in crate::memmap) type PlatformMemMap = posix::POSIXMemMap;

#[cfg(target_os = "windows")]
pub(in crate::memmap) type PlatformMemMap = windows::WINMemMap;

/// Error codes for [`MemMap`] module
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

    /// type `T` is invalid (e.g. `size_of::<T>()` exceeds page size)
    pub const INV: ErrCode =
        ErrCode::new(0x18, "`size_of::<T>()` must not exceed system page size");

    /// parent directory does not exist
    pub const DIR: ErrCode = ErrCode::new(0x1A, "directory does not exist");

    /// invalid path to file
    pub const PTH: ErrCode = ErrCode::new(0x1C, "invalid path to file");

    /// file already exists
    pub const EXS: ErrCode = ErrCode::new(0x1E, "file already exists");

    /// file does not exist
    pub const NEX: ErrCode = ErrCode::new(0x20, "file does not exist");

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

/// Configurations for [`MemMap`]
///
/// ## Example
///
/// ```
/// use frozen_core::memmap::MemMapCfg;
/// use std::time::Duration;
///
/// let cfg = MemMapCfg {
///     module_id: 1,
///     path: std::path::PathBuf::from("test.db"),
///     version: 1,
///     initial_t: 0x10,
///     sync_interval: Duration::from_millis(0x64),
/// };
///
/// assert_eq!(cfg.version, 1);
/// assert_eq!(cfg.initial_t, 0x10);
/// assert_eq!(cfg.sync_interval, Duration::from_millis(0x64));
/// ```
#[derive(Debug, Clone)]
pub struct MemMapCfg {
    /// Identifier used while error propagation
    pub module_id: u8,

    /// Base path for/of the file
    ///
    /// *NOTE:* The caller must make sure that the path represents a file and all the parent directories included
    /// in the path do exist. The actual file created on disk is derived by appending the [`version`](Self::version)
    /// to the base file name (e.g. `data.bin` with version `1` becomes `data_1.bin`).
    pub path: std::path::PathBuf,

    /// Schema or layout version of the underlying memory-mapped file
    ///
    /// Because the binary layout on disk is constructed according to the initial type `T` and structural parameters,
    /// [`MemMap`] strictly requires type invariants across a file's lifetime. When an application evolves its data
    /// structures or migrates `T`, incrementing `version` ensures a new versioned backing file (`<name>_<ver>`) is
    /// created on disk without corrupting or clashing with previously persisted data.
    pub version: u32,

    /// Number of `T` elements to pre-allocate on fs when [`MemMap`] is initialized
    ///
    /// Initial file length will be `core::mem::size_of::<T>() * initial_t` (bytes)
    pub initial_t: usize,

    /// Interval for the background sync thread
    ///
    /// The background worker thread flushes dirty mapped pages and file state to disk at this interval
    pub sync_interval: std::time::Duration,
}

impl MemMapCfg {
    /// Computes the versioned file path by appending `_{version}` to the file stem/name
    #[inline]
    fn versioned_path(&self) -> std::path::PathBuf {
        let parent = self.path.parent();
        let stem = self.path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let extension = self.path.extension().and_then(|e| e.to_str());

        let versioned_filename = match extension {
            Some(ext) if !ext.is_empty() => format!("{}_{}.{}", stem, self.version, ext),
            _ => format!("{}_{}", stem, self.version),
        };

        match parent {
            Some(p) if !p.as_os_str().is_empty() => p.join(versioned_filename),
            _ => std::path::PathBuf::from(versioned_filename),
        }
    }
}

/// Bitmask for exclusive writer flag in [`SlotLocks`]
const WRITER_BIT: u32 = 0x8000_0000;

/// Contention spin thresholds for slot locking backoff
const L1_CONTENTION: usize = 0x10;
const L2_CONTENTION: usize = 0x20;

/// Table of reader-writer spinlocks per slot for memory-mapped items
#[derive(Debug)]
struct SlotLocks {
    locks: Box<[atomic::AtomicU32]>,
}

impl SlotLocks {
    fn new(total_slots: usize) -> Self {
        let mut slots = Vec::with_capacity(total_slots);
        for _ in 0..total_slots {
            slots.push(atomic::AtomicU32::new(0));
        }
        Self { locks: slots.into_boxed_slice() }
    }

    #[inline(always)]
    fn read_lock(&self, index: usize) -> SlotReadGuard<'_> {
        let val = &self.locks[index];
        let mut spins = 0;

        loop {
            let state = val.load(atomic::Ordering::Relaxed);
            if hints::likely(state & WRITER_BIT == 0)
                && val
                    .compare_exchange_weak(
                        state,
                        state + 1,
                        atomic::Ordering::Acquire,
                        atomic::Ordering::Relaxed,
                    )
                    .is_ok()
            {
                return SlotReadGuard(val);
            }

            if hints::likely(spins < L1_CONTENTION) {
                std::hint::spin_loop();
            } else if spins < L2_CONTENTION {
                std::thread::yield_now();
            } else {
                let ns = 0x30 << (spins - L2_CONTENTION).min(0x0A);
                std::thread::sleep(std::time::Duration::from_nanos(ns));
            }

            spins += 1;
        }
    }

    #[inline(always)]
    fn write_lock(&self, index: usize) -> SlotWriteGuard<'_> {
        let val = &self.locks[index];
        let mut spins = 0;

        loop {
            if val
                .compare_exchange_weak(
                    0,
                    WRITER_BIT,
                    atomic::Ordering::Acquire,
                    atomic::Ordering::Relaxed,
                )
                .is_ok()
            {
                return SlotWriteGuard(val);
            }

            if hints::likely(spins < L1_CONTENTION) {
                std::hint::spin_loop();
            } else if spins < L2_CONTENTION {
                std::thread::yield_now();
            } else {
                let ns = 0x30 << (spins - L2_CONTENTION).min(0x0A);
                std::thread::sleep(std::time::Duration::from_nanos(ns));
            }

            spins += 1;
        }
    }
}

/// RAII read guard for a slot lock
struct SlotReadGuard<'a>(&'a atomic::AtomicU32);

impl Drop for SlotReadGuard<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        self.0.fetch_sub(1, atomic::Ordering::Release);
    }
}

/// RAII write guard for a slot lock
struct SlotWriteGuard<'a>(&'a atomic::AtomicU32);

impl Drop for SlotWriteGuard<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        self.0.store(0, atomic::Ordering::Release);
    }
}

/// Internal shared state for [`MemMap`]
#[derive(Debug)]
struct MemMapInner {
    cfg: MemMapCfg,
    mmap: RwLock<Option<PlatformMemMap>>,
    slot_locks: SlotLocks,
    length: usize,
    completion: Arc<Completion>,
    sync_condvar: Condvar,
    sync_mutex: Mutex<bool>,
    shutdown: atomic::AtomicBool,
}

impl SyncTrigger for MemMapInner {
    fn trigger_sync(&self) -> FrozenResult<()> {
        {
            let mut guard = self.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());
            *guard = true;
        }

        self.sync_condvar.notify_one();
        Ok(())
    }
}

impl MemMapInner {
    fn sync_internal(&self) -> FrozenResult<()> {
        let guard = self.mmap.read().unwrap_or_else(|e| e.into_inner());
        let mmap = match guard.as_ref() {
            Some(m) => m,
            None => return Ok(()),
        };

        let target_epoch = self.completion.read_current_epoch();
        let durable_epoch = self.completion.read_durable_epoch();

        if target_epoch == durable_epoch {
            return Ok(());
        }

        // Two-phase flush: flush mapped pages from VM to file writeback, then sync file descriptor for durability
        let sync_res = mmap.sync(self.length);

        match sync_res {
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

/// Custom memory-mapped file abstraction with background synchronization and durability tracking
pub struct MemMap<T: Sized + Clone> {
    cfg: MemMapCfg,
    file: Option<File>,
    inner: Arc<MemMapInner>,
    bg_thread: Option<std::thread::JoinHandle<()>>,
    _marker: PhantomData<T>,
}

impl<T> core::fmt::Debug for MemMap<T>
where
    T: Sized + Clone,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MemMap").field("cfg", &self.cfg).field("inner", &self.inner).finish()
    }
}

unsafe impl<T> Send for MemMap<T> where T: Sized + Clone {}
unsafe impl<T> Sync for MemMap<T> where T: Sized + Clone {}

impl<T> MemMap<T>
where
    T: Sized + Clone,
{
    fn validate_t(mid: u8) -> FrozenResult<()> {
        if core::mem::needs_drop::<T>() {
            return err::default_error(err::DRP).with_module(mid);
        }

        let size = core::mem::size_of::<T>();
        if size == 0 {
            return err::default_error(err::ZRO).with_module(mid);
        }

        if size % 8 != 0 {
            return err::default_error(err::SZE).with_module(mid);
        }

        let align = core::mem::align_of::<T>();
        if align != 8 {
            return err::default_error(err::ALN).with_module(mid);
        }

        let page_size = PlatformMemMap::page_size().with_module(mid)?;
        if size > page_size {
            return err::default_error(err::INV).with_module(mid);
        }

        Ok(())
    }

    fn validate_path(cfg: &MemMapCfg) -> FrozenResult<()> {
        let mid = cfg.module_id;

        if cfg.path.as_os_str().is_empty() {
            return err::default_error(err::PTH).with_module(mid);
        }

        let stem = cfg.path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem.is_empty() {
            return err::default_error(err::PTH).with_module(mid);
        }

        if cfg.path.exists() && cfg.path.is_dir() {
            return err::default_error(err::PTH).with_module(mid);
        }

        if let Some(parent) = cfg.path.parent() {
            if !parent.as_os_str().is_empty() && (!parent.exists() || !parent.is_dir()) {
                return err::default_error(err::DIR).with_module(mid);
            }
        }

        Ok(())
    }

    fn file_cfg(cfg: &MemMapCfg) -> FileCfg {
        FileCfg {
            module_id: cfg.module_id,
            path: cfg.versioned_path(),
            buffer_size: core::mem::size_of::<T>(),
            initial_available_buffers: cfg.initial_t,
            sync_interval: None,
        }
    }

    fn from_components(cfg: MemMapCfg, mmap: PlatformMemMap, file: File) -> FrozenResult<Self> {
        let length = mmap.length();
        let completion = Arc::new(Completion::default());
        let sync_interval = cfg.sync_interval;
        let total_slots = length / core::mem::size_of::<T>();
        let slot_locks = SlotLocks::new(total_slots);

        let inner = Arc::new(MemMapInner {
            cfg: cfg.clone(),
            mmap: RwLock::new(Some(mmap)),
            slot_locks,
            length,
            completion: completion.clone(),
            sync_condvar: Condvar::new(),
            sync_mutex: Mutex::new(false),
            shutdown: atomic::AtomicBool::new(false),
        });

        let weak_inner: std::sync::Weak<dyn SyncTrigger> =
            Arc::downgrade(&inner) as std::sync::Weak<dyn SyncTrigger>;
        completion.set_sync_trigger(weak_inner);

        let worker_inner = Arc::clone(&inner);
        let handle =
            std::thread::Builder::new().name("frozen-mmap-sync-worker".into()).spawn(move || {
                let mut guard = worker_inner.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());

                while !worker_inner.shutdown.load(atomic::Ordering::Acquire) {
                    if *guard {
                        *guard = false;
                        drop(guard);

                        let _ = worker_inner.sync_internal();
                        guard = worker_inner.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());
                        continue;
                    }

                    let (new_guard, _) = worker_inner
                        .sync_condvar
                        .wait_timeout(guard, sync_interval)
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

        let bg_thread = match handle {
            Ok(h) => h,
            Err(spawn_err) => {
                let mut mmap_guard = inner.mmap.write().unwrap_or_else(|e| e.into_inner());
                let mut err = err::make_raw_error(err::FXE, spawn_err);
                if let Some(m) = mmap_guard.take() {
                    if let Err(unmap_err) = m.unmap() {
                        err.add_suppressed(unmap_err);
                    }
                }
                return Err(err.with_module(cfg.module_id));
            }
        };

        Ok(Self { cfg, file: Some(file), inner, bg_thread: Some(bg_thread), _marker: PhantomData })
    }

    /// Creates and memory-maps a new [`File`] at `cfg.versioned_path()`
    ///
    /// *NOTE:* Returns an error if the file already exists on fs, use
    /// [`MemMap::open`] to open an pre-existing file
    ///
    /// ## Type `T`
    ///
    /// The type `T` must never change across the entire lifetime of the file
    ///
    /// In case, `T` needs to be updated, use [`MemMapCfg::version`] to create a
    /// new instace of [`MemMap`] to exists alongside the old one
    ///
    /// ## Kernal Lock
    ///
    /// Acquires an exclusive advisory lock on the underlying descriptor, to
    /// prevent running two simultaneous instances alongside each other by the
    /// same process
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    /// use std::time::Duration;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("new_memmap.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: Duration::from_millis(100),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
    /// assert_eq!(mmap.length(), std::mem::size_of::<u64>() * 4);
    ///
    /// // Creating another file at the same path fails
    /// assert!(MemMap::<u64>::new(cfg).is_err());
    /// ```
    pub fn new(cfg: MemMapCfg) -> FrozenResult<Self> {
        Self::validate_t(cfg.module_id)?;
        Self::validate_path(&cfg)?;

        let vpath = cfg.versioned_path();
        if vpath.exists() {
            return err::default_error(err::EXS).with_module(cfg.module_id);
        }

        let file_cfg = Self::file_cfg(&cfg);
        let file = match File::new(file_cfg) {
            Ok(f) => f,
            Err(e) if e.reason == err::EXS.reason => {
                return err::default_error(err::EXS).with_module(cfg.module_id);
            }
            Err(e) => return Err(e),
        };

        let mmap = match PlatformMemMap::new(file.fd(), file.length()) {
            Ok(m) => m,
            Err(mut e) => {
                if let Err(del_err) = file.delete() {
                    e.add_suppressed(del_err);
                }

                return Err(e.with_module(cfg.module_id));
            }
        };

        Self::from_components(cfg, mmap, file)
    }

    /// NA
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    /// use std::time::Duration;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("open_memmap.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: Duration::from_millis(100),
    /// };
    ///
    /// {
    ///     let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
    ///     assert_eq!(mmap.length(), std::mem::size_of::<u64>() * 4);
    /// }
    ///
    /// let opened = MemMap::<u64>::open(cfg).unwrap();
    /// assert_eq!(opened.length(), std::mem::size_of::<u64>() * 4);
    /// ```
    pub fn open(cfg: MemMapCfg) -> FrozenResult<Self> {
        Self::validate_t(cfg.module_id)?;
        Self::validate_path(&cfg)?;

        let vpath = cfg.versioned_path();
        if !vpath.exists() {
            return err::default_error(err::NEX).with_module(cfg.module_id);
        }
        if vpath.is_dir() {
            return err::default_error(err::PTH).with_module(cfg.module_id);
        }

        let file_cfg = Self::file_cfg(&cfg);
        let file = File::open(file_cfg)?;

        let mmap = PlatformMemMap::new(file.fd(), file.length()).with_module(cfg.module_id)?;

        Self::from_components(cfg, mmap, file)
    }

    /// NA
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    /// use std::time::Duration;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("open_or_create_memmap.bin"),
    ///     version: 1,
    ///     initial_t: 2,
    ///     sync_interval: Duration::from_millis(100),
    /// };
    ///
    /// {
    ///     let mmap = MemMap::<u64>::open_or_create(cfg.clone()).unwrap();
    ///     assert_eq!(mmap.length(), std::mem::size_of::<u64>() * 2);
    /// }
    ///
    /// let reopened = MemMap::<u64>::open_or_create(cfg).unwrap();
    /// assert_eq!(reopened.length(), std::mem::size_of::<u64>() * 2);
    /// ```
    pub fn open_or_create(cfg: MemMapCfg) -> FrozenResult<Self> {
        Self::validate_t(cfg.module_id)?;
        Self::validate_path(&cfg)?;

        let vpath = cfg.versioned_path();
        if vpath.exists() && vpath.is_dir() {
            return err::default_error(err::PTH).with_module(cfg.module_id);
        }

        let file_cfg = Self::file_cfg(&cfg);
        let file = File::open_or_create(file_cfg)?;

        let mmap = PlatformMemMap::new(file.fd(), file.length()).with_module(cfg.module_id)?;

        Self::from_components(cfg, mmap, file)
    }

    /// Returns the length of the memory mapping in bytes
    #[inline]
    pub fn length(&self) -> usize {
        self.inner.length
    }

    /// Get reference to configuration of [`MemMap`]
    #[inline]
    pub fn cfg(&self) -> &MemMapCfg {
        &self.cfg
    }

    /// Get reference to underlying [`File`]
    #[inline]
    pub fn file(&self) -> &File {
        self.file.as_ref().expect("file exists")
    }

    /// Fetch the latest assigned durability epoch
    #[inline]
    pub fn current_epoch(&self) -> TEpoch {
        self.inner.completion.read_current_epoch()
    }

    /// Fetch the latest durable epoch
    #[inline]
    pub fn durable_epoch(&self) -> TEpoch {
        self.inner.completion.read_durable_epoch()
    }

    /// Fetch reference to underlying durability [`Completion`]
    #[inline]
    pub fn completion(&self) -> &Arc<Completion> {
        &self.inner.completion
    }

    /// Synchronize dirty pages and file metadata to stable storage
    #[inline]
    pub fn sync(&self) -> FrozenResult<()> {
        self.inner.sync_internal()
    }

    /// Reads an element of type `T` from the memory mapping at the specified `index`
    ///
    /// ## Constraints
    ///
    /// - `index * size_of::<T>()` must not overflow `usize`, otherwise returns [`err::INV`]
    /// - Reading past the mapped memory region returns [`err::HCF`]
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    /// use std::time::Duration;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("read_test.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: Duration::from_millis(100),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// mmap.write(&42u64, 0).unwrap();
    /// assert_eq!(mmap.read(0).unwrap(), 42u64);
    /// ```
    #[inline(always)]
    pub fn read(&self, index: usize) -> FrozenResult<T> {
        let mid = self.cfg.module_id;
        let size = core::mem::size_of::<T>();

        let offset = match index.checked_mul(size) {
            Some(off) => off,
            None => return err::default_error(err::INV).with_module(mid),
        };

        if hints::unlikely(offset.checked_add(size).is_none_or(|end| end > self.length())) {
            return err::default_error(err::HCF).with_module(mid);
        }

        let _slot_guard = self.inner.slot_locks.read_lock(index);

        let guard = self.inner.mmap.read().unwrap_or_else(|e| e.into_inner());
        let mmap = match guard.as_ref() {
            Some(m) => m,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let ptr = unsafe { mmap.as_ptr::<T>(offset) };
        let val = unsafe { ptr.read() };
        Ok(val)
    }

    /// Writes an element `val` of type `T` into the memory mapping at the specified `index`
    ///
    /// Returns an [`AckTicket`] representing the durability acknowledgement of this write operation
    ///
    /// ## Constraints
    ///
    /// - `index * size_of::<T>()` must not overflow `usize`, otherwise returns [`err::INV`]
    /// - Writing past the mapped memory region returns [`err::HCF`]
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    /// use std::time::Duration;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("write_test.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: Duration::from_millis(100),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// let ticket = mmap.write(&100u64, 2).unwrap();
    /// assert_eq!(mmap.read(2).unwrap(), 100u64);
    /// ```
    #[inline(always)]
    pub fn write(&self, val: &T, index: usize) -> FrozenResult<AckTicket> {
        let mid = self.cfg.module_id;
        let size = core::mem::size_of::<T>();

        let offset = match index.checked_mul(size) {
            Some(off) => off,
            None => return err::default_error(err::INV).with_module(mid),
        };

        if hints::unlikely(offset.checked_add(size).is_none_or(|end| end > self.length())) {
            return err::default_error(err::HCF).with_module(mid);
        }

        let _slot_guard = self.inner.slot_locks.write_lock(index);

        let guard = self.inner.mmap.read().unwrap_or_else(|e| e.into_inner());
        let mmap = match guard.as_ref() {
            Some(m) => m,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let ptr = unsafe { mmap.as_mut_ptr::<T>(offset) };
        unsafe { core::ptr::write(ptr, val.clone()) };

        let epoch = self.inner.completion.increment_current_epoch();
        Ok(AckTicket::new(epoch, self.inner.completion.clone()))
    }

    /// Deletes the [`MemMap`] and underlying [`File`] entry from the storage device
    ///
    /// Consumes `self` by value to release the memory mapping and unlink the file
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

        let mut guard = self.inner.mmap.write().unwrap_or_else(|e| e.into_inner());
        if let Some(mmap) = guard.take() {
            mmap.unmap().with_module(mid)?;
        }

        let file = match self.file.take() {
            Some(f) => f,
            None => return err::default_error(err::INV).with_module(mid),
        };

        file.delete().with_module(mid)
    }
}

impl<T> Drop for MemMap<T>
where
    T: Sized + Clone,
{
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

        let _ = self.inner.sync_internal();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MID: u8 = 0;
    const INIT_T: usize = 4;

    #[derive(Debug, Clone)]
    #[repr(C, align(8))]
    struct ValidT {
        a: u64,
        b: u64,
    }

    #[derive(Debug, Clone)]
    #[repr(C)]
    struct BadAlignT {
        a: u32,
        b: u32,
    }

    #[derive(Debug, Clone)]
    #[repr(C)]
    struct BadSizeT {
        a: [u8; 12],
    }

    #[derive(Debug, Clone)]
    #[repr(C, align(8))]
    struct DropT(u64);

    impl Drop for DropT {
        fn drop(&mut self) {}
    }

    #[derive(Debug, Clone)]
    #[repr(C, align(8))]
    struct ZstT;

    #[derive(Debug, Clone)]
    #[repr(C, align(8))]
    struct OversizedT {
        data: [u8; 8192],
    }

    fn tmp_cfg() -> (tempfile::TempDir, MemMapCfg) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmp_memmap");
        let cfg = MemMapCfg {
            module_id: MID,
            path,
            version: 1,
            initial_t: INIT_T,
            sync_interval: Duration::from_millis(100),
        };

        (dir, cfg)
    }

    #[test]
    fn ok_validate_t() {
        assert!(MemMap::<u64>::validate_t(MID).is_ok());
        assert!(MemMap::<ValidT>::validate_t(MID).is_ok());
    }

    #[test]
    fn err_validate_t_invariants() {
        assert_eq!(MemMap::<DropT>::validate_t(MID).unwrap_err().reason, err::DRP.reason);
        assert_eq!(MemMap::<BadAlignT>::validate_t(MID).unwrap_err().reason, err::ALN.reason);
        assert_eq!(MemMap::<ZstT>::validate_t(MID).unwrap_err().reason, err::ZRO.reason);
        assert_eq!(MemMap::<BadSizeT>::validate_t(MID).unwrap_err().reason, err::SZE.reason);
        assert_eq!(MemMap::<OversizedT>::validate_t(MID).unwrap_err().reason, err::INV.reason);
    }

    #[test]
    fn ok_new_sets_file_sync_interval_none() {
        let (_dir, cfg) = tmp_cfg();

        let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
        assert_eq!(mmap.length(), core::mem::size_of::<u64>() * INIT_T);
        assert_eq!(mmap.file().cfg().sync_interval, None);
        assert_eq!(mmap.cfg().sync_interval, Duration::from_millis(100));
    }

    #[test]
    fn err_new_when_file_exists() {
        let (_dir, cfg) = tmp_cfg();
        let _mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
        let err = MemMap::<u64>::new(cfg).unwrap_err();
        assert_eq!(err.reason, err::EXS.reason);
    }

    #[test]
    fn err_new_when_t_invalid() {
        let (_dir, cfg) = tmp_cfg();
        assert_eq!(MemMap::<DropT>::new(cfg.clone()).unwrap_err().reason, err::DRP.reason);
        assert!(!cfg.versioned_path().exists());
    }

    #[test]
    fn ok_open() {
        let (_dir, cfg) = tmp_cfg();
        {
            let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
            assert_eq!(mmap.length(), core::mem::size_of::<u64>() * INIT_T);
        }

        let opened = MemMap::<u64>::open(cfg).unwrap();
        assert_eq!(opened.length(), core::mem::size_of::<u64>() * INIT_T);
        assert_eq!(opened.file().cfg().sync_interval, None);
    }

    #[test]
    fn err_open_when_file_missing() {
        let (_dir, cfg) = tmp_cfg();
        let err = MemMap::<u64>::open(cfg).unwrap_err();
        assert_eq!(err.reason, err::NEX.reason);
    }

    #[test]
    fn ok_open_or_create() {
        let (_dir, cfg) = tmp_cfg();
        {
            let mmap = MemMap::<u64>::open_or_create(cfg.clone()).unwrap();
            assert_eq!(mmap.length(), core::mem::size_of::<u64>() * INIT_T);
            assert_eq!(mmap.file().cfg().sync_interval, None);
        }

        let reopened = MemMap::<u64>::open_or_create(cfg).unwrap();
        assert_eq!(reopened.length(), core::mem::size_of::<u64>() * INIT_T);
        assert_eq!(reopened.file().cfg().sync_interval, None);
    }

    #[test]
    fn err_path_when_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let non_existent_dir = dir.path().join("missing_dir_xyz");
        let cfg = MemMapCfg {
            module_id: MID,
            path: non_existent_dir.join("file.bin"),
            version: 1,
            initial_t: INIT_T,
            sync_interval: Duration::from_millis(100),
        };

        let err_new = MemMap::<u64>::new(cfg.clone()).unwrap_err();
        assert_eq!(err_new.reason, err::DIR.reason);

        let err_open = MemMap::<u64>::open(cfg.clone()).unwrap_err();
        assert_eq!(err_open.reason, err::DIR.reason);

        let err_open_or_create = MemMap::<u64>::open_or_create(cfg).unwrap_err();
        assert_eq!(err_open_or_create.reason, err::DIR.reason);
    }

    #[test]
    fn err_path_when_parent_is_not_dir() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("regular_file");
        std::fs::write(&file_path, b"test").unwrap();

        let cfg = MemMapCfg {
            module_id: MID,
            path: file_path.join("subfile.bin"),
            version: 1,
            initial_t: INIT_T,
            sync_interval: Duration::from_millis(100),
        };

        let err_new = MemMap::<u64>::new(cfg.clone()).unwrap_err();
        assert_eq!(err_new.reason, err::DIR.reason);

        let err_open = MemMap::<u64>::open(cfg.clone()).unwrap_err();
        assert_eq!(err_open.reason, err::DIR.reason);

        let err_open_or_create = MemMap::<u64>::open_or_create(cfg).unwrap_err();
        assert_eq!(err_open_or_create.reason, err::DIR.reason);
    }

    #[test]
    fn err_path_when_empty() {
        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from(""),
            version: 1,
            initial_t: INIT_T,
            sync_interval: Duration::from_millis(100),
        };

        let err_new = MemMap::<u64>::new(cfg.clone()).unwrap_err();
        assert_eq!(err_new.reason, err::PTH.reason);

        let err_open = MemMap::<u64>::open(cfg.clone()).unwrap_err();
        assert_eq!(err_open.reason, err::PTH.reason);

        let err_open_or_create = MemMap::<u64>::open_or_create(cfg).unwrap_err();
        assert_eq!(err_open_or_create.reason, err::PTH.reason);
    }

    #[test]
    fn err_path_when_is_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = MemMapCfg {
            module_id: MID,
            path: dir.path().to_path_buf(),
            version: 1,
            initial_t: INIT_T,
            sync_interval: Duration::from_millis(100),
        };

        let err_new = MemMap::<u64>::new(cfg.clone()).unwrap_err();
        assert_eq!(err_new.reason, err::PTH.reason);

        let err_open = MemMap::<u64>::open(cfg.clone()).unwrap_err();
        assert_eq!(err_open.reason, err::PTH.reason);

        let err_open_or_create = MemMap::<u64>::open_or_create(cfg).unwrap_err();
        assert_eq!(err_open_or_create.reason, err::PTH.reason);
    }

    #[test]
    fn err_path_when_no_filename() {
        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from("/"),
            version: 1,
            initial_t: INIT_T,
            sync_interval: Duration::from_millis(100),
        };

        let err_new = MemMap::<u64>::new(cfg.clone()).unwrap_err();
        assert_eq!(err_new.reason, err::PTH.reason);

        let err_open = MemMap::<u64>::open(cfg.clone()).unwrap_err();
        assert_eq!(err_open.reason, err::PTH.reason);

        let err_open_or_create = MemMap::<u64>::open_or_create(cfg).unwrap_err();
        assert_eq!(err_open_or_create.reason, err::PTH.reason);
    }

    #[test]
    fn ok_delete() {
        let (_dir, cfg) = tmp_cfg();
        let path = cfg.versioned_path();
        let mmap = MemMap::<u64>::new(cfg).unwrap();
        assert!(path.exists());
        assert!(mmap.delete().is_ok());
        assert!(!path.exists());
    }

    #[test]
    fn ok_versioned_path_resolution() {
        // Simple file with extension
        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from("data.bin"),
            version: 1,
            initial_t: 2,
            sync_interval: Duration::from_millis(100),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from("data_1.bin"));

        // File without extension
        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from("datastore"),
            version: 2,
            initial_t: 2,
            sync_interval: Duration::from_millis(100),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from("datastore_2"));

        // Multi-part extension
        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from("archive.tar.gz"),
            version: 3,
            initial_t: 2,
            sync_interval: Duration::from_millis(100),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from("archive.tar_3.gz"));

        // Absolute path with directories
        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from("/var/data/store.db"),
            version: 10,
            initial_t: 2,
            sync_interval: Duration::from_millis(100),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from("/var/data/store_10.db"));

        // Relative path with directories
        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from("dir/sub/file.dat"),
            version: 0,
            initial_t: 2,
            sync_interval: Duration::from_millis(100),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from("dir/sub/file_0.dat"));

        // Dotfile without extension
        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from(".env"),
            version: 5,
            initial_t: 2,
            sync_interval: Duration::from_millis(100),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from(".env_5"));

        // Dotfile with extension
        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from(".config.json"),
            version: 1,
            initial_t: 2,
            sync_interval: Duration::from_millis(100),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from(".config_1.json"));
    }

    #[test]
    fn ok_versioned_path_and_multiversion() {
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("store.dat");

        let cfg_v1 = MemMapCfg {
            module_id: MID,
            path: base_path.clone(),
            version: 1,
            initial_t: 2,
            sync_interval: Duration::from_millis(100),
        };
        assert_eq!(cfg_v1.versioned_path(), dir.path().join("store_1.dat"));

        let cfg_v2 = MemMapCfg {
            module_id: MID,
            path: base_path.clone(),
            version: 2,
            initial_t: 4,
            sync_interval: Duration::from_millis(100),
        };
        assert_eq!(cfg_v2.versioned_path(), dir.path().join("store_2.dat"));

        // Both versions can coexist independently on disk
        let mmap_v1 = MemMap::<u64>::new(cfg_v1.clone()).unwrap();
        let mmap_v2 = MemMap::<ValidT>::new(cfg_v2.clone()).unwrap();

        assert!(cfg_v1.versioned_path().exists());
        assert!(cfg_v2.versioned_path().exists());
        assert_eq!(mmap_v1.length(), core::mem::size_of::<u64>() * 2);
        assert_eq!(mmap_v2.length(), core::mem::size_of::<ValidT>() * 4);

        assert!(mmap_v1.delete().is_ok());
        assert!(!cfg_v1.versioned_path().exists());
        assert!(cfg_v2.versioned_path().exists());

        assert!(mmap_v2.delete().is_ok());
        assert!(!cfg_v2.versioned_path().exists());
    }

    #[test]
    fn ok_write_read_single() {
        let (_dir, cfg) = tmp_cfg();
        let mmap = MemMap::<u64>::new(cfg).unwrap();

        assert_eq!(mmap.read(0).unwrap(), 0);
        let ticket = mmap.write(&0xDEAD_BEEF_CAFE_BABE, 0).unwrap();
        assert_eq!(mmap.read(0).unwrap(), 0xDEAD_BEEF_CAFE_BABE);
        assert_eq!(ticket.epoch(), 1);
    }

    #[test]
    fn ok_write_read_multiple_indices() {
        let (_dir, cfg) = tmp_cfg();
        let mmap = MemMap::<u64>::new(cfg).unwrap();

        for i in 0..INIT_T {
            let val = (i as u64 + 1) * 0x1111_2222_3333_4444;
            mmap.write(&val, i).unwrap();
        }

        for i in 0..INIT_T {
            let expected = (i as u64 + 1) * 0x1111_2222_3333_4444;
            assert_eq!(mmap.read(i).unwrap(), expected);
        }
    }

    #[test]
    fn ok_write_read_struct() {
        let (_dir, cfg) = tmp_cfg();
        let mmap = MemMap::<ValidT>::new(cfg).unwrap();

        let val0 = ValidT { a: 10, b: 20 };
        let val1 = ValidT { a: 100, b: 200 };

        mmap.write(&val0, 0).unwrap();
        mmap.write(&val1, 1).unwrap();

        let r0 = mmap.read(0).unwrap();
        let r1 = mmap.read(1).unwrap();

        assert_eq!(r0.a, 10);
        assert_eq!(r0.b, 20);
        assert_eq!(r1.a, 100);
        assert_eq!(r1.b, 200);
    }

    #[test]
    fn err_write_read_out_of_bounds() {
        let (_dir, cfg) = tmp_cfg();
        let mmap = MemMap::<u64>::new(cfg).unwrap();

        // INIT_T is 4, valid indices are 0..=3
        let err_r = mmap.read(INIT_T).unwrap_err();
        assert_eq!(err_r.reason, err::HCF.reason);

        let err_w = mmap.write(&12345, INIT_T).unwrap_err();
        assert_eq!(err_w.reason, err::HCF.reason);

        let err_r_large = mmap.read(100).unwrap_err();
        assert_eq!(err_r_large.reason, err::HCF.reason);

        let err_w_large = mmap.write(&12345, 100).unwrap_err();
        assert_eq!(err_w_large.reason, err::HCF.reason);
    }

    #[test]
    fn err_write_read_overflow_index() {
        let (_dir, cfg) = tmp_cfg();
        let mmap = MemMap::<u64>::new(cfg).unwrap();

        let err_r = mmap.read(usize::MAX).unwrap_err();
        assert_eq!(err_r.reason, err::INV.reason);

        let err_w = mmap.write(&12345, usize::MAX).unwrap_err();
        assert_eq!(err_w.reason, err::INV.reason);
    }

    #[test]
    fn ok_durability_across_reopen() {
        let (_dir, cfg) = tmp_cfg();
        {
            let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
            mmap.write(&0xCAFE_BABE_1234_5678, 1).unwrap();
            mmap.sync().unwrap();
        }

        let reopened = MemMap::<u64>::open(cfg).unwrap();
        assert_eq!(reopened.read(1).unwrap(), 0xCAFE_BABE_1234_5678);
    }

    #[test]
    fn ok_background_sync_advances_epoch() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.sync_interval = Duration::from_millis(25);

        let mmap = MemMap::<u64>::new(cfg).unwrap();
        let ticket = mmap.write(&0x1234, 0).unwrap();
        let epoch = ticket.wait().unwrap();

        assert_eq!(epoch, ticket.epoch());
        assert!(ticket.is_durable());
        assert!(mmap.durable_epoch() >= ticket.epoch());
    }

    #[test]
    fn ok_ticket_force_wakes_bg_worker() {
        let (_dir, mut cfg) = tmp_cfg();
        // Long interval to prove force wakes up immediately
        cfg.sync_interval = Duration::from_secs(10);

        let mmap = MemMap::<u64>::new(cfg).unwrap();
        let ticket = mmap.write(&0xABCD, 0).unwrap();
        assert!(!ticket.is_durable());

        let start = std::time::Instant::now();
        let epoch = ticket.force().unwrap();
        let elapsed = start.elapsed();

        assert_eq!(epoch, ticket.epoch());
        assert!(ticket.is_durable());
        assert!(elapsed < Duration::from_secs(2));
    }

    #[test]
    fn ok_drop_persists_unflushed_writes() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.sync_interval = Duration::from_secs(30);

        {
            let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
            let _ticket = mmap.write(&0xBEEF_CAFE, 0).unwrap();
            drop(mmap);
        }

        let reopened = MemMap::<u64>::open(cfg).unwrap();
        assert_eq!(reopened.read(0).unwrap(), 0xBEEF_CAFE);
    }

    #[test]
    fn ok_manual_sync_advances_durable_epoch() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.sync_interval = Duration::from_secs(60);

        let mmap = MemMap::<u64>::new(cfg).unwrap();
        let ticket = mmap.write(&0x9999, 1).unwrap();
        assert_eq!(mmap.current_epoch(), 1);
        assert_eq!(mmap.durable_epoch(), 0);

        mmap.sync().unwrap();
        assert!(ticket.is_durable());
        assert_eq!(mmap.durable_epoch(), 1);
    }

    #[test]
    fn ok_concurrent_slot_locking() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.initial_t = 8;
        cfg.sync_interval = Duration::from_secs(60);

        let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();

        // 4 writer threads, each writing to index 0 repeatedly
        for t in 0..4 {
            let mmap_clone = Arc::clone(&mmap);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                for i in 0..500 {
                    let val = ((t + 1) as u64) << 32 | (i as u64);
                    let _ = mmap_clone.write(&val, 0).unwrap();
                }
            }));
        }

        // 4 reader threads, reading from index 0 repeatedly
        for _ in 0..4 {
            let mmap_clone = Arc::clone(&mmap);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                for _ in 0..1000 {
                    let val = mmap_clone.read(0).unwrap();
                    let thread_id = (val >> 32) as usize;
                    assert!(thread_id <= 4, "read value must be valid without torn reads");
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Parallel reads across different and same indices
        let barrier2 = Arc::new(std::sync::Barrier::new(4));
        let mut read_handles = Vec::new();
        for _ in 0..4 {
            let mmap_clone = Arc::clone(&mmap);
            let barrier_clone = Arc::clone(&barrier2);
            read_handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                for _ in 0..500 {
                    let _ = mmap_clone.read(0).unwrap();
                }
            }));
        }

        for h in read_handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn err_concurrent_new_same_path() {
        let (_dir, cfg) = tmp_cfg();
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let t1 = {
            let cfg = cfg.clone();
            let b = Arc::clone(&barrier);
            std::thread::spawn(move || {
                b.wait();
                MemMap::<u64>::new(cfg)
            })
        };

        let t2 = {
            let b = Arc::clone(&barrier);
            std::thread::spawn(move || {
                b.wait();
                MemMap::<u64>::new(cfg)
            })
        };

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        match (r1, r2) {
            (Ok(m), Err(e)) | (Err(e), Ok(m)) => {
                assert_eq!(e.reason, err::EXS.reason);
                assert!(m.delete().is_ok());
            }
            (Ok(_), Ok(_)) => {
                panic!("Concurrent MemMap::new on identical path must not both succeed");
            }
            (Err(e1), Err(e2)) => {
                panic!("Unexpected error on both threads: {:?}, {:?}", e1, e2);
            }
        }
    }

    #[test]
    fn err_concurrent_open_same_path() {
        let (_dir, cfg) = tmp_cfg();
        {
            let m = MemMap::<u64>::new(cfg.clone()).unwrap();
            assert_eq!(m.length(), core::mem::size_of::<u64>() * INIT_T);
        }

        let barrier = Arc::new(std::sync::Barrier::new(2));

        let t1 = {
            let cfg = cfg.clone();
            let b = Arc::clone(&barrier);
            std::thread::spawn(move || {
                b.wait();
                MemMap::<u64>::open(cfg)
            })
        };

        let t2 = {
            let b = Arc::clone(&barrier);
            std::thread::spawn(move || {
                b.wait();
                MemMap::<u64>::open(cfg)
            })
        };

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        match (r1, r2) {
            (Ok(m), Err(e)) | (Err(e), Ok(m)) => {
                assert_eq!(e.domain, 0x08, "Error domain must be File (0x08)");
                assert_eq!(
                    e.reason, 0x1C,
                    "Concurrent open should yield LCK (0x1C) from kernel flock: {:?}",
                    e
                );
                assert!(m.delete().is_ok());
            }
            (Ok(_), Ok(_)) => {
                panic!("Exclusive advisory lock violation: both concurrent open calls succeeded");
            }
            (Err(e1), Err(e2)) => {
                panic!("Unexpected failure of both threads: {:?}, {:?}", e1, e2);
            }
        }
    }

    #[test]
    fn ok_reopen_after_drop_releases_lock() {
        let (_dir, cfg) = tmp_cfg();
        let m1 = MemMap::<u64>::new(cfg.clone()).unwrap();
        m1.write(&0x1122_3344, 0).unwrap();
        drop(m1);

        let m2 = MemMap::<u64>::open(cfg).unwrap();
        assert_eq!(m2.read(0).unwrap(), 0x1122_3344);
        assert!(m2.delete().is_ok());
    }

    #[test]
    fn ok_delete_and_recreate_cycle() {
        let (_dir, cfg) = tmp_cfg();
        for i in 0..4 {
            let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
            let val = (i as u64 + 1) * 0xAA55;
            mmap.write(&val, 0).unwrap();
            assert_eq!(mmap.read(0).unwrap(), val);
            assert!(mmap.delete().is_ok());
            assert!(!cfg.versioned_path().exists());
        }
    }

    #[test]
    fn ok_concurrent_distinct_slots_no_contention() {
        let (_dir, mut cfg) = tmp_cfg();
        const THREADS: usize = 8;
        cfg.initial_t = THREADS;
        cfg.sync_interval = Duration::from_secs(60);

        let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(THREADS));
        let mut handles = Vec::with_capacity(THREADS);

        for t in 0..THREADS {
            let mmap_clone = Arc::clone(&mmap);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                for i in 0..1000 {
                    let val = (t as u64) << 32 | (i as u64);
                    let _ = mmap_clone.write(&val, t).unwrap();
                    let read_val = mmap_clone.read(t).unwrap();
                    assert_eq!(read_val >> 32, t as u64);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        for t in 0..THREADS {
            let val = mmap.read(t).unwrap();
            assert_eq!(val >> 32, t as u64);
        }
    }

    #[test]
    fn ok_stress_interleaved_reads_writes() {
        let (_dir, mut cfg) = tmp_cfg();
        const NUM_SLOTS: usize = 16;
        const NUM_THREADS: usize = 8;
        cfg.initial_t = NUM_SLOTS;
        cfg.sync_interval = Duration::from_millis(50);

        let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(NUM_THREADS));
        let mut handles = Vec::with_capacity(NUM_THREADS);

        for t in 0..NUM_THREADS {
            let mmap_clone = Arc::clone(&mmap);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                for i in 0..1500 {
                    let slot = (t + i) % NUM_SLOTS;
                    if i % 2 == 0 {
                        let val = ((t + 1) as u64) << 32 | (i as u64);
                        let _ = mmap_clone.write(&val, slot).unwrap();
                    } else {
                        let _ = mmap_clone.read(slot).unwrap();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert!(mmap.sync().is_ok());
    }

    #[test]
    fn ok_concurrent_writers_with_background_sync() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.initial_t = 8;
        cfg.sync_interval = Duration::from_millis(5);

        let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let mut handles = Vec::new();

        for t in 0..4 {
            let mmap_clone = Arc::clone(&mmap);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                let mut tickets = Vec::with_capacity(50);
                for i in 0..50 {
                    let val = (t as u64) * 1000 + i as u64;
                    tickets.push(mmap_clone.write(&val, t).unwrap());
                    std::thread::sleep(Duration::from_millis(1));
                }
                for ticket in tickets {
                    let _ = ticket.wait().unwrap();
                    assert!(ticket.is_durable());
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert!(mmap.durable_epoch() >= mmap.current_epoch());
    }

    #[test]
    fn ok_concurrent_ticket_force_stress() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.initial_t = 8;
        cfg.sync_interval = Duration::from_secs(30);

        let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let mut handles = Vec::new();

        for t in 0..4 {
            let mmap_clone = Arc::clone(&mmap);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                for i in 0..20 {
                    let val = (t as u64) * 100 + i as u64;
                    let ticket = mmap_clone.write(&val, t).unwrap();
                    let durable_epoch = ticket.force().unwrap();
                    assert!(ticket.is_durable());
                    assert!(durable_epoch >= ticket.epoch());
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn ok_exact_boundary_indices() {
        let (_dir, cfg) = tmp_cfg();
        let mmap = MemMap::<u64>::new(cfg).unwrap();

        // Valid boundary: INIT_T - 1
        assert!(mmap.write(&0xF00D, INIT_T - 1).is_ok());
        assert_eq!(mmap.read(INIT_T - 1).unwrap(), 0xF00D);

        // Invalid boundary: INIT_T
        assert_eq!(mmap.write(&0xBAD, INIT_T).unwrap_err().reason, err::HCF.reason);
        assert_eq!(mmap.read(INIT_T).unwrap_err().reason, err::HCF.reason);
    }

    #[test]
    fn ok_large_slot_count_initialization() {
        let (_dir, mut cfg) = tmp_cfg();
        const LARGE_COUNT: usize = 10_000;
        cfg.initial_t = LARGE_COUNT;

        let mmap = MemMap::<u64>::new(cfg).unwrap();
        assert_eq!(mmap.length(), LARGE_COUNT * core::mem::size_of::<u64>());

        // First, middle, last
        assert!(mmap.write(&1, 0).is_ok());
        assert!(mmap.write(&5000, 5000).is_ok());
        assert!(mmap.write(&9999, LARGE_COUNT - 1).is_ok());

        assert_eq!(mmap.read(0).unwrap(), 1);
        assert_eq!(mmap.read(5000).unwrap(), 5000);
        assert_eq!(mmap.read(LARGE_COUNT - 1).unwrap(), 9999);
    }
}
