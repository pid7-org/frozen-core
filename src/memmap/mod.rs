//! NA

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod posix;

/// NA
pub struct MemMap<T> {
    _type: T,
}
