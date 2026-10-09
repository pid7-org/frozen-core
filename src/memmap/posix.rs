//! Implementation of memory mapped files for POSIX (Linux and macOS) systems

use super::err::{self, raw_error};
use crate::{error::FrozenResult, hints};
use core::ptr;
use libc::{
    EACCES, EAGAIN, EBADF, EBUSY, EINTR, EINVAL, EIO, ENODEV, ENOMEM, EOVERFLOW, EPERM, ETXTBSY,
    MAP_FAILED, MAP_SHARED, MS_SYNC, PROT_READ, PROT_WRITE, c_void, mmap, msync, munmap, off_t,
    size_t,
};

/// Base pointer for `mmap(2)` mapped memory
type TPtr = *mut u8;

/// Max allowed retries for `EINTR`, `EBUSY` and `EAGAIN` errors
const MAX_RETRIES: usize = 0x0A;

/// Custom implementation of memory-mapped memory via `mmap(2)` for POSIX systems
///
/// Wraps an allocated virtual memory mapping, managing its lifetime via RAII.
/// Automatic cleanup is provided via [`Drop`] unless explicitly consumed with [`POSIXMemMap::unmap`].
#[derive(Debug)]
pub(super) struct POSIXMemMap {
    ptr: TPtr,
    length: usize,
}

impl Drop for POSIXMemMap {
    /// Automatically releases mapped memory via `munmap(2)` when going out of scope if not explicitly unmapped
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.length > 0 {
            let _ = munmap_raw(self.ptr, self.length);
            self.ptr = ptr::null_mut();
            self.length = 0;
        }
    }
}

impl POSIXMemMap {
    /// Create a new [`POSIXMemMap`] by mapping `length` bytes of the file represented by `fd`
    ///
    /// Maps memory from offset 0 with `PROT_READ | PROT_WRITE` and `MAP_SHARED`.
    pub(super) fn new(fd: i32, length: size_t) -> FrozenResult<Self> {
        let ptr = mmap_raw(fd, length)?;
        Ok(Self { ptr, length })
    }

    /// Unmap [`POSIXMemMap`] to explicitly release mapped memory resources
    ///
    /// Consumes `self` to prevent use-after-free or double-unmapping at compile time.
    /// Clears internal state so subsequent [`Drop`] execution becomes a safe no-op.
    pub(super) fn unmap(mut self) -> FrozenResult<()> {
        if !self.ptr.is_null() && self.length > 0 {
            let res = munmap_raw(self.ptr, self.length);
            self.ptr = ptr::null_mut();
            self.length = 0;
            res
        } else {
            Ok(())
        }
    }

    /// Returns the length of the memory mapping in bytes
    #[inline]
    pub(super) fn length(&self) -> usize {
        self.length
    }

    /// Syncs in-cache data updates on the storage device
    ///
    /// ## Durability
    ///
    /// In POSIX systems `msync(MS_SYNC)` does not provide crash-safe durability; this syscall is used as a best-effort
    /// operation to explicitly push dirty mmapped pages into filesystem writeback.
    ///
    /// For strong durability, use of [`File::sync`](crate::file::File::sync) is required right after calling [`POSIXMemMap::sync`].
    ///
    /// ## Why do we retry?
    ///
    /// POSIX syscalls are interruptible by signals and may fail w/ `EINTR`, `EBUSY`, or `EAGAIN`. In such cases,
    /// no progress is guaranteed, so the syscall must be retried.
    pub(super) fn sync(&self, length: usize) -> FrozenResult<()> {
        msync_raw(self.ptr, length)
    }

    /// Get a mutable (read/write) typed pointer to `T` at given `offset`
    ///
    /// Given `offset` must be aligned w/ `std::mem::align_of::<T>()`
    ///
    /// # Safety
    ///
    /// - The caller must ensure that `offset + std::mem::size_of::<T>() <= length` where `length` is the mapped region size.
    /// - The pointer `base + offset` must be properly aligned for `T`.
    /// - The caller must uphold Rust's aliasing rules (no concurrent unsynchronized reads or writes to overlapping bytes).
    /// - The memory mapping must not have been unmapped via [`POSIXMemMap::unmap`].
    #[inline]
    #[allow(unsafe_op_in_unsafe_fn)]
    pub(super) unsafe fn as_mut_ptr<T>(&self, offset: usize) -> *mut T
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
    /// - The caller must ensure that `offset + std::mem::size_of::<T>() <= length` where `length` is the mapped region size.
    /// - The pointer `base + offset` must be properly aligned for `T`.
    /// - The caller must uphold Rust's aliasing rules (no concurrent unsynchronized writes).
    /// - The memory mapping must not have been unmapped via [`POSIXMemMap::unmap`].
    #[inline]
    #[allow(unsafe_op_in_unsafe_fn)]
    pub(super) unsafe fn as_ptr<T>(&self, offset: usize) -> *const T
    where
        T: Sized,
    {
        unsafe { self.ptr.add(offset) as *const T }
    }
}

