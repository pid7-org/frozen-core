use super::{
    PageSize,
    err::{self, raw_error},
    interface::MemMapInterface,
};
use crate::{error::FrozenResult, hints};
use core::ptr;
use std::time::Duration;
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_COMMITMENT_LIMIT, ERROR_INVALID_ADDRESS,
        ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_IO_DEVICE, ERROR_LOCK_VIOLATION,
        ERROR_NOT_ENOUGH_MEMORY, ERROR_NOT_LOCKED, ERROR_OUTOFMEMORY, ERROR_SHARING_VIOLATION,
        HANDLE, INVALID_HANDLE_VALUE,
    },
    System::{
        Memory::{
            CreateFileMappingW, FlushViewOfFile, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE,
            MEM_REPLACE_PLACEHOLDER, MEM_RESERVE, MEM_RESERVE_PLACEHOLDER,
            MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3, PAGE_NOACCESS, PAGE_READWRITE,
            UnmapViewOfFile, VirtualAlloc2, VirtualFreeEx, VirtualUnlock,
        },
        ProcessStatus::{
            K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_BLOCK, PSAPI_WORKING_SET_EX_INFORMATION,
        },
        SystemInformation::{GetSystemInfo, SYSTEM_INFO},
        Threading::GetCurrentProcess,
    },
};

/// File handle type for Windows systems
pub(super) type FileId = isize;

/// Base pointer for Win32 mapped memory
type TPtr = *mut u8;

/// Sentinel representing a closed or never opened handle
const CLOSED_HANDLE: isize = -1isize;

/// Win32 error code representing an invalid file
const ERROR_FILE_INVALID: u32 = 0x3EE;

/// Max allowed retries for transient lock or sharing violations
const MAX_RETRIES: usize = 0x80;

/// Custom implementation of memory-mapped memory via Win32 File Mapping APIs for Windows 64-bit
/// systems
///
/// Wraps an allocated virtual memory mapping, managing its lifetime via RAII
///
/// Automatic cleanup is provided via [`Drop`] unless explicitly consumed with [`WINMemMap::unmap`]
#[derive(Debug)]
pub(super) struct WINMemMap {
    ptr: TPtr,
    length: usize,
    max_len: usize,
    views: Vec<(TPtr, usize)>,
}

unsafe impl Send for WINMemMap {}
unsafe impl Sync for WINMemMap {}

impl Drop for WINMemMap {
    /// Automatically releases mapped memory views and reserved placeholder when going out of scope
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.max_len > 0 {
            for &(view_ptr, len) in &self.views {
                let _ = munmap_raw(view_ptr, len);
            }

            if self.length < self.max_len {
                let remaining_placeholder = unsafe { self.ptr.add(self.length) };
                let _ = unsafe {
                    VirtualFreeEx(
                        GetCurrentProcess(),
                        remaining_placeholder as *mut core::ffi::c_void,
                        0,
                        MEM_RELEASE,
                    )
                };
            }

            self.ptr = ptr::null_mut();
            self.length = 0;
            self.max_len = 0;
            self.views.clear();
        }
    }
}

impl MemMapInterface for WINMemMap {
    type Id = FileId;

    /// Create a new [`WINMemMap`] by reserving `max_len` virtual address space and mapping `length`
    /// bytes of the file represented by `handle`
    fn new(handle: FileId, length: usize, max_len: usize) -> FrozenResult<Self> {
        let (ptr, views) = mmap_raw(handle, length, max_len)?;
        Ok(Self { ptr, length, max_len, views })
    }

    /// In-place expansion of the memory mapping over a newly extended file range
    fn grow(&mut self, handle: FileId, curr_len: usize, added_len: usize) -> FrozenResult<()> {
        let view = mmap_grow_raw(self.ptr, handle, curr_len, added_len, self.max_len)?;
        if view.1 > 0 {
            self.views.push(view);
            self.length = curr_len + added_len;
        }

        Ok(())
    }

