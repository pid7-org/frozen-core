use super::{err, interface::FileInterface};
use crate::{error::FrozenResult, hints};
use std::sync::atomic;
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_BAD_PATHNAME,
        ERROR_DISK_FULL, ERROR_FILE_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_HANDLE_DISK_FULL,
        ERROR_HANDLE_EOF, ERROR_INVALID_HANDLE, ERROR_INVALID_NAME, ERROR_INVALID_PARAMETER,
        ERROR_IO_DEVICE, ERROR_LOCK_VIOLATION, ERROR_NOT_SUPPORTED, ERROR_PATH_NOT_FOUND,
        ERROR_SHARING_BUFFER_EXCEEDED, ERROR_SHARING_VIOLATION, GENERIC_READ, GENERIC_WRITE,
        HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        CREATE_NEW, CreateFileW, DeleteFileW, FILE_ALLOCATION_INFO, FILE_ATTRIBUTE_NORMAL,
        FILE_END_OF_FILE_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_RANDOM_ACCESS,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FileAllocationInfo, FileEndOfFileInfo, FlushFileBuffers,
        GetFileAttributesW, GetFileSizeEx, INVALID_FILE_ATTRIBUTES, LOCKFILE_EXCLUSIVE_LOCK,
        LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, OPEN_ALWAYS, OPEN_EXISTING, ReadFile,
        SetFileInformationByHandle, WriteFile,
    },
    System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0},
};

/// File handle type for Windows systems
///
/// ## NOTES
///
/// We store the Win32 `HANDLE` (a pointer sized kernel object reference) as `isize` rather than
/// `*mut c_void` mainly for two reasons,
///
/// - `isize` maps directly to `AtomicIsize`, giving us lock-free atomic handle swaps (same reasoning
///   as `AtomicI32` for POSIX `c_int` descriptors)
/// - Raw pointers are `!Send + !Sync`, which would leak into the public `FileId` type alias, creating a
///   pervasive `unsafe impl` burden for callers
///
/// At every Win32 callsite we convert the `handle as usize as HANDLE`;  on x86-64 and AArch64 (our only
/// supported targets), `isize` and `HANDLE` have identical bit representations
pub(super) type FileId = isize;

/// Sentinel representing a closed or never opened handle
///
/// ## NOTES
///
/// Win32 defines `INVALID_HANDLE_VALUE` as `(HANDLE)(LONG_PTR)-1`, which is `-1isize` on all
/// 64-bit Windows targets.  We deliberately avoid `0` (NULL) because some Win32 calls, such as
/// `CreateFileW` on the NUL device, legitimately return `0x0` as a valid handle
pub(in crate::file) const CLOSED_HANDLE: FileId = -1isize;

/// Maximum retries for transient `ERROR_SHARING_VIOLATION` / `ERROR_LOCK_VIOLATION` races
///
/// ## NOTES
///
/// Win32 does not have a signal-interruption model equivalent to POSIX `EINTR`, but file-share
/// and lock-violation errors can appear transiently when another process is in the middle of
/// opening or releasing a file handle.  Twelve retries with no explicit sleep keeps us responsive
/// while tolerating short storms
const MAX_RETRIES: usize = 0x0C;

/// Custom implementation of `std::fs::File` for Windows 64-bit systems
#[derive(Debug)]
pub(super) struct WINFile {
    handle: atomic::AtomicIsize,
}

impl FileInterface for WINFile {
    type Id = FileId;
    const CLOSED_ID: Self::Id = CLOSED_HANDLE;

    /// Read the raw Win32 handle value
    #[inline(always)]
    fn fd(&self) -> FileId {
        self.handle.load(atomic::Ordering::Acquire)
    }

    /// Check if the file at `path` exists using `GetFileAttributesW`
    ///
    /// ## Why `GetFileAttributesW` and not `PathFileExistsW`
    ///
    /// `GetFileAttributesW` is a single, documented kernel32 call with well defined error codes
    ///
    /// It does not require linking `shlwapi.dll` (where `PathFileExistsW` lives) and is available
    /// on all Win32 versions we target
    ///
    /// It is also the canonical check used by virtually every production grade storage engine on Windows
    /// such as RocksDB, LMDB, SQLite, etc.
    ///
    /// `INVALID_FILE_ATTRIBUTES` (0xFFFF_FFFF) is the sentinel for failure; we then inspect `GetLastError` to
    /// distinguish permission denial from genuine absence
    fn exists(path: &std::path::Path) -> FrozenResult<bool> {
        let wide = path_to_wide(path)?;
        let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };

        if attrs != INVALID_FILE_ATTRIBUTES {
            return Ok(true);
        }

        let code = last_error();
        let err_msg = err_msg(code);

