//! Implementation of `std::fs::File` for POSIX (Linux and MacOS) systems

use super::{err, interface::FileInterface};
use crate::{error::FrozenResult, hints};
use libc::{
    EACCES, EAGAIN, EBADF, EBUSY, EEXIST, EFAULT, EINTR, EINVAL, EIO, EISDIR, ELOOP, ENAMETOOLONG,
    ENOENT, ENOLCK, ENOSPC, ENOTDIR, EOPNOTSUPP, EPERM, EROFS, ESPIPE, EWOULDBLOCK, F_OK, LOCK_EX,
    LOCK_NB, O_CLOEXEC, O_CREAT, O_DIRECTORY, O_EXCL, O_RDONLY, O_RDWR, S_IRUSR, S_IWUSR, access,
    c_int, c_uint, c_void, close, flock, fstat, ftruncate, off_t, open, pread, pwrite, size_t,
    stat, unlink,
};
use std::sync::atomic;

/// File descriptor type for POSIX systems
pub(super) type FileId = c_int;

/// Placeholder value for when current fd is closed
pub(in crate::file) const CLOSED_FD: FileId = FileId::MIN;

/// Max allowed retries for `EINTR`, `EBUSY` and `EAGAIN` errors
const MAX_RETRIES: usize = 0x0C;

/// Custom implementation of `std::fs::File` for POSIX systems
#[derive(Debug)]
pub(super) struct POSIXFile {
    fd: atomic::AtomicI32,
}

impl FileInterface for POSIXFile {
    type Id = FileId;
    const CLOSED_ID: Self::Id = CLOSED_FD;

    /// Read file descriptor of [`POSIXFile`]
    fn fd(&self) -> FileId {
        self.fd.load(atomic::Ordering::Acquire)
    }

    /// Check if [`POSIXFile`] exists on storage device or not
    ///
    /// ## Access Semantics
    ///
    /// Uses `access(path, F_OK)` to verify the existence of the file
    fn exists(path: &std::path::Path) -> FrozenResult<bool> {
        let cpath = path_to_cstring(path)?;
        let res = unsafe { access(cpath.as_ptr(), F_OK) };

        if res == 0 {
            return Ok(true);
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            // File or one of the path components does not exist
            ENOENT | ENOTDIR => Ok(false),

            // Lack of search or read permission on path components
            EACCES | EPERM => err::raw_error(err::PRM, err_msg),

            // The mode was incorrectly specified
            EINVAL => err::raw_error(err::HCF, err_msg),

            // Path syntax, invalid parent directory or resolution errors (e.g. symlink cycle, path too long)
            ELOOP | ENAMETOOLONG => err::raw_error(err::INV, err_msg),

            // EIO (i.e. Hardware I/O or storage failure) or other failures
            _ => err::raw_error(err::UNK, err_msg),
        }
    }

    /// Create a new [`POSIXFile`] atomically w/ `O_CREAT | O_EXCL`
    fn create(path: &std::path::Path) -> FrozenResult<Self> {
        let fd = open_raw(path, create_flags())?;
        let file = Self { fd: atomic::AtomicI32::new(fd) };

        if let Err(mut e) = file.flock() {
            if let Err(close_err) = file.close() {
                e.add_suppressed(close_err);
            }
            return Err(e);
        }

        if let Err(mut e) = sync_parent_dir(path) {
            if let Err(close_err) = file.close() {
                e.add_suppressed(close_err);
            }
            return Err(e);
        }

        // (linux only) best effort call to provide a hint to the kernel that the file will be
        // accessed in a random pattern
        #[cfg(target_os = "linux")]
        {
            let _res = f_advise_raw(file.fd());

            // INFO: Read the function docs of [`f_advise_raw`] for detailed info on why the error is only
            // propagated in non-prod env's

            #[cfg(debug_assertions)]
            if let Err(mut e) = _res {
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }

                return Err(e);
            }
        }

