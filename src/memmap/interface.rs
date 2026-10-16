//! Interface contract for platform-specific memory map backends

use super::PageSize;
use crate::error::FrozenResult;

/// Common interface contract implemented by platform-specific memory map backends
pub(in crate::memmap) trait MemMapInterface: Sized + Send + Sync {
    /// Associated type for the underlying operating system file descriptor or kernel handle
    type Id: Copy + Eq + PartialEq + core::fmt::Debug + Send + Sync;

    /// Create a new memory mapping by mapping `length` bytes of the file represented by `id`
    ///
    /// Maps memory from offset 0 with read and write permissions in shared mode
    fn new(id: Self::Id, length: usize) -> FrozenResult<Self>;

    /// Unmap memory mapping to explicitly release mapped resources
    ///
    /// Consumes `self` to prevent use-after-free or double-unmapping at compile time
    ///
    /// Clears internal state so subsequent [`Drop`] execution becomes a safe no-op
    fn unmap(self) -> FrozenResult<()>;

    /// Returns the length of the memory mapping in bytes
    fn length(&self) -> usize;

    /// Returns the system memory page size queried from kernel settings
    fn page_size() -> FrozenResult<PageSize>;

    /// Returns the indices of memory-mapped pages that are currently resident in physical memory
    /// (RAM)
    ///
    /// ## Residency semantics
    ///
    /// Inspects the resident set status of each page in the mapped range
    ///
    /// A page index `i` is included if the corresponding page is present in physical memory
    #[allow(unused)]
    fn active_page_indices(&self) -> FrozenResult<Vec<usize>>;

    /// Evicts selected memory pages from physical memory
    ///
    /// ## Eviction semantics
    ///
    /// Advises or requests the kernel to release process page table / working set entries and free
    /// physical memory associated with the specified page indices
    ///
    /// Contiguous page indices are automatically coalesced to minimize system call overhead
    ///
    /// Duplicate page indices are deduplicated
    ///
    /// If `indices` is empty, this operation is a safe no-op
    #[allow(unused)]
    fn evict_pages(&self, indices: &[usize]) -> FrozenResult<()>;

    /// Syncs in-cache data updates on the storage device
    ///
    /// Flushes dirty pages from the memory mapping cache to storage writeback
    fn sync(&self, length: usize) -> FrozenResult<()>;

    /// Get a mutable (read/write) typed pointer to `T` at given `offset`
    ///
    /// Given `offset` must be aligned w/ `std::mem::align_of::<T>()`
    ///
    /// # Safety
    ///
    /// - The caller must ensure that `offset + std::mem::size_of::<T>() <= length` where `length`
    ///   is the mapped region size
    /// - The pointer `base + offset` must be properly aligned for `T`
    /// - The caller must uphold Rust's aliasing rules (no concurrent unsynchronized reads or writes
    ///   to overlapping bytes)
    /// - The memory mapping must not have been unmapped via [`unmap`](MemMapInterface::unmap)
    unsafe fn as_mut_ptr<T>(&self, offset: usize) -> *mut T
    where
        T: Sized;

    /// Get an immutable (read only) typed pointer to `T` at given `offset`
    ///
    /// Given `offset` must be aligned w/ `std::mem::align_of::<T>()`
    ///
    /// # Safety
    ///
    /// - The caller must ensure that `offset + std::mem::size_of::<T>() <= length` where `length`
    ///   is the mapped region size
    /// - The pointer `base + offset` must be properly aligned for `T`
    /// - The caller must uphold Rust's aliasing rules (no concurrent unsynchronized writes)
    /// - The memory mapping must not have been unmapped via [`unmap`](MemMapInterface::unmap)
    unsafe fn as_ptr<T>(&self, offset: usize) -> *const T
    where
        T: Sized;
}
