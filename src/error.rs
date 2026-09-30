//! Utilities for error propagation used across `[frozen_core]`
//!
//! This module provides,
//!
//! - [`FrozenError`] which is a structured error w/ 24-bit identifier
//! - [`FrozenResult`] which is a result alias using [`FrozenError`]
//!
//! ## Id
//!
//! Each [`FrozenError`] uses a 24-bit identifier encoded in the following format,
//!
//! `| module:8 | domain:8 | reason:8 |`
//!
//! This id packs important context which aids in the debugging process
//!
//! ## Example
//!
//! ```
//! use frozen_core::error::{FrozenError, FrozenResult, ErrCode};
//!
//! fn read_file() -> FrozenResult<()> {
//!     Err(FrozenError::new(0x10, 0x20, ErrCode::new(0x30, "io"), "read failed"))
//! }
//!
//! let err = read_file().unwrap_err();
//!
//! assert_eq!(err.module, 0x10);
//! assert_eq!(err.domain, 0x20);
//! assert_eq!(err.reason, 0x30);
//!
//! assert!(err.context.contains("[io]"));
//! ```

/// Custom result type w/ [`FrozenError`] as error type
pub type FrozenResult<T> = Result<T, FrozenError>;

/// Utility for error propagation used across [`frozen_core`]
#[derive(Clone)]
pub struct FrozenError {
    /// 8-bit unique identifier to identify the _module_ of [`FrozenError`] object
    pub module: u8,

    /// 8-bit unique identifier to identify the _domain_ of [`FrozenError`] object
    pub domain: u8,

    /// 8-bit unique identifier to identify the _reason_ of [`FrozenError`] object
    pub reason: u8,

    /// Error context for the [`FrozenError`]
    pub context: Box<str>,
}

impl FrozenError {
    /// Construct a new [`FrozenError`]
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::error::{FrozenError, ErrCode};
    ///
    /// let err = FrozenError::new(0x10, 0x20, ErrCode::new(0x30, "io"), "failed to read file");
    ///
    /// assert_eq!(err.module, 0x10);
    /// assert_eq!(err.domain, 0x20);
    /// assert_eq!(err.reason, 0x30);
    ///
    /// assert!(err.context.contains("[io]"));
    /// assert!(err.context.contains("failed to read file"));
    /// ```
    #[inline(always)]
    pub fn new(module: u8, domain: u8, code: ErrCode, errmsg: &str) -> Self {
        Self {
            module,
            domain,
            reason: code.reason,
            context: format!("[{}] {}", code.detail, errmsg).into_boxed_str(),
        }
    }

    /// Construct a new [`FrozenError`] from raw [`Error`] object
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::error::{FrozenError, ErrCode};
    ///
    /// let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
    /// let err = FrozenError::new_raw(0x14, 0x24, ErrCode::new(0x34, "io"), io_err);
    ///
    /// assert_eq!(err.module, 0x14);
    /// assert_eq!(err.domain, 0x24);
    /// assert_eq!(err.reason, 0x34);
    ///
    /// assert!(err.context.contains("[io]"));
    /// assert!(err.context.contains("file missing"));
    /// ```
    #[inline(always)]
    pub fn new_raw<E: std::fmt::Display>(module: u8, domain: u8, code: ErrCode, err: E) -> Self {
        Self { domain, module, reason: code.reason, context: format!("[{}] {}", code.detail, err).into_boxed_str() }
    }
}

impl std::fmt::Debug for FrozenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "FrozenError {{ module: {:#04x}, domain: {:#04x}, reason: {:#04x}, context: {:?} }}",
            self.module, self.domain, self.reason, self.context
        )
    }
}

impl std::fmt::Display for FrozenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{:#04x}:{:#04x}:{:#04x}] {}",
            self.module, self.domain, self.reason, self.context
        )
    }
}

impl std::error::Error for FrozenError {}

impl PartialEq for FrozenError {
    fn eq(&self, other: &Self) -> bool {
        (self.module == other.module)
            && (self.domain == other.domain)
            && (self.reason == other.reason)
    }
}

