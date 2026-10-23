//! Implementation of memory mapped files for POSIX (Linux and macOS) systems

use super::{
    PageSize,
    err::{self, raw_error},
    interface::MemMapInterface,
};
use crate::{error::FrozenResult, hints};
use core::ptr;
use libc::{
    _SC_PAGESIZE, EACCES, EAGAIN, EBADF, EBUSY, EINTR, EINVAL, EIO, ENODEV, ENOMEM, EOVERFLOW,
    EPERM, ETXTBSY, MADV_DONTNEED, MAP_ANON, MAP_FAILED, MAP_FIXED, MAP_NORESERVE, MAP_PRIVATE,
    MAP_SHARED, MS_SYNC, PROT_NONE, PROT_READ, PROT_WRITE, c_int, c_void, madvise, mincore, mmap,
    msync, munmap, off_t, size_t, sysconf,
};

/// File descriptor type for POSIX systems
pub(super) type FileId = c_int;

/// Base pointer for `mmap(2)` mapped memory
type TPtr = *mut u8;

/// Max allowed retries for `EINTR`, `EBUSY` and `EAGAIN` errors
const MAX_RETRIES: usize = 0x0A;

/// Custom implementation of memory-mapped memory via `mmap(2)` for POSIX systems
///
/// Wraps an allocated virtual memory mapping, managing its lifetime via RAII
///
/// Automatic cleanup is provided via [`Drop`] unless explicitly consumed with
/// [`POSIXMemMap::unmap`]
#[derive(Debug)]
pub(super) struct POSIXMemMap {
    ptr: TPtr,
    length: usize,
    max_len: usize,
}

unsafe impl Send for POSIXMemMap {}
unsafe impl Sync for POSIXMemMap {}

impl Drop for POSIXMemMap {
    /// Automatically releases mapped memory via `munmap(2)` when going out of scope if not
    /// explicitly unmapped
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.max_len > 0 {
            let _ = munmap_raw(self.ptr, self.max_len);

            self.ptr = ptr::null_mut();
            self.length = 0;
            self.max_len = 0;
        }
    }
}

impl MemMapInterface for POSIXMemMap {
    type Id = FileId;

    /// Create a new [`POSIXMemMap`] by reserving `max_len` virtual address space and mapping
    /// `length` bytes of the file represented by `fd`
    ///
    /// Maps memory from offset 0 with `PROT_READ | PROT_WRITE` and `MAP_SHARED`
    fn new(fd: FileId, length: usize, max_len: usize) -> FrozenResult<Self> {
        let ptr = mmap_raw(fd, length, max_len)?;
        Ok(Self { ptr, length, max_len })
    }

    /// In-place expansion of the memory mapping over a newly extended file range
    fn grow(&mut self, fd: FileId, curr_len: usize, added_len: usize) -> FrozenResult<()> {
        mmap_grow_raw(self.ptr, fd, curr_len, added_len)?;
        self.length = curr_len + added_len;
        Ok(())
    }

    /// Unmap [`POSIXMemMap`] to explicitly release mapped memory resources
    ///
    /// Consumes `self` to prevent use-after-free or double-unmapping at compile time
    ///
    /// Clears internal state so subsequent [`Drop`] execution becomes a safe no-op
    fn unmap(mut self) -> FrozenResult<()> {
        if !self.ptr.is_null() && self.max_len > 0 {
            let res = munmap_raw(self.ptr, self.max_len);

            self.ptr = ptr::null_mut();
            self.length = 0;
            self.max_len = 0;

            return res;
        }

        Ok(())
    }

    /// Returns the length of the memory mapping in bytes
    #[inline]
    fn length(&self) -> usize {
        self.length
    }

    /// Returns the maximum reserved virtual length of the memory mapping in bytes
    #[inline]
    fn max_len(&self) -> usize {
        self.max_len
    }

    /// Returns the system memory page size queried from kernel settings
    ///
    /// Under POSIX systems (Linux and macOS), this queries `sysconf(_SC_PAGESIZE)`
    fn page_size() -> FrozenResult<PageSize> {
        let res = unsafe { sysconf(_SC_PAGESIZE) };
        if hints::unlikely(res <= 0) {
            let errno = last_errno();
            let err_msg = err_msg(errno);

            return match errno {
                EINVAL => raw_error(err::HCF, err_msg),
                _ => raw_error(err::UNK, err_msg),
            };
        }

        let page_size = PageSize::try_from(res as usize)?;
        page_size.validate_platform()
    }

