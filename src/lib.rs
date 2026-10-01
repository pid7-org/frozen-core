#![deny(missing_docs)]
#![allow(unsafe_op_in_unsafe_fn)]
#![doc = include_str!("../README.md")]

#[cfg(feature = "error")]
pub mod error;

#[cfg(feature = "hints")]
pub mod hints;

#[cfg(feature = "isa")]
pub mod isa;