/// Static error descriptor used to construct [`FrozenError`]
///
/// ## Example
///
/// ```
/// use frozen_core::error::ErrCode;
///
/// const LOCK_ERR: ErrCode = frozen_core::error::ErrCode::new(0xFF, "lock error");
///
/// assert_eq!(LOCK_ERR.reason, 0xFF);
/// assert_eq!(LOCK_ERR.detail, "lock error");
/// ```
#[derive(Debug, Clone)]
pub struct ErrCode {
    /// 8-bit reason code encoded into [`FrozenError::id`]
    pub reason: u8,

    /// Short subsystem label included in the formatted error context
    pub detail: &'static str,
}

impl ErrCode {
    /// Create a new [`ErrCode`]
    ///
    /// *NOTE:* This function is `const`, allowing error codes to be defined as compile-time constants
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::error::ErrCode;
    ///
    /// const LOCK_ERR: ErrCode = frozen_core::error::ErrCode::new(0xFF, "lock error");
    ///
    /// assert_eq!(LOCK_ERR.reason, 0xFF);
    /// assert_eq!(LOCK_ERR.detail, "lock error");
    /// ```
    #[inline]
    pub const fn new(reason: u8, detail: &'static str) -> Self {
        Self { reason, detail }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_context_exact_format() {
        let err = FrozenError::new(1, 2, ErrCode::new(3, "io"), "failure");
        assert_eq!(err.context, "[io] failure".into());
    }

    #[test]
    fn ok_empty_message() {
        let err = FrozenError::new(1, 2, ErrCode::new(3, "io"), "");
        assert_eq!(err.context, "[io] ".into());
    }

    #[test]
    fn ok_empty_detail() {
        let err = FrozenError::new(1, 2, ErrCode::new(3, ""), "failure");
        assert_eq!(err.context, "[] failure".into());
    }

    #[test]
    fn ok_new_and_new_raw_same_id() {
        let e1 = FrozenError::new(1, 2, ErrCode::new(3, "io"), "fail");

        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "fail");
        let e2 = FrozenError::new_raw(1, 2, ErrCode::new(3, "io"), io_err);

        assert_eq!(e1, e2);
    }

    #[test]
    fn ok_context_formatting() {
        let err = FrozenError::new(1, 1, ErrCode::new(1, "io"), "disk failure");
        assert_eq!(err.context, "[io] disk failure".into());
    }

    #[test]
    fn ok_new_raw_uses_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err = FrozenError::new_raw(1, 2, ErrCode::new(3, "io"), io_err);

        assert!(err.context.contains("[io]"));
        assert!(err.context.contains("access denied"));
    }

    #[test]
    fn ok_compare_same_id_different_context() {
        let e1 = FrozenError::new(1, 2, ErrCode::new(3, "io"), "a");
        let e2 = FrozenError::new(1, 2, ErrCode::new(3, "io"), "b");

        assert_eq!(e1, e2);
    }

    #[test]
    fn ok_compare_different_id() {
        let e1 = FrozenError::new(1, 2, ErrCode::new(3, "io"), "a");
        let e2 = FrozenError::new(1, 2, ErrCode::new(4, "io"), "a");

        assert_ne!(e1, e2);
    }

    #[test]
    fn ok_display_format() {
        let err = FrozenError::new(0x01, 0x11, ErrCode::new(0x1C, "lock"), "file locked");
        assert_eq!(format!("{err}"), "[0x01:0x11:0x1c] [lock] file locked");
    }

    #[test]
    fn ok_debug_format_includes_context() {
        let err = FrozenError::new(0x01, 0x11, ErrCode::new(0x1C, "lock"), "file locked");
        let dbg = format!("{err:?}");
        assert!(dbg.contains("0x01"));
        assert!(dbg.contains("0x11"));
        assert!(dbg.contains("0x1c"));
        assert!(dbg.contains("[lock] file locked"));
    }

    #[test]
    fn ok_is_std_error() {
        // FrozenError must satisfy `std::error::Error` so it can be used with `?`
        // into Box<dyn Error> and ecosystem tooling.
        fn assert_is_error<E: std::error::Error>(_: &E) {}
        let err = FrozenError::new(1, 2, ErrCode::new(3, "io"), "fail");
        assert_is_error(&err);
    }
}
