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

/// Utility for error propagation used across [`frozen_core`](crate)
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

    /// Secondary errors that occurred during best-effort cleanup or rollback
    pub suppressed: Option<Box<Vec<FrozenError>>>,
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
            suppressed: None,
        }
    }

    /// Construct a new [`FrozenError`] from raw error object implementing [`std::fmt::Display`]
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
        Self {
            domain,
            module,
            reason: code.reason,
            context: format!("[{}] {}", code.detail, err).into_boxed_str(),
            suppressed: None,
        }
    }

    /// Attach a secondary error that occurred during best-effort cleanup or rollback
    pub fn add_suppressed(&mut self, err: FrozenError) {
        let list = self.suppressed.get_or_insert_with(|| Box::new(Vec::new()));
        list.push(err);
    }

    /// Builder-style helper to attach a suppressed error
    #[must_use]
    pub fn with_suppressed(mut self, err: FrozenError) -> Self {
        self.add_suppressed(err);
        self
    }

    /// Returns a slice of suppressed errors attached to this error
    #[inline]
    pub fn suppressed(&self) -> &[FrozenError] {
        match &self.suppressed {
            Some(list) => list.as_slice(),
            None => &[],
        }
    }

    /// Sentinel placeholder for an unbound or default module identifier
    pub const UNBOUND_MODULE: u8 = 0x00;

    /// Construct a [`FrozenError`] where `module_id` is not yet known
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::error::{FrozenError, ErrCode};
    ///
    /// let err = FrozenError::unbound(0x08, ErrCode::new(0x10, "inv"), "invalid path");
    /// assert_eq!(err.module, FrozenError::UNBOUND_MODULE);
    /// assert_eq!(err.domain, 0x08);
    /// assert_eq!(err.reason, 0x10);
    /// ```
    #[inline(always)]
    pub fn unbound(domain: u8, code: ErrCode, errmsg: &str) -> Self {
        Self::new(Self::UNBOUND_MODULE, domain, code, errmsg)
    }

    /// Construct a [`FrozenError`] from a raw displayable error where `module_id` is not yet known
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::error::{FrozenError, ErrCode};
    ///
    /// let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
    /// let err = FrozenError::unbound_raw(0x08, ErrCode::new(0x10, "inv"), io_err);
    /// assert_eq!(err.module, FrozenError::UNBOUND_MODULE);
    /// assert_eq!(err.domain, 0x08);
    /// assert_eq!(err.reason, 0x10);
    /// ```
    #[inline(always)]
    pub fn unbound_raw<E: std::fmt::Display>(domain: u8, code: ErrCode, err: E) -> Self {
        Self::new_raw(Self::UNBOUND_MODULE, domain, code, err)
    }

    /// Recursively stamps or overrides the 8-bit `module_id` onto this error
    /// and all attached suppressed errors
    ///
    /// ## Example
    ///
    /// ```
    /// use frozen_core::error::{FrozenError, ErrCode};
    ///
    /// let err = FrozenError::unbound(0x08, ErrCode::new(0x10, "inv"), "path error")
    ///     .with_module(0x05);
    /// assert_eq!(err.module, 0x05);
    /// assert_eq!(err.domain, 0x08);
    /// assert_eq!(err.reason, 0x10);
    /// ```
    #[must_use]
    pub fn with_module(mut self, module: u8) -> Self {
        self.module = module;
        if let Some(suppressed) = &mut self.suppressed {
            for sub in suppressed.iter_mut() {
                *sub = sub.clone().with_module(module);
            }
        }
        self
    }
}

impl FrozenError {
    fn fmt_tree(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        prefix: &str,
        is_tail: bool,
        is_root: bool,
    ) -> std::fmt::Result {
        if is_root {
            writeln!(
                f,
                "[{:#04x}:{:#04x}:{:#04x}] {}",
                self.module, self.domain, self.reason, self.context
            )?;
        } else {
            let branch = if is_tail { "└── " } else { "├── " };
            writeln!(
                f,
                "{}{}[{:#04x}:{:#04x}:{:#04x}] {}",
                prefix, branch, self.module, self.domain, self.reason, self.context
            )?;
        }

        if let Some(suppressed) = &self.suppressed {
            let next_prefix = if is_root {
                String::new()
            } else if is_tail {
                format!("{prefix}    ")
            } else {
                format!("{prefix}│   ")
            };

            let count = suppressed.len();
            for (idx, sub_err) in suppressed.iter().enumerate() {
                let is_last = idx + 1 == count;
                sub_err.fmt_tree(f, &next_prefix, is_last, false)?;
            }
        }

        Ok(())
    }
}

impl std::fmt::Debug for FrozenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.suppressed {
            None => write!(
                f,
                "FrozenError {{ module: {:#04x}, domain: {:#04x}, reason: {:#04x}, context: {:?} }}",
                self.module, self.domain, self.reason, self.context
            ),
            Some(suppressed) => write!(
                f,
                "FrozenError {{ module: {:#04x}, domain: {:#04x}, reason: {:#04x}, context: {:?}, suppressed: {:?} }}",
                self.module, self.domain, self.reason, self.context, suppressed
            ),
        }
    }
}

