//! eventfd(2): the table, poller wake and refcount plumbing for the full
//! kernel. The state machine itself (counter semantics, flag bits, readiness)
//! is `akuma-syscalls-eventfd` — extracted 2026-10-26 (composition step 3b:
//! this file was welded to `akuma-exec`'s Process/threading, and the
//! taoxichuan composition root needed the decisions without the weld). The
//! one behaviour delta on the move: a write that would push the counter past
//! `u64::MAX - 1` now answers EAGAIN like Linux, where this file used to
//! saturate silently.
use alloc::collections::BTreeMap;
use akuma_exec::threading::{WakeHandle, wake_by_handle, wake_handle_for_thread};
use akuma_syscalls_eventfd::{EFD_CLOEXEC, EFD_NONBLOCK, EventFd};
use core::sync::atomic::AtomicU32;
use super::*;

struct KernelEventFd {
    ef: EventFd,
    /// tid -> generation-tagged handle (minted at registration); see pipe.rs pollers.
    pollers: BTreeMap<usize, WakeHandle>,
    ref_count: u32,
}

static EVENTFDS: Spinlock<BTreeMap<u32, KernelEventFd>> = Spinlock::new(BTreeMap::new());
static NEXT_EVENTFD_ID: AtomicU32 = AtomicU32::new(1);

pub fn eventfd_create(initval: u32, flags: u32) -> u32 {
    let id = NEXT_EVENTFD_ID.fetch_add(1, Ordering::SeqCst);
    let ef = match EventFd::new(initval, flags) {
        Ok(ef) => ef,
        Err(_) => return 0,
    };
    akuma_primitives::irq::with_irqs_disabled(|| {
        EVENTFDS.lock().insert(id, KernelEventFd {
            ef,
            pollers: BTreeMap::new(),
            ref_count: 1,
        });
    });
    id
}

pub fn eventfd_read(id: u32) -> Result<u64, i32> {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut table = EVENTFDS.lock();
        if let Some(efd) = table.get_mut(&id) {
            let val = efd.ef.read()?;

            // Wake other pollers (e.g. ones waiting for it to become writable,
            // though eventfd is always writable in this implementation).
            while let Some((_tid, handle)) = efd.pollers.pop_first() {
                wake_by_handle(handle);
            }

            Ok(val)
        } else {
            Err(akuma_net::socket::libc_errno::EBADF)
        }
    })
}

pub fn eventfd_write(id: u32, val: u64) -> Result<(), i32> {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut table = EVENTFDS.lock();
        if let Some(efd) = table.get_mut(&id) {
            efd.ef.write(val)?;
            if akuma_config::SYSCALL_DEBUG_NET_ENABLED {
                akuma_primitives::tprint!(96, "[eventfd] write id={} val={}\n", id, val);
            }

            // Wake all pollers
            while let Some((_tid, handle)) = efd.pollers.pop_first() {
                wake_by_handle(handle);
            }

            Ok(())
        } else {
            Err(akuma_net::socket::libc_errno::EBADF)
        }
    })
}

pub(super) fn eventfd_can_read(id: u32) -> bool {
    akuma_primitives::irq::with_irqs_disabled(|| {
        EVENTFDS.lock().get(&id).is_some_and(|efd| efd.ef.can_read())
    })
}

pub(super) fn eventfd_is_nonblock(id: u32) -> bool {
    akuma_primitives::irq::with_irqs_disabled(|| {
        EVENTFDS.lock().get(&id).is_some_and(|efd| efd.ef.is_nonblock())
    })
}

/// Increment the reference count for a shared eventfd (called on fork).
/// Mirrors `pipe_clone_ref` in the pipe subsystem.
pub fn eventfd_clone_ref(id: u32) {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut efds = EVENTFDS.lock();
        if let Some(efd) = efds.get_mut(&id) {
            efd.ref_count += 1;
            if akuma_config::SYSCALL_DEBUG_NET_ENABLED {
                akuma_primitives::safe_print!(96, "[eventfd] clone_ref id={} ref_count={}\n", id, efd.ref_count);
            }
        }
    });
}

/// Decrement the reference count. Destroys the eventfd only when ref_count reaches 0.
/// Previously this blindly removed the entry, breaking parent processes after fork+exec.
pub fn eventfd_close(id: u32) {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut efds = EVENTFDS.lock();
        if let Some(efd) = efds.get_mut(&id) {
            efd.ref_count = efd.ref_count.saturating_sub(1);
            if akuma_config::SYSCALL_DEBUG_NET_ENABLED {
                akuma_primitives::safe_print!(96, "[eventfd] close id={} ref_count={}\n", id, efd.ref_count);
            }
            if efd.ref_count == 0 {
                efds.remove(&id);
            }
        }
    });
}

pub fn eventfd_add_poller(id: u32, tid: usize) {
    akuma_primitives::irq::with_irqs_disabled(|| {
        if let Some(efd) = EVENTFDS.lock().get_mut(&id) {
            efd.pollers.insert(tid, wake_handle_for_thread(tid));
        }
    });
}

pub(super) fn sys_eventfd2(initval: u32, flags: u32) -> u64 {
    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return ENOSYS };
    let efd_id = eventfd_create(initval, flags);
    let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::EventFd(efd_id));
    if flags & EFD_CLOEXEC != 0 {
        proc.set_cloexec(fd);
    }
    if flags & EFD_NONBLOCK != 0 {
        proc.set_nonblock(fd);
    }
    if akuma_config::SYSCALL_DEBUG_NET_ENABLED {
        akuma_primitives::tprint!(96, "[syscall] eventfd2(initval={}, flags=0x{:x}) = fd {} (id={})\n", initval, flags, fd, efd_id);
    }
    u64::from(fd)
}