    /// Unmap [`WINMemMap`] to explicitly release mapped memory resources
    ///
    /// Consumes `self` to prevent use-after-free or double-unmapping at compile time
    ///
    /// Clears internal state so subsequent [`Drop`] execution becomes a safe no-op
    fn unmap(mut self) -> FrozenResult<()> {
        if !self.ptr.is_null() && self.max_len > 0 {
            let mut first_err = None;
            for &(view_ptr, len) in &self.views {
                let res = munmap_raw(view_ptr, len);

                if hints::unlikely(res.is_err() && first_err.is_none()) {
                    first_err = Some(res);
                }
            }

            if self.length < self.max_len {
                let remaining_placeholder = unsafe { self.ptr.add(self.length) };
                let ok = unsafe {
                    VirtualFreeEx(
                        GetCurrentProcess(),
                        remaining_placeholder as *mut core::ffi::c_void,
                        0,
                        MEM_RELEASE,
                    )
                };

                if hints::unlikely(ok == 0 && first_err.is_none()) {
                    let code = last_error();
                    first_err = Some(raw_error(err::UNK, err_msg(code)));
                }
            }

            self.ptr = ptr::null_mut();
            self.length = 0;
            self.max_len = 0;
            self.views.clear();

            return first_err.unwrap_or(Ok(()));
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
    /// Under Windows systems, this queries `GetSystemInfo`
    fn page_size() -> FrozenResult<PageSize> {
        let mut sys_info = unsafe { core::mem::zeroed::<SYSTEM_INFO>() };
        unsafe { GetSystemInfo(&mut sys_info) };

        let page_size = PageSize::try_from(sys_info.dwPageSize as usize)?;
        page_size.validate_platform()
    }

    /// Returns the indices of memory-mapped pages that are currently resident in physical memory
    /// (RAM)
    ///
    /// ## Residency semantics
    ///
    /// Under Windows systems, this queries `K32QueryWorkingSetEx` to inspect the resident working
    /// set status of each page in the mapped range
    ///
    /// A page index `i` is included if the corresponding page is present in physical memory
    /// (`Valid` bit set)
    ///
    /// The returned list of indices can also be used by callers to compute the total active
    /// resident page count via `indices.len()`
    fn active_page_indices(&self) -> FrozenResult<Vec<usize>> {
        if hints::unlikely(self.ptr.is_null() || self.length == 0) {
            return Ok(Vec::new());
        }

        let page_size = Self::page_size()?.as_usize();
        let num_pages = self.length.div_ceil(page_size);

        let mut ws_info: Vec<PSAPI_WORKING_SET_EX_INFORMATION> = (0..num_pages)
            .map(|i| PSAPI_WORKING_SET_EX_INFORMATION {
                VirtualAddress: unsafe { self.ptr.add(i * page_size) as *mut core::ffi::c_void },
                VirtualAttributes: PSAPI_WORKING_SET_EX_BLOCK { Flags: 0 },
            })
            .collect();

        let cb = match u32::try_from(
            ws_info.len() * core::mem::size_of::<PSAPI_WORKING_SET_EX_INFORMATION>(),
        ) {
            Ok(v) => v,
            Err(_) => return raw_error(err::HCF, "working set buffer size exceeds u32 capacity"),
        };

        let mut retries = 0;
        loop {
            let ok = unsafe {
                K32QueryWorkingSetEx(
                    GetCurrentProcess(),
                    ws_info.as_mut_ptr() as *mut core::ffi::c_void,
                    cb,
                )
            };

            if hints::likely(ok != 0) {
                let indices = ws_info
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, info)| {
                        if unsafe { info.VirtualAttributes.Flags } & 1 != 0 {
                            Some(idx)
                        } else {
                            None
                        }
                    })
                    .collect();

                return Ok(indices);
            }

            let code = last_error();
            let err_msg = err_msg(code);

            match code {
                ERROR_LOCK_VIOLATION | ERROR_SHARING_VIOLATION => {
                    if retries < MAX_RETRIES {
                        retries += 1;
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }

                    return raw_error(err::UNK, err_msg);
                }
                ERROR_INVALID_PARAMETER | ERROR_INVALID_ADDRESS => {
                    return raw_error(err::HCF, err_msg);
                }
                ERROR_NOT_ENOUGH_MEMORY | ERROR_OUTOFMEMORY => return raw_error(err::NMM, err_msg),
                _ => return raw_error(err::UNK, err_msg),
            }
        }
    }

