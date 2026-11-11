//! Character devices a kernel serves itself, behind glue's fd arms.
//!
//! Glue owns the descriptor table and every fd syscall; a few devices belong to
//! one kernel's drivers. The amd64 kernel has six: `/dev/fb0`, `/dev/wifi0`,
//! `/dev/power`, `/dev/kvm0` and `/dev/kvm`, `/dev/net/tun` and
//! `/dev/input/mouse0`, whose descriptors are the `FileDescriptor::Dev*`
//! variants named in [`is_chardev`]. A kernel registers one [`CharDevOps`] at
//! boot; glue's `openat`, `read`, `write`, `readv`/`writev`, `ioctl`, `fstat`,
//! `close` and `close_range` call it for those descriptors, and for nothing
//! else. A kernel that registers nothing (AArch64) never hands one out, so the
//! arms are unreachable there.
//!
//! Before this module the amd64 kernel asked "is this one of mine?" in front
//! of every fd arm in its own dispatcher, which is what kept its fd layer a
//! separate copy (`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`, "Slice 4: fd").

use akuma_exec::process::FileDescriptor;
use akuma_syscalls_linux::Stat;

/// One kernel's character devices.
///
/// Every operation answers `Some` in syscall-return form (a count, or a
/// negative errno as `u64`), or `None` to leave the call to glue's own arm for
/// that descriptor, as if no kernel had claimed it.
pub struct CharDevOps {
    /// `open` of an absolute, resolved `path` with asm-generic `flags`.
    pub open: fn(path: &str, flags: u32) -> Option<u64>,
    /// `read(fd, buf, len)` on a device descriptor `d`.
    pub read: fn(fd: u32, d: &FileDescriptor, buf: u64, len: usize) -> Option<u64>,
    /// `write(fd, buf, len)` on a device descriptor `d`.
    pub write: fn(fd: u32, d: &FileDescriptor, buf: u64, len: usize) -> Option<u64>,
    /// `readv`/`writev` taken whole, for a device whose unit is the call (a
    /// tun frame); `None` runs the per-`iovec` loop.
    pub vectored: fn(d: &FileDescriptor, write: bool, iov: u64, cnt: u64) -> Option<u64>,
    /// `ioctl(fd, req, arg)` on a device descriptor `d`.
    pub ioctl: fn(fd: u32, d: &FileDescriptor, req: u32, arg: u64) -> Option<u64>,
    /// The `struct stat` of a device descriptor, or an errno.
    pub fstat: fn(d: &FileDescriptor) -> Option<Result<Stat, u64>>,
    /// Release whatever the device holds for `fd`, after it left the table.
    pub close: fn(fd: u32, d: &FileDescriptor),
}

static OPS: akuma_primitives::OnceCopy<&'static CharDevOps> = akuma_primitives::OnceCopy::new();

/// Register this kernel's devices. Boot-time, once (`OnceCopy`'s contract).
pub fn register(ops: &'static CharDevOps) {
    OPS.set(ops);
}

/// Is `d` a kernel-served character device?
#[must_use]
pub const fn is_chardev(d: &FileDescriptor) -> bool {
    matches!(
        d,
        FileDescriptor::DevFb
            | FileDescriptor::DevWifi
            | FileDescriptor::DevPower
            | FileDescriptor::DevKvm(_)
            | FileDescriptor::DevTun
            | FileDescriptor::DevMouse
    )
}

/// `openat`'s hook: see [`CharDevOps::open`].
pub(crate) fn open(path: &str, flags: u32) -> Option<u64> {
    OPS.get().and_then(|o| (o.open)(path, flags))
}

/// `read`'s hook, for any descriptor: `None` unless `d` is a device its kernel answers for.
pub(crate) fn read(fd: u32, d: &FileDescriptor, buf: u64, len: usize) -> Option<u64> {
    if !is_chardev(d) {
        return None;
    }
    OPS.get().and_then(|o| (o.read)(fd, d, buf, len))
}

/// `write`'s hook; see [`read`].
pub(crate) fn write(fd: u32, d: &FileDescriptor, buf: u64, len: usize) -> Option<u64> {
    if !is_chardev(d) {
        return None;
    }
    OPS.get().and_then(|o| (o.write)(fd, d, buf, len))
}

/// `readv`/`writev` on `fd`, when it is a device that takes the call whole;
/// `None` otherwise, and the caller runs the per-`iovec` loop.
#[must_use]
pub fn vectored(fd: u32, write: bool, iov: u64, cnt: u64) -> Option<u64> {
    let o = OPS.get()?;
    let d = akuma_exec::process::current_process_shared()?.get_fd(fd)?;
    if !is_chardev(&d) {
        return None;
    }
    (o.vectored)(&d, write, iov, cnt)
}

/// `ioctl`'s hook; see [`read`].
pub(crate) fn ioctl(fd: u32, d: &FileDescriptor, req: u32, arg: u64) -> Option<u64> {
    if !is_chardev(d) {
        return None;
    }
    OPS.get().and_then(|o| (o.ioctl)(fd, d, req, arg))
}

/// `fstat`'s hook; see [`read`].
pub(crate) fn fstat(d: &FileDescriptor) -> Option<Result<Stat, u64>> {
    if !is_chardev(d) {
        return None;
    }
    OPS.get().and_then(|o| (o.fstat)(d))
}

/// `close`'s hook, for any descriptor that has left the table.
pub(crate) fn close(fd: u32, d: &FileDescriptor) {
    if is_chardev(d)
        && let Some(o) = OPS.get()
    {
        (o.close)(fd, d);
    }
}
