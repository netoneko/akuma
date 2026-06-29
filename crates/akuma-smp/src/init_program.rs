//! Per-core init-program slot — the config the initiator (herd via the `core_init`
//! syscall) hands a parked core in the activation handshake (docs/MULTIKERNEL.md §6/§10,
//! acceptance/12).
//!
//! The multikernel never spawns a process across cores (there is deliberately no
//! `SpawnProcess` message — §7): a process runs on a kernel only because *that kernel's
//! own userspace* spawned it. So to make a program run on a secondary, the initiator
//! names it HERE — the path of the program the activated core should run as its first
//! (init) process — and the secondary, once it stands up its scheduler/role, spawns it
//! LOCALLY (its ELF fetched via forwarded `open`/`read`, §8.1).
//!
//! No internal synchronization: the slot is written by the initiator *before* it pushes
//! `MSG_CORE_INIT`, and read by the secondary *after* it pops that message, so the
//! [`crate::Ring`] push/pop (Release/Acquire) is the publish edge. Bytes are [`AtomicU8`]
//! purely so the shared buffer is written/read without forming an `&mut` alias across
//! cores (the same discipline as [`crate::FwdBounce`]); the loads/stores are `Relaxed`
//! because the ring is the ordering edge.

use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

/// Maximum init-program path length (NUL-padded; the active length is in `len`). One
/// page-eighth, ample for any `/bin/...` path.
pub const INIT_PROGRAM_CAP: usize = 256;

/// One core's init-program path in the shared descriptor (written by the initiator,
/// read by the activated secondary).
#[repr(C)]
pub struct InitProgram {
    path: [AtomicU8; INIT_PROGRAM_CAP],
    /// Active path length in bytes (0 ⇒ no init program named — the core just parks/idles).
    len: AtomicU32,
}

impl Default for InitProgram {
    fn default() -> Self {
        Self::new()
    }
}

impl InitProgram {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            path: [const { AtomicU8::new(0) }; INIT_PROGRAM_CAP],
            len: AtomicU32::new(0),
        }
    }

    /// Publish the init-program path (the initiator, before pushing `MSG_CORE_INIT`).
    /// Clamps to [`INIT_PROGRAM_CAP`]; returns the count stored.
    pub fn set(&self, path: &[u8]) -> usize {
        let n = path.len().min(INIT_PROGRAM_CAP);
        for (i, &b) in path.iter().take(n).enumerate() {
            self.path[i].store(b, Ordering::Relaxed);
        }
        self.len.store(n as u32, Ordering::Relaxed);
        n
    }

    /// Read the init-program path into `out` (the secondary, on activation). Returns the
    /// active length (clamped to `out.len()` for the copy, but the true length is the
    /// return value — caller can detect truncation).
    pub fn get(&self, out: &mut [u8]) -> usize {
        let len = self.len.load(Ordering::Relaxed) as usize;
        let n = len.min(out.len()).min(INIT_PROGRAM_CAP);
        for (i, slot) in out.iter_mut().take(n).enumerate() {
            *slot = self.path[i].load(Ordering::Relaxed);
        }
        len
    }

    /// Whether an init program is named for this core.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.len.load(Ordering::Relaxed) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_round_trip() {
        let p = InitProgram::new();
        assert!(!p.is_set());
        let path = b"/bin/hello";
        assert_eq!(p.set(path), path.len());
        assert!(p.is_set());
        let mut out = [0u8; 64];
        let len = p.get(&mut out);
        assert_eq!(len, path.len());
        assert_eq!(&out[..len], path);
    }

    #[test]
    fn set_clamps_to_cap() {
        let p = InitProgram::new();
        let big = [b'a'; INIT_PROGRAM_CAP + 32];
        assert_eq!(p.set(&big), INIT_PROGRAM_CAP);
    }

    #[test]
    fn get_reports_true_length_even_when_truncated() {
        let p = InitProgram::new();
        p.set(b"/bin/hello");
        let mut out = [0u8; 4];
        // Copies only 4 bytes but reports the real length so the caller sees truncation.
        assert_eq!(p.get(&mut out), 10);
        assert_eq!(&out, b"/bin");
    }
}
