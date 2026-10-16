//! Memory-mapped typed slot storage with per-slot concurrency and epoch durability
//!
//! ## Example
//!
//! ```
//! use frozen_core::memmap::{MemMap, MemMapCfg};
//! use std::time::Duration;
//!
//! let dir = tempfile::tempdir().unwrap();
//! let path = dir.path().join("memmap_demo.bin");
//!
//! let cfg = MemMapCfg {
//!     module_id: 1,
//!     path,
//!     version: 1,
//!     initial_t: 8,
//!     sync_interval: Some(Duration::from_millis(50)),
//! };
//!
//! let memmap = MemMap::<u64>::new(cfg).unwrap();
//! assert_eq!(memmap.length(), 8 * std::mem::size_of::<u64>());
//!
//! let ticket = memmap.write(&0xDEAD_BEEF_CAFE_BABE, 0).unwrap();
//! ticket.wait().unwrap();
//! assert!(ticket.is_durable());
//!
//! assert_eq!(memmap.read(0, |v| *v).unwrap(), 0xDEAD_BEEF_CAFE_BABE);
//! assert!(memmap.delete().is_ok());
//! ```

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

    /// type `T` is invalid
    /// (e.g. `size_of::<T>()` exceeds page size)
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

    /// unable to grow memory-mapped file
    pub const GRW: ErrCode = ErrCode::new(0x22, "unable to grow memory-mapped file");

    /// state conflict
    pub const CAS: ErrCode = ErrCode::new(0x24, "compare and swap check failed");

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
///     sync_interval: Some(Duration::from_millis(0x64)),
/// };
///
/// assert_eq!(cfg.version, 1);
/// assert_eq!(cfg.initial_t, 0x10);
/// assert_eq!(cfg.sync_interval, Some(Duration::from_millis(0x64)));
/// ```
#[derive(Debug, Clone)]
pub struct MemMapCfg {
    /// Identifier used while error propagation
    pub module_id: u8,

    /// Base path for/of the file
    ///
    /// ## Notes
    ///
    /// - The caller must make sure that the path represents a file and all the parent directories
    ///   included in the path do exist
    /// - The actual file created on disk is derived by appending the [`version`](Self::version) to
    ///   the base file name (e.g. `data.bin` with version `1` becomes `data_1.bin`)
    pub path: std::path::PathBuf,

    /// Schema or layout version of the underlying memory-mapped file
    ///
    /// Because the binary layout on disk is constructed according to the initial type `T` and
    /// structural parameters, [`MemMap`] strictly requires type invariants across a file's lifetime
    ///
    /// When an application evolves its data structures or migrates `T`, incrementing `version`
    /// ensures a new versioned backing file (`<name>_<ver>`) is created on disk without corrupting
    /// or clashing with previously persisted data
    pub version: u32,

    /// Number of `T` elements to pre-allocate on fs when [`MemMap`] is initialized
    ///
    /// Initial file length will be `core::mem::size_of::<T>() * initial_t` (bytes)
    pub initial_t: usize,

    /// Optional interval for the background sync thread
    ///
    /// If `Some(duration)`, a background worker thread is spawned which flushes dirty mapped pages
    /// at this interval; and if `None`, background sync is disabled and durability advances via
    /// manual sync or forced tickets
    pub sync_interval: Option<std::time::Duration>,
}

impl MemMapCfg {
    /// Computes the versioned file path by appending `_{version}` to the base file stem
    ///
    /// Preserves file extension if present (e.g., `data.bin` with version `1` becomes `data_1.bin`)
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

const WRITER_BIT: u32 = 0x8000_0000;
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

    fn grow(&mut self, additional_slots: usize) {
        if additional_slots == 0 {
            return;
        }

        let total = self.locks.len() + additional_slots;
        let mut slots = Vec::with_capacity(total);
        for val in self.locks.iter() {
            slots.push(atomic::AtomicU32::new(val.load(atomic::Ordering::Relaxed)));
        }
        for _ in 0..additional_slots {
            slots.push(atomic::AtomicU32::new(0));
        }

        self.locks = slots.into_boxed_slice();
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
    slot_locks: RwLock<SlotLocks>,
    length: atomic::AtomicUsize,
    completion: Arc<Completion>,
    sync_condvar: Condvar,
    sync_mutex: Mutex<bool>,
    shutdown: atomic::AtomicBool,
    is_parked: atomic::AtomicBool,
}

impl SyncTrigger for MemMapInner {
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

impl MemMapInner {
    /// Synchronizes dirty pages to storage if current epoch exceeds durable epoch
    fn sync_mmap_if_needed(&self, mmap: &PlatformMemMap, sync_len: usize) -> FrozenResult<()> {
        let target_epoch = self.completion.read_current_epoch();
        let durable_epoch = self.completion.read_durable_epoch();

        if target_epoch <= durable_epoch {
            return Ok(());
        }

        let sync_res = mmap.sync(sync_len);
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

    fn sync_internal(&self) -> FrozenResult<()> {
        let guard = self.mmap.read().unwrap_or_else(|e| e.into_inner());
        let mmap = match guard.as_ref() {
            Some(m) => m,
            None => return Ok(()),
        };

        let length = self.length.load(atomic::Ordering::Acquire);
        self.sync_mmap_if_needed(mmap, length)
    }
}

/// Custom memory-mapped typed array storage with slot-level locking and epoch durability
///
/// `MemMap<T>` maps an on-disk [`File`] into process virtual address space as a contiguous array of
/// fixed-size slots of type `T`
///
/// It provides,
///
/// - **Slot Synchronization**: Per-slot reader-writer spinlocks (`SlotLocks`) protecting each index
///   against concurrent write-write and read-write hazards with backoff spin/yield/sleep stages
/// - **Epoch Durability**: Monotonically advancing write epoch counter incremented on each
///   mutation, tracked via [`AckTicket`] and synchronized in the background by a dedicated worker
/// - **Process Isolation**: Advisory kernel-level locking (`flock`/`LockFileEx`) ensuring exclusive
///   file ownership by a single running instance
///
/// ## Constraints
///
/// - `T` must be sized and cloneable (`T: Sized + Clone`)
/// - `T` must be 8-byte aligned (`core::mem::align_of::<T>() == 8`)
/// - `size_of::<T>()` must be non-zero, a multiple of 8, and must not exceed the host page size
/// - `T` must not implement `Drop` (`!core::mem::needs_drop::<T>()`)
/// - Type layout is immutable across file lifetime; schema changes require bumping
///   [`MemMapCfg::version`]
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
///     path: dir.path().join("storage.bin"),
///     version: 1,
///     initial_t: 4,
///     sync_interval: Some(Duration::from_millis(100)),
/// };
///
/// let memmap = MemMap::<u64>::new(cfg).unwrap();
/// assert_eq!(memmap.length(), 4 * std::mem::size_of::<u64>());
///
/// let ticket = memmap.write(&12345u64, 0).unwrap();
/// assert_eq!(memmap.read(0, |v| *v).unwrap(), 12345u64);
/// assert!(memmap.delete().is_ok());
/// ```
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
            slot_locks: RwLock::new(slot_locks),
            length: atomic::AtomicUsize::new(length),
            completion: completion.clone(),
            sync_condvar: Condvar::new(),
            sync_mutex: Mutex::new(false),
            shutdown: atomic::AtomicBool::new(false),
            is_parked: atomic::AtomicBool::new(false),
        });

        let weak_inner: std::sync::Weak<dyn SyncTrigger> =
            Arc::downgrade(&inner) as std::sync::Weak<dyn SyncTrigger>;
        completion.set_sync_trigger(weak_inner);