        match code {
            // File or one of the path components does not exist
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => Ok(false),

            // Lack of traverse or read permission on a path component
            ERROR_ACCESS_DENIED => err::raw_error(err::PRM, err_msg),

            // Syntactically invalid path (e.g. embedded NUL, reserved name, bad drive letter)
            ERROR_INVALID_NAME | ERROR_BAD_PATHNAME => err::raw_error(err::INV, err_msg),

            _ => err::raw_error(err::UNK, err_msg),
        }
    }

    /// Create a new [`WINFile`] atomically with `CREATE_NEW`
    ///
    /// ## TOCTOU Safety
    ///
    /// `CreateFileW(CREATE_NEW)` maps directly to `NtCreateFile` with `FILE_CREATE` disposition, which is atomic
    /// at the NTFS/ReFS layer
    ///
    /// If the file already exists the kernel returns `ERROR_FILE_EXISTS` or `ERROR_ALREADY_EXISTS` before we ever
    /// hold a handle, eliminating the check-then-act window present in `open()` + `O_EXCL`
    ///
    /// ## `FILE_FLAG_RANDOM_ACCESS`
    ///
    /// We pass this flag at `CreateFileW` time because Win32 has no `posix_fadvise` equivalent for open handles
    ///
    /// The flag instructs the Cache Manager to bypass sequential read ahead heuristics, which is exactly what
    /// [`posix::f_advise_raw(FADV_RANDOM)`] does on Linux
    ///
    /// ## Exclusive Lock
    ///
    /// We open with `FILE_SHARE_READ | FILE_SHARE_WRITE` so that process-level metadata readers (e.g. monitoring
    /// tools) can still open the file
    ///
    /// Exclusive access for our engine is enforced by `LockFileEx` in [`flock`](WINFile::flock), mirroring the
    /// POSIX advisory lock model exactly
    fn create(path: &std::path::Path) -> FrozenResult<Self> {
        let handle = create_file_raw(path, CREATE_NEW)?;
        let file = Self { handle: atomic::AtomicIsize::new(handle) };

        if let Err(e) = sync_parent_dir(path) {
            let _ = file.close();
            return Err(e);
        }

        Ok(file)
    }

    /// Open an existing [`WINFile`] without creating it (`OPEN_EXISTING`)
    ///
    /// Fails with [`err::INV`] if the file does not exist, while using the same share mode and random access flag
    /// as [`create`](WINFile::create) for consistency
    fn open(path: &std::path::Path) -> FrozenResult<Self> {
        let handle = create_file_raw(path, OPEN_EXISTING)?;
        Ok(Self { handle: atomic::AtomicIsize::new(handle) })
    }

    /// Acquire an exclusive, non-blocking advisory lock on [`WINFile`]
    ///
    /// ## Why `LockFileEx` and not `LockFile`
    ///
    /// `LockFileEx` with `LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY` is the Win32 exact semantic
    /// equivalent of `flock(fd, LOCK_EX | LOCK_NB)`,
    ///
    /// - Exclusive, i.e. no other handle (in this or any other process) may hold an overlapping lock
    /// - Non-blocking, i.e. if the lock cannot be acquired immediately, it fails with `ERROR_LOCK_VIOLATION`
    ///   rather than blocking
    ///
    /// ## Lock Range
    ///
    /// We lock the full virtual 64-bit file range `[0, u64::MAX)` by passing `nNumberOfBytesToLockLow = 0xFFFFFFFF`
    /// and `nNumberOfBytesToLockHigh = 0xFFFFFFFF`
    ///
    /// This is identical to how SQLite, LMDB and the Chromium leveldb port take whole-file locks on Windows
    ///
    /// It avoids any partial-lock confusion if `grow()` later extends the file beyond a smaller initial
    /// lock range
    ///
    /// ## Advisory Semantics
    ///
    /// Win32 byte-range locks are advisory for cooperating processes using `LockFileEx`, but they are kernel
    /// enforced against direct `ReadFile`/`WriteFile` calls on overlapping locked ranges from non-cooperating
    /// handles
    ///
    /// This is slightly stricter than POSIX `flock`, but the observable behaviour from our storage engine's
    /// perspective is identical
    ///
    /// ## OVERLAPPED Requirement
    ///
    /// `LockFileEx` always requires a non-NULL `OVERLAPPED` pointer
    ///
    /// We pass a zeroed struct with `hEvent = NULL`; the kernel does not signal anything because we use the
    /// synchronous `LOCKFILE_FAIL_IMMEDIATELY` flag
    fn flock(&self) -> FrozenResult<()> {
        lock_raw(self.fd() as usize as HANDLE)
    }

    /// Close [`WINFile`] and release the Win32 kernel object
    ///
    /// Swaps the stored handle with `CLOSED_HANDLE` atomically so that a concurrent `Drop` racing with an
    /// explicit `close()` call cannot double-close the same handle (which would be a erious bug, i.e. the kernel
    /// recycles handle slots, so a delayed `CloseHandle` on a recycled slot silently closes someone else's
    /// descriptor)
    ///
    /// ## Lock Release
    ///
    /// Win32 byte range locks acquired via `LockFileEx` are automatically released when the last
    /// handle to the file is closed
    ///
    /// We do not need a matching `UnlockFileEx` call here
    ///
    /// ## Deferred I/O Errors
    ///
    /// Unlike POSIX `close(2)`, Win32 `CloseHandle` does not return buffered write errors
    ///
    /// Deferred I/O failures surface on the preceding `WriteFile` call or on `FlushFileBuffers`
    ///
    /// Our durability model enforces `FlushFileBuffers` after every write batch, so by the time `close` is called,
    /// all data is already confirmed durable and `CloseHandle` is a kernel-object teardown with no I/O side-effects
    fn close(self) -> FrozenResult<()> {
        let h = self.handle.swap(CLOSED_HANDLE, atomic::Ordering::AcqRel);
        if h == CLOSED_HANDLE {
            return Ok(());
        }

        close_raw(h as usize as HANDLE)
    }

    /// Delete the file at `path` from the filesystem namespace
    ///
    /// ## Win32 Delete Semantics vs POSIX `unlink`
    ///
    /// Unlike POSIX, Win32 `DeleteFileW` cannot remove a file that is open with the default share
    /// mode (`FILE_SHARE_DELETE` not set)
    ///
    /// We work around this by closing our handle first then deleting
    ///
    /// This creates a narrow TOCTOU window, but in our model only a single [`File`](super::File) instance
    /// ever holds the handle (enforced by `LockFileEx`), so another opener would either fail to open (no handle)
    /// or fail to lock (if it raced ahead of the delete), making practical exploitation of this window impossible
    ///
    /// An alternative is `FILE_FLAG_DELETE_ON_CLOSE` set at open time, but that requires knowing at creation that
    /// the file will be deleted, which does not fit our create-then-conditionally delete lifecycle
    ///
    /// ## Parent Directory Sync
    ///
    /// Unlike ext4/XFS, NTFS filesystems journals the directory operations by default, so a parent
    /// `FlushFileBuffers` after delete is advisory
    ///
    /// We still call `sync_parent_dir` for consistency with the POSIX path and for ReFS/non-default-journal config
    fn unlink(self, path: &std::path::Path) -> FrozenResult<()> {
        // NOTE: close first so DeleteFileW is not blocked by our own open handle
        //
        // On Windows, even with FILE_SHARE_DELETE in the share mode, the underlying NtSetInformationFile
        // call that DeleteFileW wraps still requires FILE_DELETE_CHILD access on the parent directory and
        // that the file itself is not marked non-deletable
        //
        // Closing first avoids the sharing-mode complication entirely
        self.close()?;

        let wide = path_to_wide(path)?;
        let ok = unsafe { DeleteFileW(wide.as_ptr()) };

        if ok != 0 {
            // NOTE: NTFS journals directory metadata, but we still sync the parent for correctness on ReFS and any
            // non-journaling volumes that might be mounted
            return sync_parent_dir(path);
        }

        let code = last_error();
        let err_msg = err_msg(code);

        match code {
            // File or path component does not exist
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => err::raw_error(err::INV, err_msg),

            // Another process holds the file open without FILE_SHARE_DELETE
            ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION => err::raw_error(err::LCK, err_msg),

            // Lack of permission or read-only volume
            ERROR_ACCESS_DENIED => err::raw_error(err::PRM, err_msg),

            // Syntactically invalid path
            ERROR_INVALID_NAME | ERROR_BAD_PATHNAME => err::raw_error(err::INV, err_msg),

            _ => err::raw_error(err::UNK, err_msg),
        }
    }

    /// Query the current logical file length via `GetFileSizeEx`
    ///
    /// `GetFileSizeEx` returns an `i64` (Win32 `LARGE_INTEGER`) rather than a `u64` because the
    /// underlying `NtQueryInformationFile(FileStandardInformation)` field is signed
    ///
    /// A negative result would indicate a severely corrupted NTFS record; we treat it as `err::HCF`
    fn length(&self) -> FrozenResult<usize> {
        get_size_raw(self.fd() as usize as HANDLE)
    }

    /// Grow (zero-extend) [`WINFile`] by `len_to_add` bytes
    ///
    /// ## Operation Order (Allocation Before Logical Extension)
    ///
    /// We call `SetFileInformationByHandle(FileAllocationInfo)` before `SetFileInformationByHandle(FileEndOfFileInfo)`
    /// for the same reason Linux `fallocate` must precede `ftruncate`, cause the pre-reserving disk blocks detects
    /// `ERROR_HANDLE_DISK_FULL` and/or `ERROR_DISK_FULL` before the logical file size is updated, so a failed `grow`
    /// never leaves the file in a state where `st_size` claims space that does not physically exist on disk
    ///
    /// ## `FileAllocationInfo` Semantics
    ///
    /// `AllocationSize` is the physical cluster-granular reservation, not the logical length
    ///
    /// NTFS rounds the value up to the nearest cluster boundary (typically 4 KiB on default-formatted volumes)
    ///
    /// Passing `curr_len + len_to_add` as `AllocationSize` guarantees we reserve at least that many bytes
    ///
    /// ## `FileEndOfFileInfo` Semantics
    ///
    /// Sets the logical EOF pointer, zeroing any newly allocated but previously unwritten bytes between the old
    /// EOF and the new one
    ///
    /// This is the Win32 equivalent of `ftruncate(fd, new_len)`
    ///
    /// ## Best-effort Allocation
    ///
    /// If `FileAllocationInfo` fails with `ERROR_NOT_SUPPORTED` or `ERROR_INVALID_PARAMETER` (which some third-party
    /// filesystem drivers and network shares return), we continue to `FileEndOfFileInfo` anyway
    ///
    /// The worst-case outcome is sparse file allocation, which is acceptable, cause the write ops that follow will
    /// succeed as long as enough free space exists at write time
    #[inline(always)]
    fn grow(&self, curr_len: usize, len_to_add: usize) -> FrozenResult<()> {
        if len_to_add == 0 {
            return Ok(());
        }

        let new_len = match curr_len.checked_add(len_to_add) {
            Some(len) => match i64::try_from(len) {
                Ok(l) if l >= 0 => l,
                _ => {
                    return err::raw_error(err::GRW, "target file size exceeds i64 capacity");
                }
            },
            None => {
                return err::raw_error(err::GRW, "file growth calculation overflowed usize");
            }
        };

        let h = self.fd() as usize as HANDLE;

        // NOTE: best-effort pre-reservation; unsupported errors are non-fatal (see doc above)
        let alloc_res = set_alloc_raw(h, new_len);
        if let Err(ref e) = alloc_res {
            // Only disk-full is fatal at the allocation stage; anything else we continue
            if e.reason == err::NSP.reason {
                return alloc_res;
            }

            // INFO: ERROR_NOT_SUPPORTED / ERROR_INVALID_PARAMETER / unknown, and other errors fall through
            // to EOF extension
        }

        set_eof_raw(h, new_len)
    }

    /// Flush all dirty cache pages and file metadata to stable storage
    ///
    /// ## `FlushFileBuffers` vs `fdatasync` / `F_FULLFSYNC`
    ///
    /// Win32 `FlushFileBuffers` is stronger than POSIX `fdatasync` in one key respect, that is, it also issues a
    /// `FLUSH CACHE` command to SATA/NVMe drives that support it, equivalent to macOS `fcntl(F_FULLFSYNC)`
    ///
    /// On drives with volatile write caches (BBU-less HBAs, consumer SSDs in default mode), this guarantees that
    /// data has left the controller cache and reached persistent media
    ///
    /// There is no weaker `fdatasync` or equivalent on Windows; `FlushFileBuffers` always flushes both data and
    /// metadata (file size, timestamps)
    ///
    /// For our purposes this is strictly correct behaviour, and the extra metadata flush cost is negligible
    /// compared to the I/O itself
    ///
    /// ## Indefinite Retry (Not Applicable on Windows)
    ///
    /// Unlike POSIX, Win32 I/O is not interruptible by signals
    ///
    /// `FlushFileBuffers` either succeeds, fails with a hardware error (`ERROR_IO_DEVICE`), or returns an
    /// invalid-handle error
    ///
    /// No retry loop is needed
    fn sync(&self) -> FrozenResult<()> {
        flush_raw(self.fd() as usize as HANDLE)
    }

    /// Read into `buf` from absolute `offset` without mutating the file pointer
    ///
    /// ## `ReadFile` with `OVERLAPPED` for Positional I/O
    ///
    /// Win32 has no direct `pread` equivalent, but `ReadFile` with a non-NULL `OVERLAPPED` achieves the exact
    /// same semantics, as the `Offset` and `OffsetHigh` fields in the `OVERLAPPED` structure specify the
    /// absolute byte offset, and the kernel does not advance the file pointer
    ///
    /// This is the technique used by SQLite's Windows VFS, Chrome's storage layer, and every other serious
    /// Win32 storage engine
    ///
    /// ## `DWORD` Chunk Cap
    ///
    /// Win32 `ReadFile` takes a `DWORD` (u32) byte count per call
    ///
    /// For buffers larger than `u32::MAX` bytes we loop, issuing multiple `ReadFile` calls and advancing
    /// both the buffer pointer and the OVERLAPPED offset each iteration
    ///
    /// In practice our buffers are page aligned and far below this limit, but we handle it correctly for
    /// robustness
    ///
    /// ## Retry on `ERROR_SHARING_VIOLATION`
    ///
    /// A transient `ERROR_SHARING_VIOLATION` can appear if another process briefly holds an incompatible byte
    /// range lock that overlaps our read range
    ///
    /// We retry up to `MAX_RETRIES` times before failing
    #[inline(always)]
    fn pread(&self, buf: &mut [u8], offset: usize) -> FrozenResult<()> {
        if buf.is_empty() {
            return Ok(());
        }

        let h = self.fd() as usize as HANDLE;

        let mut read = 0usize;
        let mut retries = 0usize;

        while read < buf.len() {
            let cur_offset = match offset.checked_add(read) {
                Some(off) => off,
                None => return err::raw_error(err::INV, "read offset overflow"),
            };

            // OVERLAPPED offset is split into low 32 bits and high 32 bits
            let off_lo = cur_offset as u32;
            let off_hi = (cur_offset >> 0x20) as u32;

            // Cap to DWORD max per ReadFile call
            let to_read = (buf.len() - read).min(u32::MAX as usize) as u32;

            let mut overlapped = overlapped_at(off_lo, off_hi);
            let mut bytes_read: u32 = 0;

            let ok = unsafe {
                ReadFile(h, buf[read..].as_mut_ptr(), to_read, &mut bytes_read, &mut overlapped)
            };

            if ok != 0 {
                if hints::unlikely(bytes_read == 0) {
                    // No bytes read despite success, so we treat it as unexpected EOF (hcf)
                    return err::default_error(err::HCF);
                }

                read += bytes_read as usize;
                retries = 0;
                continue;
            }

            let code = last_error();
            let err_msg = err_msg(code);

            match code {
                // Unexpected EOF, i.e. read beyond current file length, which is an hcf situation
                ERROR_HANDLE_EOF => return err::default_error(err::HCF),

                // Transient sharing/lock contention
                ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION => {
                    if retries < MAX_RETRIES {
                        retries += 1;
                        continue;
                    }

                    return err::raw_error(err::UNK, err_msg);
                }

                // Permission denied on file or byte range
                ERROR_ACCESS_DENIED => return err::raw_error(err::RED, err_msg),

                // Stale, recycled, or already closed handle
                ERROR_INVALID_HANDLE => return err::raw_error(err::HCF, err_msg),

                // Hardware / device I/O failure
                ERROR_IO_DEVICE => return err::raw_error(err::HCF, err_msg),

                _ => return err::raw_error(err::UNK, err_msg),
            }
        }

        Ok(())
    }

    /// Write `buf` at absolute `offset` without mutating the file pointer
    ///
    /// Mirrors [`pread`](WINFile::pread) in structure while using `WriteFile` with a non-NULL `OVERLAPPED`
    /// carrying the target byte offset
    ///
    /// ## Zero Write Retry
    ///
    /// Unlike POSIX `pwrite`, `WriteFile` can succeed (return non-zero) but report zero bytes written in certain
    /// edge cases (pipe write to a full buffer, rare device conditions)
    ///
    /// We retry a bounded number of times before treating this as `err::HCF`
    ///
    /// ## Disk-Full Detection
    ///
    /// `ERROR_HANDLE_DISK_FULL` and `ERROR_DISK_FULL` are both mapped to `err::NSP`
    ///
    /// NTFS reports the former for in-progress writes and the latter for quota exhaustion or volume full at
    /// the start of the allocation
    #[inline(always)]
    fn pwrite(&self, buf: &[u8], offset: usize) -> FrozenResult<()> {
        if buf.is_empty() {
            return Ok(());
        }

        let h = self.fd() as usize as HANDLE;

        let mut written = 0usize;
        let mut retries = 0usize;

        while written < buf.len() {
            let cur_offset = match offset.checked_add(written) {
                Some(off) => off,
                None => return err::raw_error(err::INV, "write offset overflow"),
            };

            let off_lo = cur_offset as u32;
            let off_hi = (cur_offset >> 0x20) as u32;

            let to_write = (buf.len() - written).min(u32::MAX as usize) as u32;

            let mut overlapped = overlapped_at(off_lo, off_hi);
            let mut bytes_written: u32 = 0;

            let ok = unsafe {
                WriteFile(h, buf[written..].as_ptr(), to_write, &mut bytes_written, &mut overlapped)
            };

            if ok != 0 {
                if bytes_written == 0 {
                    // Rare zero write with no error code, retry bounded times before giving up
                    if retries < MAX_RETRIES {
                        retries += 1;
                        continue;
                    }

                    return err::default_error(err::HCF);
                }

                written += bytes_written as usize;
                retries = 0;
                continue;
            }

            let code = last_error();
            let err_msg = err_msg(code);

            match code {
                // Transient sharing/lock contention
                ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION => {
                    if retries < MAX_RETRIES {
                        retries += 1;
                        continue;
                    }

                    return err::raw_error(err::UNK, err_msg);
                }

                // Write permission denied or read-only volume
                ERROR_ACCESS_DENIED => return err::raw_error(err::WRT, err_msg),

                // Storage device full
                ERROR_HANDLE_DISK_FULL | ERROR_DISK_FULL => {
                    return err::raw_error(err::NSP, err_msg);
                }

                // Stale, recycled, or already closed handle
                ERROR_INVALID_HANDLE => return err::raw_error(err::HCF, err_msg),

                // Hardware / device I/O failure
                ERROR_IO_DEVICE => return err::raw_error(err::HCF, err_msg),

                _ => return err::raw_error(err::UNK, err_msg),
            }
        }

        Ok(())
    }
}

