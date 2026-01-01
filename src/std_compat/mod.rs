//! std-compatible types for no_std environment
//!
//! This module provides drop-in replacements for common std types
//! that work in our no_std kernel environment.

#![allow(unused_imports)]
#![allow(dead_code)]

use core::hash::{BuildHasherDefault, Hasher};

/// Simple FNV-1a hasher for no_std HashMap
/// 
/// This is a fast, non-cryptographic hasher suitable for hash tables.
/// We use this instead of ahash because ahash requires CPU crypto extensions.
#[derive(Default)]
pub struct FnvHasher(u64);

impl Hasher for FnvHasher {
    fn write(&mut self, bytes: &[u8]) {
        const FNV_PRIME: u64 = 0x100000001b3;
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        
        if self.0 == 0 {
            self.0 = FNV_OFFSET;
        }
        
        for byte in bytes {
            self.0 ^= *byte as u64;
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// BuildHasher for FnvHasher
pub type FnvBuildHasher = BuildHasherDefault<FnvHasher>;

/// Collections compatible with std::collections
pub mod collections {
    use super::FnvBuildHasher;
    
    /// HashMap with FNV hasher (no ahash/crypto required)
    pub type HashMap<K, V> = hashbrown::HashMap<K, V, FnvBuildHasher>;
    
    /// HashSet with FNV hasher (no ahash/crypto required)
    pub type HashSet<T> = hashbrown::HashSet<T, FnvBuildHasher>;
    
    /// Raw hashbrown types if you need them with a custom hasher
    pub mod raw {
        pub use hashbrown::HashMap as RawHashMap;
        pub use hashbrown::HashSet as RawHashSet;
    }
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