    /// Evicts selected memory pages from physical memory via `VirtualUnlock`
    ///
    /// ## Eviction semantics
    ///
    /// Under Windows systems, this calls `VirtualUnlock` for the specified page indices, releasing
    /// working set entries and evicting physical memory associated with the mapping
    ///
    /// Contiguous page indices are automatically coalesced into single `VirtualUnlock` calls to
    /// minimize system call overhead
    ///
    /// Duplicate page indices are deduplicated
    ///
    /// If `indices` is empty, this operation is a safe no-op
    ///
    /// ## Durability notice
    ///
    /// If pages contain modified data that has not been synced, call [`WINMemMap::sync`] before
    /// evicting to avoid discarding unwritten changes
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
            let len = run_count * page_size;
            let target_ptr = unsafe { self.ptr.add(offset) as *mut core::ffi::c_void };

            discard_raw(target_ptr, len)?;
            i += 1;
        }

        Ok(())
    }

    /// Syncs in-cache data updates on the storage device
    ///
    /// ## POSIX vs Windows difference
    ///
    /// On POSIX, `msync(2)` accepts arbitrary contiguous virtual ranges regardless of how many
    /// times `mmap(2)` was invoked; on Windows, `FlushViewOfFile` strictly requires the target
    /// range to reside entirely within a single mapped view
    ///
    /// Crossing view boundaries causes Win32 to fail with `ERROR_INVALID_ADDRESS` (`0x1E7` / 487);
    /// we therefore iterate over each slice in `self.views` and flush them individually
    ///
    /// For strong durability, use of [`File::sync`](crate::file::File::sync) (`FlushFileBuffers`)
    /// is required right after calling [`WINMemMap::sync`]
    fn sync(&self, length: usize) -> FrozenResult<()> {
        if hints::unlikely(length == 0 || self.ptr.is_null()) {
            return Ok(());
        }

        let mut remaining = length;
        for &(view_ptr, view_len) in &self.views {
            if remaining == 0 {
                break;
            }

            let flush_len = remaining.min(view_len);
            msync_raw(view_ptr, flush_len)?;
            remaining -= flush_len;
        }

        Ok(())
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
    /// - The memory mapping must not have been unmapped via [`WINMemMap::unmap`]
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
    /// - The memory mapping must not have been unmapped via [`WINMemMap::unmap`]
    #[inline]
    #[allow(unsafe_op_in_unsafe_fn)]
    unsafe fn as_ptr<T>(&self, offset: usize) -> *const T
    where
        T: Sized,
    {
        unsafe { self.ptr.add(offset) as *const T }
    }
}

