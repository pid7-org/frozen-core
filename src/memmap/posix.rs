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

/// max allowed retries for `EINTR`, `EBUSY` and `EAGAIN` errors
const MAX_RETRIES: usize = 0x0A;

/// Custom impl of `mmap(2)` for POSIX systems
#[derive(Debug)]
pub(super) struct POSIXMemMap(TPtr);

unsafe impl Send for POSIXMemMap {}
unsafe impl Sync for POSIXMemMap {}

impl POSIXMemMap {
    /// Create a new [`POSIXMMap`] w/ given `fd` and `length`
    pub(super) fn new(fd: i32, length: size_t) -> FrozenResult<Self> {
        let ptr = mmap_raw(fd, length)?;
        Ok(Self(ptr))
    }

    /// Close [`POSIXMMap`] to give up allocated resources
    pub(super) fn unmap(&self, length: usize) -> FrozenResult<()> {
        munmap_raw(self.0, length)
    }

    /// Syncs in cache data updates on the storage device
    ///
    /// ## Durability
    ///
    /// In POSIX systems `msync(MS_SYNC)`, does not provide crash safe durability, this syscall is used as a best-effort
    /// operation, to explicitly push dirty mmaped pages into fs writeback
    ///
    /// For strong durability, use of [`FrozenFile::sync`] is required, right after calling [`FrozenMMap::sync`]
    ///
    /// ## Why do we retry?
    ///
    /// POSIX syscalls are interruptible by signals, and may fail w/ `EINTR`, in such cases, no progress is guaranteed,
    /// so the syscall must be retried
    pub(super) fn sync(&self, length: usize) -> FrozenResult<()> {
        msync_raw(self.0, length)
    }

    /// Get a mutable (read/write) typed pointer to `T` at given `offset`
    ///
    /// Given `offset` must be aligned w/ `std::mem::size_of::<T>()`
    #[inline]
    #[allow(unsafe_op_in_unsafe_fn)]
    pub(super) unsafe fn as_mut_ptr<T>(&self, offset: usize) -> *mut T
    where
        T: Sized,
    {
        unsafe { self.0.add(offset) as *mut T }
    }

    /// Get a immutable (read only) typed pointer to `T` at given `offset`
    ///
    /// Given `offset` must be aligned w/ `std::mem::size_of::<T>()`
    #[inline]
    #[allow(unsafe_op_in_unsafe_fn)]
    pub(super) unsafe fn as_ptr<T>(&self, offset: usize) -> *const T
    where
        T: Sized,
    {
        self.0.add(offset) as *const T
    }
}

/// create a new memory mapping w/ `mmap(2)` on given `fd` and `length`
///
/// ## Caveats of `mmap(2)` on POSIX
///
/// In POSIX systems, when calling `mmap(2)`, the provided offset must be multiple of page size,
/// i.e. `sysconf(_SC_PAGESIZE)`, otherwise an `EINVAL` error is thrown
///
/// For our usecase, we always map the entire file, hence this is never an issue for us
fn mmap_raw(fd: i32, length: size_t) -> FrozenResult<TPtr> {
    let mut retries = 0; // only for EINTR errors
    loop {
        let ptr = unsafe {
            mmap(ptr::null_mut(), length, PROT_WRITE | PROT_READ, MAP_SHARED, fd, 0 as off_t)
        };

        if ptr == MAP_FAILED {
            let errno = last_errno();
            let err_msg = err_msg(errno);

            match errno {
                // NOTE: We must retry on interuption errors (EINTR retry)
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

/// unmap the created memory mapping w/ `mummap(2)` by given ref `ptr` and `length`
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

/// Syncs in cache data updates on the storage device
///
/// ## Caveats of `msync(2)` on POSIX
///
/// This syscall by itself does not provide any durability guarantee, it's used as best-effort operation
/// to explicitly push dirty mmaped pages into fs writeback, to aid hard sync calls like `fdatasync` on linux
/// and `fnctl(F_FULLSYNC)` on mac
fn msync_raw(ptr: TPtr, length: size_t) -> FrozenResult<()> {
    let mut retries = 0; // only for EINTR errors
    loop {
        let res = unsafe { msync(ptr as *mut c_void, length, MS_SYNC) };
        if hints::likely(res == 0) {
            return Ok(());
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            // IO interrupt, locked file or fatel error
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

            // invalid fd or lack of support for sync
            EINVAL => return raw_error(err::HCF, err_msg),

            // no-more memory available
            ENOMEM => return raw_error(err::NMM, err_msg),

            _ => return raw_error(err::UNK, err_msg),
        }
    }
}

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

#[inline]
fn err_msg(errno: i32) -> String {
    std::io::Error::from_raw_os_error(errno).to_string()
}
