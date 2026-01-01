//! std-compatible types for no_std environment
//!
//! This module provides drop-in replacements for common std types
//! that work in our no_std kernel environment.

#![allow(unused_imports)]
#![allow(dead_code)]

/// Collections compatible with std::collections
/// 
/// Uses ahash (hardware-accelerated) with QEMU `-cpu max`.
pub mod collections {
    pub use hashbrown::HashMap;
    pub use hashbrown::HashSet;
}

/// Synchronization primitives compatible with std::sync
#[allow(dead_code)]
pub mod sync {
    pub use spinning_top::Spinlock as Mutex;
    pub use spinning_top::RwSpinlock as RwLock;
    pub use once_cell::race::OnceBox as OnceLock;

    /// MutexGuard type alias for compatibility with std::sync::MutexGuard
    pub type MutexGuard<'a, T> = spinning_top::guard::SpinlockGuard<'a, T>;

    /// RwLockReadGuard type alias for compatibility
    pub type RwLockReadGuard<'a, T> = spinning_top::guard::RwSpinlockReadGuard<'a, T>;

    /// RwLockWriteGuard type alias for compatibility
    pub type RwLockWriteGuard<'a, T> = spinning_top::guard::RwSpinlockWriteGuard<'a, T>;
}

/// Lazy initialization utilities
pub mod lazy {
    pub use once_cell::race::OnceBox;
}