/// Create a new memory mapping w/ `VirtualAlloc2` + `CreateFileMappingW` + `MapViewOfFile3` on
/// given `handle`, active `length`, and reserved `max_len`
fn mmap_raw(
    handle: isize,
    length: usize,
    max_len: usize,
) -> FrozenResult<(TPtr, Vec<(TPtr, usize)>)> {
    if hints::unlikely(
        handle == CLOSED_HANDLE
            || (handle as usize as HANDLE) == INVALID_HANDLE_VALUE
            || handle == 0,
    ) {
        return raw_error(err::HCF, "invalid handle for memory mapping");
    }

    if hints::unlikely(length == 0 || max_len == 0 || length > max_len) {
        return raw_error(err::HCF, "invalid mapping length or max_len");
    }

    let h_process = unsafe { GetCurrentProcess() };
    let base_addr = unsafe {
        VirtualAlloc2(
            h_process,
            core::ptr::null(),
            max_len,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            core::ptr::null_mut(),
            0,
        )
    };

    if hints::unlikely(base_addr.is_null()) {
        let code = last_error();
        let err_msg = err_msg(code);

        return match code {
            ERROR_NOT_ENOUGH_MEMORY | ERROR_OUTOFMEMORY | ERROR_COMMITMENT_LIMIT => {
                raw_error(err::NMM, err_msg)
            }
            _ => raw_error(err::UNK, err_msg),
        };
    }

    let base_ptr = base_addr as *mut u8;
    if length < max_len {
        let ok = unsafe {
            VirtualFreeEx(
                h_process,
                base_ptr as *mut core::ffi::c_void,
                length,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            )
        };

        if hints::unlikely(ok == 0) {
            let code = last_error();
            let err_msg = err_msg(code);

            unsafe { VirtualFreeEx(h_process, base_addr, 0, MEM_RELEASE) };
            return raw_error(err::UNK, err_msg);
        }
    }

    let h_file = handle as usize as HANDLE;
    let mut retries = 0;

    loop {
        let h_map = unsafe {
            CreateFileMappingW(h_file, core::ptr::null(), PAGE_READWRITE, 0, 0, core::ptr::null())
        };

        if hints::unlikely(h_map.is_null()) {
            let code = last_error();
            let err_msg = err_msg(code);

            match code {
                ERROR_LOCK_VIOLATION | ERROR_SHARING_VIOLATION => {
                    if retries < MAX_RETRIES {
                        retries += 1;
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }

                    unsafe { VirtualFreeEx(h_process, base_addr, 0, MEM_RELEASE) };
                    return raw_error(err::UNK, err_msg);
                }
                ERROR_ACCESS_DENIED => {
                    unsafe { VirtualFreeEx(h_process, base_addr, 0, MEM_RELEASE) };
                    return raw_error(err::PRM, err_msg);
                }
                ERROR_INVALID_HANDLE | ERROR_INVALID_PARAMETER | ERROR_FILE_INVALID => {
                    unsafe { VirtualFreeEx(h_process, base_addr, 0, MEM_RELEASE) };
                    return raw_error(err::HCF, err_msg);
                }
                ERROR_NOT_ENOUGH_MEMORY | ERROR_OUTOFMEMORY | ERROR_COMMITMENT_LIMIT => {
                    unsafe { VirtualFreeEx(h_process, base_addr, 0, MEM_RELEASE) };
                    return raw_error(err::NMM, err_msg);
                }
                _ => {
                    unsafe { VirtualFreeEx(h_process, base_addr, 0, MEM_RELEASE) };
                    return raw_error(err::UNK, err_msg);
                }
            }
        }

        let view = unsafe {
            MapViewOfFile3(
                h_map,
                h_process,
                base_ptr as *const core::ffi::c_void,
                0,
                length,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                core::ptr::null_mut(),
                0,
            )
        };

        unsafe { CloseHandle(h_map) };
        if hints::unlikely(view.Value.is_null()) {
            let code = last_error();
            let err_msg = err_msg(code);

            unsafe { VirtualFreeEx(h_process, base_addr, 0, MEM_RELEASE) };

            return match code {
                ERROR_NOT_ENOUGH_MEMORY | ERROR_OUTOFMEMORY | ERROR_COMMITMENT_LIMIT => {
                    raw_error(err::NMM, err_msg)
                }
                _ => raw_error(err::UNK, err_msg),
            };
        }

        return Ok((base_ptr, vec![(base_ptr, length)]));
    }
}

