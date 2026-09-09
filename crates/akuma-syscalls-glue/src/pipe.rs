//! The AArch64 kernel's pipe surface: the lock, the effects, and `pipe2(2)`.
//!
//! The table itself — end reference counts, the waiter set, and every rule
//! joining them — moved to [`akuma_pipes::PipeTable`] on 2026-09-06 so the amd64
//! kernel could stop carrying a second, smaller implementation of the same
//! rules and so the rules could be host-tested at all
//! (`docs/archive/AKUMA_PIPES_EXTRACTION.md`). Every `pipe_*` signature below is
//! unchanged: 540 references live outside this module.
//!
//! What stays here is exactly what the crate deliberately does **not** do — the
//! effects. `PipeTable` returns a [`Wakes`] set rather than firing it, and this
//! module fires it *after* dropping the lock, along with `send_sigpipe` and
//! `unix_channel_detach`. Both of those are here rather than there because both
//! re-enter: raising `SIGPIPE` under a default disposition runs the terminate
//! action inline and comes back through `pipe_close_write` into this same
//! lock (root-caused live 2026-07-24, `aria2c | head -1`), and the AF_UNIX
//! detach takes the socket-table lock, which is the mirror inversion.

use super::*;
use akuma_net::socket::libc_errno;
use akuma_pipes::{PipeTable, WriteOutcome, Wakes};
use akuma_exec::threading::{WakeHandle, wake_by_handle, wake_handle_for_thread};

/// Maximum kernel pipe buffer capacity (bytes).
///
/// Matches Linux's default pipe size (64 KiB), and is [`akuma_pipes`]'s own
/// default — named here because callers outside this module compute room
/// against it (`unixsock`'s record framing, `net.rs`'s sysproxy). A write that
/// would exceed it is truncated to the space available, and a write to an
/// already-full pipe accepts nothing — see `pipe_write`'s contract.
///
/// Before this cap, the buffer was an unbounded `VecDeque` that grew to whatever a
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
pub const PIPE_CAPACITY: usize = akuma_pipes::DEFAULT_CAPACITY;

static PIPES: Spinlock<PipeTable<WakeHandle>> = Spinlock::new(PipeTable::new());

/// Run `f` against the table with IRQs masked and the lock held.
///
/// The whole locked section in one place: nothing that can re-enter belongs
/// inside `f`, which is why every wake it produces is *returned* rather than
/// fired (see the module header).
fn with_table<R>(f: impl FnOnce(&mut PipeTable<WakeHandle>) -> R) -> R {
    akuma_primitives::irq::with_irqs_disabled(|| f(&mut PIPES.lock()))
}

/// The wake effect, if a kernel registered its own — see [`set_wake_sink`].
static WAKE_SINK: akuma_primitives::OnceCopy<fn(usize, WakeHandle)> =
    akuma_primitives::OnceCopy::new();

/// Route this table's wakes through `f` instead of straight to
/// [`wake_by_handle`].
///
/// **The seam that lets a second kernel share this table.** The state change a
/// wake performs is the same on both — amd64's threads *are* `akuma-threading`
/// slots, and its `sched::wake` is `wake_by_handle` underneath — but that
/// kernel wraps it to keep the `[SCHED] wakes` counter its boot suite reports,
/// which a direct call here would bypass. One `OnceCopy` load per fire buys
/// one pipe table instead of two, and two pipe tables was not a duplicate
/// implementation (both are [`akuma_pipes::PipeTable`]) but a duplicate
/// **id space**: this arm closing pipe 3 while the caller's descriptor named
/// the other table's pipe 3 is a wrong pipe, not an error.
///
/// Registered once at boot, before any pipe exists. Unregistered — every
/// AArch64 build — the default below is what runs.
pub fn set_wake_sink(f: fn(usize, WakeHandle)) {
    WAKE_SINK.set(f);
}

/// Make every returned waiter runnable. **Call with no lock held.**
fn fire(wakes: Wakes<WakeHandle>) {
    match WAKE_SINK.get() {
        Some(sink) => wakes.fire(sink),
        None => wakes.fire(|_tid, handle| wake_by_handle(handle)),
    }
}

