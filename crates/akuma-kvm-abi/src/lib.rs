//! The `/dev/kvm` ioctl protocol, as data.
//!
//! Everything here was read off the wire: the unmodified `firecracker` was run against a logging
//! stub (`amd64/src/kvm/api.rs`) and each request code, argument size and argument content was
//! recorded (`docs/archive/AKUMA_FIRECRACKER_KVM.md`, Phase 1). No Linux, KVM or Firecracker
//! source or header was consulted. Names are inferred from direction, size, contents and position
//! in the boot sequence.
//!
//! | module | |
//! |---|---|
//! | [`req`] | a request code split into direction / size / type / number, and [`req::Op`], the closed set of requests seen |
//! | [`state`] | the vCPU argument structures: registers, segment registers, CPUID and MSR entries |
//! | [`slots`] | the guest-memory slot table: `SET_USER_MEMORY_REGION` validated against the reserved slice |
//! | [`fdargs`] | `IRQFD`, `IOEVENTFD`, `PIT2` arguments |
//!
//! `no_std`, `forbid(unsafe_code)`, no dependencies, no allocation: the kernel decodes on a path
//! that must not allocate.

#![no_std]
#![forbid(unsafe_code)]

pub mod fdargs;
pub mod req;
pub mod slots;
pub mod state;

/// Little-endian reads and writes at fixed offsets; `None`/`false` when the buffer is short.
pub(crate) fn rd32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
pub(crate) fn rd64(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}
pub(crate) fn wr32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
pub(crate) fn wr64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests;