/// Commits newly added file range in-place into reserved placeholder
///
/// ## Why the base pointer remains stable
///
/// `VirtualAlloc2` with `MEM_RESERVE | MEM_RESERVE_PLACEHOLDER` reserved `max_len` bytes at
/// `base_ptr` upfront; when expanding, `VirtualFreeEx` splits the reservation at `target_addr =
/// base_ptr + curr_len`, and `MapViewOfFile3` replaces that placeholder with
/// `MEM_REPLACE_PLACEHOLDER`
///
/// Because each view is placed contiguously in the process virtual address space, all indexing
/// and caller accesses continue using standard pointer arithmetic on the original `base_ptr`
/// (`base_ptr.add(offset)`); the returned `(target_addr, added_len)` tuple is stored internally
/// solely so [`WINMemMap::unmap`] can invoke `UnmapViewOfFile` per mapped view
///
/// ## Concurrency and Win32 section guarantees
///
/// * Active reads and writes referencing earlier views continue uninterrupted without pointer
///   invalidation or section teardown
/// * Zero-extends backing storage prior to opening the transient mapping handle, avoiding
///   `ERROR_USER_MAPPED_FILE` (error `0x4C8`) conflicts on Windows
/// * `curr_len` and `added_len` are guaranteed to align with system allocation granularity,
///   satisfying Win32 placeholder replacement invariants
fn mmap_grow_raw(
    base_ptr: TPtr,
    handle: isize,
    curr_len: usize,
    added_len: usize,
    max_len: usize,
) -> FrozenResult<(TPtr, usize)> {
    if hints::unlikely(added_len == 0) {
        return Ok((base_ptr, 0));
    }

    let h_process = unsafe { GetCurrentProcess() };
    let target_addr = unsafe { base_ptr.add(curr_len) };

    if curr_len + added_len < max_len {
        let ok = unsafe {
            VirtualFreeEx(
                h_process,
                target_addr as *mut core::ffi::c_void,
                added_len,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            )
        };

        if hints::unlikely(ok == 0) {
            let code = last_error();
            return raw_error(err::UNK, err_msg(code));
        }
    }

    let h_file = handle as usize as HANDLE;
    let mut retries = 0;

    loop {
        let h_map = unsafe {
            CreateFileMappingW(h_file, core::ptr::null(), PAGE_READWRITE, 0, 0, core::ptr::null())
        };

        if hints::unlikely(h_map.is_null()) {
            let code = last_error();
            let err_msg = err_msg(code);

            match code {
                ERROR_LOCK_VIOLATION | ERROR_SHARING_VIOLATION => {
                    if retries < MAX_RETRIES {
                        retries += 1;
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    return raw_error(err::UNK, err_msg);
                }
                _ => return raw_error(err::UNK, err_msg),
            }
        }

        let view = unsafe {
            MapViewOfFile3(
                h_map,
                h_process,
                target_addr as *const core::ffi::c_void,
                curr_len as u64,
                added_len,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                core::ptr::null_mut(),
                0,
            )
        };

        unsafe { CloseHandle(h_map) };

        if hints::unlikely(view.Value.is_null()) {
            let code = last_error();
            return raw_error(err::UNK, err_msg(code));
        }

        return Ok((target_addr, added_len));
    }
}

/// Unmap the created memory mapping w/ `UnmapViewOfFile` by given raw pointer `ptr` and `length`
fn munmap_raw(ptr: TPtr, length: usize) -> FrozenResult<()> {
    if hints::unlikely(length == 0) {
        return raw_error(err::HCF, "unmap length must be greater than zero");
    }

    if hints::unlikely(ptr.is_null()) {
        return raw_error(err::HCF, "cannot unmap null pointer");
    }

    let ok = unsafe {
        UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: ptr as *mut core::ffi::c_void })
    };

    if hints::likely(ok != 0) {
        return Ok(());
    }

    let code = last_error();
    let err_msg = err_msg(code);

    match code {
        ERROR_INVALID_ADDRESS | ERROR_INVALID_PARAMETER => raw_error(err::HCF, err_msg),
        _ => raw_error(err::UNK, err_msg),
    }
}

/// Syncs in-cache data updates on the storage device w/ `FlushViewOfFile`
fn msync_raw(ptr: TPtr, length: usize) -> FrozenResult<()> {
    if hints::unlikely(length == 0) {
        return Ok(());
    }

    if hints::unlikely(ptr.is_null()) {
        return raw_error(err::NMM, "cannot sync null pointer");
    }

    let page_size = WINMemMap::page_size()?.as_usize();
    if hints::unlikely(ptr as usize % page_size != 0) {
        return raw_error(err::HCF, "address is not page-aligned");
    }

    let mut retries = 0;
    loop {
        let ok = unsafe { FlushViewOfFile(ptr as *const core::ffi::c_void, length) };

        if hints::likely(ok != 0) {
            return Ok(());
        }

        let code = last_error();
        let err_msg = err_msg(code);

        match code {
            ERROR_LOCK_VIOLATION | ERROR_SHARING_VIOLATION => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                return raw_error(err::SYN, err_msg);
            }
            ERROR_IO_DEVICE => return raw_error(err::SYN, err_msg),
            ERROR_INVALID_PARAMETER | ERROR_INVALID_ADDRESS => return raw_error(err::HCF, err_msg),
            ERROR_NOT_ENOUGH_MEMORY | ERROR_OUTOFMEMORY => return raw_error(err::NMM, err_msg),
            _ => return raw_error(err::UNK, err_msg),
        }
    }
}

