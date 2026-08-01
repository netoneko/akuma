use super::*;
use akuma_exec::mmu::user_access::copy_to_user_safe;
use akuma_net::socket::libc_errno;
use alloc::collections::{BTreeSet, VecDeque};

struct KernelPipe {
    buffer: VecDeque<u8>,
    write_count: u32,
    read_count: u32,
    /// Threads waiting on this pipe via read() or epoll/poll
    pollers: BTreeSet<usize>,
}

/// Maximum kernel pipe buffer capacity (bytes). Matches Linux's default pipe size
/// (64 KiB). A write that would exceed it is truncated to the space available, and a
/// write to an already-full pipe accepts nothing — see `pipe_write`'s contract.
///
/// Before this cap, `pipe.buffer` was an unbounded `VecDeque` that grew to whatever a
/// writer pushed, which is a userspace-driven unbounded kernel allocation. Two distinct
/// failures came out of that, both seen in `test_sigpipe_terminate_no_deadlock`
/// (`busybox yes | busybox head -n 1`):
///
/// 1. **Reader starvation.** `pipe_write` extends the buffer with IRQs disabled while
///    holding both `PIPES` and the BKL. Once the buffer is hundreds of MB, a single
///    realloc-and-copy is a multi-second window with no preemption, so the reader
///    barely runs and never drains — which only lets the writer grow it further.
///    Observed doubling 100 MB → 300 MB → 500 MB → ~1 GB, with `[BKL] stuck` from the
///    peer core throughout.
/// 2. **OOM inside the pipe lock.** That growth eventually fails an allocation, and
///    `alloc_error_handler` runs *inline* with `PIPES` still held: it calls
///    `return_to_kernel` → `cleanup_process_fds` → `pipe_close_write`, which takes
///    `PIPES` again. `spinning_top::Spinlock` is not reentrant, so the core wedged
///    permanently. Structurally the same trap as the SIGPIPE-inside-the-lock bug fixed
///    on 2026-07-24 (see `pipe_write`), and the reason this cap is a correctness fix
///    rather than a tuning knob: bounding the buffer is what keeps the allocator out
///    of the locked section at all.
///
/// Phase 7e's deferred process reclamation only changed the timing that exposed this;
/// the defect was latent in `pipe.rs` from the start.
/// See docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md.
pub const PIPE_CAPACITY: usize = 65536;

static PIPES: Spinlock<BTreeMap<u32, KernelPipe>> = Spinlock::new(BTreeMap::new());
static NEXT_PIPE_ID: AtomicU32 = AtomicU32::new(1);

pub fn pipe_create() -> u32 {
    let id = NEXT_PIPE_ID.fetch_add(1, Ordering::SeqCst);
    crate::irq::with_irqs_disabled(|| {
        PIPES.lock().insert(id, KernelPipe {
            buffer: VecDeque::new(),
            write_count: 1,
            read_count: 1,
            pollers: BTreeSet::new(),
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
                crate::safe_print!(128, "[pipe] clone_ref id={} write_count={} read_count={}\n", id, pipe.write_count, pipe.read_count);
            } else {
                pipe.read_count += 1;
                crate::safe_print!(128, "[pipe] clone_ref id={} write_count={} read_count={}\n", id, pipe.write_count, pipe.read_count);
            }
        }
    });
}

/// Register the current thread as interested in polling this pipe.
/// Called by epoll/poll check logic.
pub fn pipe_add_poller(id: u32, tid: usize) {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            pipe.pollers.insert(tid);
        }
    });
}