impl WINFile {
    /// Create or open a [`WINFile`] with `OPEN_ALWAYS` disposition
    ///
    /// Used when the caller does not care whether the file already exists (idempotent open or create)
    ///
    /// The file is never truncated `OPEN_ALWAYS` is the Win32 equivalent of `O_CREAT` without `O_EXCL`
    ///
    /// ## Crash-Safe Durability
    ///
    /// NTFS journals the new directory entry by default, so crash durability here is stronger than on ext4/XFS
    /// without the `data=journal` mount option
    ///
    /// We still sync the parent directory via `sync_parent_dir` for correctness on ReFS and non-journaling
    /// volumes (e.g. exFAT)
    #[allow(unused)]
    pub(super) fn new(path: &std::path::Path) -> FrozenResult<Self> {
        let handle = create_file_raw(path, OPEN_ALWAYS)?;
        let file = Self { handle: atomic::AtomicIsize::new(handle) };

        if let Err(e) = sync_parent_dir(path) {
            let _ = file.close();
            return Err(e);
        }

        Ok(file)
    }
}

impl Drop for WINFile {
    fn drop(&mut self) {
        let h = self.handle.swap(CLOSED_HANDLE, atomic::Ordering::AcqRel);
        if h != CLOSED_HANDLE {
            // INFO: ignore CloseHandle errors in Drop, cause we cannot propagate them, and by this
            // point all dirty pages must already have been flushed by an explicit sync() call
            let _ = close_raw(h as usize as HANDLE);
        }
    }
}