        Ok(file)
    }

    /// Open an existing [`POSIXFile`] w/o `O_CREAT`
    fn open(path: &std::path::Path) -> FrozenResult<Self> {
        let fd = open_raw(path, open_flags())?;
        let file = Self { fd: atomic::AtomicI32::new(fd) };

        // (linux only) best effort call to provide a hint to the kernel that the file will be
        // accessed in a random pattern
        #[cfg(target_os = "linux")]
        {
            let _res = f_advise_raw(file.fd());

            // INFO: Read the function docs of [`f_advise_raw`] for detailed info on why the error is only
            // propagated in non-prod env's

            #[cfg(debug_assertions)]
            if let Err(mut e) = _res {
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }

                return Err(e);
            }
        }

        Ok(file)
    }

    /// Acquire an exclusive advisory lock on [`POSIXFile`]
    ///
    /// ## Purpose
    ///
    /// We must ensure that only a single [`POSIXFile`] instance, across all processes, can operate on the
    /// underlying file, at a given time
    ///
    /// So, we acquire an exclusive lock, for the entire file, after open, so if another process tries, it could
    /// halt or choose not to exist anymore, i.e. to avoid multiple open handles across the same underlying file
    ///
    /// ## Advisory Semantics
    ///
    /// We use `flock(fd)`, which provides advisory locking only, the kernel does not prevent other processes from
    /// calling `open()`, but any cooperating process attempting to acquire the same exclusive lock will fail
    /// with `EWOULDBLOCK` i.e. [`err::LCK`]
    ///
    /// ## Why do we retry?
    ///
    /// POSIX syscalls are interruptible by signals, and may fail w/ `EINTR`, in such cases no progress is guaranteed,
    /// so the syscall must be retried
    ///
    /// ## Lock Lifecycle & TOCTOU Immunity
    ///
    /// Locks created via `flock` are associated with the underlying open file description in the kernel table
    ///
    /// We do not need an explicit unlock syscall (`LOCK_UN`) before closing; as the kernel automatically releases
    /// the lock atomically when the descriptor is closed (or on process termination)
    ///
    /// Releasing the lock implicitly on `close` prevents TOCTOU race conditions where another cooperating process
    /// could acquire the lock in a window between an explicit unlock and file closure
    ///
    /// Ref -> [https://man7.org/linux/man-pages/man2/flock.2.html]
    fn flock(&self) -> FrozenResult<()> {
        flock_raw(self.fd())
    }

    /// Close [`POSIXFile`] to give up on allocated resources
    ///
    /// Consumes `self` by value to prevent concurrent _use after close_ and descriptor reuse races
    ///
    /// ## Lock Release
    ///
    /// As detailed in [`flock`] function docs, any advisory exclusive lock held on this file is atomically
    /// released by the kernel upon closing the descriptor, eliminating the need to release the lock manually
    ///
    /// ## Sync Error (`err::SYN`)
    ///
    /// In POSIX systems, kernel may report delayed write/sync failures when closing, these are durability errors,
    /// fatal for storage layer
    ///
    /// We can easily tackle this error for each batch of writes by enforcing hard durability guarantees right after
    /// the write ops, and making sure they are completed without errors
    ///
    /// this provides strong durability for the storage engine, and if `EIO` occurs, anyhow, we treat it as `err::HCF`
    /// i.e. impl failure
    fn close(self) -> FrozenResult<()> {
        let fd = self.fd.swap(CLOSED_FD, atomic::Ordering::AcqRel);
        if fd == CLOSED_FD {
            return Ok(());
        }

        close_raw(fd)
    }

    /// Removes the [`POSIXFile`] entry from the fs
    ///
    /// ## POSIX Unlink Semantics
    ///
    /// In POSIX, open files can be unlinked safely without closing first
    ///
    /// Unlinking the directory entry before closing the descriptor avoids TOCTOU races where another process might
    /// recreate or replace the file at `path` in the window between `close` and `unlink`
    ///
    /// The fs reclaims the file's data blocks and inode once all active handles (including `self`) are closed
    fn unlink(self, path: &std::path::Path) -> FrozenResult<()> {
        let cpath = path_to_cstring(path)?;

        let res = unsafe { unlink(cpath.as_ptr()) };
        if res == 0 {
            // Close the descriptor now that the fs link has been removed
            self.close()?;

            // NOTE: In POSIX systems, `unlink(path)` only updates the entry in memory, and does not guarantee
            // crash safe durability for the operation, we must perform `fsync` on the directory to make sure
            // we get crash safe durability
            return sync_parent_dir(path);
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            // missing file or invalid path
            ENOENT | ENOTDIR => err::raw_error(err::INV, err_msg),

            // lack of permission or read only fs
            EACCES | EPERM | EROFS => err::raw_error(err::PRM, err_msg),

            // NOTE:
            //
            // In POSIX systems, `unlink` operates on directory metadata (`vfs_unlink`)
            //
            // Any `EIO` returned by `unlink` originates from updating the directory entry on storage/network,
            // not from file data writeback failures (which were already flushed and consumed by `fsync`)
            //
            // Hence, `EIO` is classified as a sync/durability failure (`err::SYN`) rather than an
            // implementation failure (`err::HCF`) unlike the `close(fd)` implementation
            //
            // Ref -> https://man7.org/linux/man-pages/man2/unlink.2.html
            EIO => err::raw_error(err::SYN, err_msg),

            _ => err::raw_error(err::UNK, err_msg),
        }
    }

    /// Read current length of [`POSIXFile`] using file metadata (w/ `fstat` syscall)
    fn length(&self) -> FrozenResult<usize> {
        let mut st = unsafe { core::mem::zeroed::<stat>() };
        let res = unsafe { fstat(self.fd(), &mut st) };

        if res != 0 {
            let errno = last_errno();
            let err_msg = err_msg(errno);

            // bad or invalid fd
            if errno == EBADF || errno == EFAULT {
                return err::raw_error(err::HCF, err_msg);
            }

            return err::raw_error(err::UNK, err_msg);
        }

        if hints::unlikely(st.st_size < 0) {
            return err::raw_error(err::HCF, "filesystem reported negative file size");
        }

        match usize::try_from(st.st_size) {
            Ok(sz) => Ok(sz),
            Err(_) => err::raw_error(err::HCF, "file size exceeds usize address space"),
        }
    }

    /// Grow (i.e. zero extend) the [`POSIXFile`] w/ given `len_to_add`
    ///
    /// ## Semantics
    ///
    /// Here `grow()` is not atomic in all or nothing sense, following scenarios may happen,
    ///
    /// - on linux `fallocate` may fail, but `ftruncate` succeeds (sparse extension)
    /// - on mac `ftruncate` succeeds but `f_preallocate` may fail (sparse extension)
    /// - and on both `ftruncate` may also fail
    ///
    /// in all these scenarios, either the `st_size` is correctly updated or not updated at all (in an atomic op sense)
    ///
    /// If either of `fallocate` or `f_preallocate` has failed or is not supported by fs, as long as `ftruncate`
    /// succeeds,
    /// our future write ops will work fine
    ///
    /// This is mainly because `fallocate` and `f_preallocate` are best-effort physical extent reservations to
    /// guarantee disk space and reduce write latency
    ///
    /// ## Crash Durability
    ///
    /// `grow()` only zero-extends the file metadata in memory (`inode->i_size`) and does not invoke an immediate
    /// sync call, by itself
    ///
    /// Because `grow()` produces unwritten, zero-filled space, losing this growth in an ungraceful crash introduces
    /// absolute no data loss
    ///
    /// If any subsequent batch of data written into the allocated space, and will always trigger an explicit sync
    /// `fdatasync` or `fcntl(F_FULLSYNC)`, which flushes both the written payload and the metadata (`st_size`)
    /// required to reference it
    ///
    /// If a crash occurs before write and sync, the storage engine simply re-observes the old durable length on
    /// restart and re-triggers `grow()` idempotently
    #[inline(always)]
    fn grow(&self, curr_len: usize, len_to_add: usize) -> FrozenResult<()> {
        if len_to_add == 0 {
            return Ok(());
        }

        // Validate arithmetic bounds upfront before allocating or truncating
        let _ = match curr_len.checked_add(len_to_add) {
            Some(len) => match off_t::try_from(len) {
                Ok(off) if off >= 0 => off,
                _ => {
                    return err::raw_error(err::GRW, "target file size exceeds off_t capacity");
                }
            },
            None => {
                return err::raw_error(err::GRW, "file growth calculation overflowed usize");
            }
        };

        let fd = self.fd();

        // NOTE:
        //
        // On linux, `fallocate` must be called before `ftruncate` to handle `ENOSPC`
        //
        // If the order is reversed, the file length may be updated despite the failure to allocate
        // space on fs, which may fail all future write ops
        #[cfg(target_os = "linux")]
        fallocate_raw(fd, curr_len, len_to_add)?;

        ftruncate_raw(fd, curr_len, len_to_add)?;

        // INFO: On mac, we can hint the kernel to allocate disk space for the added `len_to_add` as it
        // can reduce the latency of future write ops
        //
        // WARN: Must always be called after `ftruncate` on mac
        #[cfg(target_os = "macos")]
        f_preallocate_raw(fd, len_to_add)?;

        Ok(())
    }

    /// Syncs the in-cache updated/added data on the storage device
    ///
    /// ## Why do we retry?
    ///
    /// POSIX syscalls are interruptible by signals, and may fail w/ `EINTR`, in such cases no progress is guaranteed,
    /// so the syscall must be retried
    ///
    /// ## `F_FULLFSYNC` vs `fsync`
    ///
    /// The supposed best os, i.e. mac, does not provide strong durability via `fsync()`, hence the data writes/updates
    /// may be lost on crash or power failure
    ///
    /// Ref ->
    /// https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fsync.2.html
    ///
    /// To achieve true crash durability (including protection against power loss, sudden crash), we have to use the
    /// `fcntl(fd, F_FULLFSYNC)` syscall
    ///
    /// ## Fallback to `fsync`
    ///
    /// `fcntl(F_FULLSYNC)` may result in `EINVAL` or `ENOTSUP` on fs which may not support it, such as network fs,
    /// FUSE mounts, FAT32 volumes, or some external devices
    ///
    /// To guard this, we fallback to `fsync()`, which does not guarantee durability for sudden crash or power loss,
    /// which is acceptable or we must make peace w/ it, that the strong durability is simply not available or allowed
    #[cfg(target_os = "macos")]
    fn sync(&self) -> FrozenResult<()> {
        f_fullsync_raw(self.fd())
    }

    /// Syncs in cache data updates on the storage device
    ///
    /// ## Why do we retry?
    ///
    /// POSIX syscalls are interruptible by signals, and may fail w/ `EINTR`, in such cases no progress is
    /// guaranteed, so the syscall must be retried
    ///
    /// ## `fsync` vs `fdatasync`
    ///
    /// We use `fdatasync()` instead of `fsync()` for persistence, as it guarantees, all updates/writes and any
    /// metadata, such as file size, are flushed to stable storage
    ///
    /// With combination of `O_NOATIME` and `fdatasync()`, we avoid non-essential metadata updates, such as access
    /// time (`atime`), modification time (`mtime`), and other bookkeeping info
    #[cfg(target_os = "linux")]
    fn sync(&self) -> FrozenResult<()> {
        fdatasync_raw(self.fd())
    }

    /// Read into given `buf` from specified `offset` w/ `pread` syscall
    #[inline(always)]
    fn pread(&self, buf: &mut [u8], offset: usize) -> FrozenResult<()> {
        if buf.is_empty() {
            return Ok(());
        }

        let fd = self.fd();

        let mut read = 0usize;
        let mut retries = 0usize;

        while read < buf.len() {
            let chunk_offset = match offset.checked_add(read) {
                Some(off) if off <= off_t::MAX as usize => off as off_t,
                _ => return err::raw_error(err::INV, "offset overflow"),
            };

            let res = unsafe {
                pread(
                    fd,
                    buf[read..].as_mut_ptr() as *mut c_void,
                    (buf.len() - read) as size_t,
                    chunk_offset,
                )
            };

            // unexpected EOF
            if res == 0 {
                // NOTE: we treat this as `Hcf` error because this only occurs when we tried to read
                // beyond current length of the file, which is result of invalid impl
                return err::default_error(err::HCF);
            }

            if hints::unlikely(res < 0) {
                let errno = last_errno();
                let err_msg = err_msg(errno);

                match errno {
                    // io interrupt
                    EINTR | EAGAIN | EBUSY => {
                        if retries < MAX_RETRIES {
                            retries += 1;
                            continue;
                        }

                        return err::raw_error(err::UNK, err_msg);
                    }

                    // permission denied
                    EACCES | EPERM => return err::raw_error(err::RED, err_msg),

                    // invalid fd, invalid fd type, bad pointer, etc.
                    EINVAL | EBADF | EFAULT | ESPIPE => {
                        return err::raw_error(err::HCF, err_msg);
                    }

                    _ => return err::raw_error(err::UNK, err_msg),
                }
            }

            read += res as usize;
            retries = 0;
        }

        Ok(())
    }

    /// Write given `buf` at specified `offset` w/ `pwrite` syscall
    #[inline(always)]
    fn pwrite(&self, buf: &[u8], offset: usize) -> FrozenResult<()> {
        if buf.is_empty() {
            return Ok(());
        }

        let fd = self.fd();

        let mut written = 0usize;
        let mut retries = 0usize;

        while written < buf.len() {
            let chunk_offset = match offset.checked_add(written) {
                Some(off) if off <= off_t::MAX as usize => off as off_t,
                _ => return err::raw_error(err::INV, "offset overflow"),
            };

            let res = unsafe {
                pwrite(
                    fd,
                    buf[written..].as_ptr() as *const c_void,
                    (buf.len() - written) as size_t,
                    chunk_offset,
                )
            };

            // unexpected EOF / zero write
            if res == 0 {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                return err::default_error(err::HCF);
            }

            if hints::unlikely(res < 0) {
                let errno = last_errno();
                let err_msg = err_msg(errno);

                match errno {
                    // io interrupt
                    EINTR | EAGAIN | EBUSY => {
                        if retries < MAX_RETRIES {
                            retries += 1;
                            continue;
                        }

                        return err::raw_error(err::UNK, err_msg);
                    }

                    // permission denied or read-only file
                    EACCES | EPERM | EROFS => {
                        return err::raw_error(err::WRT, err_msg);
                    }

                    // no space available or quota exceeded
                    ENOSPC | libc::EDQUOT => {
                        return err::raw_error(err::NSP, err_msg);
                    }

                    // invalid fd, invalid fd type, bad pointer, etc.
                    EINVAL | EBADF | EFAULT | ESPIPE => {
                        return err::raw_error(err::HCF, err_msg);
                    }

                    _ => return err::raw_error(err::UNK, err_msg),
                }
            }

            written += res as usize;
            retries = 0;
        }

        Ok(())
    }
}

