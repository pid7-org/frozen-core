//! NA

#![allow(unused)]

use crate::{
    error::{BindModule, ErrCode, FrozenError, FrozenResult},
    file::{File, FileCfg},
};
use core::marker::PhantomData;

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

    /// Optional interval for the background sync thread
    ///
    /// If `Some(duration)`, a background worker thread is spawned which flushes dirty pages at this interval; and
    /// if `None`, background sync is disabled and durability advances via manual sync
    pub sync_interval: Option<std::time::Duration>,
}

impl MemMapCfg {
    /// Computes the versioned file path by appending `_{version}` to the file stem/name
    ///
    /// For example:
    /// - `PathBuf::from("/tmp/store.db")` with `version: 1` -> `/tmp/store_1.db`
    /// - `PathBuf::from("/tmp/store")` with `version: 2` -> `/tmp/store_2`
    #[inline]
    pub fn versioned_path(&self) -> std::path::PathBuf {
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

/// Custom memory-mapped file implementation managing an underlying [`File`]
///
/// ## Invariant: Constant Type `T` Across File Lifetime
///
/// The type `T` must never change across [`new`](MemMap::new), [`open`](MemMap::open), or
/// [`open_or_create`](MemMap::open_or_create) operations for the same underlying file
///
/// It should remain a constant type for the entire lifetime of the file
///
/// `core::mem::size_of::<T>()` is used as the internal buffer addressing unit
///
/// There are deliberately no runtime metadata safeguards or header checks to enforce this invariant;
/// upholding type consistency is left entirely to caller discipline as documented
pub struct MemMap<T> {
    cfg: MemMapCfg,
    mmap: PlatformMemMap,
    file: File,
    _marker: PhantomData<T>,
}

impl<T> core::fmt::Debug for MemMap<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MemMap")
            .field("cfg", &self.cfg)
            .field("mmap", &self.mmap)
            .field("file", &self.file)
            .finish()
    }
}

unsafe impl<T> Send for MemMap<T> {}
unsafe impl<T> Sync for MemMap<T> {}

impl<T> MemMap<T> {
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

    fn file_cfg(cfg: &MemMapCfg) -> FileCfg {
        FileCfg {
            module_id: cfg.module_id,
            path: cfg.versioned_path(),
            buffer_size: core::mem::size_of::<T>(),
            initial_available_buffers: cfg.initial_t,
            sync_interval: None,
        }
    }

    /// Creates and memory-maps a new [`File`] at `cfg.versioned_path()`
    ///
    /// ## Invariant: Constant Type `T`
    ///
    /// The type `T` must never change across the entire lifetime of the file
    ///
    /// No header safeguards exist on purpose; ensuring type consistency is caller responsibility
    ///
    /// ## TOCTAU Safe
    ///
    /// If the file already exists, an error is returned to guard against TOCTOU overwrites
    ///
    /// ## Exclusive Lock
    ///
    /// Acquires an exclusive advisory lock on the underlying descriptor
    ///
    /// ## Background Sync
    ///
    /// Internally configures the underlying [`File`] with `sync_interval: None`, delegating
    /// background sync management to [`MemMap`]
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("new_memmap.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: None,
    /// };
    ///
    /// let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
    /// assert_eq!(mmap.length(), std::mem::size_of::<u64>() * 4);
    ///
    /// // Creating another file at the same path fails
    /// assert!(MemMap::<u64>::new(cfg).is_err());
    /// ```
    pub fn new(cfg: MemMapCfg) -> FrozenResult<Self> {
        let mid = cfg.module_id;
        Self::validate_t(mid)?;

        let file_cfg = Self::file_cfg(&cfg);
        let file = File::new(file_cfg)?;

        let mmap = match PlatformMemMap::new(file.fd(), file.length()) {
            Ok(m) => m,
            Err(mut e) => {
                if let Err(del_err) = file.delete() {
                    e.add_suppressed(del_err);
                }
                return Err(e.with_module(mid));
            }
        };

        Ok(Self { cfg, mmap, file, _marker: PhantomData })
    }

    /// Open an existing [`MemMap`] while validating layout invariants
    ///
    /// ## Invariant: Constant Type `T`
    ///
    /// The type `T` must never change across the entire lifetime of the file
    ///
    /// No header safeguards existvon purpose; ensuring type consistency is caller responsibility
    ///
    /// ## Background Sync
    ///
    /// Internally configures the underlying [`File`] with `sync_interval: None`
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("open_memmap.bin"),
    ///     version: 1,
    ///     initial_t: 4,
    ///     sync_interval: None,
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
        let mid = cfg.module_id;
        Self::validate_t(mid)?;

        let file_cfg = Self::file_cfg(&cfg);
        let file = File::open(file_cfg)?;

        let mmap = PlatformMemMap::new(file.fd(), file.length()).with_module(mid)?;

        Ok(Self { cfg, mmap, file, _marker: PhantomData })
    }

