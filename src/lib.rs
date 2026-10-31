#![allow(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    trivial_casts,
    trivial_numeric_casts,
    unused_extern_crates,
    unused_import_braces,
    unused_results
)]
#![cfg_attr(test, allow(unused_results))]
#![doc = include_str!("../README.md")]

#[cfg(feature = "error")]
pub mod error;

#[cfg(feature = "hints")]
pub mod hints;

#[cfg(feature = "isa")]
pub mod isa;

#[cfg(feature = "memmap")]
pub mod memmap;

#[cfg(feature = "ack")]
pub mod ack;

#[cfg(feature = "file")]
pub mod file;