    /// Returns the indices of memory-mapped pages that are currently resident in physical memory
    /// (RAM)
    ///
    /// ## Residency semantics
    ///
    /// Under POSIX systems (Linux and macOS), this queries `mincore(2)` to inspect the resident set
    /// status of each page in the mapped range
    ///
    /// A page index `i` is included if the corresponding page is present in physical memory
    ///
    /// The returned list of indices can also be used by callers to compute the total active
    /// resident page count via `indices.len()`
    ///
    /// ## Why do we retry?
    ///
    /// POSIX syscalls are interruptible by signals and may fail w/ `EINTR`, `EBUSY`, or `EAGAIN`;
    /// in such cases, no progress is guaranteed, so the syscall must be retried
    fn active_page_indices(&self) -> FrozenResult<Vec<usize>> {
        if hints::unlikely(self.ptr.is_null() || self.length == 0) {
            return Ok(Vec::new());
        }

        let page_size = Self::page_size()?.as_usize();
        let num_pages = self.length.div_ceil(page_size);

        let mut vec = vec![0u8; num_pages];
        let mut retries = 0;

        loop {
            let res = unsafe { mincore(self.ptr as _, self.length, vec.as_mut_ptr() as *mut _) };

            if hints::likely(res == 0) {
                let indices = vec
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, &b)| if (b & 1) != 0 { Some(idx) } else { None })
                    .collect();

                return Ok(indices);
            }

            let errno = last_errno();
            let err_msg = err_msg(errno);

            match errno {
                EINTR | EBUSY | EAGAIN => {
                    if retries < MAX_RETRIES {
                        retries += 1;
                        continue;
                    }

                    return raw_error(err::UNK, err_msg);
                }
                EINVAL | ENOMEM => return raw_error(err::HCF, err_msg),
                _ => return raw_error(err::UNK, err_msg),
            }
        }
    }

    /// Evicts selected memory pages from physical memory via `madvise(MADV_DONTNEED)`
    ///
    /// ## Eviction semantics
    ///
    /// Under POSIX systems (Linux and macOS), this advises the kernel with `MADV_DONTNEED` for the
    /// specified page indices, releasing process page table entries and freeing physical memory
    /// associated with the mapping
    ///
    /// Contiguous page indices are automatically coalesced into single `madvise(2)` calls to
    /// minimize system call overhead
    ///
    /// Duplicate page indices are deduplicated
    ///
    /// If `indices` is empty, this operation is a safe no-op
    ///
    /// ## Durability notice
    ///
    /// If pages contain modified (`MAP_SHARED`) data that has not been synced, call
    /// [`POSIXMemMap::sync`] before evicting to avoid discarding unwritten changes
    fn evict_pages(&self, indices: &[usize]) -> FrozenResult<()> {
        if hints::unlikely(indices.is_empty()) {
            return Ok(());
        }

        if hints::unlikely(self.ptr.is_null() || self.length == 0) {
            return raw_error(err::HCF, "cannot evict pages on unmapped or zero-length mapping");
        }

        let page_size = Self::page_size()?.as_usize();
        let num_pages = self.length.div_ceil(page_size);

        let mut sorted_indices = indices.to_vec();
        sorted_indices.sort_unstable();
        sorted_indices.dedup();

        for &idx in &sorted_indices {
            if hints::unlikely(idx >= num_pages) {
                return raw_error(
                    err::HCF,
                    format!("page index {idx} out of bounds for mapping of {num_pages} pages"),
                );
            }
        }

        let mut i = 0;
        while i < sorted_indices.len() {
            let start_idx = sorted_indices[i];
            let mut end_idx = start_idx;

            while i + 1 < sorted_indices.len() && sorted_indices[i + 1] == end_idx + 1 {
                i += 1;
                end_idx = sorted_indices[i];
            }

            let run_count = end_idx - start_idx + 1;
            let offset = start_idx * page_size;
            let len = (self.length - offset).min(run_count * page_size);
            let target_ptr = unsafe { self.ptr.add(offset) as *mut c_void };

            madvise_raw(target_ptr, len)?;
            i += 1;
        }

        Ok(())
    }

    /// Syncs in-cache data updates on the storage device
    ///
    /// ## Durability
    ///
    /// In POSIX systems `msync(MS_SYNC)` does not provide crash-safe durability; this syscall is
    /// used as a best-effort operation to explicitly push dirty mmapped pages into filesystem
    /// writeback
    ///
    /// For strong durability, use of [`File::sync`](crate::file::File::sync) is required right
    /// after calling [`POSIXMemMap::sync`]
    ///
    /// ## Why do we retry?
    ///
    /// POSIX syscalls are interruptible by signals and may fail w/ `EINTR`, `EBUSY`, or `EAGAIN`
    ///
    /// In such cases, no progress is guaranteed, so the syscall must be retried
    fn sync(&self, length: usize) -> FrozenResult<()> {
        msync_raw(self.ptr, length)
    }

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
    /// - The memory mapping must not have been unmapped via [`POSIXMemMap::unmap`]
    #[inline]
    #[allow(unsafe_op_in_unsafe_fn)]
    unsafe fn as_mut_ptr<T>(&self, offset: usize) -> *mut T
    where
        T: Sized,
    {
        unsafe { self.ptr.add(offset) as *mut T }
    }

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
    /// - The memory mapping must not have been unmapped via [`POSIXMemMap::unmap`]
    #[inline]
    #[allow(unsafe_op_in_unsafe_fn)]
    unsafe fn as_ptr<T>(&self, offset: usize) -> *const T
    where
        T: Sized,
    {
        unsafe { self.ptr.add(offset) as *const T }
    }
}