/// Write data to a pipe. Returns Ok(n) for the number of bytes accepted, or Err(EPIPE)
/// if the pipe has been destroyed (no readers left or pipe removed). On Linux, writing
/// to a broken pipe delivers SIGPIPE and returns EPIPE; callers must replicate this.
///
/// # Short writes
/// Since `PIPE_CAPACITY` landed this is a **partial** write: `n` may be less than
/// `data.len()`, and `n == 0` means the buffer is full and nothing was accepted (for
/// non-empty `data` — an empty `data` trivially returns `Ok(0)` too). Every caller must
/// handle that. `Ok(0)` is *not* success-with-nothing-to-do: treating it as success
/// silently drops the data, which for a framed protocol like the rump sysproxy desyncs
/// the stream. Callers that need whole-buffer delivery want
/// [`pipe_write_all_blocking`]; `sys_write` instead loops so it can honour O_NONBLOCK
/// and report a partial count to userspace the way write(2) does.
pub fn pipe_write(id: u32, data: &[u8]) -> Result<usize, i32> {
    // Outcome of the locked section: Ok(n) = wrote n bytes; Ok(n) with n=0
    // means the pipe buffer is full (caller should yield/block and retry);
    // Err(true) = broken pipe, raise SIGPIPE (after unlocking!);
    // Err(false) = pipe gone, plain EPIPE.
    let outcome = crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            if pipe.read_count == 0 {
                return Err(true);
            }
            let available = PIPE_CAPACITY.saturating_sub(pipe.buffer.len());
            if available == 0 {
                return Ok(0usize); // Pipe full — caller must yield/block
            }
            let n = data.len().min(available);
            pipe.buffer.extend(&data[..n]);

            // Wake all async pollers (epoll/poll/read)
            while let Some(tid) = pipe.pollers.pop_first() {
                akuma_exec::threading::get_waker_for_thread(tid).wake();
            }

            Ok(n)
        } else {
            crate::safe_print!(128, "[pipe] write WARN: pipe id={} not found (len={})\n", id, data.len());
            Err(false)
        }
    });
    match outcome {
        Ok(n) => Ok(n),
        Err(raise_sigpipe) => {
            if raise_sigpipe {
                // Send SIGPIPE to the current process (Linux behaviour) — with NO
                // pipe lock held. For a default disposition the delivery runs the
                // terminate action INLINE (tkill → sys_exit_group → close_all →
                // pipe_close_write), which re-acquires PIPES: raising it inside
                // the locked section above self-deadlocked the core (spinning on
                // its own lock, IRQs masked, still holding the BKL) and wedged
                // every other core in KernelLock::acquire. Root-caused live via
                // lldb 2026-07-24 (aria2c `| head -1` → EPIPE storm at exit).
                super::signal::send_sigpipe();
            }
            Err(libc_errno::EPIPE)
        }
    }
}

/// Write **every** byte of `data`, sleeping while the pipe is full, and return
/// `Err(EPIPE)` if the pipe breaks before that completes.
///
/// For in-kernel callers that put framed messages on a pipe they own both ends of
/// (the rump sysproxy request path and its `sys_sendmsg` reply path). Those protocols
/// read a frame's declared length back out, so a short write desyncs the stream for
/// good — they need all-or-error, not write(2)'s partial-count semantics.
///
/// Blocking here cannot deadlock those two users: each frame is written while the peer
/// is in its matching read, so a full buffer always has a live drainer on the other
/// side. Do **not** reach for this on a pipe whose reader might be the same thread.
pub fn pipe_write_all_blocking(id: u32, data: &[u8]) -> Result<(), i32> {
    let mut off = 0usize;
    while off < data.len() {
        match pipe_write(id, &data[off..])? {
            0 => {
                // Full: park until a reader drains (or the pipe breaks — `pipe_close_read`
                // wakes writers on the last-reader close, and the retry then sees EPIPE).
                let tid = akuma_exec::threading::current_thread_id();
                if !pipe_check_set_writer(id, tid) {
                    akuma_exec::threading::schedule_blocking(u64::MAX);
                }
            }
            n => off += n,
        }
    }
    Ok(())
}

pub fn pipe_read(id: u32, buf: &mut [u8]) -> (usize, bool) {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            let n = buf.len().min(pipe.buffer.len());
            if n > 0 {
                // VecDeque::drain is O(n), not O(buffer_size).
                for (i, b) in pipe.buffer.drain(..n).enumerate() {
                    buf[i] = b;
                }
                // Wake writer pollers — space is now available in the buffer.
                while let Some(tid) = pipe.pollers.pop_first() {
                    akuma_exec::threading::get_waker_for_thread(tid).wake();
                }
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
            
            // Notify waiters (EOF is an event)
            if pipe.write_count == 0 {
                while let Some(tid) = pipe.pollers.pop_first() {
                    akuma_exec::threading::get_waker_for_thread(tid).wake();
                }
            }

            if pipe.write_count == 0 && pipe.read_count == 0 {
                crate::safe_print!(64, "[pipe] DESTROY id={} (both counts 0)\n", id);
                pipes.remove(&id);
            }
        } else if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::tprint!(64, "[pipe] close_rw WARN: id={} not found\n", id);
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

            // Losing the last reader is an event for blocked *writers*, exactly as
            // losing the last writer is one for blocked readers (see the mirror-image
            // wake in `pipe_close_write`). A writer parked in `sys_write`'s full-buffer
            // path sits in `pollers` on an untimed `schedule_blocking(u64::MAX)`; it can
            // only learn the pipe broke by retrying `pipe_write` and seeing
            // `read_count == 0`. Without this wake it never retries, so it never gets
            // the EPIPE that raises SIGPIPE, and it sleeps forever.
            //
            // This is what `busybox yes | busybox head -n 1` hits every time once pipes
            // are capped: `yes` fills the 64 KiB buffer and parks, `head` reads its one
            // line and exits, and the last-reader close lands while `yes` is asleep.
            // Uncapped pipes never blocked a writer, so this wake had nothing to wake
            // and the asymmetry was invisible. See `test_pipe_close_read_wakes_blocked_writer`.
            if pipe.read_count == 0 {
                while let Some(tid) = pipe.pollers.pop_first() {
                    akuma_exec::threading::get_waker_for_thread(tid).wake();
                }
            }

            if pipe.write_count == 0 && pipe.read_count == 0 {
                crate::safe_print!(64, "[pipe] DESTROY id={} (both counts 0)\n", id);
                pipes.remove(&id);
            }
        } else if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::tprint!(64, "[pipe] close_rw WARN: id={} not found\n", id);
        }
    });
}