/// Open or create a file at `path` using the given Win32 `disposition`
///
/// The `disposition` must be one of `CREATE_NEW`, `OPEN_EXISTING`, or `OPEN_ALWAYS`
///
/// ## Share Mode
///
/// We always request `FILE_SHARE_READ | FILE_SHARE_WRITE`
///
/// This allows other processes to open the file (for monitoring, backup, etc.) while we hold the handle
/// Exclusive I/O ownership is enforced at the advisory layer by `LockFileEx`, mirroring the POSIX `flock`
/// model
///
/// ## `FILE_FLAG_RANDOM_ACCESS`
///
/// Disables the Cache Manager's sequential read-ahead prefetcher
///
/// Equivalent to `posix_fadvise(POSIX_FADV_RANDOM)` on Linux
///
/// Our storage engine accesses file pages at arbitrary offsets, so read-ahead is counterproductive and wastes
/// I/O bandwidth and cache
///
/// ## Retry on `ERROR_SHARING_VIOLATION`
///
/// A transient `ERROR_SHARING_VIOLATION` during `CreateFileW` can occur when another process is in the middle of
/// closing its own handle to the same file (there is a brief window between the handle table teardown and the
/// lock release)
///
/// We retry up to `MAX_RETRIES` times
fn create_file_raw(path: &std::path::Path, disposition: u32) -> FrozenResult<FileId> {
    let wide = path_to_wide(path)?;

    let mut retries = 0; // only for ERROR_SHARING_VIOLATION transient races
    loop {
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                core::ptr::null(),
                disposition,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_RANDOM_ACCESS,
                core::ptr::null_mut(),
            )
        };

        if h != INVALID_HANDLE_VALUE {
            // Cast the pointer sized handle to isize for atomic storage
            return Ok(h as isize);
        }

        let code = last_error();
        let err_msg = err_msg(code);

        match code {
            // Transient race, i.e. another process is mid close on this path
            ERROR_SHARING_VIOLATION => {
                if retries < MAX_RETRIES {
                    retries += 1;
                    continue;
                }

                // Lock contention after exhausting retries
                return err::raw_error(err::LCK, err_msg);
            }

            // File or path component does not exist (OPEN_EXISTING on missing file)
            ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => {
                return err::raw_error(err::INV, err_msg);
            }

            // CREATE_NEW on a path where the file already exists
            ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS => {
                return err::raw_error(err::EXS, err_msg);
            }

            // Insufficient permission on file or parent directory
            ERROR_ACCESS_DENIED => return err::raw_error(err::PRM, err_msg),

            // Syntactically invalid path
            ERROR_INVALID_NAME | ERROR_BAD_PATHNAME => {
                return err::raw_error(err::INV, err_msg);
            }

            // Disk full at file creation time
            ERROR_HANDLE_DISK_FULL | ERROR_DISK_FULL => {
                return err::raw_error(err::NSP, err_msg);
            }

            _ => return err::raw_error(err::UNK, err_msg),
        }
    }
}

