//! Threading primitives for akuma
//!
//! Note: akuma userspace programs are currently single-threaded,
//! so these are stubs for API compatibility.

use crate::time::Duration;
use alloc::string::String;

/// Sleep for the specified duration
pub fn sleep(dur: Duration) {
    let ms = dur.as_millis() as u64;
    libakuma::sleep_ms(ms);
}

/// Yield the current thread's time slice
pub fn yield_now() {
    // No-op in single-threaded akuma userspace
    // Could call a yield syscall if available
}

/// Spawn a new thread (stub - not actually supported)
pub fn spawn<F, T>(_f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    panic!("threading not supported in akuma userspace")
}

/// Get the current thread
pub fn current() -> Thread {
    Thread { id: ThreadId(0) }
}

/// Park the current thread
pub fn park() {
    // Sleep for a short time as a substitute
    sleep(Duration::from_millis(1));
}

/// Park with timeout
pub fn park_timeout(dur: Duration) {
    sleep(dur);
}

/// Handle to a thread
#[derive(Clone)]
pub struct Thread {
    id: ThreadId,
}

impl Thread {
    /// Get the thread ID
    pub fn id(&self) -> ThreadId {
        self.id
    }

    /// Get the thread name
    pub fn name(&self) -> Option<&str> {
        Some("main")
    }

    /// Unpark the thread
    pub fn unpark(&self) {
        // No-op
    }
}

/// A unique identifier for a thread
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ThreadId(u64);

impl ThreadId {
    /// Convert to u64
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

/// Handle to a spawned thread
pub struct JoinHandle<T> {
    _marker: core::marker::PhantomData<T>,
}

impl<T> JoinHandle<T> {
    /// Wait for the thread to finish
    pub fn join(self) -> Result<T, Box<dyn core::any::Any + Send + 'static>> {
        panic!("threading not supported")
    }

    /// Get the thread
    pub fn thread(&self) -> &Thread {
        panic!("threading not supported")
    }

    /// Check if finished
    pub fn is_finished(&self) -> bool {
        true
    }
}

/// Builder for spawning threads
pub struct Builder {
    name: Option<String>,
    stack_size: Option<usize>,
}

impl Builder {
    pub fn new() -> Builder {
        Builder {
            name: None,
            stack_size: None,
        }
    }

    pub fn name(mut self, name: String) -> Builder {
        self.name = Some(name);
        self
    }

    pub fn stack_size(mut self, size: usize) -> Builder {
        self.stack_size = Some(size);
        self
    }

    pub fn spawn<F, T>(self, _f: F) -> crate::io::Result<JoinHandle<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Err(crate::io::Error::new(
            crate::io::ErrorKind::Other,
            "threading not supported",
        ))
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

/// Available parallelism (always 1 for akuma)
pub fn available_parallelism() -> crate::io::Result<core::num::NonZero<usize>> {
    Ok(core::num::NonZero::new(1).unwrap())
}

/// Box type alias for compatibility
use alloc::boxed::Box;
