//! This kernel's character devices, served through glue's fd arms.
//!
//! `/dev/fb0`, `/dev/wifi0`, `/dev/power`, `/dev/kvm0` and `/dev/kvm`,
//! `/dev/net/tun` and `/dev/input/mouse0` belong to this kernel's drivers, not
//! to glue. They used to be intercepted in front of every fd arm in this
//! kernel's dispatcher (`is_dev_fb(fd)` before `read`, `write`, `ioctl`,
//! `fstat` and `close`, the paths before `openat`). Since slice 4 step 2 glue
//! asks [`OPS`] instead (`akuma_syscalls_glue::chardev`), so the dispatcher's
//! fd arms are glue-amd64's with nothing in front of them.
//!
//! Each operation answers `None` where this kernel never answered for that
//! device: the call then reaches glue's own arm, exactly as it did before.

use akuma_exec::process::FileDescriptor;
use akuma_syscalls_glue::chardev::CharDevOps;
use akuma_syscalls_linux::Stat;

use crate::fd::{errno, open_flags};

/// The table glue calls. Registered by [`register`] at boot.
static OPS: CharDevOps = CharDevOps { open, read, write, vectored, ioctl, fstat, close };

/// Hand [`OPS`] to glue. Boot-time, once.
pub fn register() {
    akuma_syscalls_glue::chardev::register(&OPS);
}

/// `open` of a resolved path: this kernel's device nodes, or `None`.
fn open(path: &str, flags: u32) -> Option<u64> {
    Some(match path {
        // The screen's owner is decided here (`fbdev::open`, `EBUSY` for a
        // second process). A machine with no boot framebuffer has no node.
        "/dev/fb0" => open_framebuffer(flags),
        // The wireless driver's control node (`drivers::Wireless`).
        "/dev/wifi0" => open_wifi(flags),
        // The power daemon's control device (`crate::power`), on every machine:
        // a box with no EC and no screen still has idle facts.
        "/dev/power" => open_power(flags),
        // The hypervisor's control device (`drivers::Hypervisor`), when armed.
        "/dev/kvm0" => open_kvm(flags),
        // The census stub, only with the `kvmapi` boot flag.
        "/dev/kvm" if crate::drivers::hypervisor_api_enabled() => crate::drivers::hypervisor_api_open(flags),
        // The tap for a userspace VMM's NIC, behind the hypervisor's gateway.
        "/dev/net/tun" if crate::drivers::hypervisor_present() => crate::drivers::hypervisor_tun_open(flags),
        // The touchpad's pointer stream.
        "/dev/input/mouse0" => open_mouse(flags),
        _ => return None,
    })
}

/// Allocate `desc` in the caller's table, honouring `O_CLOEXEC`.
fn install(desc: FileDescriptor, flags: u32) -> u64 {
    let Some(proc) = akuma_exec::process::current_process_shared() else {
        return errno::ESRCH;
    };
    let fd = proc.alloc_fd(desc);
    if flags & open_flags::O_CLOEXEC != 0 {
        proc.set_cloexec(fd);
    }
    u64::from(fd)
}

fn open_framebuffer(flags: u32) -> u64 {
    if crate::fbdev::geometry().is_none() {
        return errno::ENOENT;
    }
    let Some(proc) = akuma_exec::process::current_process_shared() else {
        return errno::ESRCH;
    };
    if let Err(e) = crate::fbdev::open(proc.tgid) {
        return e;
    }
    install(FileDescriptor::DevFb, flags)
}

fn open_wifi(flags: u32) -> u64 {
    if !crate::drivers::wireless_present() {
        return errno::ENOENT;
    }
    install(FileDescriptor::DevWifi, flags)
}

fn open_power(flags: u32) -> u64 {
    install(FileDescriptor::DevPower, flags)
}

fn open_kvm(flags: u32) -> u64 {
    if !crate::drivers::hypervisor_armed() {
        return errno::ENOENT;
    }
    install(FileDescriptor::DevKvm(0), flags)
}

/// Only when the touchpad came up: a machine without one has no node.
fn open_mouse(flags: u32) -> u64 {
    if !crate::drivers::pointer_present() {
        return errno::ENOENT;
    }
    install(FileDescriptor::DevMouse, flags)
}