/// Create a new memory mapping w/ `mmap(2)` on given `fd` and `length`
///
/// ## Caveats of `mmap(2)` on POSIX
///
/// In POSIX systems, when calling `mmap(2)`, the provided offset must be a multiple of page size,
/// i.e. `sysconf(_SC_PAGESIZE)`, otherwise an `EINVAL` error is returned.
///
/// For our use case, we always map the entire file from offset 0, hence this is never an issue for us.
fn mmap_raw(fd: i32, length: size_t) -> FrozenResult<TPtr> {
    let mut retries = 0; // only for transient errors (EINTR, EBUSY, EAGAIN)
    loop {
        let ptr = unsafe {
            mmap(ptr::null_mut(), length, PROT_WRITE | PROT_READ, MAP_SHARED, fd, 0 as off_t)
        };

        if ptr == MAP_FAILED {
            let errno = last_errno();
            let err_msg = err_msg(errno);

            match errno {
                // NOTE: We must retry on interruption or transient busy errors
                EINTR | EBUSY | EAGAIN => {
                    if retries < MAX_RETRIES {
                        retries += 1;
                        continue;
                    }

                    return raw_error(err::UNK, err_msg);
                }

                // invalid fd, invalid fd type, invalid length, etc.
                EINVAL | EBADF | EOVERFLOW => return raw_error(err::HCF, err_msg),

                // no more memory available
                ENOMEM => return raw_error(err::NMM, err_msg),

                // permission denied or read-only file
                EACCES | EPERM | ENODEV | ETXTBSY => return raw_error(err::PRM, err_msg),

                _ => return raw_error(err::UNK, err_msg),
            };
        }

        return Ok(ptr as *mut u8);
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
        // invalid/unaligned ptr or address range is not mapped
        EINVAL | ENOMEM => raw_error(err::HCF, err_msg),

        _ => raw_error(err::UNK, err_msg),
    }
}