        let bg_thread = if let Some(interval) = sync_interval {
            let worker_inner = Arc::clone(&inner);
            let handle = std::thread::Builder::new().name("frozen-mmap-sync-worker".into()).spawn(
                move || {
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

                        let target_epoch = worker_inner.completion.read_current_epoch();
                        let durable_epoch = worker_inner.completion.read_durable_epoch();

                        if target_epoch == durable_epoch {
                            worker_inner.is_parked.store(true, atomic::Ordering::Release);

                            if worker_inner.completion.read_current_epoch() != durable_epoch {
                                worker_inner.is_parked.store(false, atomic::Ordering::Release);
                                continue;
                            }

                            let new_guard = worker_inner
                                .sync_condvar
                                .wait(guard)
                                .unwrap_or_else(|e| e.into_inner());
                            guard = new_guard;
                            worker_inner.is_parked.store(false, atomic::Ordering::Release);

                            if worker_inner.shutdown.load(atomic::Ordering::Acquire) || *guard {
                                continue;
                            }

                            if worker_inner.completion.read_current_epoch()
                                == worker_inner.completion.read_durable_epoch()
                            {
                                continue;
                            }

                            let (new_guard, _) = worker_inner
                                .sync_condvar
                                .wait_timeout(guard, interval)
                                .unwrap_or_else(|e| e.into_inner());
                            guard = new_guard;
                        } else {
                            worker_inner.is_parked.store(false, atomic::Ordering::Release);

                            let (new_guard, _) = worker_inner
                                .sync_condvar
                                .wait_timeout(guard, interval)
                                .unwrap_or_else(|e| e.into_inner());
                            guard = new_guard;
                        }

                        if worker_inner.shutdown.load(atomic::Ordering::Acquire) {
                            break;
                        }

                        *guard = false;
                        drop(guard);

                        let _ = worker_inner.sync_internal();
                        guard = worker_inner.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());
                    }
                },
            );

            let handle = match handle {
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

            Some(handle)
        } else {
            None
        };

        Ok(Self { cfg, file: Some(file), inner, bg_thread, _marker: PhantomData })
    }

    /// Creates and memory-maps a new [`File`] at `cfg.versioned_path()`
    ///
    /// Allocates an underlying file with capacity for `initial_t * size_of::<T>()` bytes, acquires
    /// an exclusive advisory lock on the descriptor, establishes the memory mapping, and spawns the
    /// background worker for periodic durability flushing
    ///
    /// ## Constraints
    ///
    /// - Returns [`err::EXS`] if the versioned file already exists on the filesystem; Use
    ///   [`MemMap::open`] or [`MemMap::open_or_create`] instead
    /// - Returns [`err::PTH`] or [`err::DIR`] if the base path or its parent directory is invalid
    /// - Enforces all structural type invariants for `T` (`align == 8`, `size % 8 == 0`, `size <=
    ///   page_size`, no `Drop`)
    /// - An exclusive advisory lock is acquired on creation; concurrent callers on the same file
    ///   path fail
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
    ///     sync_interval: Some(Duration::from_millis(100)),
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

    /// Opens an existing memory-mapped [`File`] at `cfg.versioned_path()`
    ///
    /// Opens the underlying versioned file, verifies its length matches the requested layout,
    /// acquires an exclusive advisory lock on the descriptor, maps its memory region, and spawns
    /// the background flush worker
    ///
    /// ## Constraints
    ///
    /// - Returns [`err::NEX`] if the backing file does not exist on disk
    /// - Returns [`err::PTH`] if the path resolves to a directory
    /// - Fails with advisory lock error if another handle currently owns the exclusive lock
    /// - Type `T` invariants must strictly match the binary layout originally written to the file
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
    ///     sync_interval: Some(Duration::from_millis(100)),
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

    /// Opens an existing memory-mapped [`File`] or creates a new one if it does not yet exist
    ///
    /// Idempotent entrypoint that transparently delegates to either open or creation semantics
    /// based on filesystem presence, maintaining consistent advisory locking and background
    /// flushing
    ///
    /// ## Constraints
    ///
    /// - Returns [`err::PTH`] if the resolved path points to an existing directory
    /// - Returns [`err::DIR`] if parent directory does not exist
    /// - Fails if another handle currently owns the advisory lock on the target file
    /// - All structural `T` invariants (`align`, `size`, `needs_drop`) are strictly validated
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
    ///     sync_interval: Some(Duration::from_millis(100)),
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

    /// Returns the system memory page size in bytes queried from kernel settings
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::MemMap;
    ///
    /// let page_size = MemMap::<u64>::page_size().unwrap();
    /// assert!(page_size >= 4096);
    /// assert!(page_size.is_power_of_two());
    /// ```
    #[inline]
    pub fn page_size() -> FrozenResult<usize> {
        PlatformMemMap::page_size()
    }

    /// Returns the length of the memory mapping in bytes
    ///
    /// Corresponds to `initial_t * size_of::<T>()`
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
    ///     path: dir.path().join("length_demo.bin"),
    ///     version: 1,
    ///     initial_t: 8,
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// assert_eq!(mmap.length(), 8 * std::mem::size_of::<u64>());
    /// ```
    #[inline]
    pub fn length(&self) -> usize {
        self.inner.length.load(atomic::Ordering::Acquire)
    }

    /// Dynamically grows the memory-mapped file by `count` additional elements of type `T`
    ///
    /// ## Semantics
    ///
    /// - Halts all concurrent read and write operations across all slots for the duration of the
    ///   growth
    /// - Halts background durability flushing and manual sync operations
    /// - If uncommitted dirty writes exist (`current_epoch > durable_epoch`), performs a durability
    ///   sync before modifying file boundaries
    /// - Explicitly unmaps the current virtual memory mapping
    /// - Zero-extends the backing file on disk safely via [`File::grow`] and synchronizes file
    ///   metadata
    /// - Establishes a new memory mapping over the enlarged file range
    /// - Dynamically expands per-slot spinlocks to accommodate new slots
    /// - Updates tracked mapping length and advances durability epoch
    ///
    /// ## Crash Durability & Safety
    ///
    /// - Existing payload data is flushed to persistent storage before file extension, ensuring no
    ///   data loss
    /// - Disk block pre-allocation (`fallocate` / `f_preallocate`) precedes logical size expansion;
    ///   out-of-space failures leave existing data untouched
    /// - If file growth fails, the original memory mapping is automatically restored
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
    ///     path: dir.path().join("grow_demo.bin"),
    ///     version: 1,
    ///     initial_t: 2,
    ///     sync_interval: None,
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// assert_eq!(mmap.length(), 2 * std::mem::size_of::<u64>());
    /// mmap.write(&111u64, 0).unwrap();
    ///
    /// mmap.grow(3).unwrap();
    /// assert_eq!(mmap.length(), 5 * std::mem::size_of::<u64>());
    /// assert_eq!(mmap.read(0, |v| *v).unwrap(), 111u64);
    /// assert_eq!(mmap.read(4, |v| *v).unwrap(), 0u64);
    /// ```
    pub fn grow(&self, count: usize) -> FrozenResult<()> {
        let mid = self.cfg.module_id;
        if count == 0 {
            return Ok(());
        }

        let size = core::mem::size_of::<T>();
        let len_to_add = match count.checked_mul(size) {
            Some(len) => len,
            None => {
                return err::raw_error(err::GRW, "file growth calculation overflowed usize")
                    .with_module(mid);
            }
        };

        let file = match self.file.as_ref() {
            Some(f) => f,
            None => return err::default_error(err::INV).with_module(mid),
        };

        // Halt all read and write ops by taking exclusive write lock on slot_locks
        let mut slot_locks_guard = self.inner.slot_locks.write().unwrap_or_else(|e| e.into_inner());

        // Halt all sync ops (including background sync worker) by taking exclusive write lock on mmap
        let mut mmap_guard = self.inner.mmap.write().unwrap_or_else(|e| e.into_inner());

        let mmap = match mmap_guard.as_ref() {
            Some(m) => m,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let curr_len = self.inner.length.load(atomic::Ordering::Acquire);
        let new_length = match curr_len.checked_add(len_to_add) {
            Some(len) => len,
            None => {
                return err::raw_error(err::GRW, "file growth calculation overflowed usize")
                    .with_module(mid);
            }
        };

        // Pre-growth sync: only if needed (curr_epoch > durable_epoch)
        self.inner.sync_mmap_if_needed(mmap, curr_len)?;

        let old_mmap = match mmap_guard.take() {
            Some(m) => m,
            None => return err::default_error(err::INV).with_module(mid),
        };

        if let Err(e) = old_mmap.unmap().with_module(mid) {
            if let Ok(restored) = PlatformMemMap::new(file.fd(), curr_len).with_module(mid) {
                *mmap_guard = Some(restored);
            }
            return Err(e);
        }

        if let Err(e) = file.grow(count).with_module(mid) {
            if let Ok(restored) = PlatformMemMap::new(file.fd(), curr_len).with_module(mid) {
                *mmap_guard = Some(restored);
            }
            return Err(e);
        }

        // Flush new file metadata to disk for crash safety
        if let Err(e) = file.sync().with_module(mid) {
            if let Ok(restored) = PlatformMemMap::new(file.fd(), new_length).with_module(mid) {
                *mmap_guard = Some(restored);
                self.inner.length.store(new_length, atomic::Ordering::Release);
                slot_locks_guard.grow(count);
            }
            return Err(e);
        }

        let new_mmap = match PlatformMemMap::new(file.fd(), new_length).with_module(mid) {
            Ok(m) => m,
            Err(e) => {
                if let Ok(restored) = PlatformMemMap::new(file.fd(), curr_len).with_module(mid) {
                    *mmap_guard = Some(restored);
                }
                return Err(e);
            }
        };

        *mmap_guard = Some(new_mmap);
        slot_locks_guard.grow(count);
        self.inner.length.store(new_length, atomic::Ordering::Release);

        self.inner.completion.increment_current_epoch();

        Ok(())
    }

    /// Returns a reference to the active [`MemMapCfg`] configuration
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
    ///     path: dir.path().join("cfg_demo.bin"),
    ///     version: 2,
    ///     initial_t: 4,
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// assert_eq!(mmap.cfg().version, 2);
    /// assert_eq!(mmap.cfg().initial_t, 4);
    /// ```
    #[inline]
    pub fn cfg(&self) -> &MemMapCfg {
        &self.cfg
    }

    /// Fetch the latest assigned durability epoch
    ///
    /// The current epoch monotonically increments on each [`MemMap::write`] call
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
    ///     path: dir.path().join("epoch_demo.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// assert_eq!(mmap.current_epoch(), 0);
    /// mmap.write(&999u64, 0).unwrap();
    /// assert_eq!(mmap.current_epoch(), 1);
    /// ```
    #[inline]
    pub fn current_epoch(&self) -> TEpoch {
        self.inner.completion.read_current_epoch()
    }

    /// Fetch the latest confirmed durable epoch flushed to persistent storage
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
    ///     path: dir.path().join("durable_demo.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// let ticket = mmap.write(&100u64, 0).unwrap();
    /// mmap.sync().unwrap();
    /// assert_eq!(mmap.durable_epoch(), ticket.epoch());
    /// ```
    #[inline]
    pub fn durable_epoch(&self) -> TEpoch {
        self.inner.completion.read_durable_epoch()
    }

    /// Synchronize dirty pages and file metadata to stable storage
    ///
    /// Performs a synchronous two-phase flush,
    ///
    /// - Flushes modified virtual memory pages to host cache via [`PlatformMemMap::sync`]
    /// - Advances the durable epoch to the current write epoch and awakens all pending listeners
    ///
    /// Returns `Ok(())` immediately if no writes have occurred since the last sync
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("sync_demo.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: None,
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// let ticket = mmap.write(&0xCAFE_BABE, 1).unwrap();
    /// assert!(!ticket.is_durable());
    ///
    /// mmap.sync().unwrap();
    /// assert!(ticket.is_durable());
    /// ```
    #[inline]
    pub fn sync(&self) -> FrozenResult<()> {
        self.inner.sync_internal()
    }

    /// Locks slot `index` for reading and invokes `f` with a reference to `T`
    ///
    /// ## Constraints
    ///
    /// - `index * size_of::<T>()` must not overflow `usize`, otherwise returns [`err::INV`]
    /// - Reading past the mapped memory region returns [`err::HCF`]
    ///
    /// ## Zero-Copy & Safety
    ///
    /// Callback `f` is invoked with `&T` directly referencing mapped memory under lock
    ///
    /// The higher-ranked lifetime bound (`F: FnOnce(&T) -> R`) ensures references cannot
    /// escape or outlive the locked callback scope
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
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// mmap.write(&42u64, 0).unwrap();
    /// assert_eq!(mmap.read(0, |v| *v).unwrap(), 42u64);
    /// ```
    #[inline(always)]
    pub fn read<R, F>(&self, index: usize, f: F) -> FrozenResult<R>
    where
        F: FnOnce(&T) -> R,
    {
        let mid = self.cfg.module_id;
        let size = core::mem::size_of::<T>();

        let offset = match index.checked_mul(size) {
            Some(off) => off,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let slot_locks_guard = self.inner.slot_locks.read().unwrap_or_else(|e| e.into_inner());

        if hints::unlikely(offset.checked_add(size).is_none_or(|end| end > self.length())) {
            return err::default_error(err::HCF).with_module(mid);
        }

        let _slot_guard = slot_locks_guard.read_lock(index);

        let guard = self.inner.mmap.read().unwrap_or_else(|e| e.into_inner());
        let mmap = match guard.as_ref() {
            Some(m) => m,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let ptr = unsafe { mmap.as_ptr::<T>(offset) };
        let target_ref = unsafe { &*ptr };
        let res = f(target_ref);

        drop(guard);
        drop(_slot_guard);
        drop(slot_locks_guard);

        Ok(res)
    }

    /// Atomically locks `N` consecutive slots and invokes `f` with a reference to `Target`
    ///
    /// Requires `size_of::<Target>() == N * size_of::<T>()` and compatible alignment
    ///
    /// ## Constraints
    ///
    /// - `size_of::<Target>()` must not be zero ([`err::ZRO`])
    /// - `size_of::<Target>()` must be an exact multiple of `size_of::<T>()` ([`err::SZE`])
    /// - `align_of::<Target>()` must not exceed `align_of::<T>()` ([`err::ALN`])
    /// - Slot index and byte offset calculations must not overflow `usize` ([`err::INV`])
    /// - Target slot range `start..start + N` must not exceed mapping bounds ([`err::HCF`])
    ///
    /// ## Deadlock Freedom
    ///
    /// Read spinlocks for slots `start..start + N` are acquired in strictly ascending order
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    /// use std::time::Duration;
    ///
    /// #[derive(Clone, Copy)]
    /// #[repr(C)]
    /// struct Page {
    ///     header: u64,
    ///     payload: [u64; 3],
    /// }
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("read_exact_demo.bin"),
    ///     version: 1,
    ///     initial_t: 8,
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// mmap.write(&0xAA, 0).unwrap();
    /// mmap.write(&0xBB, 1).unwrap();
    /// mmap.write(&0xCC, 2).unwrap();
    /// mmap.write(&0xDD, 3).unwrap();
    ///
    /// let header = mmap.read_exact(0, |page: &Page| page.header).unwrap();
    /// assert_eq!(header, 0xAA);
    /// ```
    #[inline(always)]
    pub fn read_exact<Target, R, F>(&self, start: usize, f: F) -> FrozenResult<R>
    where
        F: FnOnce(&Target) -> R,
    {
        let mid = self.cfg.module_id;
        let slot_size = core::mem::size_of::<T>();
        let target_size = core::mem::size_of::<Target>();

        if hints::unlikely(target_size == 0) {
            return err::default_error(err::ZRO).with_module(mid);
        }

        if hints::unlikely(target_size % slot_size != 0) {
            return err::default_error(err::SZE).with_module(mid);
        }

        if hints::unlikely(core::mem::align_of::<Target>() > core::mem::align_of::<T>()) {
            return err::default_error(err::ALN).with_module(mid);
        }

        let n = target_size / slot_size;

        let end_slot = match start.checked_add(n) {
            Some(end) => end,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let offset = match start.checked_mul(slot_size) {
            Some(off) => off,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let end_offset = match end_slot.checked_mul(slot_size) {
            Some(end) => end,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let slot_locks_guard = self.inner.slot_locks.read().unwrap_or_else(|e| e.into_inner());

        if hints::unlikely(end_offset > self.length()) {
            return err::default_error(err::HCF).with_module(mid);
        }

        let mut slot_guards = Vec::with_capacity(n);
        for slot in start..end_slot {
            slot_guards.push(slot_locks_guard.read_lock(slot));
        }

        let guard = self.inner.mmap.read().unwrap_or_else(|e| e.into_inner());
        let mmap = match guard.as_ref() {
            Some(m) => m,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let ptr = unsafe { mmap.as_ptr::<Target>(offset) };
        if hints::unlikely((ptr as usize) % core::mem::align_of::<Target>() != 0) {
            return err::default_error(err::ALN).with_module(mid);
        }

        let target_ref = unsafe { &*ptr };
        let res = f(target_ref);

        drop(guard);
        drop(slot_guards);
        drop(slot_locks_guard);

        Ok(res)
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
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// let ticket = mmap.write(&100u64, 2).unwrap();
    /// assert_eq!(mmap.read(2, |v| *v).unwrap(), 100u64);
    /// ```
    #[inline(always)]
    pub fn write(&self, val: &T, index: usize) -> FrozenResult<AckTicket> {
        let mid = self.cfg.module_id;
        let size = core::mem::size_of::<T>();

        let offset = match index.checked_mul(size) {
            Some(off) => off,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let slot_locks_guard = self.inner.slot_locks.read().unwrap_or_else(|e| e.into_inner());

        if hints::unlikely(offset.checked_add(size).is_none_or(|end| end > self.length())) {
            return err::default_error(err::HCF).with_module(mid);
        }

        let _slot_guard = slot_locks_guard.write_lock(index);

        let guard = self.inner.mmap.read().unwrap_or_else(|e| e.into_inner());
        let mmap = match guard.as_ref() {
            Some(m) => m,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let ptr = unsafe { mmap.as_mut_ptr::<T>(offset) };
        unsafe { core::ptr::write(ptr, val.clone()) };
        drop(guard);
        drop(_slot_guard);
        drop(slot_locks_guard);

        let epoch = self.inner.completion.increment_current_epoch();

        if self.inner.is_parked.load(atomic::Ordering::Acquire) {
            let _guard = self.inner.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());
            if self.inner.is_parked.swap(false, atomic::Ordering::AcqRel) {
                self.inner.sync_condvar.notify_one();
            }
        }

        Ok(AckTicket::new(epoch, self.inner.completion.clone()))
    }

    /// Atomically locks `N` consecutive slots and invokes `f` with mutable access to `Target`
    ///
    /// Requires `size_of::<Target>() == N * size_of::<T>()` and compatible alignment
    ///
    /// Returns a tuple `(AckTicket, R)` containing the durability acknowledgement ticket and the
    /// result returned by callback `f`
    ///
    /// ## Constraints
    ///
    /// - `size_of::<Target>()` must not be zero ([`err::ZRO`])
    /// - `size_of::<Target>()` must be an exact multiple of `size_of::<T>()` ([`err::SZE`])
    /// - `align_of::<Target>()` must not exceed `align_of::<T>()` ([`err::ALN`])
    /// - Slot index and byte offset calculations must not overflow `usize` ([`err::INV`])
    /// - Target slot range `start..start + N` must not exceed mapping bounds ([`err::HCF`])
    ///
    /// ## Deadlock Freedom
    ///
    /// Exclusive write spinlocks for slots `start..start + N` are acquired in strictly ascending
    /// order
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    /// use std::time::Duration;
    ///
    /// #[repr(C)]
    /// struct Page {
    ///     header: u64,
    ///     payload: [u64; 3],
    /// }
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("write_exact_demo.bin"),
    ///     version: 1,
    ///     initial_t: 8,
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// let (ticket, _) = mmap
    ///     .write_exact(0, |page: &mut Page| {
    ///         page.header = 0xAA;
    ///         page.payload[0] = 0xBB;
    ///     })
    ///     .unwrap();
    ///
    /// assert_eq!(mmap.read(0, |v| *v).unwrap(), 0xAA);
    /// assert_eq!(mmap.read(1, |v| *v).unwrap(), 0xBB);
    /// assert_eq!(ticket.epoch(), 1);
    /// ```
    #[inline(always)]
    pub fn write_exact<Target, R, F>(&self, start: usize, f: F) -> FrozenResult<(AckTicket, R)>
    where
        F: FnOnce(&mut Target) -> R,
    {
        let mid = self.cfg.module_id;
        let slot_size = core::mem::size_of::<T>();
        let target_size = core::mem::size_of::<Target>();

        if hints::unlikely(target_size == 0) {
            return err::default_error(err::ZRO).with_module(mid);
        }

        if hints::unlikely(target_size % slot_size != 0) {
            return err::default_error(err::SZE).with_module(mid);
        }

        if hints::unlikely(core::mem::align_of::<Target>() > core::mem::align_of::<T>()) {
            return err::default_error(err::ALN).with_module(mid);
        }

        let n = target_size / slot_size;

        let end_slot = match start.checked_add(n) {
            Some(end) => end,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let offset = match start.checked_mul(slot_size) {
            Some(off) => off,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let end_offset = match end_slot.checked_mul(slot_size) {
            Some(end) => end,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let slot_locks_guard = self.inner.slot_locks.read().unwrap_or_else(|e| e.into_inner());

        if hints::unlikely(end_offset > self.length()) {
            return err::default_error(err::HCF).with_module(mid);
        }

        let mut slot_guards = Vec::with_capacity(n);
        for slot in start..end_slot {
            slot_guards.push(slot_locks_guard.write_lock(slot));
        }

        let guard = self.inner.mmap.read().unwrap_or_else(|e| e.into_inner());
        let mmap = match guard.as_ref() {
            Some(m) => m,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let ptr = unsafe { mmap.as_mut_ptr::<Target>(offset) };
        if hints::unlikely((ptr as usize) % core::mem::align_of::<Target>() != 0) {
            return err::default_error(err::ALN).with_module(mid);
        }

        let target_ref = unsafe { &mut *ptr };
        let res = f(target_ref);

        drop(guard);
        drop(slot_guards);
        drop(slot_locks_guard);

        let epoch = self.inner.completion.increment_current_epoch();

        if self.inner.is_parked.load(atomic::Ordering::Acquire) {
            let _guard = self.inner.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());
            if self.inner.is_parked.swap(false, atomic::Ordering::AcqRel) {
                self.inner.sync_condvar.notify_one();
            }
        }

        Ok((AckTicket::new(epoch, self.inner.completion.clone()), res))
    }

    #[cfg(test)]
    pub(crate) fn is_parked(&self) -> bool {
        self.inner.is_parked.load(atomic::Ordering::Acquire)
    }

    /// Deletes the [`MemMap`] and underlying [`File`] entry from the storage device
    ///
    /// Consumes `self` by value to release resources in deterministic order,
    ///
    /// - Signals the background flush worker to stop and waits for its thread join
    /// - Unmaps the active memory mapping from virtual address space
    /// - Closes and unlinks the backing versioned file from the filesystem
    ///
    /// ## Constraints
    ///
    /// - Consumes `self` by value; after deletion, the instance can no longer be accessed
    /// - Returns [`err::INV`] if the underlying file handle was already released
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    /// use std::time::Duration;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let path = dir.path().join("delete_demo.bin");
    /// let versioned_path = dir.path().join("delete_demo_1.bin");
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path,
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// assert!(versioned_path.exists());
    ///
    /// mmap.delete().unwrap();
    /// assert!(!versioned_path.exists());
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

        let mut guard = self.inner.mmap.write().unwrap_or_else(|e| e.into_inner());
        let unmap_res =
            if let Some(mmap) = guard.take() { mmap.unmap().with_module(mid) } else { Ok(()) };

        let file = match self.file.take() {
            Some(f) => f,
            None => {
                unmap_res?;
                return err::default_error(err::INV).with_module(mid);
            }
        };

        let delete_res = file.delete().with_module(mid);

        match (unmap_res, delete_res) {
            (Err(mut unmap_err), Err(del_err)) => {
                unmap_err.add_suppressed(del_err);
                Err(unmap_err)
            }
            (Err(unmap_err), Ok(())) => Err(unmap_err),
            (Ok(()), Err(del_err)) => Err(del_err),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}

impl<T> MemMap<T>
where
    T: Sized + Clone + PartialEq,
{
    /// Conditionally writes `val` into slot `index` if and only if the current value matches
    /// `expected`
    ///
    /// ## Semantics
    ///
    /// Acquires the exclusive slot lock for `index` and verifies that the current slot value equals
    /// `expected`; if equal, writes `val`, advances the epoch, and returns an
    /// [`AckTicket`]. If the current value differs from `expected`, the operation immediately
    /// aborts without modifying the slot, returning [`err::CAS`] so the caller can re-read and
    /// retry
    ///
    /// ## Constraints
    ///
    /// - `index * size_of::<T>()` must not overflow `usize`, otherwise returns [`err::INV`]
    /// - Writing past the mapped memory region returns [`err::HCF`]
    /// - Returns [`err::CAS`] if `expected != current`
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
    ///     path: dir.path().join("cas_demo.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// mmap.write(&100u64, 0).unwrap();
    ///
    /// // Successful CAS: 100 -> 200
    /// let ticket = mmap.compare_and_write(&100u64, &200u64, 0).unwrap();
    /// assert_eq!(mmap.read(0, |v| *v).unwrap(), 200u64);
    ///
    /// // Failed CAS: expected 100, but slot is 200
    /// assert!(mmap.compare_and_write(&100u64, &300u64, 0).is_err());
    /// assert_eq!(mmap.read(0, |v| *v).unwrap(), 200u64);
    /// ```
    pub fn compare_and_write(
        &self,
        expected: &T,
        val: &T,
        index: usize,
    ) -> FrozenResult<AckTicket> {
        let mid = self.cfg.module_id;
        let size = core::mem::size_of::<T>();

        let offset = match index.checked_mul(size) {
            Some(off) => off,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let slot_locks_guard = self.inner.slot_locks.read().unwrap_or_else(|e| e.into_inner());

        if hints::unlikely(offset.checked_add(size).is_none_or(|end| end > self.length())) {
            return err::default_error(err::HCF).with_module(mid);
        }

        let _slot_guard = slot_locks_guard.write_lock(index);

        let guard = self.inner.mmap.read().unwrap_or_else(|e| e.into_inner());
        let mmap = match guard.as_ref() {
            Some(m) => m,
            None => return err::default_error(err::INV).with_module(mid),
        };

        let ptr = unsafe { mmap.as_mut_ptr::<T>(offset) };
        let current = unsafe { ptr.read() };
        if current != *expected {
            drop(guard);
            drop(_slot_guard);
            drop(slot_locks_guard);
            return err::raw_error(
                err::CAS,
                format!("state conflict at index {}: slot data has changed", index),
            )
            .with_module(mid);
        }

        unsafe { core::ptr::write(ptr, val.clone()) };
        drop(guard);
        drop(_slot_guard);
        drop(slot_locks_guard);

        let epoch = self.inner.completion.increment_current_epoch();

        if self.inner.is_parked.load(atomic::Ordering::Acquire) {
            let _guard = self.inner.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());
            if self.inner.is_parked.swap(false, atomic::Ordering::AcqRel) {
                self.inner.sync_condvar.notify_one();
            }
        }

        Ok(AckTicket::new(epoch, self.inner.completion.clone()))
    }

    /// Creates a new [`MemMapTransaction`] context for grouping multiple conditional writes into a
    /// single atomic epoch
    ///
    /// ## Overview
    ///
    /// `MemMapTransaction` allows grouping multiple slot mutations into a single atomic operation:
    ///
    /// - **Atomicity**: All writes succeed or fail together
    /// - **Optimistic Concurrency Control (CAS)**: Each write asserts that its target slot
    ///   currently matches `expected`; if any slot differs at commit time, all changes abort and no
    ///   slots are modified
    /// - **Single Epoch**: All committed writes share a single durability epoch and durable window
    /// - **Deadlock Freedom**: Slot locks are acquired in strictly ascending index order
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
    ///     path: dir.path().join("tx_demo.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    ///
    /// let mut tx = mmap.new_tx();
    /// tx.write(&0u64, &10u64, 0).unwrap();
    /// tx.write(&0u64, &20u64, 1).unwrap();
    ///
    /// let ticket = tx.commit().unwrap();
    /// assert_eq!(mmap.read(0, |v| *v).unwrap(), 10u64);
    /// assert_eq!(mmap.read(1, |v| *v).unwrap(), 20u64);
    /// ```
    #[inline]
    pub fn new_tx(&self) -> MemMapTransaction<'_, T> {
        MemMapTransaction { memmap: self, ops: Vec::new() }
    }
}

/// Context for grouping multiple conditional writes into a single atomic operation
///
/// Multi-slot mutations are staged in `MemMapTransaction` with optimistic concurrency
/// preconditions. At [`commit`](Self::commit) time, all target slot locks are acquired in strictly
/// ascending order, each slot is verified against its `expected` value, and if all match, all
/// updates are applied under a single epoch
///
/// If any slot fails verification, the transaction aborts with zero mutations, returning
/// [`err::CAS`] with the index of the first conflicting slot so the caller can retry
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
///     path: dir.path().join("tx_struct_demo.bin"),
///     version: 1,
///     initial_t: 4,
///     sync_interval: Some(Duration::from_millis(100)),
/// };
///
/// let mmap = MemMap::<u64>::new(cfg).unwrap();
/// let mut tx = mmap.new_tx();
/// tx.write(&0u64, &42u64, 0).unwrap();
/// tx.write(&0u64, &84u64, 2).unwrap();
///
/// let ticket = tx.commit().unwrap();
/// assert_eq!(mmap.read(0, |v| *v).unwrap(), 42u64);
/// assert_eq!(mmap.read(2, |v| *v).unwrap(), 84u64);
/// ```
#[derive(Debug)]
pub struct MemMapTransaction<'a, T: Sized + Clone + PartialEq> {
    memmap: &'a MemMap<T>,
    ops: Vec<(usize, T, T)>,
}

impl<T> MemMapTransaction<'_, T>
where
    T: Sized + Clone + PartialEq,
{
    /// Stages a conditional write of `new` at `index` requiring `expected` as the current slot
    /// value
    ///
    /// ## Requirements
    ///
    /// - Writes must be staged in strictly increasing index order (`index > previous_index`)
    /// - Duplicate indices in the same transaction return an error
    /// - `index` must not exceed mapping bounds
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
    ///     path: dir.path().join("tx_write_demo.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// let mut tx = mmap.new_tx();
    /// tx.write(&0u64, &111u64, 0).unwrap();
    /// tx.write(&0u64, &222u64, 1).unwrap();
    ///
    /// // Staging an out-of-order or duplicate index fails
    /// assert!(tx.write(&0u64, &333u64, 1).is_err());
    /// assert!(tx.write(&0u64, &333u64, 0).is_err());
    /// ```
    #[inline(always)]
    pub fn write(&mut self, expected: &T, new: &T, index: usize) -> FrozenResult<&mut Self> {
        let mid = self.memmap.cfg.module_id;
        if let Some(err) = self.memmap.inner.completion.get_err() {
            return Err(err);
        }

        if let Some((last_idx, _, _)) = self.ops.last() {
            if index <= *last_idx {
                return err::raw_error(
                    err::HCF,
                    "tx writes must be strictly ordered, with no more then single ops on given index",
                )
                .with_module(mid);
            }
        }

        let size = core::mem::size_of::<T>();
        let offset = match index.checked_mul(size) {
            Some(off) => off,
            None => return err::default_error(err::INV).with_module(mid),
        };

        if hints::unlikely(offset.checked_add(size).is_none_or(|end| end > self.memmap.length())) {
            return err::default_error(err::HCF).with_module(mid);
        }

        self.ops.push((index, expected.clone(), new.clone()));
        Ok(self)
    }

    /// Commits all staged writes atomically under a single durability epoch
    ///
    /// Acquires exclusive locks for all affected slots in order; if all slots match their expected
    /// values, updates are applied, the durable epoch advances once, and an [`AckTicket`] is
    /// returned
    ///
    /// If any slot differs from `expected`, all locks are released without modifying any data, and
    /// [`err::CAS`] is returned with context identifying the conflicting slot index
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
    ///     path: dir.path().join("tx_commit_demo.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: Some(Duration::from_millis(100)),
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// let mut tx = mmap.new_tx();
    /// tx.write(&0u64, &100u64, 0).unwrap();
    /// tx.write(&0u64, &200u64, 1).unwrap();
    ///
    /// let ticket = tx.commit().unwrap();
    /// assert_eq!(mmap.read(0, |v| *v).unwrap(), 100u64);
    /// assert_eq!(mmap.read(1, |v| *v).unwrap(), 200u64);
    /// ```
    pub fn commit(self) -> FrozenResult<AckTicket> {
        let mid = self.memmap.cfg.module_id;
        if self.ops.is_empty() {
            return err::raw_error(err::INV, "empty transaction").with_module(mid);
        }

        if let Some(err) = self.memmap.inner.completion.get_err() {
            return Err(err);
        }

        let size = core::mem::size_of::<T>();
        let slot_locks_guard =
            self.memmap.inner.slot_locks.read().unwrap_or_else(|e| e.into_inner());
        let current_length = self.memmap.length();

        // Verify bounds under slot_locks read guard
        for (index, _, _) in &self.ops {
            let offset = match index.checked_mul(size) {
                Some(off) => off,
                None => return err::default_error(err::INV).with_module(mid),
            };
            if hints::unlikely(offset.checked_add(size).is_none_or(|end| end > current_length)) {
                return err::default_error(err::HCF).with_module(mid);
            }
        }

        // Acquire all slot write locks in strictly ascending index order
        let mut slot_guards = Vec::with_capacity(self.ops.len());
        for (index, _, _) in &self.ops {
            slot_guards.push(slot_locks_guard.write_lock(*index));
        }

        let guard = self.memmap.inner.mmap.read().unwrap_or_else(|e| e.into_inner());
        let mmap = match guard.as_ref() {
            Some(m) => m,
            None => return err::default_error(err::INV).with_module(mid),
        };

        // Phase 1: State Precondition Verification (CAS)
        for (index, expected, _) in &self.ops {
            let offset = *index * size;
            let ptr = unsafe { mmap.as_ptr::<T>(offset) };
            let current = unsafe { ptr.read() };
            if current != *expected {
                drop(guard);
                drop(slot_guards);
                drop(slot_locks_guard);
                return err::raw_error(
                    err::CAS,
                    format!("state conflict at index {}: slot data has changed", index),
                )
                .with_module(mid);
            }
        }

        // Phase 2: Apply mutations
        for (index, _, new) in self.ops {
            let offset = index * size;
            let ptr = unsafe { mmap.as_mut_ptr::<T>(offset) };
            unsafe { core::ptr::write(ptr, new) };
        }

        drop(guard);
        drop(slot_guards);
        drop(slot_locks_guard);

        let epoch = self.memmap.inner.completion.increment_current_epoch();

        if self.memmap.inner.is_parked.load(atomic::Ordering::Acquire) {
            let _guard = self.memmap.inner.sync_mutex.lock().unwrap_or_else(|e| e.into_inner());
            if self.memmap.inner.is_parked.swap(false, atomic::Ordering::AcqRel) {
                self.memmap.inner.sync_condvar.notify_one();
            }
        }

        Ok(AckTicket::new(epoch, self.memmap.inner.completion.clone()))
    }

    /// Returns the number of staged operations in this transaction
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("tx_len_demo.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: None,
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// let mut tx = mmap.new_tx();
    /// assert_eq!(tx.len(), 0);
    /// tx.write(&0u64, &10u64, 0).unwrap();
    /// assert_eq!(tx.len(), 1);
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Returns `true` if no operations have been staged in this transaction
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("tx_empty_demo.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: None,
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg).unwrap();
    /// let tx = mmap.new_tx();
    /// assert!(tx.is_empty());
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
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
        a: [u8; 0x0C],
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
        data: [u8; 0x20000],
    }

    fn tmp_cfg() -> (tempfile::TempDir, MemMapCfg) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmp_memmap");
        let cfg = MemMapCfg {
            module_id: MID,
            path,
            version: 1,
            initial_t: INIT_T,
            sync_interval: Some(Duration::from_millis(0x64)),
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
        assert_eq!(mmap.cfg().sync_interval, Some(Duration::from_millis(0x64)));
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
        }

        let reopened = MemMap::<u64>::open_or_create(cfg).unwrap();
        assert_eq!(reopened.length(), core::mem::size_of::<u64>() * INIT_T);
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
            sync_interval: Some(Duration::from_millis(0x64)),
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
            sync_interval: Some(Duration::from_millis(0x64)),
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
            sync_interval: Some(Duration::from_millis(0x64)),
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
            sync_interval: Some(Duration::from_millis(0x64)),
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
            sync_interval: Some(Duration::from_millis(0x64)),
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
        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from("data.bin"),
            version: 1,
            initial_t: 2,
            sync_interval: Some(Duration::from_millis(0x64)),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from("data_1.bin"));

        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from("datastore"),
            version: 2,
            initial_t: 2,
            sync_interval: Some(Duration::from_millis(0x64)),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from("datastore_2"));

        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from("archive.tar.gz"),
            version: 3,
            initial_t: 2,
            sync_interval: Some(Duration::from_millis(0x64)),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from("archive.tar_3.gz"));

        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from("/var/data/store.db"),
            version: 0x0A,
            initial_t: 2,
            sync_interval: Some(Duration::from_millis(0x64)),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from("/var/data/store_10.db"));

        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from("dir/sub/file.dat"),
            version: 0,
            initial_t: 2,
            sync_interval: Some(Duration::from_millis(0x64)),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from("dir/sub/file_0.dat"));

        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from(".env"),
            version: 5,
            initial_t: 2,
            sync_interval: Some(Duration::from_millis(0x64)),
        };
        assert_eq!(cfg.versioned_path(), std::path::PathBuf::from(".env_5"));

        let cfg = MemMapCfg {
            module_id: MID,
            path: std::path::PathBuf::from(".config.json"),
            version: 1,
            initial_t: 2,
            sync_interval: Some(Duration::from_millis(0x64)),
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
            sync_interval: Some(Duration::from_millis(0x64)),
        };
        assert_eq!(cfg_v1.versioned_path(), dir.path().join("store_1.dat"));

        let cfg_v2 = MemMapCfg {
            module_id: MID,
            path: base_path.clone(),
            version: 2,
            initial_t: 4,
            sync_interval: Some(Duration::from_millis(0x64)),
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

        assert_eq!(mmap.read(0, |v| *v).unwrap(), 0);
        let ticket = mmap.write(&0xDEAD_BEEF_CAFE_BABE, 0).unwrap();
        assert_eq!(mmap.read(0, |v| *v).unwrap(), 0xDEAD_BEEF_CAFE_BABE);
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
            assert_eq!(mmap.read(i, |v| *v).unwrap(), expected);
        }
    }

    #[test]
    fn ok_write_read_struct() {
        let (_dir, cfg) = tmp_cfg();
        let mmap = MemMap::<ValidT>::new(cfg).unwrap();

        let val0 = ValidT { a: 0x0A, b: 0x14 };
        let val1 = ValidT { a: 0x64, b: 0xC8 };

        mmap.write(&val0, 0).unwrap();
        mmap.write(&val1, 1).unwrap();

        let r0 = mmap.read(0, |v| v.clone()).unwrap();
        let r1 = mmap.read(1, |v| v.clone()).unwrap();

        assert_eq!(r0.a, 0x0A);
        assert_eq!(r0.b, 0x14);
        assert_eq!(r1.a, 0x64);
        assert_eq!(r1.b, 0xC8);
    }

    #[test]
    fn ok_read_callback_zero_copy_projection() {
        let (_dir, cfg) = tmp_cfg();
        let mmap = MemMap::<ValidT>::new(cfg).unwrap();

        let val = ValidT { a: 0x1234, b: 0x5678 };
        mmap.write(&val, 0).unwrap();

        let a_val = mmap.read(0, |v| v.a).unwrap();
        assert_eq!(a_val, 0x1234);

        let sum = mmap.read(0, |v| v.a + v.b).unwrap();
        assert_eq!(sum, 0x1234 + 0x5678);

        let is_match = mmap.read(0, |v| v.b == 0x5678).unwrap();
        assert!(is_match);
    }

    #[test]
    fn ok_read_callback_unwind_releases_lock() {
        let (_dir, cfg) = tmp_cfg();
        let mmap = MemMap::<u64>::new(cfg).unwrap();
        mmap.write(&0x42, 0).unwrap();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = mmap.read(0, |_| panic!("simulated panic in read callback"));
        }));
        assert!(result.is_err());

        assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x42);
        assert!(mmap.write(&0x99, 0).is_ok());
        assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x99);
    }

    #[test]
    fn err_write_read_out_of_bounds() {
        let (_dir, cfg) = tmp_cfg();
        let mmap = MemMap::<u64>::new(cfg).unwrap();

        let err_r = mmap.read(INIT_T, |_| ()).unwrap_err();
        assert_eq!(err_r.reason, err::HCF.reason);

        let err_w = mmap.write(&0x3039, INIT_T).unwrap_err();
        assert_eq!(err_w.reason, err::HCF.reason);

        let err_r_large = mmap.read(0x64, |_| ()).unwrap_err();
        assert_eq!(err_r_large.reason, err::HCF.reason);

        let err_w_large = mmap.write(&0x3039, 0x64).unwrap_err();
        assert_eq!(err_w_large.reason, err::HCF.reason);
    }

    #[test]
    fn err_write_read_overflow_index() {
        let (_dir, cfg) = tmp_cfg();
        let mmap = MemMap::<u64>::new(cfg).unwrap();

        let err_r = mmap.read(usize::MAX, |_| ()).unwrap_err();
        assert_eq!(err_r.reason, err::INV.reason);

        let err_w = mmap.write(&0x3039, usize::MAX).unwrap_err();
        assert_eq!(err_w.reason, err::INV.reason);
    }

    mod read_exact {
        use super::*;

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(C)]
        struct TestNode {
            id: u64,
            flags: u64,
            keys: [u64; 2],
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(C)]
        struct TestBadSizeTarget([u8; 12]);

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(C, align(16))]
        struct TestOveralignedTarget {
            _a: u64,
            _b: u64,
        }

        #[test]
        fn ok_read_exact_single_and_multi_slot() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            mmap.write(&0x1111, 0).unwrap();
            mmap.write(&0x2222, 1).unwrap();
            mmap.write(&0x3333, 2).unwrap();
            mmap.write(&0x4444, 3).unwrap();

            let single = mmap.read_exact(0, |val: &u64| *val).unwrap();
            assert_eq!(single, 0x1111);

            let node = mmap.read_exact(0, |n: &TestNode| *n).unwrap();
            assert_eq!(node, TestNode { id: 0x1111, flags: 0x2222, keys: [0x3333, 0x4444] });

            let keys = mmap.read_exact(2, |k: &[u64; 2]| *k).unwrap();
            assert_eq!(keys, [0x3333, 0x4444]);

            let flag_is_set = mmap.read_exact(0, |n: &TestNode| n.flags == 0x2222).unwrap();
            assert!(flag_is_set);
        }

        #[test]
        fn err_read_exact_invariants() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            let err_zro = mmap.read_exact::<(), _, _>(0, |_| ()).unwrap_err();
            assert_eq!(err_zro.reason, err::ZRO.reason);

            let err_sze = mmap.read_exact::<TestBadSizeTarget, _, _>(0, |_| ()).unwrap_err();
            assert_eq!(err_sze.reason, err::SZE.reason);

            let err_aln = mmap.read_exact::<TestOveralignedTarget, _, _>(0, |_| ()).unwrap_err();
            assert_eq!(err_aln.reason, err::ALN.reason);

            let err_inv = mmap.read_exact::<u64, _, _>(usize::MAX, |_| ()).unwrap_err();
            assert_eq!(err_inv.reason, err::INV.reason);

            let err_inv_add =
                mmap.read_exact::<TestNode, _, _>(usize::MAX - 1, |_| ()).unwrap_err();
            assert_eq!(err_inv_add.reason, err::INV.reason);

            let err_hcf_start = mmap.read_exact::<u64, _, _>(INIT_T, |_| ()).unwrap_err();
            assert_eq!(err_hcf_start.reason, err::HCF.reason);

            let err_hcf_end = mmap.read_exact::<TestNode, _, _>(INIT_T - 2, |_| ()).unwrap_err();
            assert_eq!(err_hcf_end.reason, err::HCF.reason);
        }

        #[test]
        fn ok_read_exact_concurrent_readers() {
            let (_dir, mut cfg) = tmp_cfg();
            cfg.initial_t = 8;
            let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());

            mmap.write(&0xCAFE, 0).unwrap();
            mmap.write(&0xBABE, 1).unwrap();
            mmap.write(&0x1234, 2).unwrap();
            mmap.write(&0x5678, 3).unwrap();

            let barrier = Arc::new(std::sync::Barrier::new(4));
            let mut handles = Vec::new();

            for _ in 0..4 {
                let mmap_clone = Arc::clone(&mmap);
                let barrier_clone = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier_clone.wait();
                    for _ in 0..0x1F4 {
                        let id = mmap_clone.read_exact(0, |n: &TestNode| n.id).unwrap();
                        assert_eq!(id, 0xCAFE);
                        let k1 = mmap_clone.read_exact(0, |n: &TestNode| n.keys[1]).unwrap();
                        assert_eq!(k1, 0x5678);
                    }
                }));
            }

            for h in handles {
                h.join().unwrap();
            }
        }

        #[test]
        fn ok_read_exact_concurrent_writers_isolation() {
            let (_dir, mut cfg) = tmp_cfg();
            cfg.initial_t = 8;
            let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());

            let barrier = Arc::new(std::sync::Barrier::new(5));
            let mut handles = Vec::new();

            {
                let mmap_clone = Arc::clone(&mmap);
                let barrier_clone = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier_clone.wait();
                    for i in 0..0x1F4 {
                        let v = i as u64;
                        let _ = mmap_clone.write(&v, 0).unwrap();
                        let _ = mmap_clone.write(&v, 1).unwrap();
                    }
                }));
            }

            for _ in 0..4 {
                let mmap_clone = Arc::clone(&mmap);
                let barrier_clone = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier_clone.wait();
                    for _ in 0..0x1F4 {
                        mmap_clone
                            .read_exact(0, |pair: &[u64; 2]| {
                                assert!(pair[0] <= 0x1F4 && pair[1] <= 0x1F4);
                            })
                            .unwrap();
                    }
                }));
            }

            for h in handles {
                h.join().unwrap();
            }
        }

        #[test]
        fn ok_read_exact_unwind_releases_locks() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            mmap.write(&0x1111, 0).unwrap();
            mmap.write(&0x2222, 1).unwrap();

            let panic_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = mmap.read_exact(0, |_pair: &[u64; 2]| {
                    panic!("intentional panic inside read_exact callback");
                });
            }));
            assert!(panic_res.is_err());

            let ticket = mmap.write(&0x9999, 0).unwrap();
            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x9999);
            assert_eq!(ticket.epoch(), 3);

            let ticket2 = mmap.write(&0x8888, 1).unwrap();
            assert_eq!(mmap.read(1, |v| *v).unwrap(), 0x8888);
            assert_eq!(ticket2.epoch(), 4);
        }
    }

    mod write_exact {
        use super::*;

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(C)]
        struct TestNode {
            id: u64,
            flags: u64,
            keys: [u64; 2],
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(C)]
        struct TestBadSizeTarget([u8; 12]);

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(C, align(16))]
        struct TestOveralignedTarget {
            _a: u64,
            _b: u64,
        }

        #[test]
        fn ok_write_exact_single_and_multi_slot() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            let (ticket_single, res) = mmap
                .write_exact(0, |val: &mut u64| {
                    *val = 0xAAAA;
                    42u32
                })
                .unwrap();
            assert_eq!(res, 42);
            assert_eq!(ticket_single.epoch(), 1);
            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0xAAAA);

            let (ticket_multi, _) = mmap
                .write_exact(0, |node: &mut TestNode| {
                    node.id = 0x1111;
                    node.flags = 0x2222;
                    node.keys = [0x3333, 0x4444];
                })
                .unwrap();
            assert_eq!(ticket_multi.epoch(), 2);
            assert_eq!(
                mmap.read_exact(0, |n: &TestNode| *n).unwrap(),
                TestNode { id: 0x1111, flags: 0x2222, keys: [0x3333, 0x4444] }
            );

            let (ticket_partial, _) = mmap
                .write_exact(2, |keys: &mut [u64; 2]| {
                    keys[0] = 0x5555;
                    keys[1] = 0x6666;
                })
                .unwrap();
            assert_eq!(ticket_partial.epoch(), 3);
            assert_eq!(mmap.read(2, |v| *v).unwrap(), 0x5555);
            assert_eq!(mmap.read(3, |v| *v).unwrap(), 0x6666);
        }

        #[test]
        fn err_write_exact_invariants() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            let err_zro = mmap.write_exact::<(), _, _>(0, |_| ()).unwrap_err();
            assert_eq!(err_zro.reason, err::ZRO.reason);

            let err_sze = mmap.write_exact::<TestBadSizeTarget, _, _>(0, |_| ()).unwrap_err();
            assert_eq!(err_sze.reason, err::SZE.reason);

            let err_aln = mmap.write_exact::<TestOveralignedTarget, _, _>(0, |_| ()).unwrap_err();
            assert_eq!(err_aln.reason, err::ALN.reason);

            let err_inv = mmap.write_exact::<u64, _, _>(usize::MAX, |_| ()).unwrap_err();
            assert_eq!(err_inv.reason, err::INV.reason);

            let err_inv_add =
                mmap.write_exact::<TestNode, _, _>(usize::MAX - 1, |_| ()).unwrap_err();
            assert_eq!(err_inv_add.reason, err::INV.reason);

            let err_hcf_start = mmap.write_exact::<u64, _, _>(INIT_T, |_| ()).unwrap_err();
            assert_eq!(err_hcf_start.reason, err::HCF.reason);

            let err_hcf_end = mmap.write_exact::<TestNode, _, _>(INIT_T - 2, |_| ()).unwrap_err();
            assert_eq!(err_hcf_end.reason, err::HCF.reason);
        }

        #[test]
        fn ok_write_exact_concurrent_writers() {
            let (_dir, mut cfg) = tmp_cfg();
            cfg.initial_t = 8;
            let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());

            let barrier = Arc::new(std::sync::Barrier::new(2));
            let mut handles = Vec::new();

            {
                let mmap_clone = Arc::clone(&mmap);
                let barrier_clone = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier_clone.wait();
                    for i in 0..0x1F4 {
                        let _ = mmap_clone
                            .write_exact(0, |node: &mut TestNode| {
                                node.id = i as u64;
                                node.flags = (i as u64) + 1;
                                node.keys = [(i as u64) + 2, (i as u64) + 3];
                            })
                            .unwrap();
                    }
                }));
            }

            {
                let mmap_clone = Arc::clone(&mmap);
                let barrier_clone = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier_clone.wait();
                    for i in 0..0x1F4 {
                        let _ = mmap_clone
                            .write_exact(4, |node: &mut TestNode| {
                                node.id = (i as u64) * 10;
                                node.flags = (i as u64) * 10 + 1;
                                node.keys = [(i as u64) * 10 + 2, (i as u64) * 10 + 3];
                            })
                            .unwrap();
                    }
                }));
            }

            for h in handles {
                h.join().unwrap();
            }

            let node0 = mmap.read_exact(0, |n: &TestNode| *n).unwrap();
            assert_eq!(node0.id, 0x1F3);
            assert_eq!(node0.flags, 0x1F4);
            assert_eq!(node0.keys, [0x1F5, 0x1F6]);

            let node1 = mmap.read_exact(4, |n: &TestNode| *n).unwrap();
            assert_eq!(node1.id, 0x1F3 * 10);
            assert_eq!(node1.flags, 0x1F3 * 10 + 1);
            assert_eq!(node1.keys, [0x1F3 * 10 + 2, 0x1F3 * 10 + 3]);
        }

        #[test]
        fn ok_write_exact_concurrent_readers_writers_isolation() {
            let (_dir, mut cfg) = tmp_cfg();
            cfg.initial_t = 8;
            let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());

            let barrier = Arc::new(std::sync::Barrier::new(5));
            let mut handles = Vec::new();

            {
                let mmap_clone = Arc::clone(&mmap);
                let barrier_clone = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier_clone.wait();
                    for i in 0..0x1F4 {
                        let v = i as u64;
                        let _ = mmap_clone
                            .write_exact(0, |pair: &mut [u64; 2]| {
                                pair[0] = v;
                                pair[1] = v;
                            })
                            .unwrap();
                    }
                }));
            }

            for _ in 0..4 {
                let mmap_clone = Arc::clone(&mmap);
                let barrier_clone = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier_clone.wait();
                    for _ in 0..0x1F4 {
                        mmap_clone
                            .read_exact(0, |pair: &[u64; 2]| {
                                assert_eq!(pair[0], pair[1]);
                            })
                            .unwrap();
                    }
                }));
            }

            for h in handles {
                h.join().unwrap();
            }
        }

        #[test]
        fn ok_write_exact_unwind_releases_locks() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            let panic_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = mmap.write_exact(0, |_pair: &mut [u64; 2]| {
                    panic!("intentional panic inside write_exact callback");
                });
            }));
            assert!(panic_res.is_err());

            let ticket = mmap.write(&0x9999, 0).unwrap();
            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x9999);
            assert_eq!(ticket.epoch(), 1);

            let ticket2 = mmap.write(&0x8888, 1).unwrap();
            assert_eq!(mmap.read(1, |v| *v).unwrap(), 0x8888);
            assert_eq!(ticket2.epoch(), 2);
        }

        #[test]
        fn ok_write_exact_durability() {
            let (_dir, cfg) = tmp_cfg();
            {
                let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
                let (ticket, _) = mmap
                    .write_exact(0, |node: &mut TestNode| {
                        node.id = 0xDEAD;
                        node.flags = 0xBEEF;
                        node.keys = [0xCAFE, 0xBABE];
                    })
                    .unwrap();
                mmap.inner.sync_internal().unwrap();
                assert_eq!(ticket.epoch(), 1);
            }

            let reopened = MemMap::<u64>::open(cfg).unwrap();
            assert_eq!(
                reopened.read_exact(0, |n: &TestNode| *n).unwrap(),
                TestNode { id: 0xDEAD, flags: 0xBEEF, keys: [0xCAFE, 0xBABE] }
            );
        }
    }

    #[test]
    fn ok_durability_across_reopen() {
        let (_dir, cfg) = tmp_cfg();
        {
            let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
            mmap.write(&0xCAFE_BABE_1234_5678, 1).unwrap();
            mmap.inner.sync_internal().unwrap();
        }

        let reopened = MemMap::<u64>::open(cfg).unwrap();
        assert_eq!(reopened.read(1, |v| *v).unwrap(), 0xCAFE_BABE_1234_5678);
    }

    #[test]
    fn ok_background_sync_advances_epoch() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.sync_interval = Some(Duration::from_millis(0x19));

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
        cfg.sync_interval = Some(Duration::from_secs(0x0A));

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
        cfg.sync_interval = Some(Duration::from_secs(0x1E));

        {
            let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
            let _ticket = mmap.write(&0xBEEF_CAFE, 0).unwrap();
            drop(mmap);
        }

        let reopened = MemMap::<u64>::open(cfg).unwrap();
        assert_eq!(reopened.read(0, |v| *v).unwrap(), 0xBEEF_CAFE);
    }

    #[test]
    fn ok_concurrent_slot_locking() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.initial_t = 8;
        cfg.sync_interval = Some(Duration::from_secs(0x3C));

        let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();

        // 4 writer threads, each writing to index 0 repeatedly
        for t in 0..4 {
            let mmap_clone = Arc::clone(&mmap);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                for i in 0..0x1F4 {
                    let val = ((t + 1) as u64) << 0x20 | (i as u64);
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
                for _ in 0..0x3E8 {
                    let val = mmap_clone.read(0, |v| *v).unwrap();
                    let thread_id = (val >> 0x20) as usize;
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
                for _ in 0..0x1F4 {
                    mmap_clone.read(0, |_| ()).unwrap();
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
        assert_eq!(m2.read(0, |v| *v).unwrap(), 0x1122_3344);
        assert!(m2.delete().is_ok());
    }

    #[test]
    fn ok_delete_and_recreate_cycle() {
        let (_dir, cfg) = tmp_cfg();
        for i in 0..4 {
            let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
            let val = (i as u64 + 1) * 0xAA55;
            mmap.write(&val, 0).unwrap();
            assert_eq!(mmap.read(0, |v| *v).unwrap(), val);
            assert!(mmap.delete().is_ok());
            assert!(!cfg.versioned_path().exists());
        }
    }

    #[test]
    fn ok_concurrent_distinct_slots_no_contention() {
        let (_dir, mut cfg) = tmp_cfg();
        const THREADS: usize = 8;
        cfg.initial_t = THREADS;
        cfg.sync_interval = Some(Duration::from_secs(0x3C));

        let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(THREADS));
        let mut handles = Vec::with_capacity(THREADS);

        for t in 0..THREADS {
            let mmap_clone = Arc::clone(&mmap);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                for i in 0..0x3E8 {
                    let val = (t as u64) << 0x20 | (i as u64);
                    let _ = mmap_clone.write(&val, t).unwrap();
                    let read_val = mmap_clone.read(t, |v| *v).unwrap();
                    assert_eq!(read_val >> 0x20, t as u64);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        for t in 0..THREADS {
            let val = mmap.read(t, |v| *v).unwrap();
            assert_eq!(val >> 0x20, t as u64);
        }
    }

    #[test]
    fn ok_stress_interleaved_reads_writes() {
        let (_dir, mut cfg) = tmp_cfg();
        const NUM_SLOTS: usize = 0x10;
        const NUM_THREADS: usize = 8;
        cfg.initial_t = NUM_SLOTS;
        cfg.sync_interval = Some(Duration::from_millis(0x32));

        let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(NUM_THREADS));
        let mut handles = Vec::with_capacity(NUM_THREADS);

        for t in 0..NUM_THREADS {
            let mmap_clone = Arc::clone(&mmap);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                for i in 0..0x5DC {
                    let slot = (t + i) % NUM_SLOTS;
                    if i % 2 == 0 {
                        let val = ((t + 1) as u64) << 0x20 | (i as u64);
                        let _ = mmap_clone.write(&val, slot).unwrap();
                    } else {
                        mmap_clone.read(slot, |_| ()).unwrap();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert!(mmap.inner.sync_internal().is_ok());
    }

    #[test]
    fn ok_concurrent_writers_with_background_sync() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.initial_t = 8;
        cfg.sync_interval = Some(Duration::from_millis(5));

        let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let mut handles = Vec::new();

        for t in 0..4 {
            let mmap_clone = Arc::clone(&mmap);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                let mut tickets = Vec::with_capacity(0x32);
                for i in 0..0x32 {
                    let val = (t as u64) * 0x3E8 + i as u64;
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

        assert!(mmap.durable_epoch() >= mmap.inner.completion.read_current_epoch());
    }

    #[test]
    fn ok_concurrent_ticket_force_stress() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.initial_t = 8;
        cfg.sync_interval = Some(Duration::from_secs(0x1E));

        let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let mut handles = Vec::new();

        for t in 0..4 {
            let mmap_clone = Arc::clone(&mmap);
            let barrier_clone = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier_clone.wait();
                for i in 0..0x14 {
                    let val = (t as u64) * 0x64 + i as u64;
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

        assert!(mmap.write(&0xF00D, INIT_T - 1).is_ok());
        assert_eq!(mmap.read(INIT_T - 1, |v| *v).unwrap(), 0xF00D);

        assert_eq!(mmap.write(&0xBAD, INIT_T).unwrap_err().reason, err::HCF.reason);
        assert_eq!(mmap.read(INIT_T, |_| ()).unwrap_err().reason, err::HCF.reason);
    }

    #[test]
    fn ok_large_slot_count_initialization() {
        let (_dir, mut cfg) = tmp_cfg();
        const LARGE_COUNT: usize = 0x2710;
        cfg.initial_t = LARGE_COUNT;

        let mmap = MemMap::<u64>::new(cfg).unwrap();
        assert_eq!(mmap.length(), LARGE_COUNT * core::mem::size_of::<u64>());

        assert!(mmap.write(&1, 0).is_ok());
        assert!(mmap.write(&0x1388, 0x1388).is_ok());
        assert!(mmap.write(&0x270F, LARGE_COUNT - 1).is_ok());

        assert_eq!(mmap.read(0, |v| *v).unwrap(), 1);
        assert_eq!(mmap.read(0x1388, |v| *v).unwrap(), 0x1388);
        assert_eq!(mmap.read(LARGE_COUNT - 1, |v| *v).unwrap(), 0x270F);
    }

    #[test]
    fn ok_new_with_sync_interval_none() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.sync_interval = None;

        let mmap = MemMap::<u64>::new(cfg).unwrap();
        assert!(mmap.bg_thread.is_none());

        let ticket = mmap.write(&0x55AA, 0).unwrap();
        assert!(!ticket.is_durable());

        mmap.sync().unwrap();
        assert!(ticket.is_durable());
        assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x55AA);
    }

    #[test]
    fn ok_ticket_force_with_sync_interval_none() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.sync_interval = None;

        let mmap = MemMap::<u64>::new(cfg).unwrap();
        assert!(mmap.bg_thread.is_none());

        let ticket = mmap.write(&0x1122, 1).unwrap();
        assert!(!ticket.is_durable());

        let epoch = ticket.force().unwrap();
        assert_eq!(epoch, ticket.epoch());
        assert!(ticket.is_durable());
        assert!(mmap.durable_epoch() >= ticket.epoch());
    }

    #[test]
    fn ok_drop_with_sync_interval_none() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.sync_interval = None;

        {
            let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
            assert!(mmap.bg_thread.is_none());
            let _ticket = mmap.write(&0xCAFE_F00D, 0).unwrap();
            drop(mmap);
        }

        let reopened = MemMap::<u64>::open(cfg).unwrap();
        assert_eq!(reopened.read(0, |v| *v).unwrap(), 0xCAFE_F00D);
    }

    #[test]
    fn ok_bg_worker_parks_when_idle_and_unparks_on_write() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.sync_interval = Some(Duration::from_millis(0x0F));

        let mmap = MemMap::<u64>::new(cfg).unwrap();

        // Initially without writes, the worker thread should park within a short delay
        let start = std::time::Instant::now();
        while !mmap.is_parked() && start.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(mmap.is_parked());

        // First write should unpark the worker
        let ticket = mmap.write(&0x9999, 0).unwrap();
        assert!(!mmap.is_parked());

        // Ticket should become durable via the unparked background thread
        let epoch = ticket.wait().unwrap();
        assert_eq!(epoch, ticket.epoch());
        assert!(ticket.is_durable());
        assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x9999);

        // After flushing, worker should re-park when no new writes arrive
        let start = std::time::Instant::now();
        while !mmap.is_parked() && start.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(mmap.is_parked());
    }

    #[test]
    fn ok_batched_writes_while_active_remain_unparked() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.sync_interval = Some(Duration::from_millis(0x1E));

        let mmap = MemMap::<u64>::new(cfg).unwrap();

        // Wait until parked
        let start = std::time::Instant::now();
        while !mmap.is_parked() && start.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(mmap.is_parked());

        // First write unparks
        let t1 = mmap.write(&0x1111, 0).unwrap();
        assert!(!mmap.is_parked());

        // Subsequent rapid writes find worker already unparked (is_parked == false)
        let t2 = mmap.write(&0x2222, 1).unwrap();
        let t3 = mmap.write(&0x3333, 2).unwrap();
        assert!(!mmap.is_parked());

        t1.wait().unwrap();
        t2.wait().unwrap();
        t3.wait().unwrap();

        assert!(t1.is_durable());
        assert!(t2.is_durable());
        assert!(t3.is_durable());
        assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x1111);
        assert_eq!(mmap.read(1, |v| *v).unwrap(), 0x2222);
        assert_eq!(mmap.read(2, |v| *v).unwrap(), 0x3333);
    }

    #[test]
    fn ok_concurrent_stress_park_unpark_no_lost_wakeups() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.initial_t = 0x10;
        cfg.sync_interval = Some(Duration::from_millis(5));

        let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());
        const THREADS: usize = 4;
        const WRITES_PER_THREAD: usize = 0x19;
        let mut handles = Vec::with_capacity(THREADS);

        for t in 0..THREADS {
            let mmap_clone = Arc::clone(&mmap);
            handles.push(std::thread::spawn(move || {
                let mut tickets = Vec::with_capacity(WRITES_PER_THREAD);
                for i in 0..WRITES_PER_THREAD {
                    let val = (t as u64) << 0x20 | (i as u64);
                    let slot = t;
                    let ticket = mmap_clone.write(&val, slot).unwrap();
                    tickets.push(ticket);

                    // Alternate between short sleep (allowing worker to park) and rapid writes
                    if i % 5 == 0 {
                        std::thread::sleep(Duration::from_millis(0x0F));
                    }
                }

                for ticket in tickets {
                    ticket.wait().unwrap();
                    assert!(ticket.is_durable());
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert!(mmap.durable_epoch() >= mmap.inner.completion.read_current_epoch());
    }

    #[test]
    fn ok_spurious_wakeup_resumes_park_immediately() {
        let (_dir, mut cfg) = tmp_cfg();
        cfg.sync_interval = Some(Duration::from_secs(0x0A)); // Long interval

        let mmap = MemMap::<u64>::new(cfg).unwrap();

        // Wait until parked
        let start = std::time::Instant::now();
        while !mmap.is_parked() && start.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(mmap.is_parked());

        // Send spurious condvar notification without any writes
        mmap.inner.sync_condvar.notify_one();
        std::thread::sleep(Duration::from_millis(0x0A));

        // Because no writes occurred, worker should detect spurious wakeup and re-park immediately rather than sleeping
        // for the 10-second interval
        let start = std::time::Instant::now();
        while !mmap.is_parked() && start.elapsed() < Duration::from_millis(0x1F4) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(mmap.is_parked());
    }

    #[test]
    fn ok_delete_cleans_up_file_when_mmap_already_unmapped() {
        let (_dir, cfg) = tmp_cfg();
        let path = cfg.versioned_path();
        let mmap = MemMap::<u64>::new(cfg).unwrap();
        assert!(path.exists());

        // Simulate mmap having been taken/unmapped prior to delete
        {
            let mut guard = mmap.inner.mmap.write().unwrap();
            let _ = guard.take();
        }

        // delete should still successfully clean up the backing file on disk
        assert!(mmap.delete().is_ok());
        assert!(!path.exists());
    }

    mod memmap_grow {
        use super::*;

        #[test]
        fn ok_grow_updates_length() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();
            let initial_len = INIT_T * core::mem::size_of::<u64>();
            assert_eq!(mmap.length(), initial_len);

            mmap.grow(4).unwrap();
            assert_eq!(mmap.length(), initial_len + 4 * core::mem::size_of::<u64>());
        }

        #[test]
        fn ok_grow_zero_count() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();
            let initial_len = mmap.length();

            mmap.grow(0).unwrap();
            assert_eq!(mmap.length(), initial_len);
        }

        #[test]
        fn ok_grow_extends_with_zero() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();
            mmap.grow(3).unwrap();

            for idx in INIT_T..(INIT_T + 3) {
                assert_eq!(mmap.read(idx, |v| *v).unwrap(), 0u64);
            }
        }

        #[test]
        fn ok_grow_preserves_existing_data() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            for i in 0..INIT_T {
                mmap.write(&(0xCAFE_0000 + i as u64), i).unwrap();
            }

            mmap.grow(4).unwrap();

            for i in 0..INIT_T {
                assert_eq!(mmap.read(i, |v| *v).unwrap(), 0xCAFE_0000 + i as u64);
            }

            for i in INIT_T..(INIT_T + 4) {
                mmap.write(&(0xBEEF_0000 + i as u64), i).unwrap();
                assert_eq!(mmap.read(i, |v| *v).unwrap(), 0xBEEF_0000 + i as u64);
            }
        }

        #[test]
        fn ok_grow_syncs_dirty_pages_when_curr_epoch_gt_durable() {
            let (_dir, mut cfg) = tmp_cfg();
            cfg.sync_interval = None; // Manual sync mode

            let mmap = MemMap::<u64>::new(cfg).unwrap();
            let ticket = mmap.write(&0x1234_5678u64, 1).unwrap();

            assert!(!ticket.is_durable());
            assert!(mmap.current_epoch() > mmap.durable_epoch());

            // grow should trigger durability sync before re-mapping
            mmap.grow(2).unwrap();

            assert!(ticket.is_durable());
            assert!(mmap.durable_epoch() >= ticket.epoch());
        }

        #[test]
        fn ok_grow_when_already_durable() {
            let (_dir, mut cfg) = tmp_cfg();
            cfg.sync_interval = None;

            let mmap = MemMap::<u64>::new(cfg).unwrap();
            let ticket = mmap.write(&0x9999u64, 0).unwrap();
            mmap.sync().unwrap();
            assert!(ticket.is_durable());
            assert_eq!(mmap.current_epoch(), mmap.durable_epoch());

            mmap.grow(2).unwrap();
            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x9999u64);
        }

        #[test]
        fn err_grow_overflow_usize() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            let err = mmap.grow(usize::MAX).unwrap_err();
            assert_eq!(err.reason, err::GRW.reason);
        }

        #[test]
        fn ok_grow_multiple_times() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            mmap.write(&0x0Au64, 0).unwrap();
            mmap.grow(2).unwrap();
            mmap.write(&0x14u64, INIT_T).unwrap();
            mmap.grow(3).unwrap();
            mmap.write(&0x1Eu64, INIT_T + 2).unwrap();

            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x0Au64);
            assert_eq!(mmap.read(INIT_T, |v| *v).unwrap(), 0x14u64);
            assert_eq!(mmap.read(INIT_T + 2, |v| *v).unwrap(), 0x1Eu64);
            assert_eq!(mmap.length(), (INIT_T + 5) * core::mem::size_of::<u64>());
        }

        #[test]
        fn ok_concurrent_grow_and_reads_writes() {
            let (_dir, mut cfg) = tmp_cfg();
            cfg.sync_interval = Some(Duration::from_millis(0x0A));
            let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());

            let num_writers = 4;
            let mut handles = Vec::new();

            for w in 0..num_writers {
                let m = Arc::clone(&mmap);
                handles.push(std::thread::spawn(move || {
                    for i in 0..0x32 {
                        let _ = m.write(&((w * 0x3E8 + i) as u64), w % INIT_T);
                        m.read(w % INIT_T, |_| ()).unwrap();
                    }
                }));
            }

            let m_grower = Arc::clone(&mmap);
            let grower_handle = std::thread::spawn(move || {
                for _ in 0..3 {
                    std::thread::sleep(Duration::from_millis(5));
                    m_grower.grow(2).unwrap();
                }
            });

            for h in handles {
                h.join().unwrap();
            }
            grower_handle.join().unwrap();

            assert_eq!(mmap.length(), (INIT_T + 6) * core::mem::size_of::<u64>());
        }
    }

    mod thread_failure_lifecycle {
        use super::*;

        #[test]
        fn err_new_failure_paths_spawn_no_sync_thread() {
            let (_dir, cfg) = tmp_cfg();
            // Pre-create file to force EXS error
            std::fs::write(cfg.versioned_path(), b"preexisting").unwrap();

            let res = MemMap::<u64>::new(cfg);
            assert!(res.is_err());
            // No MemMap instance was created, and no background sync thread was spawned
        }

        #[test]
        fn err_open_failure_paths_spawn_no_sync_thread() {
            let (_dir, mut cfg) = tmp_cfg();
            cfg.path = std::path::PathBuf::from("/non/existent/path/memmap.bin");

            let res = MemMap::<u64>::open(cfg);
            assert!(res.is_err());
            // No MemMap instance was created, and no background sync thread was spawned
        }

        #[test]
        fn ok_thread_shutdown_on_panic_unwinding() {
            let (_dir, cfg) = tmp_cfg();
            let thread_inner = Arc::new(atomic::AtomicBool::new(false));
            let thread_inner_clone = Arc::clone(&thread_inner);

            // Spawn worker thread that panics with MemMap in scope
            let handle = std::thread::spawn(move || {
                let mmap = MemMap::<u64>::new(cfg).unwrap();
                let _inner = Arc::clone(&mmap.inner);
                thread_inner_clone.store(true, atomic::Ordering::Release);
                // Trigger panic while mmap is in scope
                panic!("intentional test panic to trigger Drop unwinding");
            });

            assert!(handle.join().is_err());
            assert!(thread_inner.load(atomic::Ordering::Acquire));
            // Stack unwinding drops MemMap, setting shutdown = true and joining the background worker
        }

        #[test]
        fn ok_thread_shutdown_in_delete_even_if_already_unmapped() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            // Simulate mmap having been taken/unmapped prior to delete
            {
                let mut guard = mmap.inner.mmap.write().unwrap();
                let _ = guard.take();
            }

            // delete shuts down and joins bg_thread before performing unmap and file deletion
            assert!(mmap.delete().is_ok());
        }

        #[test]
        fn ok_grow_failure_keeps_thread_healthy() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            assert!(mmap.grow(usize::MAX).is_err());

            // The instance and background sync worker remain healthy
            let ticket = mmap.write(&0x2Au64, 0).unwrap();
            mmap.sync().unwrap();
            assert!(ticket.is_durable());
            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x2Au64);
        }
    }

    mod memmap_transactions {
        use super::*;

        #[test]
        fn ok_single_slot_cas_success_and_conflict() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0);

            let t1 = mmap.compare_and_write(&0, &0x64, 0).unwrap();
            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x64);
            assert_eq!(mmap.current_epoch(), 1);

            let err = mmap.compare_and_write(&0, &0xC8, 0).unwrap_err();
            assert_eq!(err.reason, err::CAS.reason);
            assert!(err.context.contains("slot data has changed"));
            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x64);
            assert_eq!(mmap.current_epoch(), 1); // Epoch does not advance on failure

            let t2 = mmap.compare_and_write(&0x64, &0xC8, 0).unwrap();
            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0xC8);
            assert_eq!(mmap.current_epoch(), 2);
            assert!(t2.epoch() > t1.epoch());
        }

        #[test]
        fn ok_tx_multiple_writes_single_epoch() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            let epoch_before = mmap.current_epoch();

            let mut tx = mmap.new_tx();
            assert!(tx.is_empty());
            tx.write(&0, &0x6F, 0).unwrap();
            tx.write(&0, &0xDE, 1).unwrap();
            tx.write(&0, &0x14D, 2).unwrap();
            assert_eq!(tx.len(), 3);

            let ticket = tx.commit().unwrap();
            let epoch_after = mmap.current_epoch();

            // Single epoch increment for all 3 writes
            assert_eq!(epoch_after, epoch_before + 1);
            assert_eq!(ticket.epoch(), epoch_after);

            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x6F);
            assert_eq!(mmap.read(1, |v| *v).unwrap(), 0xDE);
            assert_eq!(mmap.read(2, |v| *v).unwrap(), 0x14D);

            ticket.wait().unwrap();
            assert!(ticket.is_durable());
        }

        #[test]
        fn err_tx_cas_conflict_aborts_all_with_zero_side_effects() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            // Prepare slot 1 with a different value
            mmap.write(&0x3E7, 1).unwrap();
            let epoch_before = mmap.current_epoch();

            let mut tx = mmap.new_tx();
            tx.write(&0, &0x0A, 0).unwrap(); // Expected 0, currently 0 (would succeed)
            tx.write(&0, &0x14, 1).unwrap(); // Expected 0, currently 999 (will fail!)
            tx.write(&0, &0x1E, 2).unwrap(); // Expected 0, currently 0

            let err = tx.commit().unwrap_err();
            assert_eq!(err.reason, err::CAS.reason);
            assert!(err.context.contains("state conflict at index 1"));

            // Verify NONE of the slots were modified (all-or-nothing rollback)
            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0);
            assert_eq!(mmap.read(1, |v| *v).unwrap(), 0x3E7);
            assert_eq!(mmap.read(2, |v| *v).unwrap(), 0);

            // Epoch must NOT have advanced
            assert_eq!(mmap.current_epoch(), epoch_before);
        }

        #[test]
        fn ok_tx_retry_after_conflict() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            // Simulate concurrent write modifying slot 1
            mmap.write(&0x2A, 1).unwrap();

            // First attempt assuming slot 1 is 0 fails
            let mut tx1 = mmap.new_tx();
            tx1.write(&0, &0x64, 0).unwrap();
            tx1.write(&0, &0xC8, 1).unwrap();
            assert_eq!(tx1.commit().unwrap_err().reason, err::CAS.reason);

            // Caller observes current slot 1 value is 42, retries transaction with updated precondition
            let curr_val = mmap.read(1, |v| *v).unwrap();
            assert_eq!(curr_val, 0x2A);

            let mut tx2 = mmap.new_tx();
            tx2.write(&0, &0x64, 0).unwrap();
            tx2.write(&0x2A, &0xC8, 1).unwrap();
            let ticket = tx2.commit().unwrap();

            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x64);
            assert_eq!(mmap.read(1, |v| *v).unwrap(), 0xC8);
            ticket.wait().unwrap();
            assert!(ticket.is_durable());
        }

        #[test]
        fn err_tx_out_of_order() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            let mut tx = mmap.new_tx();
            tx.write(&0, &0x0A, 2).unwrap();
            let err = tx.write(&0, &0x14, 1).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
            assert!(err.context.contains("strictly ordered"));
        }

        #[test]
        fn err_tx_duplicate_index() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            let mut tx = mmap.new_tx();
            tx.write(&0, &0x0A, 1).unwrap();
            let err = tx.write(&0, &0x14, 1).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
            assert!(err.context.contains("strictly ordered"));
        }

        #[test]
        fn err_tx_out_of_bounds() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            let mut tx = mmap.new_tx();
            let err = tx.write(&0, &0x0A, INIT_T).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
        }

        #[test]
        fn err_tx_empty_commit() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            let tx = mmap.new_tx();
            let err = tx.commit().unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
            assert!(err.context.contains("empty transaction"));
        }

        #[test]
        fn ok_tx_discard_on_drop() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            let epoch_before = mmap.current_epoch();
            {
                let mut tx = mmap.new_tx();
                tx.write(&0, &0x309, 0).unwrap();
                tx.write(&0, &0x378, 1).unwrap();
                // tx dropped without calling commit()
            }

            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0);
            assert_eq!(mmap.read(1, |v| *v).unwrap(), 0);
            assert_eq!(mmap.current_epoch(), epoch_before);
        }

        #[test]
        fn ok_tx_concurrent_non_overlapping() {
            let (_dir, mut cfg) = tmp_cfg();
            cfg.initial_t = 0x10;
            let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());

            let mut handles = Vec::new();
            for thread_id in 0..4u64 {
                let m = mmap.clone();
                handles.push(std::thread::spawn(move || {
                    let base = (thread_id as usize) * 4;
                    let mut tx = m.new_tx();
                    tx.write(&0, &(thread_id * 0x0A + 1), base).unwrap();
                    tx.write(&0, &(thread_id * 0x0A + 2), base + 1).unwrap();
                    tx.write(&0, &(thread_id * 0x0A + 3), base + 2).unwrap();
                    tx.write(&0, &(thread_id * 0x0A + 4), base + 3).unwrap();
                    let ticket = tx.commit().unwrap();
                    ticket.wait().unwrap();
                }));
            }

            for h in handles {
                h.join().unwrap();
            }

            for thread_id in 0..4u64 {
                let base = (thread_id as usize) * 4;
                assert_eq!(mmap.read(base, |v| *v).unwrap(), thread_id * 0x0A + 1);
                assert_eq!(mmap.read(base + 1, |v| *v).unwrap(), thread_id * 0x0A + 2);
                assert_eq!(mmap.read(base + 2, |v| *v).unwrap(), thread_id * 0x0A + 3);
                assert_eq!(mmap.read(base + 3, |v| *v).unwrap(), thread_id * 0x0A + 4);
            }
        }

        #[test]
        fn ok_tx_durability_across_reopen() {
            let (_dir, cfg) = tmp_cfg();

            {
                let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
                let mut tx = mmap.new_tx();
                tx.write(&0, &0xAAAA_BBBB, 0).unwrap();
                tx.write(&0, &0xCCCC_DDDD, 1).unwrap();
                let ticket = tx.commit().unwrap();
                ticket.wait().unwrap();
                assert!(ticket.is_durable());
            }

            {
                let reopened = MemMap::<u64>::open(cfg).unwrap();
                assert_eq!(reopened.read(0, |v| *v).unwrap(), 0xAAAA_BBBB);
                assert_eq!(reopened.read(1, |v| *v).unwrap(), 0xCCCC_DDDD);
            }
        }

        #[test]
        fn ok_tx_unparks_idle_bg_worker() {
            let (_dir, cfg) = tmp_cfg();
            let mmap = MemMap::<u64>::new(cfg).unwrap();

            // Wait for worker to park
            let mut parked = false;
            for _ in 0..0x64 {
                if mmap.is_parked() {
                    parked = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(0x0A));
            }
            assert!(parked);

            let mut tx = mmap.new_tx();
            tx.write(&0, &0x22B, 0).unwrap();
            let ticket = tx.commit().unwrap();

            // Background worker was unparked and should make ticket durable
            ticket.wait().unwrap();
            assert!(ticket.is_durable());
            assert_eq!(mmap.read(0, |v| *v).unwrap(), 0x22B);
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(C)]
        struct TxNode {
            id: u64,
            flags: u64,
            keys: [u64; 2],
        }

        #[test]
        fn ok_concurrent_tx_read_exact_and_write_exact_same_slots() {
            let (_dir, mut cfg) = tmp_cfg();
            cfg.initial_t = 8;
            let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());

            let barrier = Arc::new(std::sync::Barrier::new(6));
            let mut handles = Vec::new();

            for t in 0..2 {
                let m = Arc::clone(&mmap);
                let b = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    b.wait();
                    for i in 0..0xC8 {
                        let val = ((t + 1) as u64) << 0x20 | (i as u64);
                        let _ = m
                            .write_exact(0, |node: &mut TxNode| {
                                node.id = val;
                                node.flags = val;
                                node.keys = [val, val];
                            })
                            .unwrap();
                    }
                }));
            }

            for t in 2..4 {
                let m = Arc::clone(&mmap);
                let b = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    b.wait();
                    for i in 0..0xC8 {
                        let new_val = ((t + 1) as u64) << 0x20 | (i as u64);
                        let current_node = m.read_exact(0, |n: &TxNode| *n).unwrap();
                        let mut tx = m.new_tx();
                        if tx.write(&current_node.id, &new_val, 0).is_ok()
                            && tx.write(&current_node.flags, &new_val, 1).is_ok()
                            && tx.write(&current_node.keys[0], &new_val, 2).is_ok()
                            && tx.write(&current_node.keys[1], &new_val, 3).is_ok()
                        {
                            let _ = tx.commit();
                        }
                    }
                }));
            }

            for _ in 0..2 {
                let m = Arc::clone(&mmap);
                let b = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    b.wait();
                    for _ in 0..0x190 {
                        m.read_exact(0, |node: &TxNode| {
                            assert_eq!(node.id, node.flags);
                            assert_eq!(node.id, node.keys[0]);
                            assert_eq!(node.id, node.keys[1]);
                        })
                        .unwrap();
                    }
                }));
            }

            for h in handles {
                h.join().unwrap();
            }

            let final_node = mmap.read_exact(0, |n: &TxNode| *n).unwrap();
            assert_eq!(final_node.id, final_node.flags);
            assert_eq!(final_node.id, final_node.keys[0]);
            assert_eq!(final_node.id, final_node.keys[1]);
        }

        #[test]
        fn ok_tx_retry_loop_against_concurrent_write_exact_and_read_exact() {
            let (_dir, mut cfg) = tmp_cfg();
            cfg.initial_t = 8;
            let mmap = Arc::new(MemMap::<u64>::new(cfg).unwrap());

            let barrier = Arc::new(std::sync::Barrier::new(4));
            let mut handles = Vec::new();

            {
                let m = Arc::clone(&mmap);
                let b = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    b.wait();
                    for i in 0..0x64 {
                        let _ = m
                            .write_exact(0, |pair: &mut [u64; 2]| {
                                pair[0] = i as u64;
                                pair[1] = i as u64;
                            })
                            .unwrap();
                    }
                }));
            }

            {
                let m = Arc::clone(&mmap);
                let b = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    b.wait();
                    let mut successful_commits = 0;
                    while successful_commits < 10 {
                        let current = m.read_exact(0, |pair: &[u64; 2]| *pair).unwrap();
                        let new_val = 0x8000_0000 + successful_commits;
                        let mut tx = m.new_tx();
                        if tx.write(&current[0], &new_val, 0).is_ok()
                            && tx.write(&current[1], &new_val, 1).is_ok()
                            && tx.commit().is_ok()
                        {
                            successful_commits += 1;
                        }
                    }
                }));
            }

            for _ in 0..2 {
                let m = Arc::clone(&mmap);
                let b = Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    b.wait();
                    for _ in 0..0xC8 {
                        m.read_exact(0, |pair: &[u64; 2]| {
                            assert_eq!(pair[0], pair[1]);
                        })
                        .unwrap();
                    }
                }));
            }

            for h in handles {
                h.join().unwrap();
            }

            mmap.read_exact(0, |pair: &[u64; 2]| {
                assert_eq!(pair[0], pair[1]);
            })
            .unwrap();
        }
    }

    mod memmap_page_size {
        use super::*;

        #[test]
        fn ok_page_size_returns_valid_power_of_two() {
            let page_size = MemMap::<u64>::page_size().unwrap();
            assert!(page_size >= 0x1000);
            assert!(page_size.is_power_of_two());
        }

        #[test]
        fn ok_page_size_consistency() {
            let ps1 = MemMap::<u64>::page_size().unwrap();
            let ps2 = MemMap::<u64>::page_size().unwrap();
            let ps3 = MemMap::<ValidT>::page_size().unwrap();
            assert_eq!(ps1, ps2);
            assert_eq!(ps1, ps3);
        }

        #[test]
        fn ok_page_size_across_threads() {
            let expected = MemMap::<u64>::page_size().unwrap();
            let mut handles = Vec::new();

            for _ in 0..8 {
                handles.push(std::thread::spawn(move || {
                    let ps = MemMap::<u64>::page_size().unwrap();
                    assert_eq!(ps, expected);
                }));
            }

            for h in handles {
                h.join().unwrap();
            }
        }
    }
}
