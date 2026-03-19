use super::*;
use akuma_exec::mmu::user_access::copy_to_user_safe;
use akuma_net::socket::libc_errno;

struct KernelPipe {
    buffer: Vec<u8>,
    write_count: u32,
    read_count: u32,
    reader_thread: Option<usize>,
}

static PIPES: Spinlock<BTreeMap<u32, KernelPipe>> = Spinlock::new(BTreeMap::new());
static NEXT_PIPE_ID: AtomicU32 = AtomicU32::new(1);

pub(crate) fn pipe_create() -> u32 {
    let id = NEXT_PIPE_ID.fetch_add(1, Ordering::SeqCst);
    crate::irq::with_irqs_disabled(|| {
        PIPES.lock().insert(id, KernelPipe {
            buffer: Vec::new(),
            write_count: 1,
            read_count: 1,
            reader_thread: None,
        });
    });
    crate::safe_print!(64, "[pipe] create id={}\n", id);
    id
}

pub fn pipe_clone_ref(id: u32, is_write: bool) {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            if is_write {
                pipe.write_count += 1;
                crate::safe_print!(64, "[pipe] clone_ref id={} write_count={}\n", id, pipe.write_count);
            } else {
                pipe.read_count += 1;
                crate::safe_print!(64, "[pipe] clone_ref id={} read_count={}\n", id, pipe.read_count);
            }
        }
    });
}

/// Write data to a pipe. Returns Ok(n) on success or Err(EPIPE) if the pipe
/// has been destroyed (no readers left or pipe removed). On Linux, writing to
/// a broken pipe delivers SIGPIPE and returns EPIPE; callers must replicate this.
pub(crate) fn pipe_write(id: u32, data: &[u8]) -> Result<usize, i32> {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            if pipe.read_count == 0 {
                return Err(libc_errno::EPIPE as i32);
            }
            pipe.buffer.extend_from_slice(data);
            if let Some(tid) = pipe.reader_thread.take() {
                akuma_exec::threading::get_waker_for_thread(tid).wake();
            }
            Ok(data.len())
        } else {
            crate::safe_print!(128, "[pipe] write WARN: pipe id={} not found (len={})\n", id, data.len());
            Err(libc_errno::EPIPE as i32)
        }
    })
}

pub(crate) fn pipe_read(id: u32, buf: &mut [u8]) -> (usize, bool) {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            let n = buf.len().min(pipe.buffer.len());
            if n > 0 {
                buf[..n].copy_from_slice(&pipe.buffer[..n]);
                pipe.buffer.drain(..n);
                (n, false)
            } else if pipe.write_count == 0 {
                (0, true)
            } else {
                (0, false)
            }
        } else {
            (0, true)
        }
    })
}

pub fn pipe_close_write(id: u32) {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            pipe.write_count = pipe.write_count.saturating_sub(1);
            // Always log close_write so we can trace use-after-close bugs.
            crate::safe_print!(128, "[pipe] close_write id={} write_count={} read_count={}\n", id, pipe.write_count, pipe.read_count);
            if pipe.write_count == 0 {
                if let Some(tid) = pipe.reader_thread.take() {
                    akuma_exec::threading::get_waker_for_thread(tid).wake();
                }
            }
            if pipe.write_count == 0 && pipe.read_count == 0 {
                crate::safe_print!(64, "[pipe] DESTROY id={} (both counts 0)\n", id);
                pipes.remove(&id);
            }
        }
    });
}

pub fn pipe_close_read(id: u32) {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            pipe.read_count = pipe.read_count.saturating_sub(1);
            // Always log close_read so we can trace use-after-close bugs.
            crate::safe_print!(128, "[pipe] close_read id={} write_count={} read_count={}\n", id, pipe.write_count, pipe.read_count);
            if pipe.write_count == 0 && pipe.read_count == 0 {
                crate::safe_print!(64, "[pipe] DESTROY id={} (both counts 0)\n", id);
                pipes.remove(&id);
            }
        }
    });
}

pub(super) fn pipe_set_reader_thread(id: u32, tid: usize) {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            pipe.reader_thread = Some(tid);
        }
    });
}

pub(super) fn pipe_can_read(id: u32) -> bool {
    crate::irq::with_irqs_disabled(|| {
        PIPES.lock().get(&id).map_or(false, |p| !p.buffer.is_empty() || p.write_count == 0)
    })
}

pub(crate) fn pipe_bytes_available(id: u32) -> usize {
    crate::irq::with_irqs_disabled(|| {
        PIPES.lock().get(&id).map_or(0, |p| p.buffer.len())
    })
}

pub(super) fn pipe_can_write(id: u32) -> bool {
    crate::irq::with_irqs_disabled(|| {
        PIPES.lock().get(&id).map_or(false, |p| p.read_count > 0)
    })
}

pub(super) fn sys_pipe2(fds_ptr: u64, flags: u32) -> u64 {
    if !validate_user_ptr(fds_ptr, 8) { return EFAULT; }
    let proc = match akuma_exec::process::current_process() { Some(p) => p, None => return ENOSYS };

    let pipe_id = pipe_create();
    let fd_r = proc.alloc_fd(akuma_exec::process::FileDescriptor::PipeRead(pipe_id));
    let fd_w = proc.alloc_fd(akuma_exec::process::FileDescriptor::PipeWrite(pipe_id));

    if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
        proc.set_cloexec(fd_r);
        proc.set_cloexec(fd_w);
    }

    let fds = [fd_r as i32, fd_w as i32];
    if unsafe { copy_to_user_safe(fds_ptr as *mut u8, fds.as_ptr() as *const u8, 8).is_err() } {
        return EFAULT;
    }
    0
}