/// Syncs in-cache data updates on the storage device
///
/// ## Caveats of `msync(2)` on POSIX
///
/// This syscall by itself does not provide any durability guarantee; it is used as a best-effort operation
/// to explicitly push dirty mmapped pages into filesystem writeback, to aid hard sync calls like `fdatasync` on Linux
/// and `fcntl(F_FULLFSYNC)` on macOS.
fn msync_raw(ptr: TPtr, length: size_t) -> FrozenResult<()> {
    let mut retries = 0; // only for transient errors (EINTR, EBUSY, EAGAIN)
    loop {
        let res = unsafe { msync(ptr as *mut c_void, length, MS_SYNC) };
        if hints::likely(res == 0) {
            return Ok(());
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            // IO interrupt, locked file or transient error
            EINTR | EBUSY | EAGAIN => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                // NOTE: sync error indicates that retries exhausted and durability is broken
                // in the current/last window/batch
                return raw_error(err::SYN, err_msg);
            }

            // fatal error, i.e. no sync for writes in recent window/batch
            EIO => return raw_error(err::SYN, err_msg),

            // invalid address range, unaligned pointer, or invalid flags
            EINVAL => return raw_error(err::HCF, err_msg),

            // no-more memory available
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
            unsafe {
                let msg = err_msg(libc::ENOENT);
                assert!(!msg.is_empty(), "ENOENT must produce message");
            }
        }
    }

    mod map_unmap {
        use super::*;

        #[test]
        fn ok_map_unmap_cycle() {
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();
                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_map_zero_bytes_on_new() {
            let (_dir, file) = new_tmp();
            const BUF: [u8; LENGTH] = [0; LENGTH];

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();

                let ptr = mmap.as_ptr::<[u8; LENGTH]>(0);
                assert_eq!(*ptr, BUF);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_unmap_consumes_self() {
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();
                assert!(mmap.unmap().is_ok());
            }
        }

        #[test]
        fn err_map_on_invalid_length() {
            let (_dir, file) = new_tmp();
            unsafe {
                let err = POSIXMemMap::new(file.fd(), 0).unwrap_err();
                assert_eq!(err.reason, err::HCF.reason);
            }
        }

        #[test]
        fn err_map_on_invalid_fd() {
            let (_dir, _) = new_tmp();
            unsafe {
                let err = POSIXMemMap::new(-1, LENGTH).unwrap_err();
                assert_eq!(err.reason, err::HCF.reason);
            }
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
                let err = POSIXMemMap::new(fd, LENGTH).unwrap_err();
                assert_eq!(err.reason, err::PRM.reason);
                libc::close(fd);
            }
        }

        #[test]
        fn err_unmap_on_invalid_length() {
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();
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

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();
                mmap.sync(LENGTH).unwrap();
                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_sync_after_sync() {
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();

                mmap.sync(LENGTH).unwrap();
                mmap.sync(LENGTH).unwrap();
                mmap.sync(LENGTH).unwrap();
                mmap.sync(LENGTH).unwrap();

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_sync_zero_length() {
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();
                assert!(mmap.sync(0).is_ok());
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
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();

                // write
                let wptr = mmap.as_mut_ptr::<u64>(0);
                *wptr = VAL;

                // read
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
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();

                // write
                let wptr = mmap.as_mut_ptr::<u64>(8);
                *wptr = VAL;

                // read
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
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();

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
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();

                // write
                let wptr = mmap.as_mut_ptr::<[u32; 0x0A]>(0);
                *wptr = VAL;

                // sync
                mmap.sync(LENGTH).unwrap();

                // read
                let rptr = mmap.as_ptr::<[u32; 0x0A]>(0);
                assert_eq!(*rptr, VAL);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_read_zero_bytes() {
            let (_dir, file) = new_tmp();

            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();

                let rptr = mmap.as_ptr::<u64>(0);
                assert_eq!(*rptr, 0);

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

            // create + map + write + sync
            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();

                let ptr = mmap.as_mut_ptr::<u64>(0);
                *ptr = VAL;

                mmap.sync(LENGTH).unwrap();
                mmap.unmap().unwrap();
            }

            // open + map + read
            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();

                let ptr = mmap.as_ptr::<u64>(0);
                assert_eq!(*ptr, VAL);

                mmap.unmap().unwrap();
            }
        }

        #[test]
        fn ok_map_durability_after_unmap_and_close() {
            const VAL: u64 = 0xDEADC0DEDEADC0DE;
            let (dir, file) = new_tmp();

            // create + map + write + sync + unmap
            unsafe {
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();

                let ptr = mmap.as_mut_ptr::<u64>(0);
                *ptr = VAL;

                mmap.sync(LENGTH).unwrap();
                mmap.unmap().unwrap();
                drop(file);
            }

            // open + map + read
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
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();

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
            let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();
            assert_eq!(mmap.length(), LENGTH);
        }

        #[test]
        fn ok_drop_persists_written_data() {
            const VAL: u64 = 0xCAFEBABE_DEADBEEF;
            let (_dir, file) = new_tmp();

            unsafe {
                {
                    let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();
                    let wptr = mmap.as_mut_ptr::<u64>(0);
                    *wptr = VAL;
                    mmap.sync(LENGTH).unwrap();
                    // mmap goes out of scope and is dropped here without calling unmap
                }

                // Re-open and verify data persisted across drop
                let mmap2 = POSIXMemMap::new(file.fd(), LENGTH).unwrap();
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
                let mmap = POSIXMemMap::new(file.fd(), LENGTH).unwrap();
                addr = mmap.as_ptr::<u8>(0) as usize;

                let is_mapped = || {
                    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
                    maps.lines().any(|line| {
                        if line.contains(path_str) {
                            if let Some((start_s, end_s)) =
                                line.split_whitespace().next().and_then(|r| r.split_once('-'))
                            {
                                if let (Ok(start), Ok(end)) = (
                                    usize::from_str_radix(start_s, 16),
                                    usize::from_str_radix(end_s, 16),
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