/// Close a Win32 handle via `CloseHandle`
///
/// Unlike POSIX `close(2)`, `CloseHandle` does not report deferred write errors, those must be caught by
/// `FlushFileBuffers` (our `sync()`)
///
/// The only errors we expect here are `ERROR_INVALID_HANDLE` (double-close or handle corruption), which
/// we surface as `err::HCF`
fn close_raw(handle: HANDLE) -> FrozenResult<()> {
    let ok = unsafe { CloseHandle(handle) };
    if ok != 0 {
        return Ok(());
    }

    let code = last_error();
    let err_msg = err_msg(code);

    // A bad handle on close is an implementation error, we either double closed or corrupted
    // the handle storage
    if code == ERROR_INVALID_HANDLE {
        return err::raw_error(err::HCF, err_msg);
    }

    err::raw_error(err::UNK, err_msg)
}

/// Flush dirty cache pages to stable storage via `FlushFileBuffers`
///
/// `FlushFileBuffers` is equivalent to a combined `fdatasync` + drive-level `FLUSH CACHE` command
///
/// On modern NVMe drives on Windows, this issues an `NVMe Flush` command, making it the strongest available
/// durability primitive on the platform
///
/// ## No Retry Needed
///
/// Win32 I/O is not signal interruptible
///
/// `FlushFileBuffers` either completes or returns a hardware error
///
/// No EINTR style retry is required
fn flush_raw(handle: HANDLE) -> FrozenResult<()> {
    let ok = unsafe { FlushFileBuffers(handle) };
    if ok != 0 {
        return Ok(());
    }

    let code = last_error();
    let err_msg = err_msg(code);

    match code {
        // Bad or closed handle (hcf)
        ERROR_INVALID_HANDLE => err::raw_error(err::HCF, err_msg),

        // Hardware I/O failure
        ERROR_IO_DEVICE => err::raw_error(err::SYN, err_msg),

        // Access denied (e.g. flushing a read-only handle)
        ERROR_ACCESS_DENIED => err::raw_error(err::PRM, err_msg),

        _ => err::raw_error(err::SYN, err_msg),
    }
}

