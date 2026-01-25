//! Synchronization primitives for akuma
//!
//! Note: akuma is currently single-threaded from the perspective of
//! userspace programs, so most of these are simple wrappers.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A mutual exclusion primitive
pub struct Mutex<T: ?Sized> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// Create a new mutex
    pub const fn new(t: T) -> Mutex<T> {
        Mutex {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(t),
        }
    }

    /// Consume and return the inner value
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Acquire the lock
    pub fn lock(&self) -> Result<MutexGuard<'_, T>, PoisonError<MutexGuard<'_, T>>> {
        // Spin until we get the lock (simple implementation)
        while self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
        Ok(MutexGuard { lock: self })
    }

    /// Try to acquire the lock
    pub fn try_lock(&self) -> Result<MutexGuard<'_, T>, TryLockError<MutexGuard<'_, T>>> {
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            Ok(MutexGuard { lock: self })
        } else {
            Err(TryLockError::WouldBlock)
        }
    }

    /// Get mutable reference to underlying data
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }
}

impl<T: ?Sized + Default> Default for Mutex<T> {
    fn default() -> Mutex<T> {
        Mutex::new(Default::default())
    }
}

/// RAII guard for Mutex
pub struct MutexGuard<'a, T: ?Sized + 'a> {
    lock: &'a Mutex<T>,
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// A reader-writer lock
pub struct RwLock<T: ?Sized> {
    // Simplified: just use a mutex
    inner: Mutex<T>,
}

unsafe impl<T: ?Sized + Send + Sync> Send for RwLock<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    pub const fn new(t: T) -> RwLock<T> {
        RwLock { inner: Mutex::new(t) }
    }

    pub fn into_inner(self) -> T {
        self.inner.into_inner()
    }
}

impl<T: ?Sized> RwLock<T> {
    pub fn read(&self) -> Result<RwLockReadGuard<'_, T>, PoisonError<RwLockReadGuard<'_, T>>> {
        let guard = self.inner.lock().map_err(|_| PoisonError::new(RwLockReadGuard { 
            inner: unsafe { &*self.inner.data.get() }
        }))?;
        Ok(RwLockReadGuard { 
            inner: unsafe { &*self.inner.data.get() },
        })
    }

    pub fn write(&self) -> Result<RwLockWriteGuard<'_, T>, PoisonError<RwLockWriteGuard<'_, T>>> {
        self.inner.lock().map_err(|_| PoisonError::new(RwLockWriteGuard {
            lock: self,
        }))?;
        Ok(RwLockWriteGuard { lock: self })
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }
}

pub struct RwLockReadGuard<'a, T: ?Sized + 'a> {
    inner: &'a T,
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.inner
    }
}

pub struct RwLockWriteGuard<'a, T: ?Sized + 'a> {
    lock: &'a RwLock<T>,
}

impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.lock.inner.data.get() }
    }
}

impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.inner.data.get() }
    }
}

impl<T: ?Sized> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.inner.locked.store(false, Ordering::Release);
    }
}

/// Poison error type
#[derive(Debug)]
pub struct PoisonError<T> {
    guard: T,
}

impl<T> PoisonError<T> {
    pub fn new(guard: T) -> PoisonError<T> {
        PoisonError { guard }
    }

    pub fn into_inner(self) -> T {
        self.guard
    }

    pub fn get_ref(&self) -> &T {
        &self.guard
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> core::fmt::Display for PoisonError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "poisoned lock: another task failed inside")
    }
}

/// Try lock error type
#[derive(Debug)]
pub enum TryLockError<T> {
    Poisoned(PoisonError<T>),
    WouldBlock,
}

impl<T> core::fmt::Display for TryLockError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TryLockError::Poisoned(_) => write!(f, "poisoned lock"),
            TryLockError::WouldBlock => write!(f, "would block"),
        }
    }
}

/// Once cell - run initialization exactly once
pub struct Once {
    done: AtomicBool,
}

impl Once {
    pub const fn new() -> Once {
        Once { done: AtomicBool::new(false) }
    }

    pub fn call_once<F: FnOnce()>(&self, f: F) {
        if !self.done.swap(true, Ordering::AcqRel) {
            f();
        }
    }

    pub fn is_completed(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

/// Arc - atomically reference counted pointer
pub use alloc::sync::Arc;

/// Weak reference to Arc
pub use alloc::sync::Weak;

// Re-export atomics
pub mod atomic {
    pub use core::sync::atomic::*;
}

/// A cell which can be written to only once
pub struct OnceLock<T> {
    once: Once,
    value: UnsafeCell<Option<T>>,
}

unsafe impl<T: Send + Sync> Sync for OnceLock<T> {}
unsafe impl<T: Send> Send for OnceLock<T> {}

impl<T> OnceLock<T> {
    pub const fn new() -> OnceLock<T> {
        OnceLock {
            once: Once::new(),
            value: UnsafeCell::new(None),
        }
    }

    pub fn get(&self) -> Option<&T> {
        if self.once.is_completed() {
            unsafe { (*self.value.get()).as_ref() }
        } else {
            None
        }
    }

    pub fn set(&self, value: T) -> Result<(), T> {
        let mut value = Some(value);
        self.once.call_once(|| {
            unsafe { *self.value.get() = value.take(); }
        });
        match value {
            None => Ok(()),
            Some(v) => Err(v),
        }
    }

    pub fn get_or_init<F: FnOnce() -> T>(&self, f: F) -> &T {
        self.once.call_once(|| {
            unsafe { *self.value.get() = Some(f()); }
        });
        unsafe { (*self.value.get()).as_ref().unwrap() }
    }
}

impl<T> Default for OnceLock<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// A lazily-initialized cell
pub struct LazyLock<T, F = fn() -> T> {
    once: Once,
    init: UnsafeCell<Option<F>>,
    value: UnsafeCell<Option<T>>,
}

unsafe impl<T: Send + Sync, F: Send> Sync for LazyLock<T, F> {}
unsafe impl<T: Send, F: Send> Send for LazyLock<T, F> {}

impl<T, F: FnOnce() -> T> LazyLock<T, F> {
    pub const fn new(f: F) -> LazyLock<T, F> {
        LazyLock {
            once: Once::new(),
            init: UnsafeCell::new(Some(f)),
            value: UnsafeCell::new(None),
        }
    }
}

impl<T, F: FnOnce() -> T> Deref for LazyLock<T, F> {
    type Target = T;

    fn deref(&self) -> &T {
        self.once.call_once(|| {
            let f = unsafe { (*self.init.get()).take().unwrap() };
            unsafe { *self.value.get() = Some(f()); }
        });
        unsafe { (*self.value.get()).as_ref().unwrap() }
    }
}