    /// Open an existing [`MemMap`] or create it if it does not yet exist
    ///
    /// ## Invariant: Constant Type `T`
    ///
    /// The type `T` must never change across the entire lifetime of the file
    ///
    /// No header safeguards exist on purpose; ensuring type consistency is caller responsibility
    ///
    /// ## Background Sync
    ///
    /// Internally configures the underlying [`File`] with `sync_interval: None`
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::memmap::{MemMap, MemMapCfg};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cfg = MemMapCfg {
    ///     module_id: 1,
    ///     path: dir.path().join("open_or_create_memmap.bin"),
    ///     version: 1,
    ///     initial_t: 2,
    ///     sync_interval: None,
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
        let mid = cfg.module_id;
        Self::validate_t(mid)?;

        let file_cfg = Self::file_cfg(&cfg);
        let file = File::open_or_create(file_cfg)?;

        let mmap = PlatformMemMap::new(file.fd(), file.length()).with_module(mid)?;

        Ok(Self { cfg, mmap, file, _marker: PhantomData })
    }

    /// Returns the length of the memory mapping in bytes
    #[inline]
    pub fn length(&self) -> usize {
        self.mmap.length()
    }

    /// Get reference to configuration of [`MemMap`]
    #[inline]
    pub fn cfg(&self) -> &MemMapCfg {
        &self.cfg
    }

    /// Get reference to underlying [`File`]
    #[inline]
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Deletes the [`MemMap`] and underlying [`File`] entry from the storage device
    ///
    /// Consumes `self` by value to release the memory mapping and unlink the file
    pub fn delete(self) -> FrozenResult<()> {
        let mid = self.cfg.module_id;
        let MemMap { cfg: _, mmap, file, _marker: _ } = self;
        mmap.unmap().with_module(mid)?;
        file.delete().with_module(mid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MID: u8 = 0;
    const INIT_T: usize = 4;

    #[derive(Debug)]
    #[repr(C, align(8))]
    struct ValidT {
        a: u64,
        b: u64,
    }

    #[derive(Debug)]
    #[repr(C)]
    struct BadAlignT {
        a: u32,
        b: u32,
    }

    #[derive(Debug)]
    #[repr(C)]
    struct BadSizeT {
        a: [u8; 12],
    }

    #[derive(Debug)]
    #[repr(C, align(8))]
    struct DropT(u64);

    impl Drop for DropT {
        fn drop(&mut self) {}
    }

    #[derive(Debug)]
    #[repr(C, align(8))]
    struct ZstT;

    #[derive(Debug)]
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
            sync_interval: Some(Duration::from_millis(100)),
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
        assert!(cfg.sync_interval.is_some());

        let mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
        assert_eq!(mmap.length(), core::mem::size_of::<u64>() * INIT_T);
        assert_eq!(mmap.file().cfg().sync_interval, None);
        assert_eq!(mmap.cfg().sync_interval, Some(Duration::from_millis(100)));
    }

    #[test]
    fn err_new_when_file_exists() {
        let (_dir, cfg) = tmp_cfg();
        let _mmap = MemMap::<u64>::new(cfg.clone()).unwrap();
        assert!(MemMap::<u64>::new(cfg).is_err());
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
        assert!(MemMap::<u64>::open(cfg).is_err());
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
    fn ok_delete() {
        let (_dir, cfg) = tmp_cfg();
        let path = cfg.versioned_path();
        let mmap = MemMap::<u64>::new(cfg).unwrap();
        assert!(path.exists());
        assert!(mmap.delete().is_ok());
        assert!(!path.exists());
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
            sync_interval: None,
        };
        assert_eq!(cfg_v1.versioned_path(), dir.path().join("store_1.dat"));

        let cfg_v2 = MemMapCfg {
            module_id: MID,
            path: base_path.clone(),
            version: 2,
            initial_t: 4,
            sync_interval: None,
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
}