/// Atomically check if there is data (or EOF) available on the pipe, and if
/// not, register `tid` as the blocking reader. Returns `true` if the caller
/// should NOT block (data available, EOF, or pipe gone), `false` if it should
/// block (and the tid has been registered so it will be woken on next write).
///
/// This eliminates the TOCTOU window in the old two-step:
///   pipe_read() → (empty, no-eof) → pipe_set_reader_thread() → schedule_blocking()
/// A concurrent write between the first and second step would fire the wakeup
/// with no reader registered, causing the blocking thread to sleep forever.
pub fn pipe_check_set_reader(id: u32, tid: usize) -> bool {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            if !pipe.buffer.is_empty() || pipe.write_count == 0 {
                return true;
            }
            pipe.pollers.insert(tid);
            false
        } else {
            true // pipe gone → treat as EOF, don't block
        }
    })
}

/// Test helper: return the current reader_thread tid registered on `id`.
/// For the new poller-based implementation, we return true if tid is in the set.
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
pub fn pipe_is_poller_registered(id: u32, tid: usize) -> bool {
    crate::irq::with_irqs_disabled(|| {
        PIPES.lock().get(&id).is_some_and(|p| p.pollers.contains(&tid))
    })
}

/// Atomically check if there is space available to write to the pipe, and if
/// not, register `tid` as a blocking writer. Returns `true` if the caller
/// should NOT block (space available, no readers, or pipe gone), `false` if it
/// should block (and the tid has been registered so it will be woken when the
/// reader drains data).
pub fn pipe_check_set_writer(id: u32, tid: usize) -> bool {
    crate::irq::with_irqs_disabled(|| {
        let mut pipes = PIPES.lock();
        if let Some(pipe) = pipes.get_mut(&id) {
            if pipe.read_count == 0 {
                return true; // No readers → EPIPE, don't block
            }
            if pipe.buffer.len() < PIPE_CAPACITY {
                return true; // Space available
            }
            pipe.pollers.insert(tid);
            false
        } else {
            true // pipe gone → treat as EPIPE, don't block
        }
    })
}

/// Test helper: return how many pollers are registered on `id`.
#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]
pub fn pipe_pollers_count(id: u32) -> usize {
    crate::irq::with_irqs_disabled(|| {
        PIPES.lock().get(&id).map_or(0, |p| p.pollers.len())
    })
}

pub fn pipe_can_read(id: u32) -> bool {
    crate::irq::with_irqs_disabled(|| {
        PIPES.lock().get(&id).is_some_and(|p| !p.buffer.is_empty() || p.write_count == 0)
    })
}

pub fn pipe_bytes_available(id: u32) -> usize {
    crate::irq::with_irqs_disabled(|| {
        PIPES.lock().get(&id).map_or(0, |p| p.buffer.len())
    })
}

/// Whether a write would make progress: a live reader AND room under `PIPE_CAPACITY`.
/// The capacity term is what stops poll/epoll from reporting POLLOUT on a full pipe and
/// spinning a userspace event loop. `pub` to match `pipe_can_read` (asserted by tests).
pub fn pipe_can_write(id: u32) -> bool {
    crate::irq::with_irqs_disabled(|| {
        PIPES.lock().get(&id).is_some_and(|p| p.read_count > 0 && p.buffer.len() < PIPE_CAPACITY)
    })
}

pub(super) fn sys_pipe2(fds_ptr: u64, flags: u32) -> u64 {
    if !validate_user_ptr(fds_ptr, 8) { return EFAULT; }
    let proc = match akuma_exec::process::current_process_shared() { Some(p) => p, None => return ENOSYS };

    let pipe_id = pipe_create();
    let fd_r = proc.alloc_fd(akuma_exec::process::FileDescriptor::PipeRead(pipe_id));
    let fd_w = proc.alloc_fd(akuma_exec::process::FileDescriptor::PipeWrite(pipe_id));

    if flags & akuma_exec::process::open_flags::O_CLOEXEC != 0 {
        proc.set_cloexec(fd_r);
        proc.set_cloexec(fd_w);
    }

    let fds = [fd_r as i32, fd_w as i32];
    if unsafe { copy_to_user_safe(fds_ptr as *mut u8, fds.as_ptr().cast::<u8>(), 8).is_err() } {
        return EFAULT;
    }
    0
}