fn read(fd: u32, d: &FileDescriptor, buf: u64, len: usize) -> Option<u64> {
    Some(match d {
        // This descriptor's snapshot of the driver state.
        FileDescriptor::DevWifi => {
            let Some(proc) = akuma_exec::process::current_process_shared() else {
                return Some(errno::ESRCH);
            };
            crate::drivers::wireless_dev_read(proc.tgid, fd, buf, len)
        }
        // One frame behind its virtio-net header, or `EAGAIN`.
        FileDescriptor::DevTun => crate::drivers::hypervisor_tun_read(buf, len),
        // Whole 8-byte pointer records, or `EAGAIN`.
        FileDescriptor::DevMouse => crate::drivers::pointer_dev_read(buf, len),
        // The guest's console stream: `EAGAIN` while quiet, 0 once it ended.
        FileDescriptor::DevKvm(0) => crate::drivers::hypervisor_ctl_read(buf, len),
        // The kernel's idle/dim facts, fresh per read.
        FileDescriptor::DevPower => crate::power::dev_read(buf, len),
        _ => return None,
    })
}

fn write(_fd: u32, d: &FileDescriptor, buf: u64, len: usize) -> Option<u64> {
    Some(match d {
        FileDescriptor::DevWifi => crate::drivers::wireless_dev_write(buf, len),
        FileDescriptor::DevTun => crate::drivers::hypervisor_tun_write(buf, len),
        FileDescriptor::DevKvm(0) => crate::drivers::hypervisor_ctl_write(buf, len),
        FileDescriptor::DevMouse => crate::drivers::pointer_dev_write(buf, len),
        FileDescriptor::DevPower => crate::power::dev_write(buf, len),
        _ => return None,
    })
}

/// A tun frame is one `readv`/`writev`, never a run of per-`iovec` calls.
fn vectored(d: &FileDescriptor, write: bool, iov: u64, cnt: u64) -> Option<u64> {
    match d {
        FileDescriptor::DevTun if write => Some(crate::drivers::hypervisor_tun_writev(iov, cnt as usize)),
        FileDescriptor::DevTun => Some(crate::drivers::hypervisor_tun_readv(iov, cnt as usize)),
        _ => None,
    }
}

fn ioctl(fd: u32, d: &FileDescriptor, req: u32, arg: u64) -> Option<u64> {
    Some(match d {
        FileDescriptor::DevKvm(id) if *id > 0 => crate::drivers::hypervisor_api_ioctl(u64::from(fd), *id, req, arg),
        FileDescriptor::DevTun => crate::drivers::hypervisor_tun_ioctl(req, arg),
        FileDescriptor::DevFb => crate::fbdev::ioctl(req, arg),
        _ => return None,
    })
}

/// A character device `stat`: `S_IFCHR | mode`, `st_rdev` = `major:minor`.
fn chr(ino: u64, mode: u32, major: u64, minor: u64) -> Stat {
    Stat {
        st_ino: ino,
        st_mode: 0o020_000 | mode,
        st_nlink: 1,
        st_rdev: (major << 8) | minor,
        st_blksize: 4096,
        ..Default::default()
    }
}

fn fstat(d: &FileDescriptor) -> Option<Result<Stat, u64>> {
    Some(Ok(match d {
        // A misc character device (10, 242), root-only.
        FileDescriptor::DevWifi => chr(19, 0o600, 10, 0xF2),
        // Linux's mousedev (13, 32), readable by all.
        FileDescriptor::DevMouse => chr(21, 0o644, 13, 32),
        // A misc character device (10, 244), root-only: the next free minor.
        FileDescriptor::DevKvm(0) => chr(22, 0o600, 10, 0xF4),
        // A misc character device (10, 243), beside wifi0's.
        FileDescriptor::DevPower => chr(20, 0o600, 10, 0xF3),
        // An fbdev node (29, 0), size 0: the size is `FBIOGET_FSCREENINFO`'s business.
        FileDescriptor::DevFb => chr(17, 0o660, 29, 0),
        _ => return None,
    }))
}

fn close(fd: u32, d: &FileDescriptor) {
    match d {
        FileDescriptor::DevFb => {
            if let Some(p) = akuma_exec::process::current_process_shared() {
                crate::fbdev::release(p.tgid);
            }
        }
        FileDescriptor::DevTun => crate::drivers::hypervisor_tun_release(),
        FileDescriptor::DevWifi => {
            if let Some(p) = akuma_exec::process::current_process_shared() {
                crate::drivers::wireless_release(p.tgid, fd);
            }
        }
        _ => {}
    }
}
