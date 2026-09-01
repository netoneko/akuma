use alloc::collections::BTreeMap;
use akuma_exec::threading::{WakeHandle, wake_by_handle, wake_handle_for_thread};
use super::*;

struct KernelEventFd {
    counter: u64,
    flags: u32,
    /// tid -> generation-tagged handle (minted at registration); see pipe.rs pollers.
    pollers: BTreeMap<usize, WakeHandle>,
    ref_count: u32,
}

static EVENTFDS: Spinlock<BTreeMap<u32, KernelEventFd>> = Spinlock::new(BTreeMap::new());
static NEXT_EVENTFD_ID: AtomicU32 = AtomicU32::new(1);

pub(super) const EFD_SEMAPHORE: u32 = 1;
pub(super) const EFD_NONBLOCK: u32 = 0x800;
pub(super) const EFD_CLOEXEC: u32 = 0x80000;

pub fn eventfd_create(initval: u32, flags: u32) -> u32 {
    let id = NEXT_EVENTFD_ID.fetch_add(1, Ordering::SeqCst);
    akuma_primitives::irq::with_irqs_disabled(|| {
        EVENTFDS.lock().insert(id, KernelEventFd {
            counter: u64::from(initval),
            flags,
            pollers: BTreeMap::new(),
            ref_count: 1,
        });
    });
    id
}

pub(super) fn eventfd_read(id: u32) -> Result<u64, i32> {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut table = EVENTFDS.lock();
        if let Some(efd) = table.get_mut(&id) {
            if efd.counter == 0 {
                return Err(akuma_net::socket::libc_errno::EAGAIN);
            }
            let val = if efd.flags & EFD_SEMAPHORE != 0 {
                efd.counter -= 1;
                1
            } else {
                let v = efd.counter;
                efd.counter = 0;
                v
            };
            
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
            efd.counter = efd.counter.saturating_add(val);
            if akuma_config::SYSCALL_DEBUG_NET_ENABLED {
                akuma_primitives::tprint!(96, "[eventfd] write id={} val={} counter={}\n", id, val, efd.counter);
            }
            
            // Wake all pollers
            while let Some((_tid, handle)) = efd.pollers.pop_first() {
                wake_by_handle(handle);
            }

            Ok(val)
        } else {
            Err(akuma_net::socket::libc_errno::EBADF)
        }
    }).map(|_| ())
}

pub(super) fn eventfd_can_read(id: u32) -> bool {
    akuma_primitives::irq::with_irqs_disabled(|| {
        EVENTFDS.lock().get(&id).is_some_and(|efd| efd.counter > 0)
    })
}

pub(super) fn eventfd_is_nonblock(id: u32) -> bool {
    akuma_primitives::irq::with_irqs_disabled(|| {
        EVENTFDS.lock().get(&id).is_some_and(|efd| efd.flags & EFD_NONBLOCK != 0)
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
