//! Interface contract for platform-specific file backends

use crate::error::FrozenResult;
use std::path::Path;

/// Common interface contract implemented by platform-specific file backends
pub(in crate::file) trait FileInterface: Sized + Send + Sync {
    /// Associated type for the underlying operating system file descriptor or kernel handle
    type Id: Copy + Eq + PartialEq + core::fmt::Debug + Send + Sync;

    /// Sentinel placeholder value representing an invalid or already-closed descriptor/handle
    const CLOSED_ID: Self::Id;

    /// Returns the raw operating system file descriptor or kernel handle
    ///
    /// ## Concurrency
    ///
    /// Implementations should load the handle atomically using [`Acquire`](std::sync::atomic::Ordering::Acquire)
    /// ordering to ensure thread-safe descriptor inspection
    fn fd(&self) -> Self::Id;

    /// Returns `true` if the underlying descriptor or handle is currently closed
    ///
    /// Compares the current descriptor returned by [`fd`](FileInterface::fd) against
    /// [`CLOSED_ID`](FileInterface::CLOSED_ID)
    #[inline(always)]
    fn is_closed(&self) -> bool {
        self.fd() == Self::CLOSED_ID
    }

    /// Verifies the physical existence of a file at `path` on the storage device
    ///
    /// Returns `Ok(true)` if the file exists, `Ok(false)` if it does not, or an error if permission or
    /// path resolution fails
    fn exists(path: &Path) -> FrozenResult<bool>;

    /// Atomically creates and opens a new file at `path`
    ///
    /// ## TOCTOU Safety
    ///
    /// The creation must be atomic at the operating system / filesystem level
    ///
    /// If a file already exists at `path`, the operation must fail immediately and return [`super::err::EXS`]
    fn create(path: &Path) -> FrozenResult<Self>;

    /// Opens an existing file at `path` for read and write operations without creating it
    ///
    /// If the file does not exist, the operation must fail and return [`super::err::INV`]
    fn open(path: &Path) -> FrozenResult<Self>;

    /// Acquires an exclusive, non-blocking advisory lock on the entire file
    ///
    /// ## Lock Semantics
    ///
    /// If another process or handle holds a conflicting lock, the call fails immediately and returns
    /// [`super::err::LCK`]
    ///
    /// The lock is automatically released when the underlying handle is closed (i.e. RAII Safe)
    fn flock(&self) -> FrozenResult<()>;

    /// Closes the underlying operating system descriptor or handle and releases allocated resources
    ///
    /// Consumes `self` by value to prevent concurrent use-after-close or double-close bugs (i.e. TOCTOU errors)
    fn close(self) -> FrozenResult<()>;

    /// Deletes and removes the file at `path` from the filesystem namespace
    ///
    /// Consumes `self` by value to guarantee that no subsequent operations can be executed on the
    /// deleted file instance (i.e. TOCTOU errors)
    ///
    /// ## Crash-Safe Durability
    ///
    /// Where supported, implementations must ensure the parent directory is synchronized to storage
    /// to guarantee durable deletion across power failures or crashes
    fn unlink(self, path: &Path) -> FrozenResult<()>;

    /// Queries the current logical length of the file in bytes
    fn length(&self) -> FrozenResult<usize>;

    /// Grows the file by extending its allocation and logical size by `len_to_add` bytes
    ///
    /// ## Zero-Fill Guarantee
    ///
    /// Any newly allocated bytes between `curr_len` and `curr_len + len_to_add` must be initialized
    /// to zero by the underlying filesystem
    fn grow(&self, curr_len: usize, len_to_add: usize) -> FrozenResult<()>;

    /// Synchronizes cached dirty pages and file metadata to stable, non-volatile storage
    ///
    /// Must guarantees that preceding writes survive unexpected system crashes or power failures
    fn sync(&self) -> FrozenResult<()>;

    /// Reads bytes starting from `offset` into `buf` without mutating the underlying file pointer
    fn pread(&self, buf: &mut [u8], offset: usize) -> FrozenResult<()>;

    /// Writes bytes from `buf` starting at `offset` without mutating the underlying file pointer.
    fn pwrite(&self, buf: &[u8], offset: usize) -> FrozenResult<()>;
}