/// Discards/evicts mapped pages w/ `VirtualUnlock`
fn discard_raw(ptr: *mut core::ffi::c_void, length: usize) -> FrozenResult<()> {
    if hints::unlikely(ptr.is_null()) {
        return raw_error(err::NMM, "cannot advise unmapped null pointer");
    }

    let page_size = WINMemMap::page_size()?.as_usize();
    if hints::unlikely(ptr as usize % page_size != 0) {
        return raw_error(err::HCF, "address is not page-aligned");
    }

    let aligned_len = length.div_ceil(page_size) * page_size;

    let mut retries = 0;
    loop {
        // Calling VirtualUnlock on an unlocked range removes those pages from the process working set, returning FALSE
        // with last error ERROR_NOT_LOCKED; if the pages were locked, it unlocks them and returns nonzero
        let ok = unsafe { VirtualUnlock(ptr, aligned_len) };
        if hints::likely(ok != 0) {
            return Ok(());
        }

        let code = last_error();
        if hints::likely(code == ERROR_NOT_LOCKED) {
            return Ok(());
        }

        let err_msg = err_msg(code);
        match code {
            ERROR_LOCK_VIOLATION | ERROR_SHARING_VIOLATION => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                return raw_error(err::UNK, err_msg);
            }
            ERROR_INVALID_PARAMETER => return raw_error(err::HCF, err_msg),
            ERROR_INVALID_ADDRESS => return raw_error(err::NMM, err_msg),
            ERROR_NOT_ENOUGH_MEMORY | ERROR_OUTOFMEMORY => return raw_error(err::NMM, err_msg),
            _ => return raw_error(err::UNK, err_msg),
        }
    }
}

