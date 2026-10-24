//! Interface contract for platform-specific memory map backends

use super::PageSize;
use crate::error::FrozenResult;

/// Common interface contract implemented by platform-specific memory map backends
pub(in crate::memmap) trait MemMapInterface: Sized + Send + Sync {
    /// Associated type for the underlying operating system file descriptor or kernel handle
    type Id: Copy + Eq + PartialEq + core::fmt::Debug + Send + Sync;

    /// Create a new memory mapping by reserving `max_len` virtual address space and mapping
    /// `length` bytes of the file represented by `id`
    ///
    /// Maps memory from offset 0 with read and write permissions in shared mode
    fn new(id: Self::Id, length: usize, max_len: usize) -> FrozenResult<Self>;

    /// In-place expansion of the memory mapping over a newly extended file range
    ///
    /// Maps `added_len` bytes at `base_ptr + curr_len` from file `id` at offset `curr_len`
    fn grow(&mut self, id: Self::Id, curr_len: usize, added_len: usize) -> FrozenResult<()>;

    /// Unmap memory mapping to explicitly release mapped resources
    ///
    /// Consumes `self` to prevent use-after-free or double-unmapping at compile time
    ///
    /// Clears internal state so subsequent [`Drop`] execution becomes a safe no-op
    fn unmap(self) -> FrozenResult<()>;

    /// Returns the active length of the memory mapping in bytes
    fn length(&self) -> usize;

    /// Returns the maximum reserved virtual length of the memory mapping in bytes
    #[allow(unused)]
    fn max_len(&self) -> usize;

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

    /// Lowers process memory priority to optimize page reclamation under pressure
    ///
    /// On Windows, sets `ProcessMemoryPriority` to `MEMORY_PRIORITY_LOW` (0x02)
    ///
    /// On POSIX systems, this operation is a safe no-op
    fn apply_memory_priority_hint() -> FrozenResult<()>;

    /// Advises the kernel that the mapped range from `offset` for `length` bytes will be accessed
    /// sequentially
    ///
    /// On POSIX systems, invokes `madvise(2)` w/ `MADV_SEQUENTIAL`
    ///
    /// On Windows, leverages `PrefetchVirtualMemory` where applicable; file handle caching is
    /// governed by `FILE_FLAG_SEQUENTIAL_SCAN`
    fn advise_sequential(&self, offset: usize, length: usize) -> FrozenResult<()>;

    /// Prefetches the specified virtual memory range into physical memory (RAM) asynchronously
    ///
    /// On POSIX systems, invokes `madvise(2)` w/ `MADV_WILLNEED`
    ///
    /// On Windows, invokes `PrefetchVirtualMemory` across intersecting mapped views
    fn prefetch(&self, offset: usize, length: usize) -> FrozenResult<()>;

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