/// Dump every live pipe: buffered bytes, endpoint refcounts, and the tids parked on it.
///
/// The decisive diagnostic for the `-j4` self-host jam
/// (docs/archive/SELFHOST_DEVBOX_SMOLTCP.md): at the jam cargo's jobserver helper sits
/// in `read(fd=5)` forever with no child alive. Two very different causes produce that
/// same `[THR-DUMP]` line, and only pipe state separates them:
///
/// - `bytes>0` with a parked reader  => a genuine LOST WAKEUP in the kernel: data is
///   available and the reader was never woken.
/// - `bytes=0`, reader parked, `writers>0` => the kernel is behaving; nobody wrote the
///   token, i.e. the stall is userspace/jobserver accounting (a token lost with a dead
///   child), and the kernel is exonerated.
///
/// Printed next to `[THR-DUMP]` under the same `DEADLOCK_THREAD_DUMP_ENABLED` gate.
pub fn pipe_dump() {
    with_table(|pipes| {
        if pipes.live_count() == 0 {
            return;
        }
        akuma_primitives::tprint!(48, "[PIPE-DUMP] {} live\n", pipes.live_count());
        for (id, readers, writers, bytes, pollers) in pipes.iter() {
            // Reader tids are printed so the parked `read()` in `[THR-DUMP]` can be
            // matched to the pipe it is parked on.
            let mut waiters = [0usize; 8];
            let mut n = 0;
            for t in pipes.poller_tids(id) {
                if n >= waiters.len() { break; }
                waiters[n] = t;
                n += 1;
            }
            akuma_primitives::tprint!(160, "  pipe={} bytes={} readers={} writers={} pollers={}\n",
                id, bytes, readers, writers, pollers);
            for w in waiters.iter().take(n) {
                akuma_primitives::tprint!(48, "    poller tid={}\n", w);
            }
        }
    });
}

pub fn pipe_create() -> u32 {
    let id = with_table(PipeTable::create);
    if akuma_config::PIPE_TRACE_ENABLED {
        akuma_primitives::safe_print!(64, "[pipe] create id={}\n", id);
    }
    id
}

pub fn pipe_clone_ref(id: u32, is_write: bool) {
    let counts = with_table(|t| {
        t.clone_ref(id, is_write);
        t.counts(id)
    });
    if akuma_config::PIPE_TRACE_ENABLED && let Some((read_count, write_count)) = counts {
        akuma_primitives::safe_print!(128, "[pipe] clone_ref id={} write_count={} read_count={}\n", id, write_count, read_count);
    }
}

/// Register the current thread as interested in polling this pipe.
/// Called by epoll/poll check logic.
pub fn pipe_add_poller(id: u32, tid: usize) {
    // Minted before the lock: it is two atomic loads, and nothing that can be
    // done outside the locked section belongs inside it.
    let handle = wake_handle_for_thread(tid);
    with_table(|t| t.add_poller(id, tid, handle));
}

/// How many threads are registered as pollers on this pipe.
///
/// The pipe counterpart to `akuma_net::socket::KernelSocket::waker_count`, and
/// it exists for the same reason: evidence that a waiter actually announced
/// itself, rather than silently riding the `BLOCKING_POLL_INTERVAL_US` tick.
/// `sys_pselect6` passed `None` for its waker for as long as nobody could see
/// this number — see `run_pselect6_registers_waker_test`.
#[cfg(kernel_tests)]
pub fn pipe_poller_count(id: u32) -> usize {
    with_table(|t| t.poller_count(id))
}

/// Write data to a pipe.
///
/// Returns Ok(n) for the number of bytes accepted, or Err(EPIPE) if the pipe
/// has been destroyed (no readers left or pipe removed). On Linux, writing to a
/// broken pipe delivers SIGPIPE and returns EPIPE; callers must replicate this.
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
    write_inner(id, data, true)
}

