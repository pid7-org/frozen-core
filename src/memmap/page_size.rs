//! Strongly-typed virtual memory page size definitions and platform validation

use crate::{
    error::{FrozenError, FrozenResult},
    hints,
    memmap::err,
};

/// Strongly-typed virtual memory page size representing hardware MMU translation granules
///
/// Across supported operating systems and CPU architectures, base virtual memory page sizes
/// form a strictly bounded set corresponding to hardware translation granules:
///
/// - **4 KiB** (`4,096` bytes): Standard on x86/x86_64, Windows, and default Linux AArch64
/// - **16 KiB** (`16,384` bytes): Apple Silicon macOS and 16k Linux kernels
/// - **64 KiB** (`65,536` bytes): Enterprise Linux AArch64 (RHEL, CentOS Stream, Amazon Linux)
///
/// ## Example
///
/// ```
/// use frozen_core::memmap::PageSize;
///
/// let ps = PageSize::try_from(4096).unwrap();
/// assert_eq!(ps, PageSize::K4);
/// assert_eq!(ps.as_usize(), 4096);
/// assert_eq!(ps.as_u32(), 4096);
/// assert!(ps.is_power_of_two());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(usize)]
pub enum PageSize {
    /// 4 KiB (4,096 bytes) - Standard x86_64, Windows, and default Linux AArch64
    K4 = 4096,
    /// 16 KiB (16,384 bytes) - Native Apple Silicon macOS and 16k Linux kernels
    K16 = 16384,
    /// 64 KiB (65,536 bytes) - Enterprise Linux AArch64 (RHEL, CentOS, Amazon Linux)
    K64 = 65536,
}

impl PageSize {
    /// Returns the page size in bytes as `usize`
    #[inline]
    pub const fn as_usize(self) -> usize {
        self as usize
    }

    /// Returns the page size in bytes as `u32`
    #[inline]
    pub const fn as_u32(self) -> u32 {
        self as usize as u32
    }

    /// Returns `true` since all valid hardware page sizes are powers of two
    #[inline]
    pub const fn is_power_of_two(self) -> bool {
        true
    }

    /// Validates whether this page size is supported on the active host operating system and
    /// architecture
    ///
    /// ## Platform rules
    ///
    /// - **Windows**: Only `PageSize::K4` (4096 bytes) is valid
    /// - **macOS**: `PageSize::K16` (16384 bytes) on Apple Silicon (`aarch64`); `PageSize::K4` (4096 bytes) on Intel (`x86_64`)
    /// - **Linux**: `PageSize::K4` (4096 bytes) on `x86`/`x86_64`; `PageSize::K4`, `PageSize::K16`, or `PageSize::K64` on `aarch64`
    pub fn validate_platform(self) -> FrozenResult<Self> {
        Self::validate_target_platform(self, std::env::consts::OS, std::env::consts::ARCH)
    }

    pub(crate) fn validate_target_platform(self, os: &str, arch: &str) -> FrozenResult<Self> {
        let valid = match (os, arch) {
            ("windows", _) => self == Self::K4,
            ("macos", "aarch64") => self == Self::K16,
            ("macos", "x86_64") => self == Self::K4,
            ("linux", "x86" | "x86_64") => self == Self::K4,
            ("linux", "aarch64") => matches!(self, Self::K4 | Self::K16 | Self::K64),
            _ => false,
        };

        if hints::likely(valid) {
            Ok(self)
        } else {
            err::raw_error(
                err::INV,
                format!(
                    "unsupported page size {} for target platform {os}/{arch}",
                    self.as_usize()
                ),
            )
        }
    }
}

impl From<PageSize> for usize {
    #[inline]
    fn from(ps: PageSize) -> Self {
        ps as usize
    }
}

impl From<PageSize> for u32 {
    #[inline]
    fn from(ps: PageSize) -> Self {
        ps as usize as u32
    }
}

impl From<PageSize> for u64 {
    #[inline]
    fn from(ps: PageSize) -> Self {
        ps as usize as u64
    }
}

impl core::fmt::Display for PageSize {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.as_usize())
    }
}

impl TryFrom<usize> for PageSize {
    type Error = FrozenError;