impl POSIXFile {
    /// Create a new or open an existing [`POSIXFile`]
    ///
    /// ## Crash safe durability
    ///
    /// In POSIX systems, `open(O_CREATE)` only creates the directory entry in memory, it may be visible
    /// immediately, but the file entry is not crash durable on many fs
    ///
    /// On some linux systems, journaling fs (ext4, xfs, etc) often replay their journal on mount after a crash is
    /// observed, which usually restores recent directory updates, i.e. our newly created file entry, as a result
    /// newly created file often survive the crash
    ///
    /// In our case, when a new [`FrozenFile`] is created, we zero-extend it using `ftruncate()`, and perform
    /// `fdatasync()` or `fcntl(F_FULLSYNC)`, which in result provides us the crash safe durability we need
    pub(super) fn new(path: &std::path::Path) -> FrozenResult<Self> {
        let fd = open_raw(path, prep_flags())?;
        let file = Self { fd: atomic::AtomicI32::new(fd) };

        // Ensure newly created directory entries are persisted to disk
        if let Err(mut e) = sync_parent_dir(path) {
            if let Err(close_err) = file.close() {
                e.add_suppressed(close_err);
            }
            return Err(e);
        }

        // (linux only) best effort call to provide a hint to the kernel that the file will be
        // accessed in a random pattern
        #[cfg(target_os = "linux")]
        {
            let _res = f_advise_raw(file.fd());

            // INFO: Read the function docs of [`f_advise_raw`] for detailed info on why the error is only
            // propagated in non-prod env's

            #[cfg(debug_assertions)]
            if let Err(mut e) = _res {
                if let Err(close_err) = file.close() {
                    e.add_suppressed(close_err);
                }

                return Err(e);
            }
        }

        Ok(file)
    }

    /// Initiates writeback (best-effort) of dirty pages in the specified range
    ///
    /// ## Purpose
    ///
    /// In our case, `sync_range` is used as a prompt for the kernel to start flushing dirty pages in the
    /// specified range, which result in reduced latency for `fdatasync` and `fcntl(F_FULLSYNC)` syscalls
    ///
    /// This syscall, by itself, does not guarantee any kind of durability, and must always be paired with
    /// strong sync call i.e. `fdatasync()`
    ///
    /// ## Why do we retry?
    ///
    /// POSIX syscalls are interruptible by signals, and may fail w/ `EINTR`, in such cases no progress is
    /// guaranteed, so the syscall must be retried
    #[cfg(target_os = "linux")]
    pub(super) fn sync_range(&self, offset: usize, len: usize) -> FrozenResult<()> {
        sync_file_range_raw(self.fd(), offset, len)
    }
}

impl Drop for POSIXFile {
    fn drop(&mut self) {
        let fd = self.fd.swap(CLOSED_FD, atomic::Ordering::AcqRel);
        if fd != CLOSED_FD {
            let _ = close_raw(fd);
        }
    }
}

/// create/open a new file w/ `open` syscall
///
/// ## Caveats of `O_NOATIME` (`EPERM` err_msg)
///
/// `open()` with `O_NOATIME` may fail with `EPERM` instead of silently ignoring the flag
///
/// `EPERM` indicates a kernel level permission violation, as the kernel rejects the request outright, even
/// though the flag only affects metadata behavior
///
/// To remain sane across ownership models, containers, and shared filesystems, we explicitly retry the `open()`
/// w/o `O_NOATIME` when `EPERM` is encountered
fn open_raw(path: &std::path::Path, flags: c_int) -> FrozenResult<FileId> {
    let cpath = path_to_cstring(path)?;

    // write + read permissions
    let perm = (S_IRUSR | S_IWUSR) as c_uint;

    #[cfg(target_os = "linux")]
    let (mut flags, mut tried_noatime) = (flags, false);

    let mut retries = 0; // only for EINTR errors
    loop {
        let fd = if flags & O_CREAT != 0 {
            unsafe { open(cpath.as_ptr(), flags, perm) }
        } else {
            unsafe { open(cpath.as_ptr(), flags) }
        };

        if hints::unlikely(fd < 0) {
            let errno = last_errno();
            let err_msg = err_msg(errno);

            // NOTE: if the error is EPERM and flags contains O_NOATIME flag, we try to open again
            // w/o the O_NOATIME flag, as some fs does not support this flag

            #[cfg(target_os = "linux")]
            if errno == EPERM && (flags & libc::O_NOATIME) != 0 && !tried_noatime {
                flags &= !libc::O_NOATIME;
                tried_noatime = true;
                continue;
            }

            match errno {
                // NOTE: We must retry on interuption errors (EINTR retry)
                EINTR | EAGAIN | EBUSY => {
                    if retries < MAX_RETRIES {
                        retries += 1;
                        continue;
                    }

                    return err::raw_error(err::UNK, err_msg);
                }

                // no space available on disk
                ENOSPC => return err::raw_error(err::NSP, err_msg),

                // path is a directory or invalid/missing path
                EISDIR | ENOENT | ENOTDIR => {
                    return err::raw_error(err::INV, err_msg);
                }

                // file already exists (O_CREAT | O_EXCL)
                EEXIST => return err::raw_error(err::EXS, err_msg),

                // permission denied or read-only fs
                EACCES | EPERM | EROFS => {
                    return err::raw_error(err::PRM, err_msg);
                }

                _ => return err::raw_error(err::UNK, err_msg),
            }
        }

        return Ok(fd);
    }
}

