use super::*;

struct KernelEventFd {
    counter: u64,
    flags: u32,
    reader_thread: Option<usize>,
}

static EVENTFDS: Spinlock<BTreeMap<u32, KernelEventFd>> = Spinlock::new(BTreeMap::new());
static NEXT_EVENTFD_ID: AtomicU32 = AtomicU32::new(1);

pub(super) const EFD_SEMAPHORE: u32 = 1;
pub(super) const EFD_NONBLOCK: u32 = 0x800;
pub(super) const EFD_CLOEXEC: u32 = 0x80000;

pub(super) fn eventfd_create(initval: u32, flags: u32) -> u32 {
    let id = NEXT_EVENTFD_ID.fetch_add(1, Ordering::SeqCst);
    crate::irq::with_irqs_disabled(|| {
        EVENTFDS.lock().insert(id, KernelEventFd {
            counter: initval as u64,
            flags,
            reader_thread: None,
        });
    });
    id
}

pub(super) fn eventfd_read(id: u32) -> Result<u64, i32> {
    crate::irq::with_irqs_disabled(|| {
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
            Ok(val)
        } else {
            Err(akuma_net::socket::libc_errno::EBADF)
        }
    })
}

pub(super) fn eventfd_write(id: u32, val: u64) -> Result<(), i32> {
    crate::irq::with_irqs_disabled(|| {
        let mut table = EVENTFDS.lock();
        if let Some(efd) = table.get_mut(&id) {
            efd.counter = efd.counter.saturating_add(val);
            if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                crate::tprint!(96, "[eventfd] write id={} val={} counter={}\n", id, val, efd.counter);
            }
            if let Some(tid) = efd.reader_thread.take() {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    crate::tprint!(64, "[eventfd] waking reader thread {}\n", tid);
                }
                akuma_exec::threading::get_waker_for_thread(tid).wake();
            }
            Ok(())
        } else {
            Err(akuma_net::socket::libc_errno::EBADF)
        }
    })
}

pub(super) fn eventfd_can_read(id: u32) -> bool {
    crate::irq::with_irqs_disabled(|| {
        EVENTFDS.lock().get(&id).map_or(false, |efd| efd.counter > 0)
    })
}

pub(super) fn eventfd_is_nonblock(id: u32) -> bool {
    crate::irq::with_irqs_disabled(|| {
        EVENTFDS.lock().get(&id).map_or(false, |efd| efd.flags & EFD_NONBLOCK != 0)
    })
}

pub fn eventfd_close(id: u32) {
    crate::irq::with_irqs_disabled(|| {
        EVENTFDS.lock().remove(&id);
    });
}

pub(super) fn eventfd_set_reader_thread(id: u32, tid: usize) {
    crate::irq::with_irqs_disabled(|| {
        if let Some(efd) = EVENTFDS.lock().get_mut(&id) {
            efd.reader_thread = Some(tid);
        }
    });
}

pub(super) fn sys_eventfd2(initval: u32, flags: u32) -> u64 {
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return ENOSYS };
    let efd_id = eventfd_create(initval, flags);
    let fd = proc.alloc_fd(akuma_exec::process::FileDescriptor::EventFd(efd_id));
    if flags & EFD_CLOEXEC != 0 {
        proc.set_cloexec(fd);
    }
    if flags & EFD_NONBLOCK != 0 {
        proc.set_nonblock(fd);
    }
    crate::tprint!(64, "[syscall] eventfd2(initval={}, flags=0x{:x}) = fd {}\n", initval, flags, fd);
    fd as u64
}
