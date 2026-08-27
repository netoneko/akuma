//! Measurement and diagnostic helpers for the syscall layer.
//!
//! Nothing here is on the functional path: these are instruments the syscall
//! implementations call into, kept beside them rather than at the crate root so
//! the dispatcher's neighbours are the things that measure the dispatcher.

pub mod read_profile;