/// Acquire an exclusive, non-blocking byte range lock over the entire 64-bit file range
///
/// `LockFileEx` requires an `OVERLAPPED` pointer even in synchronous mode
///
/// The struct's `hEvent` field is `NULL`, which is valid cause Win32 only signals the event if the
/// lock request is async (i.e. `LOCKFILE_FAIL_IMMEDIATELY` not set)
///
/// Since we always use `LOCKFILE_FAIL_IMMEDIATELY`, the event is never triggered
///
/// Lock range is `[0, u64::MAX)` the maximal Win32 byte-range, which is encoded as two `u32` pairs
/// `(nNumberOfBytesToLockLow, nNumberOfBytesToLockHigh) = (0xFFFFFFFF, 0xFFFFFFFF)`
fn lock_raw(handle: HANDLE) -> FrozenResult<()> {
    // OVERLAPPED with offset 0 (lock starts at byte 0)
    let mut overlapped = overlapped_at(0, 0);

    let ok = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            0xFFFF_FFFF, // nNumberOfBytesToLockLow
            0xFFFF_FFFF, // nNumberOfBytesToLockHigh
            &mut overlapped,
        )
    };

    if ok != 0 {
        return Ok(());
    }

    let code = last_error();
    let err_msg = err_msg(code);

    match code {
        // Another process already holds an incompatible lock (non-blocking fail)
        ERROR_LOCK_VIOLATION | ERROR_SHARING_VIOLATION => err::raw_error(err::LCK, err_msg),

        // System or network lock table exhausted (Win32 equivalent of POSIX ENOLCK)
        ERROR_SHARING_BUFFER_EXCEEDED => err::raw_error(err::LEX, err_msg),

        // Bad or closed handle
        ERROR_INVALID_HANDLE => err::raw_error(err::HCF, err_msg),

        _ => err::raw_error(err::UNK, err_msg),
    }
}

/// Extend the logical file size (EOF pointer) to `new_len` bytes
///
/// NTFS zeroes any bytes between the previous EOF and the new EOF, satisfying our zero-fill
/// guarantee
///
/// This is the Win32 equivalent of `ftruncate(fd, new_len)`
fn set_eof_raw(handle: HANDLE, new_len: i64) -> FrozenResult<()> {
    let info = FILE_END_OF_FILE_INFO { EndOfFile: new_len };

    let ok = unsafe {
        SetFileInformationByHandle(
            handle,
            FileEndOfFileInfo,
            &info as *const _ as *const core::ffi::c_void,
            core::mem::size_of::<FILE_END_OF_FILE_INFO>() as u32,
        )
    };

    if ok != 0 {
        return Ok(());
    }

    let code = last_error();
    let err_msg = err_msg(code);

    match code {
        // Disk full at logical extension time
        ERROR_HANDLE_DISK_FULL | ERROR_DISK_FULL => err::raw_error(err::NSP, err_msg),

        // Bad handle
        ERROR_INVALID_HANDLE => err::raw_error(err::HCF, err_msg),

        // Access denied (read-only handle or volume)
        ERROR_ACCESS_DENIED => err::raw_error(err::PRM, err_msg),

        _ => err::raw_error(err::GRW, err_msg),
    }
}

/// Reserve physical disk clusters ahead of logical file extension
///
/// NTFS will round `AllocationSize` up to the next cluster boundary (default 4 KiB on most volumes)
///
/// This is the Win32 equivalent of `fallocate(fd, 0, offset, length)`
///
/// ## Best-effort
///
/// Some filesystem drivers (network shares, older CIFS/SMB drivers, some third-party antivirus filter drivers)
/// return `ERROR_NOT_SUPPORTED` or `ERROR_INVALID_PARAMETER` for `FileAllocationInfo`
///
/// We treat those as non-fatal and let the caller fall through to `set_eof_raw`
///
/// The risk is that `set_eof_raw` succeeds but the blocks are sparse; future writes may then encounter
/// `ERROR_HANDLE_DISK_FULL` at write time rather than at `grow` time
///
/// This is the same trade-off POSIX makes on filesystems that do not support `fallocate`
fn set_alloc_raw(handle: HANDLE, new_len: i64) -> FrozenResult<()> {
    let info = FILE_ALLOCATION_INFO { AllocationSize: new_len };

    let ok = unsafe {
        SetFileInformationByHandle(
            handle,
            FileAllocationInfo,
            &info as *const _ as *const core::ffi::c_void,
            core::mem::size_of::<FILE_ALLOCATION_INFO>() as u32,
        )
    };

    if ok != 0 {
        return Ok(());
    }

    let code = last_error();
    let err_msg = err_msg(code);

    match code {
        // Driver or fs does not support physical pre-reservation as best-effort, non-fatal
        ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER => Ok(()),

        // Disk physically full, fatal, caller must propagate
        ERROR_HANDLE_DISK_FULL | ERROR_DISK_FULL => err::raw_error(err::NSP, err_msg),

        // Bad handle
        ERROR_INVALID_HANDLE => err::raw_error(err::HCF, err_msg),

        // Access denied
        ERROR_ACCESS_DENIED => err::raw_error(err::PRM, err_msg),

        _ => err::raw_error(err::GRW, err_msg),
    }
}

/// Query the current logical file length via `GetFileSizeEx`
///
/// Returns a `LARGE_INTEGER` (i64); negative values indicate severe NTFS corruption
fn get_size_raw(handle: HANDLE) -> FrozenResult<usize> {
    let mut size: i64 = 0;
    let ok = unsafe { GetFileSizeEx(handle, &mut size) };

    if ok == 0 {
        let code = last_error();
        let err_msg = err_msg(code);

        if code == ERROR_INVALID_HANDLE {
            return err::raw_error(err::HCF, err_msg);
        }

        return err::raw_error(err::UNK, err_msg);
    }

    if hints::unlikely(size < 0) {
        return err::raw_error(err::HCF, "filesystem reported negative file size");
    }

    match usize::try_from(size) {
        Ok(sz) => Ok(sz),
        Err(_) => err::raw_error(err::HCF, "file size exceeds usize address space"),
    }
}