/// Fetch the last thread-local error code for Windows
#[inline]
fn last_error() -> u32 {
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

/// Convert a raw OS error code into a formatted human-readable error description
#[inline]
fn err_msg(code: u32) -> String {
    std::io::Error::from_raw_os_error(code as i32).to_string()
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
        fn ok_last_error() {
            unsafe {
                windows_sys::Win32::Foundation::SetLastError(ERROR_INVALID_HANDLE);
                assert_eq!(last_error(), ERROR_INVALID_HANDLE);
            }
        }

        #[test]
        fn ok_err_msg() {
            let msg = err_msg(windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND);
            assert!(!msg.is_empty(), "ERROR_FILE_NOT_FOUND must produce message");
        }
    }

    mod page_size {
        use super::*;

        #[test]
        fn ok_page_size_returns_valid_power_of_two() {
            let size = WINMemMap::page_size().expect("page_size must succeed");
            assert!(size.is_power_of_two(), "page size must be a power of two, got {size}");
            assert!(
                size.as_usize() >= 0x1000,
                "page size must be at least 4096 bytes on supported 64-bit systems, got {size}"
            );
        }

        #[test]
        fn ok_page_size_matches_system_info() {
            let size = WINMemMap::page_size().expect("page_size must succeed");
            let mut sys_info = unsafe { core::mem::zeroed::<SYSTEM_INFO>() };
            unsafe { GetSystemInfo(&mut sys_info) };
            assert_eq!(size.as_usize(), sys_info.dwPageSize as usize);
        }

        #[test]
        fn ok_page_size_consistency() {
            let first = WINMemMap::page_size().unwrap();
            for _ in 0..0x3E8 {
                assert_eq!(WINMemMap::page_size().unwrap(), first);
            }
        }

        #[test]
        fn ok_page_size_across_threads() {
            let expected = WINMemMap::page_size().unwrap();
            let mut handles = Vec::new();
            for _ in 0..0x10 {
                let handle = std::thread::spawn(move || {
                    for _ in 0..0x64 {
                        assert_eq!(WINMemMap::page_size().unwrap(), expected);
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
            let page_size = WINMemMap::page_size().expect("page_size").as_usize();
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
            let page_size = WINMemMap::page_size().unwrap().as_usize();

            unsafe {
                let mmap = WINMemMap::new(file.fd(), total_len, total_len).unwrap();

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
            let page_size = WINMemMap::page_size().unwrap().as_usize();

            unsafe {
                let mmap = WINMemMap::new(file.fd(), total_len, total_len).unwrap();

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
            let page_size = WINMemMap::page_size().unwrap().as_usize();

            unsafe {
                let mmap = WINMemMap::new(file.fd(), total_len, total_len).unwrap();

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
            let mmap = WINMemMap::new(file.fd(), total_len, total_len).unwrap();
            let first = mmap.active_page_indices().unwrap();
            let second = mmap.active_page_indices().unwrap();
            assert_eq!(first, second);
            mmap.unmap().unwrap();
        }
    }

    mod eviction {
        use super::*;

        fn new_paged_tmp(num_pages: usize) -> (tempfile::TempDir, File, usize) {
            let page_size = WINMemMap::page_size().expect("page_size").as_usize();
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
            let mmap = WINMemMap::new(file.fd(), total_len, total_len).unwrap();
            assert!(mmap.evict_pages(&[]).is_ok());
            mmap.unmap().unwrap();
        }

        #[test]
        fn ok_evict_single_page() {
            let (_dir, file, total_len) = new_paged_tmp(2);
            unsafe {
                let mmap = WINMemMap::new(file.fd(), total_len, total_len).unwrap();
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
            let page_size = WINMemMap::page_size().unwrap().as_usize();

            unsafe {
                let mmap = WINMemMap::new(file.fd(), total_len, total_len).unwrap();
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
            let page_size = WINMemMap::page_size().unwrap().as_usize();

            unsafe {
                let mmap = WINMemMap::new(file.fd(), total_len, total_len).unwrap();
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
            let page_size = WINMemMap::page_size().unwrap().as_usize();

            unsafe {
                let mmap = WINMemMap::new(file.fd(), total_len, total_len).unwrap();
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
            let mmap = WINMemMap::new(file.fd(), total_len, total_len).unwrap();
            let err = mmap.evict_pages(&[2]).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);

            let err_large = mmap.evict_pages(&[0x64]).unwrap_err();
            assert_eq!(err_large.reason, err::HCF.reason);

            mmap.unmap().unwrap();
        }

        #[test]
        fn err_evict_unmapped_and_unaligned_ptr() {
            let err_unmapped = discard_raw(ptr::null_mut(), BUFFER_SIZE).unwrap_err();
            assert_eq!(err_unmapped.reason, err::NMM.reason);

            let (_dir, file, total_len) = new_paged_tmp(1);
            unsafe {
                let mmap = WINMemMap::new(file.fd(), total_len, total_len).unwrap();
                let unaligned = (mmap.as_ptr::<u8>(0) as usize + 1) as *mut core::ffi::c_void;
                let err_unaligned = discard_raw(unaligned, BUFFER_SIZE).unwrap_err();
                assert_eq!(err_unaligned.reason, err::HCF.reason);

                mmap.unmap().unwrap();
            }
        }
    }

    mod map_unmap {
        use super::*;

        #[test]
        fn ok_map_unmap_cycle() {
            let (_dir, file) = new_tmp();

            let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
            mmap.unmap().unwrap();
        }

        #[test]
        fn ok_map_zero_bytes_on_new() {
            let (_dir, file) = new_tmp();
            const BUF: [u8; LENGTH] = [0; LENGTH];

            unsafe {
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let ptr = mmap.as_ptr::<[u8; LENGTH]>(0);
                assert_eq!(*ptr, BUF);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_unmap_consumes_self() {
            let (_dir, file) = new_tmp();

            let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
            assert!(mmap.unmap().is_ok());
        }

        #[test]
        fn ok_multiple_shared_mappings_reflect_writes() {
            const VAL: u64 = 0x1234_5678_9ABC_DEF0;
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap1 = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
                let mmap2 = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

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
            let err = WINMemMap::new(file.fd(), 0, 0).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
        }

        #[test]
        fn err_map_on_invalid_handle() {
            let (_dir, _) = new_tmp();
            let err = WINMemMap::new(-1isize, LENGTH, LENGTH).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);

            let err_zero = WINMemMap::new(0isize, LENGTH, LENGTH).unwrap_err();
            assert_eq!(err_zero.reason, err::HCF.reason);
        }

        #[test]
        fn err_map_on_read_only_handle() {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::{
                Foundation::GENERIC_READ,
                Storage::FileSystem::{
                    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
                    FILE_SHARE_WRITE, OPEN_EXISTING,
                },
            };

            let (dir, file) = new_tmp();
            drop(file);

            let path = dir.path().join("tmp_map");
            let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            wide.push(0);

            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    core::ptr::null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    core::ptr::null_mut(),
                )
            };
            assert_ne!(handle, INVALID_HANDLE_VALUE, "open read-only must succeed");

            let err = WINMemMap::new(handle as isize, LENGTH, LENGTH).unwrap_err();
            assert_eq!(err.reason, err::PRM.reason);
            unsafe {
                CloseHandle(handle);
            }
        }

        #[test]
        fn err_unmap_on_invalid_length() {
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
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

            let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
            mmap.sync(LENGTH).unwrap();
            mmap.unmap().unwrap();
        }

        #[test]
        fn ok_sync_after_sync() {
            let (_dir, file) = new_tmp();

            let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

            mmap.sync(LENGTH).unwrap();
            mmap.sync(LENGTH).unwrap();
            mmap.sync(LENGTH).unwrap();
            mmap.sync(LENGTH).unwrap();

            mmap.unmap().unwrap();
        }

        #[test]
        fn ok_sync_zero_length() {
            let (_dir, file) = new_tmp();

            let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
            assert!(mmap.sync(0).is_ok());
            mmap.unmap().unwrap();
        }

        #[test]
        fn ok_sync_partial_range() {
            const VAL: u64 = 0xA5A5_A5A5_A5A5_A5A5;
            let (_dir, file) = new_tmp();
            unsafe {
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
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
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
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
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

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
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

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
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

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
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

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
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let rptr = mmap.as_ptr::<u64>(0);
                assert_eq!(*rptr, 0);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_write_read_heterogeneous_types() {
            let (_dir, file) = new_tmp();
            unsafe {
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

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
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

                let ptr = mmap.as_mut_ptr::<u64>(0);
                *ptr = VAL;

                mmap.sync(LENGTH).unwrap();
                mmap.unmap().unwrap();
            }

            unsafe {
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

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
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

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
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();

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
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
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
                let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
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
            let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
            assert_eq!(mmap.length(), LENGTH);
        }

        #[test]
        fn ok_explicit_unmap_prevents_drop_double_unmap() {
            let (_dir, file) = new_tmp();
            let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
            assert!(mmap.unmap().is_ok());
        }

        #[test]
        fn ok_drop_persists_written_data() {
            const VAL: u64 = 0xCAFEBABE_DEADBEEF;
            let (_dir, file) = new_tmp();

            unsafe {
                {
                    let mmap = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
                    let wptr = mmap.as_mut_ptr::<u64>(0);
                    *wptr = VAL;
                    mmap.sync(LENGTH).unwrap();
                }

                // Re-open and verify data persisted across drop
                let mmap2 = WINMemMap::new(file.fd(), LENGTH, LENGTH).unwrap();
                let rptr = mmap2.as_ptr::<u64>(0);
                assert_eq!(*rptr, VAL);
                mmap2.unmap().unwrap();
            }
        }
    }
}
