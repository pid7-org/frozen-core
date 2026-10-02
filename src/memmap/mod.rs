//!

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod posix;

///
pub struct MemMap<T> {
    _type: T,
}