fn close_raw(fd: FileId) -> FrozenResult<()> {
    let res = unsafe { close(fd) };
    if res == 0 {
        return Ok(());
    }

    let errno = last_errno();
    let err_msg = err_msg(errno);

    // POSIX allows `close(fd)` to return `EINTR` when the fd is already closed
    if errno == EINTR {
        return Ok(());
    }

    // NOTE:
    //
    // In POSIX systems, kernel may report delayed io failures on `close`, these are fatal errors, and can not
    // be retried
    //
    // We protect this by enforcing hard durability right after write ops, so the occurrence of this error is
    // preceived as an implementation failure
    //
    // INFO:
    //
    // Under Linux (`errseq_t`), `fsync` samples and advances the file handle's writeback error cursor (`f_wb_err`)
    //
    // If writeback fails, `fsync` consumes the error and returns it
    //
    // Consequently, a subsequent `close` will not re-surface that same error unless new writeback failures occurred
    // in the interim
    //
    // Ref1 -> https://docs.kernel.org/core-api/errseq.html
    // Ref2 -> https://man7.org/linux/man-pages/man2/close.2.html
    if errno == EIO {
        return err::raw_error(err::HCF, err_msg);
    }

    err::raw_error(err::UNK, err_msg)
}

/// Flush file data to disk using `fdatasync(2)`
///
/// ## Indefinite Retry on `EINTR`
///
/// In POSIX, `fdatasync` interrupted by a signal (`EINTR`) leaves unwritten blocks safely in the page cache;
/// no data or descriptor state is lost
///
/// Unlike reads/writes, flush operations have no partial progress metrics
///
/// Capping `EINTR` retries would cause transient signal storms to report false durability failures (`err::SYN`)
///
/// Hence, `EINTR` is retried unconditionally until completed or a real hardware I/O error (`EIO`) occurs
/// (standard practice in engines like Postgres and SQLite)
#[cfg(target_os = "linux")]
fn fdatasync_raw(fd: FileId) -> FrozenResult<()> {
    let mut retries = 0; // only for transient EAGAIN & EBUSY errors
    loop {
        let res = unsafe { libc::fdatasync(fd) };
        if hints::likely(res == 0) {
            return Ok(());
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            // invalid fd or lack of support for sync
            EINVAL | EBADF => return err::raw_error(err::HCF, err_msg),

            // read-only file (can also be caused by TOCTOU)
            EROFS => return err::raw_error(err::PRM, err_msg),

            // fatal error, i.e. no sync for writes in recent window/batch
            EIO => return err::raw_error(err::SYN, err_msg),

            // INFO:
            //
            // Signal interruption does not indicate media or filesystem failure; dirty pages remain intact
            // in cache
            //
            // Retrying unconditionally prevents false durability failure alerts (`err::SYN`) during external
            // signal activity
            EINTR => continue,

            // Transient device/resource contention
            EAGAIN | EBUSY => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                // NOTE: sync error indicates that retries exhausted and durability is broken in the current/last
                // window/batch
                return err::raw_error(err::SYN, err_msg);
            }

            _ => return err::raw_error(err::UNK, err_msg),
        }
    }
}

#[cfg(target_os = "macos")]
fn f_fullsync_raw(fd: FileId) -> FrozenResult<()> {
    let mut retries = 0; // only for transient EAGAIN & EBUSY errors
    loop {
        let res = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
        if hints::likely(res == 0) {
            return Ok(());
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            // INFO:
            //
            // Signal interruption does not indicate media or filesystem failure; dirty pages remain intact
            // in cache
            //
            // Retrying unconditionally prevents false durability failure alerts (`err::SYN`) during external
            // signal activity
            EINTR => continue,

            // Transient device/resource contention
            EAGAIN | EBUSY => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                // NOTE: sync error indicates that retries exhausted and durability is broken in the current/last
                // window/batch
                return err::raw_error(err::SYN, err_msg);
            }

            // lack of support for `F_FULLFSYNC` (e.g. non-APFS/HFS+ mounts like FAT32, exFAT, SMB, NFS, FUSE)
            libc::ENOTSUP | EOPNOTSUPP | EINVAL => break,

            // invalid fd or bad impl
            EBADF => return err::raw_error(err::HCF, err_msg),

            // read-only file (can also be caused by TOCTOU)
            EROFS => return err::raw_error(err::PRM, err_msg),

            // fatal error, i.e. no sync for writes in recent window/batch
            EIO => return err::raw_error(err::SYN, err_msg),

            _ => return err::raw_error(err::UNK, err_msg),
        }
    }

    // NOTE: when the storage device or fs, does not support fullsync, we fallback to `fsync()`, which does not
    // guaranty durability for sudden crash or power loss, which is acceptable when strong durability is simply
    // not available or allowed
    fsync_raw(fd)
}

/// Syncs in-cache data updates on the storage device
///
/// ## Indefinite Retry on `EINTR`
///
/// In POSIX, `fsync` interrupted by a signal (`EINTR`) leaves unwritten blocks safely in the page cache; no
/// data or descriptor state is lost
///
/// Unlike reads/writes, flush operations have no partial progress metrics; Capping `EINTR` retries would
/// cause transient signal storms to report false durability failures (`err::SYN`)
///
/// Hence, `EINTR` is retried unconditionally until completed or a real hardware I/O error (`EIO`) occurs
/// (standard practice in engines like Postgres and SQLite)
fn fsync_raw(fd: FileId) -> FrozenResult<()> {
    let mut retries = 0; // only for transient EAGAIN & EBUSY errors
    loop {
        let res = unsafe { libc::fsync(fd) };
        if hints::unlikely(res != 0) {
            let errno = last_errno();
            let err_msg = err_msg(errno);

            match errno {
                // INFO:
                //
                // Signal interruption does not indicate media or filesystem failure; dirty pages remain
                // intact in cache
                //
                // Retrying unconditionally prevents false durability failure alerts (`err::SYN`) during
                // external signal activity
                EINTR => continue,

                // Transient device/resource contention
                EAGAIN | EBUSY => {
                    if retries < MAX_RETRIES {
                        retries += 1;
                        continue;
                    }

                    // NOTE: sync error indicates that retries exhausted and durability is broken
                    // in the current/last window/batch
                    return err::raw_error(err::SYN, err_msg);
                }

                // invalid fd or lack of support for sync
                EBADF | EINVAL => return err::raw_error(err::HCF, err_msg),

                // read-only file (can also be caused by TOCTOU)
                EROFS => return err::raw_error(err::PRM, err_msg),

                // fatal error, i.e. no sync for writes in recent window/batch
                EIO => return err::raw_error(err::SYN, err_msg),

                _ => return err::raw_error(err::UNK, err_msg),
            }
        }

        return Ok(());
    }
}

#[cfg(target_os = "linux")]
fn sync_file_range_raw(fd: FileId, offset: usize, len: usize) -> FrozenResult<()> {
    let flag = libc::SYNC_FILE_RANGE_WRITE;
    let mut retries = 0; // only for EINTR errors

    loop {
        let res = unsafe { libc::sync_file_range(fd, offset as off_t, len as off_t, flag) };

        if hints::likely(res == 0) {
            return Ok(());
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            // IO interrupt
            EINTR | EAGAIN | EBUSY => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                // NOTE: sync error indicates that retries exhausted and durability is broken
                // in the current/last window/batch
                return err::raw_error(err::SYN, err_msg);
            }

            // invalid fd or lack of support for sync
            EBADF | EINVAL => return err::raw_error(err::HCF, err_msg),

            // read-only file (can also be caused by TOCTOU)
            EROFS => return err::raw_error(err::PRM, err_msg),

            // fatal error, i.e. no sync for writes in recent window/batch
            EIO => return err::raw_error(err::SYN, err_msg),

            // NOTE: on many fs mainly ones w/o local journaling, and older kernels does not support
            // `sync_file_range(SYNC_FILE_RANGE_WRITE)`, also as the use of this is only to hint the
            // fs (for perf gains for later sync), we simply let go of this and do not elivate any
            // kind of errors
            EOPNOTSUPP | libc::ENOSYS => return Ok(()),

            _ => return err::raw_error(err::UNK, err_msg),
        }
    }
}

