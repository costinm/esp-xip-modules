//! NAN service discovery and NOW synchronization, with a module-owned API.
#![no_std]

#[cfg(feature = "runtime")]
extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod api;

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::*;
