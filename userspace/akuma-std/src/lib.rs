//! Akuma Standard Library
//!
//! This crate provides a std-like interface on top of akuma syscalls.
//! It allows crates that depend on `std` to compile for akuma.

#![no_std]
#![feature(alloc_error_handler)]

extern crate alloc;

// Re-export core and alloc types under std names
pub use core::*;
pub use alloc::borrow;
pub use alloc::boxed;
pub use alloc::collections;
pub use alloc::fmt;
pub use alloc::rc;
pub use alloc::string::{self, String, ToString};
pub use alloc::vec::{self, Vec};
pub use alloc::str;

// Our custom modules
pub mod env;
pub mod fs;
pub mod io;
pub mod process;
pub mod time;
pub mod path;
pub mod ffi;
pub mod os;
pub mod sync;
pub mod thread;

// Prelude
pub mod prelude {
    pub mod v1 {
        pub use alloc::borrow::ToOwned;
        pub use alloc::boxed::Box;
        pub use alloc::string::{String, ToString};
        pub use alloc::vec::Vec;
        pub use core::clone::Clone;
        pub use core::cmp::{Eq, Ord, PartialEq, PartialOrd};
        pub use core::convert::{AsRef, AsMut, From, Into};
        pub use core::default::Default;
        pub use core::iter::{Iterator, IntoIterator, Extend, DoubleEndedIterator, ExactSizeIterator};
        pub use core::marker::{Copy, Send, Sized, Sync, Unpin};
        pub use core::mem::drop;
        pub use core::ops::{Drop, Fn, FnMut, FnOnce};
        pub use core::option::Option::{self, Some, None};
        pub use core::result::Result::{self, Ok, Err};

        pub use crate::io::Write;
        pub use crate::io::Read;
    }
    pub use v1::*;
}