/// Disk space preallocation using Linux `fallocate()`.
///
/// ## Allocation Semantics
///
/// Mode `0` (default allocation) allocates physical blocks and extends `st_size` if
/// `curr_len + len_to_add > st_size`
///
/// Any unwritten allocated space within this range is initialized to zero by the filesystem (Unlike
/// `FALLOC_FL_KEEP_SIZE`, mode 0 adjusts file size upon allocation)
///
/// Calling `fallocate()` with `len_to_add == 0` returns `EINVAL` per Linux man pages; this is guarded by an
/// early-return check
///
/// ## Filesystem Support
///
/// Not all filesystems (e.g. NFS, older CIFS, or non-extent-based filesystems) support block allocation
/// (`fallocate`), while `EOPNOTSUPP` and `ENOSYS` are treated as non-fatal because preallocation is a latency and
/// `ENOSPC`
/// avoidance optimization
///
/// standard zero-fill or `ftruncate()` will handle subsequent space growth
///
/// ## Interruption & Retry
///
/// Syscalls may fail with `EINTR`, `EAGAIN`, or `EBUSY` under signal pressure or lock contention, and are
/// retried up to `MAX_RETRIES`
#[cfg(target_os = "linux")]
fn fallocate_raw(fd: FileId, curr_len: usize, len_to_add: usize) -> FrozenResult<()> {
    if len_to_add == 0 {
        return Ok(());
    }

    let offset = match off_t::try_from(curr_len) {
        Ok(off) if off >= 0 => off,
        _ => return err::raw_error(err::GRW, "offset exceeds off_t capacity"),
    };
    let length = match off_t::try_from(len_to_add) {
        Ok(off) if off >= 0 => off,
        _ => {
            return err::raw_error(err::GRW, "len_to_add exceeds off_t capacity");
        }
    };

    let mut retries = 0; // only for EINTR errors
    loop {
        let res = unsafe { libc::fallocate(fd, 0, offset, length) };
        if hints::likely(res == 0) {
            return Ok(());
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            // IO interrupt
            EINTR | EAGAIN | EBUSY => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                return err::raw_error(err::GRW, err_msg);
            }

            // invalid fd
            EBADF | EINVAL => return err::raw_error(err::HCF, err_msg),

            // read-only fs (can also be caused by TOCTOU)
            EROFS => return err::raw_error(err::PRM, err_msg),

            // no space available on disk to grow
            ENOSPC => return err::raw_error(err::NSP, err_msg),

            // NOTE: on many fs `fallocate()` may not be supported due to old kernel or fs limitations, as use
            // of this is only to hint the fs (for perf gains while writes), we simply let go of this and do
            // not elivate any kind of errors
            EOPNOTSUPP | libc::ENOSYS => return Ok(()),

            _ => return err::raw_error(err::UNK, err_msg),
        }
    }
}

/// sets the size of [`POSIXFile`] to `len = curr_len + len_to_add` on fs
///
/// ## Why do we retry?
///
/// POSIX syscalls are interruptible by signals, and may fail w/ `EINTR`, in such cases no progress is guaranteed,
/// so the syscall must be retried
fn ftruncate_raw(fd: FileId, curr_len: usize, len_to_add: usize) -> FrozenResult<()> {
    let new_len = match curr_len.checked_add(len_to_add) {
        Some(len) => match off_t::try_from(len) {
            Ok(off) if off >= 0 => off,
            _ => {
                return err::raw_error(err::GRW, "target file size exceeds off_t capacity");
            }
        },
        None => {
            return err::raw_error(err::GRW, "file length calculation overflowed usize");
        }
    };
    let mut retries = 0; // only for EINTR errors

    loop {
        let res = unsafe { ftruncate(fd, new_len) };
        if hints::likely(res == 0) {
            return Ok(());
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            // IO interrupt
            EINTR | EAGAIN | EBUSY => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                return err::raw_error(err::GRW, err_msg);
            }

            // invalid fd or lack of support for sync
            EINVAL | EBADF => return err::raw_error(err::HCF, err_msg),

            // read-only fs (can also be caused by TOCTOU)
            EROFS => return err::raw_error(err::PRM, err_msg),

            // no space available on disk to grow
            ENOSPC => return err::raw_error(err::NSP, err_msg),

            _ => return err::raw_error(err::UNK, err_msg),
        }
    }
}

/// disk space (best-effort) preallocations using `F_PREALLOCATE`
///
/// ## Semantics
///
/// This syscall does not change the size, nor the file capacity, the use is to attempt to reserve disk blocks
/// in advance to reduce latency during write ops to the [`POSIXFile`]
///
/// ## Support on fs
///
/// On many fs, `fcntl(F_PREALLOCATE)` may not be supported due to older kernels, or fs limitations; in such cases,
/// we simply let go, and do not surface any errors, as this operation is mostly used as a best-effort call, and
/// despite the failure of `fcntl(F_PREALLOCATE)`, the later call to `ftruncate()` would succeed, and the subsequent
/// write ops would work all well, so we are good ;)
///
/// ## Contiguous vs Non-contiguous Allocations
///
/// In `F_PREALLOCATE` calls, we get two allocation modes, contiguous and non-contiguous,
///
/// Calls w/ `F_ALLOCATECONTIG` are more likely to fail on fragmented fs, so we instantly fallback
/// to using `F_ALLOCATEALL` for reliability and correctness
///
/// ## Caveats (more like stupidity) of `F_ALLOCATEALL`
///
/// The preallocations may be revoked by fs due to (intentional) waker semantics, this acts more like a hint
/// and not a command to the fs, so the perf is not always guaranteed
///
/// ## Physical EOF Semantics (`F_PEOFPOSMODE`)
///
/// In XNU, `fst_offset` under `F_PEOFPOSMODE` is a delta relative to physical EOF (`PEOF`), and not an
/// absolute offset from 0
///
/// Passing `0` allocates directly starting from current physical EOF
///
/// ## Why do we retry?
///
/// POSIX syscalls are interruptible by signals, and may fail w/ `EINTR`, in such cases no progress is
/// guaranteed, so the syscall must be retried
#[cfg(target_os = "macos")]
fn f_preallocate_raw(fd: FileId, len_to_add: usize) -> FrozenResult<()> {
    let mut retries = 0; // only for EINTR errors

    // NOTE:
    //
    // Under `F_PEOFPOSMODE`, `fst_offset` is defined by XNU as a delta from physical EOF (PEOF), not logical
    // offset 0
    //
    // Passing offset > 0 allocates at (PEOF + offset), creating an unallocated sparse hole between existing
    // file blocks and new extents
    //
    // Hence fst_offset must always be 0
    //
    // By default we try w/ contiguous allocations for optimal perf; when not available (i.e. ENOSPC), we
    // fallback to non-contiguous allocations
    let length = match off_t::try_from(len_to_add) {
        Ok(len) if len >= 0 => len,
        _ => {
            return err::raw_error(err::GRW, "len_to_add exceeds off_t capacity");
        }
    };

    let mut store = libc::fstore_t {
        fst_flags: libc::F_ALLOCATECONTIG,
        fst_posmode: libc::F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: length,
        fst_bytesalloc: 0,
    };

    loop {
        let res = unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &store) };
        if res == 0 {
            return Ok(());
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            // IO interrupt
            EINTR | EAGAIN | EBUSY => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                return err::raw_error(err::GRW, err_msg);
            }

            // no space available on disk to grow
            ENOSPC => {
                // NOTE: we must retry w/ non-contiguous allocs for correctness, as sometimes
                // we do get `ENOSPC` only for contiguous allocs
                if store.fst_flags == libc::F_ALLOCATECONTIG {
                    store.fst_flags = libc::F_ALLOCATEALL;
                    retries = 0;
                    continue;
                }

                return err::raw_error(err::NSP, err_msg);
            }

            // NOTE: on many fs `fcntl(F_PREALLOCATE)` may not be supported due to old kernel or fs limitations,
            // as use of this is only to hint the fs (for perf gains while writes), we simply let go of this and
            // do not elivate any kind of errors
            EOPNOTSUPP | libc::ENOTSUP => return Ok(()),

            // lack of support or weird fs behavior
            EINVAL => return Ok(()), // same reason as above to not elivate the error

            // invalid fd
            EBADF => return err::raw_error(err::HCF, err_msg),

            // read-only fs
            EROFS => return err::raw_error(err::PRM, err_msg),

            _ => return err::raw_error(err::UNK, err_msg),
        }
    }
}

