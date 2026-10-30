//! Optional discovery policy and module-owned experimental route records.
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