/// Flush the parent directory of `path` to durable storage
///
/// ## Why Sync the Parent Directory on Windows?
///
/// NTFS with default settings journals directory updates, so a `DeleteFileW` or `CreateFileW` followed by
/// a crash will typically be replayed correctly on the next mount
///
/// However,
///
/// - `ReFS` does not have a traditional journal; it uses copy-on-write tree updates, and un-flushed metadata
///   changes may be lost on a sudden power failure
/// - Network-redirected volumes (SMB, iSCSI) often have their own caching layers that do not participate in NTFS's
///   local journal
/// - Developer machines running exFAT or FAT32 (common on SD cards or cross-platform shares) have no jouranaling
///   at all
///
/// We open the parent directory with `FILE_FLAG_BACKUP_SEMANTICS` (which is required to open directories) and
/// call `FlushFileBuffers`
///
/// On NTFS this is a no-op most of the time (journal replay handles it), but on the filesystems above it
/// provides an actual durability guarantee
///
/// ## Best-effort
///
/// If the parent directory cannot be opened (permission denied, path resolution failure), we silently continue
/// the worst outcome is that a crash loses the directory entry, which is recoverable by the caller's higher-level
/// recovery logic
fn sync_parent_dir(path: &std::path::Path) -> FrozenResult<()> {
    let parent = extract_parent_dir(path);
    let wide = match path_to_wide(&parent) {
        Ok(w) => w,
        Err(_) => return Ok(()), // best effort (skip if path encoding fails)
    };

    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            core::ptr::null(),
            OPEN_EXISTING,
            // FILE_FLAG_BACKUP_SEMANTICS is required to open a directory handle on Win32
            FILE_FLAG_BACKUP_SEMANTICS,
            core::ptr::null_mut(),
        )
    };

    if h == INVALID_HANDLE_VALUE {
        // Best effort (inability to open the parent directory is non fatal)
        return Ok(());
    }

    // INFO:
    //
    // We intentionally ignore flush errors on the directory handle
    //
    // On NTFS, directory flushes are almost always a no-op because the journal already protects directory
    // metadata
    //
    // On ReFS/exFAT the flush is best-effort due to driver limitations
    //
    // In neither case does a failed directory flush constitute a data loss event for the file itself; only
    // for the directory entry, which is recoverable
    let _ = flush_raw(h);

    // INFO:
    //
    // Ignore close errors on the directory handle
    //
    // The handle was opened read-only (no writes pending), so CloseHandle cannot report deferred I/O errors
    let _ = close_raw(h);

    Ok(())
}

/// Convert a `std::path::Path` to a NUL-terminated UTF-16 wide string
///
/// Win32 Unicode APIs (`CreateFileW`, `DeleteFileW`, etc.) require UTF-16LE strings with a NULL terminator
///
/// Rust paths on Windows are `OsString` backed and can be converted losslessly via `encode_wide()`
///
/// We append a trailing `0u16` for the NUL terminator
///
/// ## Errors
///
/// Returns `err::INV` if `path` contains embedded NUL characters (which would truncate the string passed to Win32
/// and silently open a different file)
fn path_to_wide(path: &std::path::Path) -> FrozenResult<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();

    // Guard against embedded NUL in the path, which would silently truncate the Win32 call
    if wide.contains(&0u16) {
        return err::raw_error(err::INV, "path contains embedded NUL character");
    }

    wide.push(0); // NULL terminator
    Ok(wide)
}

/// Read the Win32 last error code for the current thread
///
/// All Win32 APIs set the thread local error code before returning
///
/// We read it immediately after a failed call to avoid it being overwritten by any intermediate operation
#[inline]
fn last_error() -> u32 {
    unsafe { windows_sys::Win32::Foundation::GetLastError() }
}

/// Format a Win32 error code as a human-readable string
///
/// Delegates to `std::io::Error::from_raw_os_error` which calls `FormatMessageW` internally
///
/// The resulting string is suitable for inclusion in [`FrozenError`](crate::error::FrozenError) messages
#[inline]
fn err_msg(code: u32) -> String {
    std::io::Error::from_raw_os_error(code as i32).to_string()
}

/// Build the parent directory path from `path`
///
/// If `path` has no parent component (e.g. a bare filename), returns `"."` so that `sync_parent_dir` operates
/// on the current working directory
fn extract_parent_dir(path: &std::path::Path) -> std::path::PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => std::path::Path::new(".").to_path_buf(),
    }
}