fn flock_raw(fd: FileId) -> FrozenResult<()> {
    let mut retries = 0; // only for EINTR errors
    loop {
        let res = unsafe { flock(fd, LOCK_EX | LOCK_NB) };
        if res == 0 {
            return Ok(());
        }

        let errno = last_errno();
        let err_msg = err_msg(errno);

        match errno {
            // another process already holds the lock
            _ if errno == EWOULDBLOCK || errno == EAGAIN => {
                return err::raw_error(err::LCK, err_msg);
            }

            // IO interrupt
            EINTR => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                return err::raw_error(err::UNK, err_msg);
            }

            // invalid fd or lack of support
            EBADF | EINVAL => return err::raw_error(err::HCF, err_msg),

            // os or fs out of locks (lock exhaustion, e.g. NFS)
            ENOLCK => return err::raw_error(err::LEX, err_msg),

            _ => return err::raw_error(err::UNK, err_msg),
        }
    }
}

/// perform `fsync` for parent directory of file at given `path`
///
/// ## Purpose
///
/// In POSIX systems, syscalls like `open(path)`, `close(fd)` and `unlink(path)`, does not provide crash safe
/// durability,
/// hence after a sudden crash or power loss, the operation may reverse, resulting in catastrophic consequences
///
/// we must `fsync(parent_dir)`, for crash safe durability
///
/// ## Best-effort on Unsupported Filesystems
///
/// Not all OS kernels (e.g. macOS Darwin) or filesystems (e.g. NFS, CIFS, exFAT, VFAT) support calling `fsync` on
/// directory file descriptors
///
/// On these systems, directory sync is treated as best-effort and unsupported errors are safely ignored
fn sync_parent_dir(path: &std::path::Path) -> FrozenResult<()> {
    let parent = extract_parent_dir(path);
    let flags = O_RDONLY | O_DIRECTORY | O_CLOEXEC;

    let fd = match open_raw(&parent, flags) {
        Ok(fd) => fd,
        Err(e) if e.reason == err::PRM.reason || e.reason == err::INV.reason => {
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    #[cfg(target_os = "linux")]
    let res = fsync_raw(fd);

    #[cfg(target_os = "macos")]
    let res = f_fullsync_raw(fd);

    // INFO:
    //
    // We intentionally ignore `close_raw` errors for the dir descriptor
    //
    // Directory is opened w/ `O_RDONLY` flag, so there are no dirty pages or delayed writeback failures to flush
    // (unlike writable files where close may report deferred `EIO`/`ENOSPC`)
    //
    // The only possible errors are `EBADF` or `EINTR` (where the kernel already deallocates the fd slot on
    // Linux/Darwin), making error propagation here unnecessary and counterproductive
    let _ = close_raw(fd);

    match res {
        Err(e) if e.reason == err::HCF.reason || e.reason == err::UNK.reason => Ok(()),
        other => other,
    }
}

/// preps flags for `open()` syscall
///
/// ## Access Time Updates (O_NOATIME)
///
/// On linux, we can use the `O_NOATIME` flag to disable access time updates on the [`POSIXFile`]
///
/// Normally every I/O operation triggers an `atime` update for every write to disk, w/ use of this flag, we try to
/// eliminate counterproductive measures
///
/// ## Limitations of `O_NOATIME`
///
/// - not all fs support this flag, many silently ignore it, but some throw `EPERM` error
/// - It only works when the UID's are matched for calling process and file owner
#[cfg(target_os = "linux")]
const fn prep_flags() -> c_int {
    O_RDWR | O_CLOEXEC | libc::O_NOATIME | O_CREAT
}

/// preps flags for `open()` syscall
///
/// ## Why no `O_NOATIME` on macOS?
///
/// Unlike Linux, Darwin (macOS/XNU) does not support or even define the `O_NOATIME` flag for `open()` syscall
///
/// In macOS, `atime` updates can only be disabled globally at mount-time (`mount -o noatime`), and Apple's POSIX
/// layer does not provide any per-descriptor flag to bypass access time writes, so we simply omit it
#[cfg(target_os = "macos")]
const fn prep_flags() -> c_int {
    O_RDWR | O_CLOEXEC | O_CREAT
}

/// preps flags for atomic file creation (`O_CREAT | O_EXCL`)
#[cfg(target_os = "linux")]
const fn create_flags() -> c_int {
    O_RDWR | O_CLOEXEC | libc::O_NOATIME | O_CREAT | O_EXCL
}

/// preps flags for atomic file creation (`O_CREAT | O_EXCL`)
#[cfg(target_os = "macos")]
const fn create_flags() -> c_int {
    O_RDWR | O_CLOEXEC | O_CREAT | O_EXCL
}

/// preps flags for opening existing file (without `O_CREAT`)
#[cfg(target_os = "linux")]
const fn open_flags() -> c_int {
    O_RDWR | O_CLOEXEC | libc::O_NOATIME
}

/// preps flags for opening existing file (without `O_CREAT`)
#[cfg(target_os = "macos")]
const fn open_flags() -> c_int {
    O_RDWR | O_CLOEXEC
}

/// convert a `std::path::Path` into `std::ffi::CString`
fn path_to_cstring(path: &std::path::Path) -> FrozenResult<std::ffi::CString> {
    match std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(cs) => Ok(cs),
        Err(e) => err::raw_error(err::INV, e),
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

fn extract_parent_dir(path: &std::path::Path) -> std::path::PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::path::Path::new(".").to_path_buf(),
    }
}

/// Provide access pattern hint using `posix_fadvise(POSIX_FADV_RANDOM)`
///
/// ## Semantics
///
/// This syscall provides a hint to the kernel that the file will be accessed in a random pattern
///
/// The kernel may have disabled read-ahead heuristics for the file
///
/// ## Best-effort behavior
///
/// This call is purely advisory
///
/// If the kernel or filesystem does not support the hint (e.g. `ENOSYS`, `EINVAL`, `ESPIPE`), or if signal interruption
/// retries exhaust, the failure is safely ignored. Only descriptor corruption (`EBADF`) returns `err::HCF`
///
/// ## Why do we retry?
///
/// POSIX syscalls are interruptible by signals, and may fail w/ `EINTR`, in such cases no progress is guaranteed, so
/// the
/// syscall must be retried
///
/// ## Failure Management
///
/// As this call is purely advisory, and expect the resource effectiveness and performance gains, does not create
/// any kind of negative impact on the functionality of [`POSIXFile`], so any errors or failures, should be
/// ignored in prod environment, while propagating the failure in debug (non-prod) environments
#[inline]
#[cfg(target_os = "linux")]
fn f_advise_raw(fd: FileId) -> FrozenResult<()> {
    let mut retries = 0;
    loop {
        let res = unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_RANDOM) };
        if res == 0 {
            return Ok(());
        }

        match res {
            // IO interrupt or lock contention - retry bounded
            EINTR | EAGAIN | EBUSY => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                // Advisory hint failure under signal pressure is non-fatal
                return Ok(());
            }

            // Programmer / descriptor corruption bug (i.e. descriptor is completely invalid)
            EBADF => {
                let err_msg = err_msg(res);
                return err::raw_error(err::HCF, err_msg);
            }

            // Advisory hints are best efforts, i.e ilently ignore lack of kernel/filesystem support (ENOSYS),
            // unsupported vnode/pipe/device (EINVAL, ESPIPE), or any other filesystem-specific refusal
            _ => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmp_file");
        (dir, path)
    }

    mod file_new_close {
        use super::*;

        #[test]
        fn ok_new_close_cycle() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            assert!(path.exists());
            file.close().unwrap();
        }

        #[test]
        fn ok_new_close_cycle_on_existing() {
            let (_dir, path) = tmp_path();
            let file1 = POSIXFile::new(&path).unwrap();
            file1.close().unwrap();

            let file2 = POSIXFile::new(&path).unwrap();
            file2.close().unwrap();
        }

        #[test]
        fn err_new_on_missing_parent_dir() {
            let (_dir, path) = tmp_path();
            let missing = path.join("missing/sub/dir/file");
            let err = POSIXFile::new(&missing).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_new_on_directory() {
            let dir = tempfile::tempdir().unwrap();
            let err = POSIXFile::new(dir.path()).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_new_on_interior_nul() {
            use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

            let bad_path = std::path::Path::new(OsStr::from_bytes(b"bad\0file.db"));
            let err = POSIXFile::new(bad_path).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_new_on_permission_denied() {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let sub_dir = dir.path().join("readonly_dir");
            std::fs::create_dir(&sub_dir).unwrap();
            std::fs::set_permissions(&sub_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

            let target = sub_dir.join("forbidden.db");
            let err = POSIXFile::new(&target).unwrap_err();

            std::fs::set_permissions(&sub_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

            assert_eq!(err.reason, err::PRM.reason);
        }
    }

    mod file_create_open {
        use super::*;

        #[test]
        fn ok_create_close_cycle() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::create(&path).unwrap();
            assert!(path.exists());
            file.close().unwrap();
        }

        #[test]
        fn err_create_when_already_exists() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::create(&path).unwrap();
            let err = POSIXFile::create(&path).unwrap_err();
            assert_eq!(err.reason, err::EXS.reason);
            file.close().unwrap();
        }

        #[test]
        fn err_create_on_missing_parent_dir() {
            let (_dir, path) = tmp_path();
            let missing = path.join("missing/sub/dir/file");
            let err = POSIXFile::create(&missing).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_create_on_directory() {
            let dir = tempfile::tempdir().unwrap();
            let err = POSIXFile::create(dir.path()).unwrap_err();
            assert_eq!(err.reason, err::EXS.reason);
        }

        #[test]
        fn ok_open_existing_file() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::create(&path).unwrap();
            file.close().unwrap();

            let opened = POSIXFile::open(&path).unwrap();
            opened.close().unwrap();
        }

        #[test]
        fn err_open_on_missing_file() {
            let (_dir, path) = tmp_path();
            let err = POSIXFile::open(&path).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_open_on_directory() {
            let dir = tempfile::tempdir().unwrap();
            let err = POSIXFile::open(dir.path()).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }
    }

    mod file_unlink {
        use super::*;

        #[test]
        fn ok_unlink_existing() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            assert!(path.exists());

            file.unlink(&path).unwrap();
            assert!(!path.exists());
        }

        #[test]
        fn err_unlink_missing() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            file.unlink(&path).unwrap();

            let missing_path = path.join("non_existent_file");
            let file2 = POSIXFile {
                fd: atomic::AtomicI32::new(unsafe {
                    libc::open(b"/dev/null\0".as_ptr() as *const _, libc::O_RDONLY)
                }),
            };
            let err = file2.unlink(&missing_path).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn err_unlink_permission_denied() {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let sub_dir = dir.path().join("readonly_dir");
            std::fs::create_dir(&sub_dir).unwrap();

            let target = sub_dir.join("victim.db");
            let file = POSIXFile::new(&target).unwrap();

            std::fs::set_permissions(&sub_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

            let err = file.unlink(&target).unwrap_err();

            std::fs::set_permissions(&sub_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

            assert_eq!(err.reason, err::PRM.reason);
        }
    }

    mod file_lock {
        use super::*;

        #[test]
        fn ok_flock_acquires_exclusive_lock() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            file.flock().unwrap();
            file.close().unwrap();
        }

        #[test]
        fn err_flock_when_already_locked() {
            let (_dir, path) = tmp_path();
            let file1 = POSIXFile::new(&path).unwrap();
            file1.flock().unwrap();

            let file2 = POSIXFile::new(&path).unwrap();
            let err = file2.flock().unwrap_err();
            assert_eq!(err.reason, err::LCK.reason);

            file1.close().unwrap();
            file2.close().unwrap();
        }

        #[test]
        fn ok_flock_released_after_close() {
            let (_dir, path) = tmp_path();
            let file1 = POSIXFile::new(&path).unwrap();
            file1.flock().unwrap();
            file1.close().unwrap();

            let file2 = POSIXFile::new(&path).unwrap();
            file2.flock().unwrap();
            file2.close().unwrap();
        }
    }

    mod file_grow {
        use super::*;

        #[test]
        fn ok_grow() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();

            let initial = file.length().unwrap();
            assert_eq!(initial, 0);

            file.grow(0, 0x1000).unwrap();
            let new_len = file.length().unwrap();
            assert_eq!(new_len, 0x1000);

            file.close().unwrap();
        }

        #[test]
        fn ok_grow_extends_with_zero() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            file.grow(0, 0x500).unwrap();

            let mut buf = vec![0u8; 0x500];
            file.pread(&mut buf, 0).unwrap();

            assert!(buf.iter().all(|b| *b == 0));
            file.close().unwrap();
        }

        #[test]
        fn ok_grow_len_zero() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            file.grow(0, 0).unwrap();
            assert_eq!(file.length().unwrap(), 0);
            file.close().unwrap();
        }

        #[test]
        fn err_grow_overflow_usize() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();

            let err = file.grow(usize::MAX - 10, 20).unwrap_err();
            assert_eq!(err.reason, err::GRW.reason);

            file.close().unwrap();
        }

        #[test]
        fn err_grow_exceeds_off_t_max() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();

            if let Ok(too_large) = usize::try_from(libc::off_t::MAX) {
                if let Some(target) = too_large.checked_add(1) {
                    let err = file.grow(0, target).unwrap_err();
                    assert_eq!(err.reason, err::GRW.reason);
                }
            }

            file.close().unwrap();
        }
    }

    mod fil_sync {
        use super::*;

        #[test]
        fn ok_sync() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            file.sync().unwrap();
            file.close().unwrap();
        }

        #[test]
        fn ok_sync_after_sync() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();

            file.sync().unwrap();
            file.sync().unwrap();
            file.sync().unwrap();
            file.sync().unwrap();

            file.close().unwrap();
        }
    }

    mod write_read_single {
        use super::*;

        #[test]
        fn ok_pwrite_pread_cycle() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            file.grow(0, 0x200).unwrap();

            let data = b"grave_engine";
            file.pwrite(data, 0x80).unwrap();

            let mut buf = vec![0u8; data.len()];
            file.pread(&mut buf, 0x80).unwrap();
            assert_eq!(&buf[..], data);
            file.close().unwrap();
        }

        #[test]
        fn ok_pwrite_pread_across_sessions() {
            let (_dir, path) = tmp_path();

            // session 1
            {
                let file = POSIXFile::new(&path).unwrap();
                file.grow(0, 0x1000).unwrap();

                let data = b"persist_me";
                file.pwrite(data, 0).unwrap();

                file.sync().unwrap();
                file.close().unwrap();
            }

            // session 2
            {
                let file = POSIXFile::new(&path).unwrap();
                let mut buf = vec![0u8; 10];
                file.pread(&mut buf, 0).unwrap();
                assert_eq!(&buf[..], b"persist_me");
                file.close().unwrap();
            }
        }

        #[test]
        fn ok_pwrite_concurrent_non_overlapping() {
            let (_dir, path) = tmp_path();
            let file = std::sync::Arc::new(POSIXFile::new(&path).unwrap());
            file.grow(0, 0x2000).unwrap();

            let mut handles = vec![];
            for i in 0..0x0A {
                let f = file.clone();
                handles.push(std::thread::spawn(move || {
                    let data = vec![i as u8; 0x100];
                    f.pwrite(&data, i * 0x100).unwrap();
                }));
            }

            for h in handles {
                h.join().unwrap();
            }

            file.sync().unwrap();
            for i in 0..0x0A {
                let mut buf = vec![0u8; 0x100];
                file.pread(&mut buf, i * 0x100).unwrap();
                assert!(buf.iter().all(|b| *b == i as u8));
            }
        }

        #[test]
        fn ok_pwrite_when_overlapping_last_wins() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            file.grow(0, 0x100).unwrap();

            let a = [1u8; 0x80];
            let b = [2u8; 0x80];

            file.pwrite(&a, 0).unwrap();
            file.pwrite(&b, 0).unwrap();

            let mut buf = vec![0u8; 0x80];
            file.pread(&mut buf, 0).unwrap();
            assert!(buf.iter().all(|b| *b == 2));
            file.close().unwrap();
        }

        #[test]
        fn ok_pread_zero_len() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            let mut empty = [];
            file.pread(&mut empty, 0).unwrap();
            file.close().unwrap();
        }

        #[test]
        fn ok_pwrite_zero_len() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            let empty = [];
            file.pwrite(&empty, 0).unwrap();
            file.close().unwrap();
        }

        #[test]
        fn err_pread_past_eof() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            let mut buf = [0u8; 16];
            let err = file.pread(&mut buf, 0).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
            file.close().unwrap();
        }

        #[test]
        fn err_pwrite_offset_exceeds_off_t() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            let data = [1u8; 8];
            let err = file.pwrite(&data, usize::MAX).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
            file.close().unwrap();
        }

        #[test]
        fn err_pread_offset_exceeds_off_t() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            let mut buf = [0u8; 8];
            let err = file.pread(&mut buf, usize::MAX).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
            file.close().unwrap();
        }
    }

    mod file_exists {
        use super::*;

        #[test]
        fn ok_exists_true_on_existing_file() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            assert!(POSIXFile::exists(&path).unwrap());
            file.close().unwrap();
        }

        #[test]
        fn ok_exists_false_on_missing_file() {
            let (_dir, path) = tmp_path();
            assert!(!POSIXFile::exists(&path).unwrap());
        }

        #[test]
        fn ok_exists_false_on_enotdir() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            file.close().unwrap();

            let non_dir_child = path.join("child.db");
            let exists = POSIXFile::exists(&non_dir_child).unwrap();
            assert!(!exists);
        }

        #[test]
        fn err_exists_on_permission_denied() {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let inaccessible_dir = dir.path().join("inaccessible");
            std::fs::create_dir(&inaccessible_dir).unwrap();
            std::fs::set_permissions(&inaccessible_dir, std::fs::Permissions::from_mode(0o000))
                .unwrap();

            let target = inaccessible_dir.join("secret.db");
            let res = POSIXFile::exists(&target);

            std::fs::set_permissions(&inaccessible_dir, std::fs::Permissions::from_mode(0o755))
                .unwrap();

            let err = res.unwrap_err();
            assert_eq!(err.reason, err::PRM.reason);
        }

        #[test]
        fn err_exists_on_symlink_loop() {
            let dir = tempfile::tempdir().unwrap();
            let loop_path = dir.path().join("loop_link");
            std::os::unix::fs::symlink(&loop_path, &loop_path).unwrap();

            let err = POSIXFile::exists(&loop_path).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }
    }

    mod file_lifecycle {
        use super::*;

        #[test]
        fn err_length_after_closed() {
            let file = POSIXFile { fd: atomic::AtomicI32::new(CLOSED_FD) };
            let err = file.length().unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
        }

        #[test]
        fn err_pread_after_closed() {
            let file = POSIXFile { fd: atomic::AtomicI32::new(CLOSED_FD) };
            let mut buf = vec![0u8; 8];
            let err = file.pread(&mut buf, 0).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
        }

        #[test]
        fn err_pwrite_after_closed() {
            let file = POSIXFile { fd: atomic::AtomicI32::new(CLOSED_FD) };
            let data = b"dead";
            let err = file.pwrite(data, 0).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
        }

        #[test]
        fn err_sync_after_closed() {
            let file = POSIXFile { fd: atomic::AtomicI32::new(CLOSED_FD) };
            let err = file.sync().unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
        }

        #[test]
        fn err_grow_after_closed() {
            let file = POSIXFile { fd: atomic::AtomicI32::new(CLOSED_FD) };
            let err = file.grow(0, 0x100).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
        }

        #[test]
        fn ok_drop_closes_descriptor() {
            let (_dir, path) = tmp_path();
            let fd = {
                let file = POSIXFile::new(&path).unwrap();
                let fd = file.fd();
                assert_ne!(fd, CLOSED_FD);
                fd
            };

            // NOTE: calling libc::close on the fd after Drop must return EBADF since Drop already closed it
            let res = unsafe { libc::close(fd) };
            assert_eq!(res, -1);
            let errno = last_errno();
            assert_eq!(errno, libc::EBADF);
        }
    }

    mod raw_syscalls {
        use super::*;

        #[test]
        fn ok_sync_cycle() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            file.grow(0, 0x400).unwrap();

            let data = [7u8; 0x80];
            file.pwrite(&data, 0).unwrap();
            file.sync().unwrap();

            let mut buf = vec![0u8; 0x80];
            file.pread(&mut buf, 0).unwrap();
            assert_eq!(buf, data);
            file.close().unwrap();
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn ok_sync_range() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            file.grow(0, 0x1000).unwrap();

            let data = [5u8; 0x100];
            file.pwrite(&data, 0x200).unwrap();

            file.sync_range(0x200, 0x100).unwrap();
            file.sync().unwrap();

            let mut buf = vec![0u8; 0x100];
            file.pread(&mut buf, 0x200).unwrap();
            assert_eq!(buf, data);
            file.close().unwrap();
        }

        #[test]
        fn ok_write_read_at_eof_boundary() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            file.grow(0, 0x200).unwrap();

            let data = [3u8; 0x40];
            file.pwrite(&data, 0x200 - 0x40).unwrap();

            let mut buf = vec![0u8; 0x40];
            file.pread(&mut buf, 0x200 - 0x40).unwrap();
            assert_eq!(buf, data);
            file.close().unwrap();
        }

        #[test]
        fn ok_multiple_open_close_cycles() {
            let (_dir, path) = tmp_path();
            for _ in 0..0x0A {
                let file = POSIXFile::new(&path).unwrap();
                file.sync().unwrap();
                file.close().unwrap();
            }
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn ok_f_advice_random() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            f_advise_raw(file.fd()).unwrap();
            file.close().unwrap();
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn err_f_advise_closed_fd() {
            let err = f_advise_raw(CLOSED_FD).unwrap_err();
            assert_eq!(err.reason, err::HCF.reason);
        }

        #[test]
        fn err_ftruncate_overflow() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            let err = ftruncate_raw(file.fd(), usize::MAX - 10, 20).unwrap_err();
            assert_eq!(err.reason, err::GRW.reason);
            file.close().unwrap();
        }

        #[test]
        fn ok_sync_parent_dir() {
            let (_dir, path) = tmp_path();
            let file = POSIXFile::new(&path).unwrap();
            sync_parent_dir(&path).unwrap();
            file.close().unwrap();
        }
    }

    mod utils {
        use super::*;
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        #[test]
        fn ok_extract_parent_dir() {
            let cases = [
                ("/", "."),
                ("file.db", "."),
                ("./a/b/c.log", "./a/b"),
                ("data/file.db", "data"),
                ("/var/lib/grave/", "/var/lib"),
                ("/tmp/grave/file.db", "/tmp/grave"),
            ];

            for (input, expected) in cases {
                let path = PathBuf::from(input);
                let parent = extract_parent_dir(&path);
                assert_eq!(parent, PathBuf::from(expected), "failed for input: {input}");
            }
        }

        #[test]
        fn ok_path_to_cstring() {
            let cases: &[(&[u8], bool)] = &[
                (b"", true),
                (b"file.db", true),
                (b"bad\0path.db", false),
                (b"relative/path.db", true),
                (b"/tmp/grave/file.db", true),
            ];

            for (bytes, should_ok) in cases {
                let path = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
                let res = path_to_cstring(&path);

                match (res, should_ok) {
                    (Ok(cs), true) => {
                        let expected = CString::new(*bytes)
                            .expect("valid test case must not contain interior NUL");
                        assert_eq!(
                            cs.as_bytes(),
                            expected.as_bytes(),
                            "mismatch for input: {:?}",
                            bytes
                        );
                    }
                    (Err(_), false) => {}
                    (other, _) => {
                        panic!("unexpected result for input {:?}: {:?}", bytes, other);
                    }
                }
            }
        }

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
}