/// [`pipe_write`] without the `SIGPIPE`.
///
/// For a kernel with no signal delivery: amd64 answers a broken pipe with
/// `EPIPE` alone, which is "the whole of Linux's answer that applies" there
/// (`amd64/src/fd.rs`, `write_pipe`). Spelled as a second entry point rather
/// than as a global policy flag so the choice is visible at the call site that
/// makes it, and so nothing can change it after boot.
pub fn pipe_write_no_sigpipe(id: u32, data: &[u8]) -> Result<usize, i32> {
    write_inner(id, data, false)
}

fn write_inner(id: u32, data: &[u8], raise_sigpipe: bool) -> Result<usize, i32> {
    let (outcome, wakes) = with_table(|t| t.write(id, data));
    // Outside the lock, and before the SIGPIPE below: a wake is cheap and
    // cannot re-enter, where the signal can and does.
    fire(wakes);
    match outcome {
        WriteOutcome::Wrote(n) => Ok(n),
        WriteOutcome::BrokenPipe => {
            // Send SIGPIPE to the current process (Linux behaviour) — with NO
            // pipe lock held. For a default disposition the delivery runs the
            // terminate action INLINE (tkill → sys_exit_group → close_all →
            // pipe_close_write), which re-acquires PIPES: raising it inside
            // the locked section self-deadlocked the core (spinning on its own
            // lock, IRQs masked, still holding the BKL) and wedged every other
            // core in KernelLock::acquire. Root-caused live via lldb 2026-07-24
            // (aria2c `| head -1` → EPIPE storm at exit).
            if raise_sigpipe {
                super::signal::send_sigpipe();
            }
            Err(libc_errno::EPIPE)
        }
        // No pipe at all: plain EPIPE, and explicitly *no* signal. A missing id
        // is a caller bug, not a peer that hung up.
        WriteOutcome::NoSuchPipe => {
            if akuma_config::PIPE_TRACE_ENABLED {
                akuma_primitives::safe_print!(128, "[pipe] write WARN: pipe id={} not found (len={})\n", id, data.len());
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
// Callers live on the rump-sysproxy paths (`not(smoltcp)` sendmsg, `feature = "rump"`
// proxy) and in the boot tests; size/extreme (smoltcp + no-tests, no rump) compile none
// of them. Allow rather than cfg-mirror three caller gates.
#[allow(dead_code)]
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
    // The wake is not symmetry: draining makes room, and a writer parked on a
    // full buffer has no other way to learn it.
    let (r, wakes) = with_table(|t| t.read(id, buf));
    fire(wakes);
    (r.bytes, r.eof)
}

pub fn pipe_close_write(id: u32) {
    // The detach is decided inside the locked section and acted on outside it:
    // the AF_UNIX channel detach takes the socket-table lock, and taking that
    // while `PIPES` is held (IRQs masked) is the lock-ordering inversion that
    // wedged a core the last time an allocation ran inside this lock (see
    // `PIPE_CAPACITY`'s docs, failure 2).
    let (existed, counts, res, wakes) = with_table(|t| {
        let existed = t.exists(id);
        let (res, wakes) = t.close_write(id);
        (existed, t.counts(id).unwrap_or((0, 0)), res, wakes)
    });
    // EOF is an event; `close_write` returns the readers it freed.
    fire(wakes);
    if akuma_config::PIPE_TRACE_ENABLED && existed {
        // Always log close_write so we can trace use-after-close bugs.
        akuma_primitives::safe_print!(128, "[pipe] close_write id={} write_count={} read_count={}\n", id, counts.1, counts.0);
    }
    if !existed && akuma_config::SYSCALL_DEBUG_INFO_ENABLED {
        akuma_primitives::tprint!(64, "[pipe] close_rw WARN: id={} not found\n", id);
    }
    if res.destroyed {
        if akuma_config::PIPE_TRACE_ENABLED {
            akuma_primitives::safe_print!(64, "[pipe] DESTROY id={} (both counts 0)\n", id);
        }
        // The pipe is gone, so its AF_UNIX framing metadata must go with it —
        // and any SCM_RIGHTS descriptors still sitting in unread records must be
        // closed rather than dropped. A channel that outlives its pipe would be
        // re-attached by the next pipe to reuse the id, handing a fresh socket
        // the previous one's record boundaries.
        super::unixsock::unix_channel_detach(id);
    }
}

pub fn pipe_close_read(id: u32) {
    // Same locked-section rule as `pipe_close_write`.
    let (existed, counts, res, wakes) = with_table(|t| {
        let existed = t.exists(id);
        let (res, wakes) = t.close_read(id);
        (existed, t.counts(id).unwrap_or((0, 0)), res, wakes)
    });
    // Losing the last reader is an event for blocked *writers*, exactly as losing the
    // last writer is one for blocked readers. A writer parked in `sys_write`'s
    // full-buffer path sits in the poller set on an untimed
    // `schedule_blocking(u64::MAX)`; it can only learn the pipe broke by retrying
    // `pipe_write` and seeing `read_count == 0`. Without this wake it never retries, so
    // it never gets the EPIPE that raises SIGPIPE, and it sleeps forever.
    //
    // This is what `busybox yes | busybox head -n 1` hits every time once pipes are
    // capped: `yes` fills the 64 KiB buffer and parks, `head` reads its one line and
    // exits, and the last-reader close lands while `yes` is asleep. Uncapped pipes
    // never blocked a writer, so this wake had nothing to wake and the asymmetry was
    // invisible. See `test_pipe_close_read_wakes_blocked_writer`.
    fire(wakes);
    if akuma_config::PIPE_TRACE_ENABLED && existed {
        // Always log close_read so we can trace use-after-close bugs.
        akuma_primitives::safe_print!(128, "[pipe] close_read id={} write_count={} read_count={}\n", id, counts.1, counts.0);
    }
    if !existed && akuma_config::SYSCALL_DEBUG_INFO_ENABLED {
        akuma_primitives::tprint!(64, "[pipe] close_rw WARN: id={} not found\n", id);
    }
    if res.destroyed {
        if akuma_config::PIPE_TRACE_ENABLED {
            akuma_primitives::safe_print!(64, "[pipe] DESTROY id={} (both counts 0)\n", id);
        }
        super::unixsock::unix_channel_detach(id);
    }
}

/// Atomically check if there is data (or EOF) available on the pipe, and if
/// not, register `tid` as the blocking reader.
///
/// Returns `true` if the caller should NOT block (data available, EOF, or pipe
/// gone), `false` if it should block (and the tid has been registered so it
/// will be woken on next write).
///
/// This eliminates the TOCTOU window in the old two-step:
///   pipe_read() → (empty, no-eof) → pipe_set_reader_thread() → schedule_blocking()
/// A concurrent write between the first and second step would fire the wakeup
/// with no reader registered, causing the blocking thread to sleep forever.
pub fn pipe_check_set_reader(id: u32, tid: usize) -> bool {
    let handle = wake_handle_for_thread(tid);
    with_table(|t| t.check_set_reader(id, tid, handle))
}

/// Test helper: return the current reader_thread tid registered on `id`.
/// For the new poller-based implementation, we return true if tid is in the set.
#[cfg(kernel_tests)]
pub fn pipe_is_poller_registered(id: u32, tid: usize) -> bool {
    with_table(|t| t.is_poller_registered(id, tid))
}

/// Atomically check if there is space available to write to the pipe, and if
/// not, register `tid` as a blocking writer.
///
/// Returns `true` if the caller should NOT block (space available, no readers,
/// or pipe gone), `false` if it should block (and the tid has been registered
/// so it will be woken when the reader drains data).
pub fn pipe_check_set_writer(id: u32, tid: usize) -> bool {
    let handle = wake_handle_for_thread(tid);
    with_table(|t| t.check_set_writer(id, tid, handle))
}

/// Test helper: return how many pollers are registered on `id`.
#[cfg(kernel_tests)]
pub fn pipe_pollers_count(id: u32) -> usize {
    with_table(|t| t.poller_count(id))
}

/// Is there data to read, or EOF to report?
///
/// A **missing** pipe is `false` here where [`akuma_pipes::PipeTable::readable`]
/// says `true`: the crate answers "must the caller avoid blocking", which a gone
/// pipe satisfies by reading EOF, while this is `poll(2)`'s POLLIN on a live
/// descriptor. Every caller reaches it through an fd that names the pipe, so the
/// two only differ on an id nobody holds — kept distinct rather than merged so a
/// stale id reports "not ready" instead of manufacturing a readable event.
///
/// **`amd64/src/pipe.rs` takes the crate's answer instead**, and that divergence
/// is deliberate rather than drift: the fixed slot array it replaced reported on
/// the array entry, not on a live pipe, so "missing" was never a state it could
/// answer `false` for, and it has no epoll/`tokio` callers pinned to this one.
pub fn pipe_can_read(id: u32) -> bool {
    with_table(|t| t.exists(id) && t.readable(id))
}

/// True once every write end is gone (or the pipe is already destroyed): the
/// read end can never produce anything but EOF again.
///
/// This is `POLLHUP` on a pipe read end, and it is the bit that distinguishes
/// "drained, writer still alive" from "at EOF" — `pipe_can_read` folds both
/// into one `POLLIN`, so an edge-triggered watcher has nothing else to key the
/// EOF transition on. See `docs/archive/TOKIO_PIPE_EPOLL_HANG.md`.
pub fn pipe_hup(id: u32) -> bool {
    with_table(|t| t.counts(id).is_none_or(|(_, write_count)| write_count == 0))
}

pub fn pipe_bytes_available(id: u32) -> usize {
    with_table(|t| t.buffered(id))
}

/// Whether a write would make progress: a live reader AND room under
/// `PIPE_CAPACITY`.
///
/// The capacity term is what stops poll/epoll from reporting POLLOUT on a full
/// pipe and spinning a userspace event loop. `pub` to match `pipe_can_read`
/// (asserted by tests), and a missing pipe is `false` here for the same reason
/// it is there.
pub fn pipe_can_write(id: u32) -> bool {
    with_table(|t| t.counts(id).is_some_and(|(read_count, _)| read_count > 0) && t.writable(id))
}

/// Does this id name a live pipe?
///
/// For a caller that needs to tell "gone" from "not ready" — which is the one
/// axis [`pipe_can_read`] folds away, and the axis amd64's `poll` answers
/// differently (see that function's note).
#[must_use]
pub fn pipe_exists(id: u32) -> bool {
    with_table(|t| t.exists(id))
}

/// `(read_count, write_count)` — open descriptions naming each end — or `None`
/// for an id that names no pipe.
///
/// The honest accessor behind the readiness predicates: a caller whose `poll`
/// rules differ from this module's builds them from the counts rather than
/// getting a second predicate added here.
#[must_use]
pub fn pipe_counts(id: u32) -> Option<(u32, u32)> {
    with_table(|t| t.counts(id))
}

/// How many pipes are live right now.
///
/// A machine-wide ceiling is a policy, not a table rule, so the crate does not
/// impose one — amd64 caps at 64 so that a pipe leak announces itself instead
/// of being absorbed (each pipe is up to `PIPE_CAPACITY` of kernel buffer,
/// claimed on a userspace request), and asks this to enforce it.
#[must_use]
pub fn pipe_live_count() -> usize {
    with_table(|t| t.live_count())
}

/// Remove a pipe outright, whatever its end counts say, waking anyone parked
/// on it.
///
/// For a lifetime managed by hand rather than by reference counting: a spawned
/// child's stdin pipe on amd64 is read by the child by number, so its read end
/// never closes and refcounting alone would never free it — `waitpid` is what
/// knows the child is gone. A `pipe(2)` pair must **not** come through here;
/// it is destroyed by the last [`pipe_close_read`]/[`pipe_close_write`], and
/// short-circuiting that frees the buffer under a live peer.
pub fn pipe_destroy(id: u32) {
    let (_, wakes) = with_table(|t| t.destroy(id));
    fire(wakes);
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
    if write_user_val(fds_ptr, &fds).is_err() {
        return EFAULT;
    }
    0
}
