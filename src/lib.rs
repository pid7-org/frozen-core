#![deny(missing_docs)]
#![allow(unsafe_op_in_unsafe_fn)]
#![doc = include_str!("../README.md")]

#[cfg(feature = "ack")]
pub mod ack;

#[cfg(feature = "bufpool")]
pub mod bufpool;

#[cfg(feature = "error")]
pub mod error;

#[cfg(feature = "hints")]
pub mod hints;

#[cfg(feature = "crc32")]
pub mod crc32;

#[cfg(feature = "mpscq")]
pub mod mpscq;

#[cfg(feature = "ffile")]
pub mod ffile;

#[cfg(feature = "fmmap")]
pub mod fmmap;

#[cfg(feature = "wpipe")]
pub mod wpipe;

#[cfg(feature = "reservoir")]
pub mod reservoir;

#[cfg(feature = "isa")]
pub mod isa;

pub mod utils;