impl std::fmt::Display for FrozenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.suppressed.is_none() {
            write!(
                f,
                "[{:#04x}:{:#04x}:{:#04x}] {}",
                self.module, self.domain, self.reason, self.context
            )
        } else {
            // Write tree format, stripping the trailing newline so Display behaves well in format!("{err}")
            let mut formatted = String::new();
            // Using a helper struct or formatting directly to a String buffer
            struct TreeHelper<'a>(&'a FrozenError);
            impl std::fmt::Display for TreeHelper<'_> {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    self.0.fmt_tree(f, "", true, true)
                }
            }
            use std::fmt::Write as _;
            let _ = write!(formatted, "{}", TreeHelper(self));
            if formatted.ends_with('\n') {
                formatted.pop();
            }
            write!(f, "{formatted}")
        }
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
    /// 8-bit reason code encoded into [`FrozenError`]
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

/// Extension trait for [`FrozenResult`] to bind a `module_id` seamlessly
///
/// ## Example
///
/// ```
/// use frozen_core::error::{BindModule, FrozenError, FrozenResult, ErrCode};
///
/// fn backend_call() -> FrozenResult<()> {
///     Err(FrozenError::unbound(0x08, ErrCode::new(0x10, "io"), "failed"))
/// }
///
/// let res = backend_call().with_module(0x02);
/// assert_eq!(res.unwrap_err().module, 0x02);
/// ```
pub trait BindModule<T> {
    /// Attaches `module_id` to the inner [`FrozenError`] if `self` is `Err`
    fn with_module(self, module_id: u8) -> FrozenResult<T>;
}

impl<T> BindModule<T> for FrozenResult<T> {
    #[inline(always)]
    fn with_module(self, module_id: u8) -> FrozenResult<T> {
        self.map_err(|e| e.with_module(module_id))
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
        // NOTE:
        //
        // `FrozenError` must satisfy `std::error::Error` so it can be used with `?` into `Box<dyn Error>`
        // and ecosystem tooling
        fn assert_is_error<E: std::error::Error>(_: &E) {}
        let err = FrozenError::new(1, 2, ErrCode::new(3, "io"), "fail");
        assert_is_error(&err);
    }

    #[test]
    fn ok_suppressed_tree_display() {
        let mut root = FrozenError::new(0x01, 0x10, ErrCode::new(0x20, "posix"), "f_advise failed");
        let sub1 = FrozenError::new(0x01, 0x10, ErrCode::new(0x30, "posix"), "close failed");
        let mut sub2 = FrozenError::new(0x01, 0x10, ErrCode::new(0x40, "posix"), "flush failed");
        let sub2_nested = FrozenError::new(0x01, 0x10, ErrCode::new(0x50, "posix"), "sync failed");

        sub2.add_suppressed(sub2_nested);
        root.add_suppressed(sub1);
        root.add_suppressed(sub2);

        assert_eq!(root.suppressed().len(), 2);

        let displayed = format!("{root}");
        let expected = "\
[0x01:0x10:0x20] [posix] f_advise failed
├── [0x01:0x10:0x30] [posix] close failed
└── [0x01:0x10:0x40] [posix] flush failed
    └── [0x01:0x10:0x50] [posix] sync failed";

        assert_eq!(displayed, expected);
    }

    #[test]
    fn ok_with_suppressed_builder() {
        let err = FrozenError::new(1, 2, ErrCode::new(3, "io"), "parent")
            .with_suppressed(FrozenError::new(1, 2, ErrCode::new(4, "io"), "cleanup"));

        assert_eq!(err.suppressed().len(), 1);
        assert_eq!(err.suppressed()[0].reason, 4);
    }

    #[test]
    fn ok_unbound_and_with_module() {
        let mut err = FrozenError::unbound(0x08, ErrCode::new(0x10, "inv"), "unbound err");
        assert_eq!(err.module, FrozenError::UNBOUND_MODULE);
        assert_eq!(err.domain, 0x08);
        assert_eq!(err.reason, 0x10);

        let suppressed = FrozenError::unbound(0x08, ErrCode::new(0x20, "close"), "suppressed err");
        err.add_suppressed(suppressed);

        let bound = err.with_module(0x42);
        assert_eq!(bound.module, 0x42);
        assert_eq!(bound.domain, 0x08);
        assert_eq!(bound.reason, 0x10);
        assert_eq!(bound.suppressed()[0].module, 0x42);
    }

    #[test]
    fn ok_bind_module_trait() {
        let res: FrozenResult<()> =
            Err(FrozenError::unbound(0x08, ErrCode::new(0x10, "inv"), "fail"));
        let bound_res = res.with_module(0x07);
        assert_eq!(bound_res.unwrap_err().module, 0x07);

        let ok_res: FrozenResult<i32> = Ok(10);
        assert_eq!(ok_res.with_module(0x07).unwrap(), 10);
    }
}