/// Create a new memory mapping w/ `mmap(2)` on given `fd`, active `length`, and reserved `max_len`
///
/// ## Over-reservation semantics
///
/// * reserves `max_len` VA space w/ `PROT_NONE` & `MAP_PRIVATE | MAP_ANON | MAP_NORESERVE`
/// * overlays the active file range `[0..length)` at `base_ptr` w/ `MAP_SHARED | MAP_FIXED`
fn mmap_raw(fd: i32, length: size_t, max_len: size_t) -> FrozenResult<TPtr> {
    if hints::unlikely(length == 0 || max_len == 0 || length > max_len) {
        return raw_error(err::HCF, "invalid mapping length or max_len");
    }

    let mut retries = 0;
    let base_ptr = loop {
        let ptr = unsafe {
            mmap(
                ptr::null_mut(),
                max_len,
                PROT_NONE,
                MAP_PRIVATE | MAP_ANON | MAP_NORESERVE,
                -1,
                0 as off_t,
            )
        };

        if ptr == MAP_FAILED {
            let errno = last_errno();
            let err_msg = err_msg(errno);

            match errno {
                EINTR | EBUSY | EAGAIN => {
                    if retries < MAX_RETRIES {
                        retries += 1;
                        continue;
                    }

                    return raw_error(err::UNK, err_msg);
                }

                EINVAL | EBADF | EOVERFLOW => return raw_error(err::HCF, err_msg),

                ENOMEM => return raw_error(err::NMM, err_msg),

                EACCES | EPERM | ENODEV | ETXTBSY => return raw_error(err::PRM, err_msg),

                _ => return raw_error(err::UNK, err_msg),
            };
        }

        break ptr as *mut u8;
    };

    retries = 0;
    loop {
        let ptr = unsafe {
            mmap(
                base_ptr as *mut c_void,
                length,
                PROT_WRITE | PROT_READ,
                MAP_SHARED | MAP_FIXED,
                fd,
                0 as off_t,
            )
        };

        if ptr == MAP_FAILED {
            let errno = last_errno();
            let err_msg = err_msg(errno);

            match errno {
                EINTR | EBUSY | EAGAIN => {
                    if retries < MAX_RETRIES {
                        retries += 1;
                        continue;
                    }

                    let _ = munmap_raw(base_ptr, max_len);
                    return raw_error(err::UNK, err_msg);
                }

                _ => {
                    let _ = munmap_raw(base_ptr, max_len);
                    return match errno {
                        EINVAL | EBADF | EOVERFLOW => raw_error(err::HCF, err_msg),

                        ENOMEM => raw_error(err::NMM, err_msg),

                        EACCES | EPERM | ENODEV | ETXTBSY => raw_error(err::PRM, err_msg),

                        _ => raw_error(err::UNK, err_msg),
                    };
                }
            };
        }

        return Ok(base_ptr);
    }
}

/// Commits newly added file range in-place into reserved virtual address space
///
/// ## Why the returned pointer is not stored
///
/// Passing `MAP_FIXED` with `target_addr = base_ptr + curr_len` instructs the kernel to map the
/// file range starting at that exact virtual address; on success, `mmap(2)` is guaranteed to return
/// `target_addr`
///
/// Because the initial allocation reserved `max_len` bytes at `base_ptr`, this in-place overlay
/// seamlessly extends the contiguous virtual memory range; callers access newly mapped pages
/// through standard pointer arithmetic on the original `base_ptr` (`base_ptr.add(offset)`), so
/// tracking separate slice pointers is unnecessary
///
/// ## Concurrency and cleanup guarantees
///
/// * Active reads and writes referencing existing mapped pages continue uninterrupted without
///   pointer invalidation, VMA relocation, or use-after-free hazards
/// * Releasing resources requires only a single `munmap(base_ptr, max_len)` syscall across the
///   entire reserved window, cleaning up all mapped file slices and unused reservations together
/// * `curr_len` and `added_len` are guaranteed to be page-size aligned by the caller, satisfying
///   POSIX `mmap(2)` offset and address alignment invariants
fn mmap_grow_raw(base_ptr: TPtr, fd: i32, curr_len: size_t, added_len: size_t) -> FrozenResult<()> {
    if hints::unlikely(added_len == 0) {
        return Ok(());
    }

    let target_addr = unsafe { base_ptr.add(curr_len) };
    let mut retries = 0;
    loop {
        let ptr = unsafe {
            mmap(
                target_addr as *mut c_void,
                added_len,
                PROT_WRITE | PROT_READ,
                MAP_SHARED | MAP_FIXED,
                fd,
                curr_len as off_t,
            )
        };

        if ptr == MAP_FAILED {
            let errno = last_errno();
            let err_msg = err_msg(errno);

            match errno {
                EINTR | EBUSY | EAGAIN => {
                    if retries < MAX_RETRIES {
                        retries += 1;
                        continue;
                    }

                    return raw_error(err::UNK, err_msg);
                }

                EINVAL | EBADF | EOVERFLOW => return raw_error(err::HCF, err_msg),

                ENOMEM => return raw_error(err::NMM, err_msg),

                EACCES | EPERM | ENODEV | ETXTBSY => return raw_error(err::PRM, err_msg),

                _ => return raw_error(err::UNK, err_msg),
            };
        }

        return Ok(());
    }
}

