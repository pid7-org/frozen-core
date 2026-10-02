//!

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod posix;

///
pub struct File {}