/// Construct a zeroed `OVERLAPPED` with the I/O offset pre filled
///
/// Win32 decomposes the 64-bit file offset into two 32-bit fields (`Offset` and `OffsetHigh`) stored inside
/// the `Anonymous.Anonymous` union within `OVERLAPPED`
///
/// We zero the struct first (satisfying any reserved-field requirements) and then set the two offset fields
///
/// `hEvent` is left `NULL` because we use synchronous I/O (`ReadFile`/`WriteFile` with a non-NULL `OVERLAPPED` still
/// completes synchronously when the file is opened without `FILE_FLAG_OVERLAPPED`)
#[inline(always)]
fn overlapped_at(offset_lo: u32, offset_hi: u32) -> OVERLAPPED {
    OVERLAPPED {
        Internal: 0,
        InternalHigh: 0,
        Anonymous: OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 { Offset: offset_lo, OffsetHigh: offset_hi },
        },
        hEvent: core::ptr::null_mut(),
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
            let file = WINFile::new(&path).unwrap();
            assert!(path.exists());
            file.close().unwrap();
        }

        #[test]
        fn ok_new_close_cycle_on_existing() {
            let (_dir, path) = tmp_path();
            let file1 = WINFile::new(&path).unwrap();
            file1.close().unwrap();

            let file2 = WINFile::new(&path).unwrap();
            file2.close().unwrap();
        }

        #[test]
        fn err_new_on_missing_parent_dir() {
            let (_dir, path) = tmp_path();
            let missing = path.join("missing\\sub\\dir\\file");
            let err = WINFile::new(&missing).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }
    }

    mod file_create_open {
        use super::*;

        #[test]
        fn ok_create_close_cycle() {
            let (_dir, path) = tmp_path();
            let file = WINFile::create(&path).unwrap();
            assert!(path.exists());
            file.close().unwrap();
        }

        #[test]
        fn err_create_when_already_exists() {
            let (_dir, path) = tmp_path();
            let file = WINFile::create(&path).unwrap();
            let err = WINFile::create(&path).unwrap_err();
            assert_eq!(err.reason, err::EXS.reason);
            file.close().unwrap();
        }

        #[test]
        fn err_create_on_missing_parent_dir() {
            let (_dir, path) = tmp_path();
            let missing = path.join("missing\\sub\\file");
            let err = WINFile::create(&missing).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }

        #[test]
        fn ok_open_existing_file() {
            let (_dir, path) = tmp_path();
            let file = WINFile::create(&path).unwrap();
            file.close().unwrap();

            let opened = WINFile::open(&path).unwrap();
            opened.close().unwrap();
        }

        #[test]
        fn err_open_on_missing_file() {
            let (_dir, path) = tmp_path();
            let err = WINFile::open(&path).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);
        }
    }

    mod file_unlink {
        use super::*;

        #[test]
        fn ok_unlink_existing() {
            let (_dir, path) = tmp_path();
            let file = WINFile::new(&path).unwrap();
            assert!(path.exists());

            file.unlink(&path).unwrap();
            assert!(!path.exists());
        }

        #[test]
        fn err_unlink_missing() {
            let (_dir, path) = tmp_path();

            // Create a dummy file just to get a valid fd, then unlink a non-existent path
            let file = WINFile::create(&path).unwrap();
            let missing_path = path.with_file_name("definitely_does_not_exist.db");

            // close first so unlink's internal close is a no-op (already CLOSED_HANDLE)
            let inner_file = WINFile { handle: atomic::AtomicIsize::new(CLOSED_HANDLE) };
            let err = inner_file.unlink(&missing_path).unwrap_err();
            assert_eq!(err.reason, err::INV.reason);

            file.close().unwrap();
        }
    }

    mod file_lock {
        use super::*;

        #[test]
        fn ok_flock_acquires_exclusive_lock() {
            let (_dir, path) = tmp_path();
            let file = WINFile::new(&path).unwrap();
            file.flock().unwrap();
            file.close().unwrap();
        }

        #[test]
        fn err_flock_when_already_locked() {
            let (_dir, path) = tmp_path();
            let file1 = WINFile::new(&path).unwrap();
            file1.flock().unwrap();

            let file2 = WINFile::new(&path).unwrap();
            let err = file2.flock().unwrap_err();
            assert_eq!(err.reason, err::LCK.reason);

            file1.close().unwrap();
            file2.close().unwrap();
        }

        #[test]
        fn ok_flock_released_after_close() {
            let (_dir, path) = tmp_path();
            let file1 = WINFile::new(&path).unwrap();
            file1.flock().unwrap();
            file1.close().unwrap();

            let file2 = WINFile::new(&path).unwrap();
            file2.flock().unwrap();
            file2.close().unwrap();
        }
    }

    mod file_grow {
        use super::*;

        #[test]
        fn ok_grow() {
            let (_dir, path) = tmp_path();
            let file = WINFile::new(&path).unwrap();

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
            let file = WINFile::new(&path).unwrap();
            file.grow(0, 0x500).unwrap();

            let mut buf = vec![0u8; 0x500];
            file.pread(&mut buf, 0).unwrap();

            assert!(buf.iter().all(|b| *b == 0));
            file.close().unwrap();
        }

        #[test]
        fn ok_grow_zero_noop() {
            let (_dir, path) = tmp_path();
            let file = WINFile::new(&path).unwrap();
            file.grow(0, 0).unwrap();
            assert_eq!(file.length().unwrap(), 0);
            file.close().unwrap();
        }

        #[test]
        fn ok_grow_multiple_times() {
            let (_dir, path) = tmp_path();
            let file = WINFile::new(&path).unwrap();

            file.grow(0, 0x1000).unwrap();
            file.grow(0x1000, 0x2000).unwrap();

            assert_eq!(file.length().unwrap(), 0x3000);
            file.close().unwrap();
        }
    }

    mod file_sync {
        use super::*;

        #[test]
        fn ok_sync() {
            let (_dir, path) = tmp_path();
            let file = WINFile::new(&path).unwrap();
            file.grow(0, 0x1000).unwrap();
            file.sync().unwrap();
            file.close().unwrap();
        }
    }

    mod file_pread_pwrite {
        use super::*;

        #[test]
        fn ok_pwrite_and_pread() {
            let (_dir, path) = tmp_path();
            let file = WINFile::new(&path).unwrap();
            file.grow(0, 0x1000).unwrap();

            let data = [0xABu8; 0x100];
            file.pwrite(&data, 0x200).unwrap();

            let mut buf = [0u8; 0x100];
            file.pread(&mut buf, 0x200).unwrap();

            assert_eq!(buf, data);
            file.close().unwrap();
        }

        #[test]
        fn ok_pwrite_empty_noop() {
            let (_dir, path) = tmp_path();
            let file = WINFile::new(&path).unwrap();
            file.pwrite(&[], 0).unwrap();
            file.close().unwrap();
        }

        #[test]
        fn ok_pread_empty_noop() {
            let (_dir, path) = tmp_path();
            let file = WINFile::new(&path).unwrap();
            file.pread(&mut [], 0).unwrap();
            file.close().unwrap();
        }

        #[test]
        fn ok_pwrite_at_multiple_offsets() {
            let (_dir, path) = tmp_path();
            let file = WINFile::new(&path).unwrap();
            file.grow(0, 0x1000).unwrap();

            file.pwrite(&[0x11u8; 0x100], 0x000).unwrap();
            file.pwrite(&[0x22u8; 0x100], 0x100).unwrap();
            file.pwrite(&[0x33u8; 0x100], 0x200).unwrap();

            let mut a = [0u8; 0x100];
            let mut b = [0u8; 0x100];
            let mut c = [0u8; 0x100];

            file.pread(&mut a, 0x000).unwrap();
            file.pread(&mut b, 0x100).unwrap();
            file.pread(&mut c, 0x200).unwrap();

            assert_eq!(a, [0x11u8; 0x100]);
            assert_eq!(b, [0x22u8; 0x100]);
            assert_eq!(c, [0x33u8; 0x100]);

            file.close().unwrap();
        }
    }

    mod file_exists {
        use super::*;

        #[test]
        fn ok_exists_true_for_existing_file() {
            let (_dir, path) = tmp_path();
            let file = WINFile::create(&path).unwrap();
            file.close().unwrap();

            assert_eq!(WINFile::exists(&path).unwrap(), true);
        }

        #[test]
        fn ok_exists_false_for_missing_file() {
            let (_dir, path) = tmp_path();
            assert_eq!(WINFile::exists(&path).unwrap(), false);
        }
    }

    mod file_fd {
        use super::*;

        #[test]
        fn ok_fd_not_closed_after_open() {
            let (_dir, path) = tmp_path();
            let file = WINFile::new(&path).unwrap();
            assert_ne!(file.fd(), CLOSED_HANDLE);
            assert!(!file.is_closed());
            file.close().unwrap();
        }

        #[test]
        fn ok_is_closed_after_close() {
            let (_dir, path) = tmp_path();
            let file = WINFile::new(&path).unwrap();
            file.close().unwrap();
            // After close(), the file struct is consumed — verify via Drop semantics only
            // (no access to `file` after `.close()`)
        }
    }
}