/// Unmap the created memory mapping w/ `munmap(2)` by given raw pointer `ptr` and `length`
fn munmap_raw(ptr: TPtr, length: size_t) -> FrozenResult<()> {
    if unsafe { munmap(ptr as *mut c_void, length) == 0 } {
        return Ok(());
    }

    let errno = last_errno();
    let err_msg = err_msg(errno);

    match errno {
        EINVAL | ENOMEM => raw_error(err::HCF, err_msg),

        _ => raw_error(err::UNK, err_msg),
    }
}

/// Syncs in-cache data updates on the storage device
///
/// ## Caveats of `msync(2)` on POSIX
///
/// This syscall by itself does not provide any durability guarantee; it is used as a best-effort
/// operation to explicitly push dirty mmapped pages into filesystem writeback, to aid hard sync
/// calls like `fdatasync` on Linux and `fcntl(F_FULLFSYNC)` on macOS
fn msync_raw(ptr: TPtr, length: size_t) -> FrozenResult<()> {
    if hints::unlikely(length == 0) {
        return Ok(());
    }

    let mut retries = 0; // only for transient errors (EINTR, EBUSY, EAGAIN)
    loop {
        let res = unsafe { msync(ptr as *mut c_void, length, MS_SYNC) };
        if hints::likely(res == 0) {
            return Ok(());
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            EINTR | EBUSY | EAGAIN => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                // NOTE: sync error indicates that retries exhausted and durability is broken in the current/last
                // window/batch
                return raw_error(err::SYN, err_msg);
            }

            EIO => return raw_error(err::SYN, err_msg),

            EINVAL => return raw_error(err::HCF, err_msg),

            ENOMEM => return raw_error(err::NMM, err_msg),

            _ => return raw_error(err::UNK, err_msg),
        }
    }
}

/// Advises the kernel to discard/evict mapped pages w/ `madvise(MADV_DONTNEED)`
#[allow(unused)]
fn madvise_raw(ptr: *mut c_void, length: size_t) -> FrozenResult<()> {
    if hints::unlikely(ptr.is_null()) {
        return raw_error(err::NMM, "cannot advise unmapped null pointer");
    }

    let page_size = POSIXMemMap::page_size()?.as_usize();
    if hints::unlikely(ptr as usize % page_size != 0) {
        return raw_error(err::HCF, "address is not page-aligned");
    }

    let mut retries = 0;
    loop {
        let res = unsafe { madvise(ptr, length, MADV_DONTNEED) };
        if hints::likely(res == 0) {
            return Ok(());
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            EINTR | EBUSY | EAGAIN => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                return raw_error(err::UNK, err_msg);
            }

            EINVAL => return raw_error(err::HCF, err_msg),

            ENOMEM => return raw_error(err::NMM, err_msg),

            _ => return raw_error(err::UNK, err_msg),
        }
    }
}

/// Fetch the last thread-local `errno` value for the current target OS
#[inline]
fn last_errno() -> i32 {
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location()
    }

    #[cfg(target_os = "macos")]
    unsafe {
        *libc::__error()
    }
}