    fn try_from(size: usize) -> Result<Self, Self::Error> {
        match size {
            4096 => Ok(Self::K4),
            16384 => Ok(Self::K16),
            65536 => Ok(Self::K64),
            other => err::raw_error(err::INV, format!("unsupported page size: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_page_size_conversions() {
        assert_eq!(PageSize::K4.as_usize(), 4096);
        assert_eq!(PageSize::K16.as_usize(), 16384);
        assert_eq!(PageSize::K64.as_usize(), 65536);

        assert_eq!(PageSize::K4.as_u32(), 4096);
        assert_eq!(PageSize::K16.as_u32(), 16384);
        assert_eq!(PageSize::K64.as_u32(), 65536);

        assert_eq!(usize::from(PageSize::K4), 4096);
        assert_eq!(u32::from(PageSize::K16), 16384);
        assert_eq!(u64::from(PageSize::K64), 65536);

        assert_eq!(PageSize::try_from(4096).unwrap(), PageSize::K4);
        assert_eq!(PageSize::try_from(16384).unwrap(), PageSize::K16);
        assert_eq!(PageSize::try_from(65536).unwrap(), PageSize::K64);

        assert_eq!(format!("{}", PageSize::K4), "4096");
        assert_eq!(format!("{}", PageSize::K16), "16384");
        assert_eq!(format!("{}", PageSize::K64), "65536");

        assert!(PageSize::K4.is_power_of_two());
        assert!(PageSize::K16.is_power_of_two());
        assert!(PageSize::K64.is_power_of_two());
    }

    #[test]
    fn err_page_size_invalid_values() {
        assert!(PageSize::try_from(0).is_err());
        assert!(PageSize::try_from(1024).is_err());
        assert!(PageSize::try_from(2048).is_err());
        assert!(PageSize::try_from(5000).is_err());
        assert!(PageSize::try_from(8192).is_err());
        assert!(PageSize::try_from(32768).is_err());
        assert!(PageSize::try_from(131072).is_err());
    }

    #[test]
    fn ok_platform_validation_matrix() {
        // Windows allows only K4
        assert!(PageSize::K4.validate_target_platform("windows", "x86_64").is_ok());
        assert!(PageSize::K4.validate_target_platform("windows", "aarch64").is_ok());
        assert!(PageSize::K16.validate_target_platform("windows", "x86_64").is_err());
        assert!(PageSize::K64.validate_target_platform("windows", "x86_64").is_err());

        // macOS Apple Silicon (aarch64) allows only K16
        assert!(PageSize::K16.validate_target_platform("macos", "aarch64").is_ok());
        assert!(PageSize::K4.validate_target_platform("macos", "aarch64").is_err());
        assert!(PageSize::K64.validate_target_platform("macos", "aarch64").is_err());

        // macOS Intel (x86_64) allows only K4
        assert!(PageSize::K4.validate_target_platform("macos", "x86_64").is_ok());
        assert!(PageSize::K16.validate_target_platform("macos", "x86_64").is_err());
        assert!(PageSize::K64.validate_target_platform("macos", "x86_64").is_err());

        // Linux x86 / x86_64 allows only K4
        assert!(PageSize::K4.validate_target_platform("linux", "x86_64").is_ok());
        assert!(PageSize::K4.validate_target_platform("linux", "x86").is_ok());
        assert!(PageSize::K16.validate_target_platform("linux", "x86_64").is_err());
        assert!(PageSize::K64.validate_target_platform("linux", "x86_64").is_err());

        // Linux aarch64 allows K4, K16, K64
        assert!(PageSize::K4.validate_target_platform("linux", "aarch64").is_ok());
        assert!(PageSize::K16.validate_target_platform("linux", "aarch64").is_ok());
        assert!(PageSize::K64.validate_target_platform("linux", "aarch64").is_ok());

        // Unsupported platform
        assert!(PageSize::K4.validate_target_platform("freebsd", "x86_64").is_err());
        assert!(PageSize::K4.validate_target_platform("unknown", "unknown").is_err());
    }

    #[test]
    fn ok_current_platform_validation() {
        #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
        assert!(PageSize::K4.validate_platform().is_ok());

        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        {
            assert!(PageSize::K4.validate_platform().is_ok());
            assert!(PageSize::K16.validate_platform().is_ok());
            assert!(PageSize::K64.validate_platform().is_ok());
        }

        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            assert!(PageSize::K16.validate_platform().is_ok());
            assert!(PageSize::K4.validate_platform().is_err());
        }

        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        {
            assert!(PageSize::K4.validate_platform().is_ok());
            assert!(PageSize::K16.validate_platform().is_err());
        }

        #[cfg(target_os = "windows")]
        {
            assert!(PageSize::K4.validate_platform().is_ok());
            assert!(PageSize::K16.validate_platform().is_err());
        }
    }
}