/// Convert a raw OS error code into a formatted human-readable error description
#[inline]
fn err_msg(errno: i32) -> String {
    std::io::Error::from_raw_os_error(errno).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::{File, FileCfg};

    const MOD_ID: u8 = 0;
    const BUFFER_SIZE: usize = 0x10;
    const INIT_BUFFERS: usize = 0x0A;
    const LENGTH: usize = BUFFER_SIZE * INIT_BUFFERS;

    fn new_tmp() -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmp_map");

        let file = File::new(FileCfg {
            path,
            module_id: MOD_ID,
            buffer_size: BUFFER_SIZE,
            initial_available_buffers: INIT_BUFFERS,
            sync_interval: None,
        })
        .expect("new FF");

        (dir, file)
    }

    mod utils {
        use super::*;

        #[test]
        fn ok_last_errno() {
            unsafe {
                let _ = libc::close(-1);
                assert_eq!(last_errno(), libc::EBADF);
            }
        }

        #[test]
        fn ok_err_msg() {
            let msg = err_msg(libc::ENOENT);
            assert!(!msg.is_empty(), "ENOENT must produce message");
        }
    }

    mod page_size {
        use super::*;

        #[test]
        fn ok_page_size_returns_valid_power_of_two() {
            let size = POSIXMemMap::page_size().expect("page_size must succeed");
            assert!(size.is_power_of_two(), "page size must be a power of two, got {size}");
            assert!(
                size.as_usize() >= 0x1000,
                "page size must be at least 4096 bytes on supported 64-bit systems, got {size}"
            );
        }

        #[test]
        fn ok_page_size_matches_libc_sysconf() {
            let size = POSIXMemMap::page_size().expect("page_size must succeed");
            let sysconf_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
            assert_eq!(size.as_usize(), sysconf_size);
        }

        #[test]
        fn ok_page_size_consistency() {
            let first = POSIXMemMap::page_size().unwrap();
            for _ in 0..0x3E8 {
                assert_eq!(POSIXMemMap::page_size().unwrap(), first);
            }
        }

        #[test]
        fn ok_page_size_across_threads() {
            let expected = POSIXMemMap::page_size().unwrap();
            let mut handles = Vec::new();
            for _ in 0..0x10 {
                let handle = std::thread::spawn(move || {
                    for _ in 0..0x64 {
                        assert_eq!(POSIXMemMap::page_size().unwrap(), expected);
                    }
                });
                handles.push(handle);
            }
            for handle in handles {
                handle.join().unwrap();
            }
        }
    }

    mod active_pages {
        use super::*;

        fn new_paged_tmp(num_pages: usize) -> (tempfile::TempDir, File, usize) {
            let page_size = POSIXMemMap::page_size().expect("page_size").as_usize();
            let total_len = page_size * num_pages;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("tmp_paged_map");

            let file = File::new(FileCfg {
                path,
                module_id: MOD_ID,
                buffer_size: BUFFER_SIZE,
                initial_available_buffers: total_len / BUFFER_SIZE,
                sync_interval: None,
            })
            .expect("new paged file");

            (dir, file, total_len)
        }

        #[test]
        fn ok_active_pages_after_touch() {
            let (_dir, file, total_len) = new_paged_tmp(4);
            let page_size = POSIXMemMap::page_size().unwrap().as_usize();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), total_len, total_len).unwrap();

                let p0 = mmap.as_mut_ptr::<u8>(0);
                *p0 = 0xAA;
                let p2 = mmap.as_mut_ptr::<u8>(page_size * 2);
                *p2 = 0xBB;

                let active = mmap.active_page_indices().unwrap();
                assert!(active.contains(&0), "page 0 must be active after write");
                assert!(active.contains(&2), "page 2 must be active after write");
                for &idx in &active {
                    assert!(idx < 4, "active page index must be within mapping range");
                }

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_active_pages_all_touched() {
            let (_dir, file, total_len) = new_paged_tmp(3);
            let page_size = POSIXMemMap::page_size().unwrap().as_usize();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), total_len, total_len).unwrap();

                for i in 0..3 {
                    let ptr = mmap.as_mut_ptr::<u8>(i * page_size);
                    *ptr = (i + 1) as u8;
                }

                let active = mmap.active_page_indices().unwrap();
                assert_eq!(active, vec![0, 1, 2]);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_active_pages_read_faults_page() {
            let (_dir, file, total_len) = new_paged_tmp(2);
            let page_size = POSIXMemMap::page_size().unwrap().as_usize();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), total_len, total_len).unwrap();

                // Read page 1 to trigger page fault
                let p1 = mmap.as_ptr::<u8>(page_size);
                let _val = ptr::read_volatile(p1);

                let active = mmap.active_page_indices().unwrap();
                assert!(active.contains(&1), "reading page 1 must bring it into memory");

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_active_pages_consecutive_consistency() {
            let (_dir, file, total_len) = new_paged_tmp(2);

            let mmap = POSIXMemMap::new(file.fd(), total_len, total_len).unwrap();
            let first = mmap.active_page_indices().unwrap();
            let second = mmap.active_page_indices().unwrap();
            assert_eq!(first, second);
            mmap.unmap().unwrap();
        }
    }

    mod eviction {
        use super::*;

        fn new_paged_tmp(num_pages: usize) -> (tempfile::TempDir, File, usize) {
            let page_size = POSIXMemMap::page_size().expect("page_size").as_usize();
            let total_len = page_size * num_pages;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("tmp_evict_map");

            let file = File::new(FileCfg {
                path,
                module_id: MOD_ID,
                buffer_size: BUFFER_SIZE,
                initial_available_buffers: total_len / BUFFER_SIZE,
                sync_interval: None,
            })
            .expect("new paged file");

            (dir, file, total_len)
        }

        #[test]
        fn ok_evict_empty_indices() {
            let (_dir, file, total_len) = new_paged_tmp(2);

            let mmap = POSIXMemMap::new(file.fd(), total_len, total_len).unwrap();
            assert!(mmap.evict_pages(&[]).is_ok());
            mmap.unmap().unwrap();
        }

        #[test]
        fn ok_evict_single_page() {
            let (_dir, file, total_len) = new_paged_tmp(2);
            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), total_len, total_len).unwrap();
                let p0 = mmap.as_mut_ptr::<u8>(0);
                *p0 = 0xAA;
                mmap.sync(total_len).unwrap();

                assert!(mmap.evict_pages(&[0]).is_ok());
                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_evict_contiguous_runs() {
            let (_dir, file, total_len) = new_paged_tmp(4);
            let page_size = POSIXMemMap::page_size().unwrap().as_usize();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), total_len, total_len).unwrap();
                for i in 0..4 {
                    let ptr = mmap.as_mut_ptr::<u8>(i * page_size);
                    *ptr = (i + 1) as u8;
                }
                mmap.sync(total_len).unwrap();

                assert!(mmap.evict_pages(&[0, 1]).is_ok());
                assert!(mmap.evict_pages(&[2, 3]).is_ok());

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_evict_unsorted_and_duplicate_indices() {
            let (_dir, file, total_len) = new_paged_tmp(3);
            let page_size = POSIXMemMap::page_size().unwrap().as_usize();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), total_len, total_len).unwrap();
                for i in 0..3 {
                    let ptr = mmap.as_mut_ptr::<u8>(i * page_size);
                    *ptr = 0xFF;
                }
                mmap.sync(total_len).unwrap();

                assert!(mmap.evict_pages(&[2, 0, 2, 1, 0]).is_ok());
                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_evict_all_using_active_page_indices() {
            let (_dir, file, total_len) = new_paged_tmp(3);
            let page_size = POSIXMemMap::page_size().unwrap().as_usize();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), total_len, total_len).unwrap();
                for i in 0..3 {
                    let ptr = mmap.as_mut_ptr::<u8>(i * page_size);
                    *ptr = 0x55;
                }
                mmap.sync(total_len).unwrap();

                let active = mmap.active_page_indices().unwrap();
                assert!(mmap.evict_pages(&active).is_ok());

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn err_evict_out_of_bounds() {
            let (_dir, file, total_len) = new_paged_tmp(2);

            let mmap = POSIXMemMap::new(file.fd(), total_len, total_len).unwrap();
            let err = mmap.evict_pages(&[2]).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);

            let err_large = mmap.evict_pages(&[0x64]).unwrap_err();
            assert_eq!(err_large.reason, err::HCF.reason);

            mmap.unmap().unwrap();
        }

        #[test]
        fn err_evict_unmapped_and_unaligned_ptr() {
            // Unmapped memory returns ENOMEM -> NMM
            let err_unmapped = madvise_raw(ptr::null_mut(), BUFFER_SIZE).unwrap_err();
            assert_eq!(err_unmapped.reason, err::NMM.reason);

            // Unaligned pointer returns EINVAL -> HCF
            let (_dir, file, total_len) = new_paged_tmp(1);
            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), total_len, total_len).unwrap();
                let unaligned = (mmap.as_ptr::<u8>(0) as usize + 1) as *mut c_void;
                let err_unaligned = madvise_raw(unaligned, BUFFER_SIZE).unwrap_err();
                assert_eq!(err_unaligned.reason, err::HCF.reason);

                // Target-specific raw syscall assertion: Linux kernel rejects unaligned address with -1 (EINVAL)
                #[cfg(target_os = "linux")]
                {
                    let raw = madvise(unaligned, BUFFER_SIZE, MADV_DONTNEED);
                    assert_eq!(raw, -1);
                }
                // Darwin/macOS XNU kernel auto-truncates unaligned address to page boundary and returns 0
                #[cfg(target_os = "macos")]
                {
                    let raw = madvise(unaligned, BUFFER_SIZE, MADV_DONTNEED);
                    assert_eq!(raw, 0);
                }

                mmap.unmap().unwrap();
            }
        }
    }

    mod map_unmap {
        use super::*;

        #[test]
        fn ok_map_unmap_cycle() {
            let (_dir, file) = new_tmp();

            let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
            mmap.unmap().unwrap();
        }

        #[test]
        fn ok_map_zero_bytes_on_new() {
            let (_dir, file) = new_tmp();
            const BUF: [u8; LENGTH] = [0; LENGTH];

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let ptr = mmap.as_ptr::<[u8; LENGTH]>(0);
                assert_eq!(*ptr, BUF);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_unmap_consumes_self() {
            let (_dir, file) = new_tmp();

            let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
            assert!(mmap.unmap().is_ok());
        }

        #[test]
        fn ok_multiple_shared_mappings_reflect_writes() {
            const VAL: u64 = 0x1234_5678_9ABC_DEF0;
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap1 = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
                let mmap2 = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let wptr = mmap1.as_mut_ptr::<u64>(0);
                *wptr = VAL;

                let rptr = mmap2.as_ptr::<u64>(0);
                assert_eq!(*rptr, VAL);

                mmap1.unmap().unwrap();
                mmap2.unmap().unwrap();
            }
        }

        #[test]
        fn err_map_on_invalid_length() {
            let (_dir, file) = new_tmp();

            let err = POSIXMemMap::new(file.fd(), 0, 0).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
        }

        #[test]
        fn err_map_on_invalid_fd() {
            let (_dir, _) = new_tmp();

            let err = POSIXMemMap::new(-1, LENGTH, LENGTH).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
        }

        #[test]
        fn err_map_on_read_only_fd() {
            let (dir, file) = new_tmp();
            drop(file);

            let path = dir.path().join("tmp_map");
            let cpath = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY) };
            assert!(fd >= 0, "open O_RDONLY must succeed");

            unsafe {
                let err = POSIXMemMap::new(fd, LENGTH, LENGTH).unwrap_err();
                assert_eq!(err.reason, err::PRM.reason);
                libc::close(fd);
            }
        }

        #[test]
        fn err_unmap_on_invalid_length() {
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
                let err = munmap_raw(mmap.as_ptr::<u8>(0) as *mut u8, 0).unwrap_err();
                assert_eq!(err.reason, err::HCF.reason);
                mmap.unmap().unwrap();
            }
        }
    }

    mod map_sync {
        use super::*;

        #[test]
        fn ok_sync() {
            let (_dir, file) = new_tmp();

            let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
            mmap.sync(LENGTH).unwrap();
            mmap.unmap().unwrap();
        }

        #[test]
        fn ok_sync_after_sync() {
            let (_dir, file) = new_tmp();

            let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

            mmap.sync(LENGTH).unwrap();
            mmap.sync(LENGTH).unwrap();
            mmap.sync(LENGTH).unwrap();
            mmap.sync(LENGTH).unwrap();

            mmap.unmap().unwrap();
        }

        #[test]
        fn ok_sync_zero_length() {
            let (_dir, file) = new_tmp();

            let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
            assert!(mmap.sync(0).is_ok());
            mmap.unmap().unwrap();
        }

        #[test]
        fn ok_sync_partial_range() {
            const VAL: u64 = 0xA5A5_A5A5_A5A5_A5A5;
            let (_dir, file) = new_tmp();
            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
                let wptr = mmap.as_mut_ptr::<u64>(0);
                *wptr = VAL;
                assert!(mmap.sync(BUFFER_SIZE).is_ok());
                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn err_sync_on_unaligned_pointer() {
            let (_dir, file) = new_tmp();
            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
                let unaligned = (mmap.as_ptr::<u8>(0) as usize + 1) as *mut u8;
                let err = msync_raw(unaligned, BUFFER_SIZE).unwrap_err();
                assert_eq!(err.reason, err::HCF.reason);
                mmap.unmap().unwrap();
            }
        }
    }

    mod map_write_read {
        use super::*;

        #[test]
        fn ok_write_read_cycle() {
            const VAL: u64 = 0xDEADC0DE;
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let wptr = mmap.as_mut_ptr::<u64>(0);
                *wptr = VAL;

                let rptr = mmap.as_ptr::<u64>(0);
                assert_eq!(*rptr, VAL);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_write_read_with_offset() {
            const VAL: u64 = 0xDEADC0DE;
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let wptr = mmap.as_mut_ptr::<u64>(8);
                *wptr = VAL;

                let rptr = mmap.as_ptr::<u64>(8);
                assert_eq!(*rptr, VAL);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_write_read_at_boundary() {
            const VAL: u64 = 0xFEEDFACECAFEBEEF;
            const OFFSET: usize = LENGTH - std::mem::size_of::<u64>();
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let wptr = mmap.as_mut_ptr::<u64>(OFFSET);
                *wptr = VAL;

                let rptr = mmap.as_ptr::<u64>(OFFSET);
                assert_eq!(*rptr, VAL);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_write_read_sync_cycle() {
            const VAL: [u32; 0x0A] = [0xDEADC0DE; 0x0A];
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let wptr = mmap.as_mut_ptr::<[u32; 0x0A]>(0);
                *wptr = VAL;

                mmap.sync(LENGTH).unwrap();

                let rptr = mmap.as_ptr::<[u32; 0x0A]>(0);
                assert_eq!(*rptr, VAL);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_read_zero_bytes() {
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let rptr = mmap.as_ptr::<u64>(0);
                assert_eq!(*rptr, 0);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_write_read_heterogeneous_types() {
            let (_dir, file) = new_tmp();
            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let p_u8 = mmap.as_mut_ptr::<u8>(0);
                *p_u8 = 0xAA;

                let p_u16 = mmap.as_mut_ptr::<u16>(2);
                *p_u16 = 0xBBCC;

                let p_u32 = mmap.as_mut_ptr::<u32>(4);
                *p_u32 = 0xDDEEFF00;

                let p_u64 = mmap.as_mut_ptr::<u64>(8);
                *p_u64 = 0x11223344_55667788;

                assert_eq!(*mmap.as_ptr::<u8>(0), 0xAA);
                assert_eq!(*mmap.as_ptr::<u16>(2), 0xBBCC);
                assert_eq!(*mmap.as_ptr::<u32>(4), 0xDDEEFF00);
                assert_eq!(*mmap.as_ptr::<u64>(8), 0x11223344_55667788);

                mmap.unmap().unwrap();
            }
        }
    }

    mod map_durability {
        use super::*;

        #[test]
        fn ok_map_durability_after_unmap() {
            const VAL: u64 = 0xCAFEBABEDEADC0DE;
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let ptr = mmap.as_mut_ptr::<u64>(0);
                *ptr = VAL;

                mmap.sync(LENGTH).unwrap();
                mmap.unmap().unwrap();
            }

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let ptr = mmap.as_ptr::<u64>(0);
                assert_eq!(*ptr, VAL);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_map_durability_after_unmap_and_close() {
            const VAL: u64 = 0xDEADC0DEDEADC0DE;
            let (dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let ptr = mmap.as_mut_ptr::<u64>(0);
                *ptr = VAL;

                mmap.sync(LENGTH).unwrap();
                mmap.unmap().unwrap();
                drop(file);
            }

            unsafe {
                let path = dir.path().join("tmp_map");
                let cfg = FileCfg {
                    path,
                    module_id: MOD_ID,
                    buffer_size: BUFFER_SIZE,
                    initial_available_buffers: INIT_BUFFERS,
                    sync_interval: None,
                };

                let file = File::open(cfg).expect("open FF");
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let ptr = mmap.as_ptr::<u64>(0);
                assert_eq!(*ptr, VAL);

                mmap.unmap().unwrap();
            }
        }
        #[test]
        fn ok_map_durability_on_drop_and_reopen() {
            const VAL: u64 = 0x0123_4567_89AB_CDEF;
            let (dir, file) = new_tmp();
            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
                let ptr = mmap.as_mut_ptr::<u64>(0);
                *ptr = VAL;
                mmap.sync(LENGTH).unwrap();
                drop(mmap);
                drop(file);
            }

            unsafe {
                let path = dir.path().join("tmp_map");
                let cfg = FileCfg {
                    path,
                    module_id: MOD_ID,
                    buffer_size: BUFFER_SIZE,
                    initial_available_buffers: INIT_BUFFERS,
                    sync_interval: None,
                };
                let file = File::open(cfg).expect("open FF");
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
                let ptr = mmap.as_ptr::<u64>(0);
                assert_eq!(*ptr, VAL);
                mmap.unmap().unwrap();
            }
        }
    }

    mod map_drop {
        use super::*;

        #[test]
        fn ok_drop_tracks_length() {
            let (_dir, file) = new_tmp();
            let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
            assert_eq!(mmap.length(), LENGTH);
        }

        #[test]
        fn ok_explicit_unmap_prevents_drop_double_unmap() {
            let (_dir, file) = new_tmp();

            let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
            assert!(mmap.unmap().is_ok());
            // explicit unmap consumed mmap; drop should be a safe no-op without double-unmap
        }

        #[test]
        fn ok_drop_persists_written_data() {
            const VAL: u64 = 0xCAFEBABE_DEADBEEF;
            let (_dir, file) = new_tmp();

            unsafe {
                {
                    let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
                    let wptr = mmap.as_mut_ptr::<u64>(0);
                    *wptr = VAL;
                    mmap.sync(LENGTH).unwrap();
                    // mmap goes out of scope and is dropped here without calling unmap
                }

                // Re-open and verify data persisted across drop
                let mmap2 = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
                let rptr = mmap2.as_ptr::<u64>(0);
                assert_eq!(*rptr, VAL);
                mmap2.unmap().unwrap();
            }
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn ok_drop_unmaps_from_proc_maps() {
            let (dir, file) = new_tmp();
            let path_str = dir.path().join("tmp_map");
            let path_str = path_str.to_str().unwrap();
            let addr: usize;

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
                addr = mmap.as_ptr::<u8>(0) as usize;

                let is_mapped = || {
                    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
                    maps.lines().any(|line| {
                        if line.contains(path_str) {
                            if let Some((start_s, end_s)) =
                                line.split_whitespace().next().and_then(|r| r.split_once('-'))
                            {
                                if let (Ok(start), Ok(end)) = (
                                    usize::from_str_radix(start_s, 0x10),
                                    usize::from_str_radix(end_s, 0x10),
                                ) {
                                    return addr >= start && addr < end;
                                }
                            }
                        }
                        false
                    })
                };

                assert!(is_mapped(), "address must be mapped before drop");
                drop(mmap);
                assert!(!is_mapped(), "address must not be mapped after drop");
            }
        }
    }
}
