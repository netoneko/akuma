use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use spinning_top::Spinlock;

use crate::runtime::{config, with_irqs_disabled};
use akuma_terminal as terminal;

/// Channel for streaming process output between threads
///
/// Used to pass output from a process running on a user thread
/// to the async shell that spawned it.
pub struct ProcessChannel {
    /// Output buffer (spinlock-protected for thread safety)
    buffer: Spinlock<VecDeque<u8>>,
    /// Stdin buffer for interactive input (SSH -> process)
    stdin_buffer: Spinlock<VecDeque<u8>>,
    /// Exit code (set when process exits)
    exit_code: AtomicI32,
    /// Whether the process has exited
    exited: AtomicBool,
    /// Interrupt signal (set by Ctrl+C, checked by process)
    interrupted: AtomicBool,
    /// Raw mode flag (true if terminal is in raw mode, false for cooked)
    raw_mode: AtomicBool,
    /// Stdin closed flag (true if no more data will be written to stdin)
    stdin_closed: AtomicBool,
    /// Whether this channel's stdin/stdout is a real terminal. `true` for
    /// console/boot processes; set `false` for channel-fed spawned children
    /// (their fd 0 is a pipe, not a tty) so `isatty()` reports false and shells
    /// like busybox run non-interactively instead of starting a line editor
    /// that hangs querying an absent terminal (ESC[6n).
    is_terminal: AtomicBool,
    /// Threads waiting for output (epoll, blocking read)
    /// tid -> generation-tagged wake handle, minted at registration (the waiter
    /// registers itself, so the handle is live by construction). Keyed by tid for
    /// dedup and `is_poller_registered`; the handle is what gets woken, so an
    /// entry left behind by a dead poller wakes nobody instead of whoever owns
    /// the recycled slot (see threading::SLOT_GEN).
    pollers: Spinlock<BTreeMap<usize, crate::threading::WakeHandle>>,
}

/// Maximum size for process channel buffers to prevent memory exhaustion (1 MB)
const MAX_BUFFER_SIZE: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Shared FIFO bodies
//
// `buffer` (stdout) and `stdin_buffer` (stdin) are two independent spinlocks on
// one `ProcessChannel`, and the two directions grew as copies of each other —
// which is how `write_bounded`'s short-write fix reached only the stdout half
// (`docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §6). These helpers
// are the single body for both directions, so the next fix lands once.
//
// Each takes exactly **one** `&Spinlock<VecDeque<u8>>`, so the shape that nests
// the two FIFO locks is not expressible through this API. Only
// `check_set_writer` nests locks at all (`buffer` then `pollers`, deliberately,
// to close its TOCTOU window) — see `wake_pollers` for why every `pollers`
// access runs with IRQs disabled.
//
// None of these wake or register anything: the poller policy is genuinely
// different per direction (see `read_stdin`) and stays with the caller.
// ---------------------------------------------------------------------------

/// Append as much of `data` as fits under `MAX_BUFFER_SIZE` and return the
/// number of bytes accepted (`0` means full). Never drops already-buffered
/// bytes to make room: on a byte-faithful stream, drop-oldest silently deletes
/// the middle of what the reader is about to consume, so the contract is a
/// short write plus a caller retry (mirrors `pipe_write`).
///
/// `data` must be kernel memory — see [`fifo_push_drop_oldest`].
fn fifo_push_bounded(fifo: &Spinlock<VecDeque<u8>>, data: &[u8]) -> usize {
    with_irqs_disabled(|| {
        let mut buf = fifo.lock();
        let available = MAX_BUFFER_SIZE.saturating_sub(buf.len());
        let n = data.len().min(available);
        buf.extend(&data[..n]);
        n
    })
}

/// Append `data`, evicting the oldest bytes when it does not fit. Scrollback
/// semantics: correct only where losing the *start* of the stream is
/// acceptable, i.e. a terminal. Anything byte-faithful wants
/// [`fifo_push_bounded`] instead.
///
/// `data` must be kernel memory. The copy happens under a spinlock with IRQs
/// disabled, so a data abort on the *source* would be taken inside that
/// critical section — with no way to service it. Every caller already copies
/// out of userspace first (`sys_write`'s `kernel_buf`, the terminal layer's
/// owned translation buffer, `ioctl`'s literals).
fn fifo_push_drop_oldest(fifo: &Spinlock<VecDeque<u8>>, data: &[u8]) {
    with_irqs_disabled(|| {
        let mut buf = fifo.lock();

        if buf.len() + data.len() > MAX_BUFFER_SIZE {
            // If the write itself is larger than the buffer, keep only its tail.
            let data_to_write = if data.len() > MAX_BUFFER_SIZE {
                &data[data.len() - MAX_BUFFER_SIZE..]
            } else {
                data
            };

            // Remove old data to make room
            let current_len = buf.len();
            let overflow = (current_len + data_to_write.len()).saturating_sub(MAX_BUFFER_SIZE);
            if overflow > 0 {
                buf.drain(..overflow.min(current_len));
            }
            buf.extend(data_to_write);
        } else {
            buf.extend(data);
        }
    });
}

/// Drain up to `out.len()` bytes into `out` and return the count.
fn fifo_drain_into(fifo: &Spinlock<VecDeque<u8>>, out: &mut [u8]) -> usize {
    with_irqs_disabled(|| {
        let mut buf = fifo.lock();
        let to_read = out.len().min(buf.len());
        for (i, byte) in buf.drain(..to_read).enumerate() {
            out[i] = byte;
        }
        to_read
    })
}

/// Drain everything buffered, returning it (empty if there was nothing).
fn fifo_drain_all(fifo: &Spinlock<VecDeque<u8>>) -> Vec<u8> {
    with_irqs_disabled(|| fifo.lock().drain(..).collect())
}

/// Number of bytes currently buffered.
fn fifo_len(fifo: &Spinlock<VecDeque<u8>>) -> usize {
    with_irqs_disabled(|| fifo.lock().len())
}

/// Render up to 32 leading bytes of `data` into `out` as printable ASCII,
/// substituting `.` for anything else, and return how many bytes were written.
///
/// Split out from [`trace_transfer`] so the formatting is a pure function: it
/// takes no globals, so tests can drive it over arbitrary input without
/// standing up a config, and `trace_transfer` is left holding only the
/// should-I-trace decision.
fn trace_snippet(data: &[u8], out: &mut [u8; 32]) -> usize {
    let len = data.len().min(out.len());
    out[..len].copy_from_slice(&data[..len]);
    for byte in &mut out[..len] {
        if !byte.is_ascii_graphic() && *byte != b' ' {
            *byte = b'.';
        }
    }
    len
}

/// Trace a channel transfer, with a printable snippet of the payload.
///
/// Callers must invoke this with the FIFO lock **released**. Until 2026-08-13
/// the stdout read trace ran inside the locked, IRQs-disabled region; it was
/// harmless only because it used `log::debug!` and this tree never registers a
/// `log` logger, so all 68 `log::*` sites in the kernel are no-ops. Emitting a
/// real console write from there is the freeze shape `wake_pollers` documents.
fn trace_transfer(op: &str, n: usize, data: &[u8]) {
    if n == 0 || !config().syscall_debug_info_enabled {
        return;
    }
    let mut snippet = [0u8; 32];
    let len = trace_snippet(&data[..n.min(data.len())], &mut snippet);
    let text = core::str::from_utf8(&snippet[..len]).unwrap_or("...");
    crate::safe_print!(128, "[ProcessChannel] {} {} bytes \"{}\"\n", op, n, text);
}

impl ProcessChannel {
    /// Create a new empty process channel
    pub fn new() -> Self {
        Self {
            buffer: Spinlock::new(VecDeque::new()),
            stdin_buffer: Spinlock::new(VecDeque::new()),
            exit_code: AtomicI32::new(0),
            exited: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            raw_mode: AtomicBool::new(false),
            stdin_closed: AtomicBool::new(false),
            is_terminal: AtomicBool::new(true),
            pollers: Spinlock::new(BTreeMap::new()),
        }
    }

    /// Mark whether this channel is backed by a real terminal. Spawned children
    /// (pipe-fed stdin) call this with `false` so `isatty()` reports false.
    pub fn set_terminal(&self, is_terminal: bool) {
        self.is_terminal.store(is_terminal, Ordering::Release);
    }

    /// Whether this channel is backed by a real terminal (vs a pipe).
    pub fn is_terminal(&self) -> bool {
        self.is_terminal.load(Ordering::Acquire)
    }

    /// Mark stdin as closed (no more data will be arriving)
    pub fn close_stdin(&self) {
        self.stdin_closed.store(true, Ordering::Release);
    }

    /// Check if stdin is closed
    pub fn is_stdin_closed(&self) -> bool {
        self.stdin_closed.load(Ordering::Acquire)
    }

    /// Write data to the channel buffer (stdout from process), dropping the
    /// oldest buffered bytes if it does not fit. Terminal scrollback semantics
    /// — see [`Self::write_bounded`] for the byte-faithful variant that
    /// exec-channel children use instead.
    ///
    /// `data` must be kernel memory (see [`fifo_push_drop_oldest`]). Until
    /// 2026-08-13 this method defensively re-copied `data` into a fresh `Vec`
    /// before the critical section; the allocation was redundant — every
    /// caller already hands over kernel memory, and `write_bounded` takes the
    /// very same slice at `syscall/fs.rs` with no copy at all. The invariant is
    /// now stated at the boundary rather than paid for per write.
    pub fn write(&self, data: &[u8]) {
        if data.is_empty() { return; }

        fifo_push_drop_oldest(&self.buffer, data);
        trace_transfer("write stdout", data.len(), data);

        self.wake_pollers();
    }

    /// Write as much of `data` as fits under `MAX_BUFFER_SIZE` and return the
    /// number of bytes accepted (`0` means the buffer is full). Unlike `write`,
    /// this never drops already-buffered bytes to make room — callers that hit
    /// `0` must block (see `check_set_writer`) and retry, mirroring
    /// `pipe_write`'s short-write contract. For exec-channel (non-terminal)
    /// children, where drop-oldest silently corrupts a byte-faithful stdout
    /// stream — see `EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md`. Terminal
    /// channels keep using `write`'s drop-oldest scrollback behaviour.
    pub fn write_bounded(&self, data: &[u8]) -> usize {
        if data.is_empty() { return 0; }

        let n = fifo_push_bounded(&self.buffer, data);
        trace_transfer("write stdout", n, data);

        if n > 0 {
            self.wake_pollers();
        }
        n
    }

    /// Atomically check whether `write_bounded` has room, and if not, register
    /// `tid` as a blocked writer so it is woken once a reader drains the
    /// buffer. Returns `true` if the caller should NOT block. Checking and
    /// registering happen under the same `buffer` lock so a concurrent drain
    /// can never land in the gap between "found full" and "registered" and
    /// leave the writer parked with nobody left to wake it — mirrors
    /// `pipe_check_set_writer`'s TOCTOU fix.
    pub fn check_set_writer(&self, tid: usize) -> bool {
        with_irqs_disabled(|| {
            let buf = self.buffer.lock();
            if buf.len() < MAX_BUFFER_SIZE {
                return true;
            }
            self.pollers.lock().insert(tid, crate::threading::wake_handle_for_thread(tid));
            false
        })
    }

    /// Wake every thread registered in `pollers` (blocked readers waiting for
    /// data, or blocked writers waiting for space) and clear the set.
    ///
    /// Always under `with_irqs_disabled`, like every other `pollers` access
    /// (`add_poller`, `check_set_writer`, `is_poller_registered`) — `pollers`
    /// and `buffer` are two independent spinlocks, and `check_set_writer`
    /// locks both together (nested) to close its TOCTOU window. If any other
    /// site locked `pollers` with IRQs enabled, a timer tick could preempt it
    /// mid-hold and switch to a thread spinning on `pollers` with IRQs
    /// *disabled* (inside `check_set_writer`'s `with_irqs_disabled`): that
    /// spinner can never be preempted back off, so the original holder never
    /// resumes to release the lock — a permanent, silent freeze (no panic, no
    /// log, the timer tick itself stops firing). Reproduced empirically
    /// 2026-08-01 while adding exec-channel backpressure: `add_poller` was
    /// unprotected and got hammered by sshd's tight non-blocking read loop
    /// racing a blocked writer's `check_set_writer` under heavy throughput.
    fn wake_pollers(&self) {
        with_irqs_disabled(|| {
            let mut pollers = self.pollers.lock();
            while let Some((_tid, handle)) = pollers.pop_first() {
                crate::threading::wake_by_handle(handle);
            }
        });
    }

    /// Read available data from the channel (non-blocking)
    /// Returns None if no data is available
    pub fn try_read(&self) -> Option<Vec<u8>> {
        let data = fifo_drain_all(&self.buffer);
        if data.is_empty() {
            return None;
        }
        trace_transfer("read stdout", data.len(), &data);
        // Draining frees space for any writer parked in `check_set_writer`.
        self.wake_pollers();
        Some(data)
    }

    /// Read available data from the channel into a buffer
    /// Returns number of bytes read
    pub fn read(&self, buf: &mut [u8]) -> usize {
        let n = fifo_drain_into(&self.buffer, buf);
        trace_transfer("read stdout", n, buf);

        if n == 0 {
            // Register current thread as reader so it can be woken by next write
            self.add_poller(crate::threading::current_thread_id());
        } else {
            // Draining frees space for any writer parked in `check_set_writer`.
            self.wake_pollers();
        }
        n
    }

    /// Register a thread to be woken when new data arrives or the process exits.
    pub fn add_poller(&self, tid: usize) {
        with_irqs_disabled(|| {
            self.pollers.lock().insert(tid, crate::threading::wake_handle_for_thread(tid));
        });
    }

    /// Check if a thread is registered as a poller (test helper).
    pub fn is_poller_registered(&self, tid: usize) -> bool {
        with_irqs_disabled(|| {
            self.pollers.lock().contains_key(&tid)
        })
    }

    pub fn has_stdout_data(&self) -> bool {
        fifo_len(&self.buffer) > 0
    }

    /// Read all remaining data from the channel
    pub fn read_all(&self) -> Vec<u8> {
        let data = fifo_drain_all(&self.buffer);

        if !data.is_empty() {
            trace_transfer("read stdout", data.len(), &data);
            // Draining frees space for any writer parked in `check_set_writer`.
            self.wake_pollers();
        }
        data
    }

    /// Write as much of `data` into the stdin buffer as fits under
    /// `MAX_BUFFER_SIZE`, and return the number of bytes accepted (`0` means the
    /// buffer is full). Never drops already-buffered bytes to make room.
    ///
    /// This is the stdin counterpart of [`Self::write_bounded`], and it exists for
    /// the same reason: stdin is a byte-faithful stream, so drop-oldest on
    /// overflow silently deletes the middle of the input a process is about to
    /// read. Until 2026-08-13 only the stdout half had been fixed — the
    /// exec-channel truncation work
    /// (`userspace/sshd/docs/EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md`) landed
    /// `write_bounded` on the stdout copy and left this drop-oldest copy alone,
    /// the canonical copy-paste outcome documented in
    /// `docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §6. It is
    /// reachable: sshd forwards client input here through `/proc/<pid>/fd/0`, so
    /// any client piping more than `MAX_BUFFER_SIZE` past a slow-reading child
    /// (`ssh host 'cat > f' < big`) outran the drain.
    ///
    /// Unlike the stdout side there is deliberately NO `check_set_writer`
    /// equivalent to park a blocked writer on: the only in-tree writer is sshd's
    /// `bridge_process`, a single loop that must keep draining the child's
    /// *stdout* in the same iteration. Blocking it here would recreate exactly
    /// the deadlock its own "make BOTH ends non-blocking" comment describes.
    /// Short write plus a userspace retry is the deadlock-free shape.
    pub fn write_stdin(&self, data: &[u8]) -> usize {
        if data.is_empty() {
            return 0;
        }

        let n = fifo_push_bounded(&self.stdin_buffer, data);
        trace_transfer("write stdin", n, data);

        // Wake any thread blocked in poll/ppoll on its stdin (e.g. busybox sh over
        // sshd: write_to_process_stdin fills this buffer, but a process sleeping in
        // ppoll(fd 0) only wakes when a registered poller is notified). Without this
        // the shell never sees typed input. Mirrors the stdout `write` wake path.
        if n > 0 {
            self.wake_pollers();
        }
        n
    }

    /// Read from stdin buffer (process reads from SSH input)
    /// Returns number of bytes read into buf
    ///
    /// Same body as [`Self::read`], **deliberately different poller policy** —
    /// this asymmetry is real, not drift:
    ///
    /// * No `add_poller` on an empty read. The stdin readers
    ///   (`syscall/fs.rs`'s `Stdin` arm, `syscall/term.rs`) run their own
    ///   blocking loops and register through `poll`/`ppoll`; `read`'s
    ///   self-registration exists for the stdout consumers that do not.
    /// * No `wake_pollers` after draining. On the stdout side that wake exists
    ///   to release a writer parked in `check_set_writer`; stdin has no such
    ///   writer by design (see [`Self::write_stdin`]), and `pollers` is a
    ///   single set shared by both directions, so waking here would only spin
    ///   stdout waiters that gained nothing.
    pub fn read_stdin(&self, buf: &mut [u8]) -> usize {
        let n = fifo_drain_into(&self.stdin_buffer, buf);
        trace_transfer("read stdin", n, buf);
        n
    }

    /// Check if stdin has data available
    pub fn has_stdin_data(&self) -> bool {
        fifo_len(&self.stdin_buffer) > 0
    }

    /// Return the number of bytes available in the stdin buffer
    pub fn stdin_bytes_available(&self) -> usize {
        fifo_len(&self.stdin_buffer)
    }

    /// Clear all pending data from the stdin buffer
    pub fn flush_stdin(&self) {
        with_irqs_disabled(|| {
            self.stdin_buffer.lock().clear();
        })
    }

    /// Mark the process as exited with the given exit code
    pub fn set_exited(&self, code: i32) {
        self.exit_code.store(code, Ordering::Release);
        self.exited.store(true, Ordering::Release);

        // Wake all pollers waiting for output (EOF/exit is an event)
        self.wake_pollers();
    }

    /// Check if the process has exited
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// Get the exit code (only valid after has_exited() returns true)
    pub fn exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::Acquire)
    }

    /// Set the interrupt flag (called when Ctrl+C is pressed)
    pub fn set_interrupted(&self) {
        self.interrupted.store(true, Ordering::Release);
    }

    /// Check if the process has been interrupted (auto-clears the flag).
    /// This ensures blocking syscalls see EINTR exactly once per signal,
    /// not on every subsequent call.
    pub fn is_interrupted(&self) -> bool {
        self.interrupted.swap(false, Ordering::AcqRel)
    }

    /// Clear the interrupt flag
    pub fn clear_interrupted(&self) {
        self.interrupted.store(false, Ordering::Release);
    }

    /// Set the raw mode flag
    pub fn set_raw_mode(&self, enabled: bool) {
        self.raw_mode.store(enabled, Ordering::Release);
    }

    /// Check if raw mode is enabled
    pub fn is_raw_mode(&self) -> bool {
        self.raw_mode.load(Ordering::Acquire)
    }
}

impl Default for ProcessChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Global registry mapping thread IDs to their process channels
static PROCESS_CHANNELS: Spinlock<BTreeMap<usize, Arc<ProcessChannel>>> =
    Spinlock::new(BTreeMap::new());

/// Global registry mapping thread IDs to their shared terminal states
static TERMINAL_STATES: Spinlock<BTreeMap<usize, Arc<Spinlock<terminal::TerminalState>>>> =
    Spinlock::new(BTreeMap::new());

/// Per-thread registry accessors. Both statics above are the same shape
/// (`tid -> Arc<_>`) and their six accessors were the same three bodies twice
/// over; these are the single copy. Like the FIFO helpers, each takes exactly
/// one registry, and each runs under `with_irqs_disabled` — a timer tick inside
/// the hold would let a spinner that *does* have IRQs off wedge the holder off
/// the CPU permanently (the freeze reproduced in `wake_pollers`).
fn registry_insert<T>(reg: &Spinlock<BTreeMap<usize, T>>, thread_id: usize, value: T) {
    with_irqs_disabled(|| {
        reg.lock().insert(thread_id, value);
    });
}

fn registry_get<T: Clone>(reg: &Spinlock<BTreeMap<usize, T>>, thread_id: usize) -> Option<T> {
    with_irqs_disabled(|| reg.lock().get(&thread_id).cloned())
}

fn registry_remove<T>(reg: &Spinlock<BTreeMap<usize, T>>, thread_id: usize) -> Option<T> {
    with_irqs_disabled(|| reg.lock().remove(&thread_id))
}

/// Register a process channel for a thread
pub fn register_channel(thread_id: usize, channel: Arc<ProcessChannel>) {
    registry_insert(&PROCESS_CHANNELS, thread_id, channel);
}

/// Register a terminal state for a thread
pub fn register_terminal_state(thread_id: usize, state: Arc<Spinlock<terminal::TerminalState>>) {
    registry_insert(&TERMINAL_STATES, thread_id, state);
}

/// Register a process channel for a system thread (one that doesn't have a
/// Process struct). Kept as a distinct name because it documents the caller's
/// situation; the registry does not distinguish the two, and this was already a
/// byte-identical copy of [`register_channel`].
pub fn register_system_thread_channel(thread_id: usize, channel: Arc<ProcessChannel>) {
    register_channel(thread_id, channel);
}

/// Get the process channel for a thread (if any)
pub fn get_channel(thread_id: usize) -> Option<Arc<ProcessChannel>> {
    registry_get(&PROCESS_CHANNELS, thread_id)
}

/// Get the terminal state for a thread (if any)
pub fn get_terminal_state(thread_id: usize) -> Option<Arc<Spinlock<terminal::TerminalState>>> {
    registry_get(&TERMINAL_STATES, thread_id)
}

/// Remove and return the process channel for a thread
pub fn remove_channel(thread_id: usize) -> Option<Arc<ProcessChannel>> {
    registry_remove(&PROCESS_CHANNELS, thread_id)
}

/// Remove and return the terminal state for a thread
pub fn remove_terminal_state(thread_id: usize) -> Option<Arc<Spinlock<terminal::TerminalState>>> {
    registry_remove(&TERMINAL_STATES, thread_id)
}

/// These run on the **host**, which the FIFO half of `ProcessChannel` reaches
/// because everything under it already has a host build: `with_irqs_disabled`
/// is a no-op outside `target_os = "none"`, `current_thread_id` is
/// `akuma_primitives::preempt::current_tid`, and the wake path is atomic-array
/// bookkeeping over `SLOT_GEN`/`THREAD_STATES` — no context switch is involved
/// in *registering* or *signalling* a waiter, only in the waiter's own
/// `schedule_blocking`, which none of these call.
///
/// The one thing that genuinely was missing is the config, and the fix is to
/// **inject it** ([`runtime::register_config_for_test`]) rather than teach
/// production code to run without it. Its `syscall_debug_info_enabled` is
/// `true`, so these tests execute the tracing path instead of skipping it.
///
/// What is therefore **not** covered here, and needs the boot suite: that a
/// woken thread actually runs. `test_process_channel_write_bounded_backpressure`
/// in `src/process_tests.rs` is the in-VM half.
#[cfg(test)]
mod tests {
    use super::*;

    /// Idempotent (`OnceCopy::set` no-ops when already set), so every test calls
    /// it unconditionally — `cargo test` runs these in parallel threads of one
    /// process and there is no ordering to rely on.
    fn setup() {
        crate::runtime::register_config_for_test();
    }

    /// The single most likely defect this merge could introduce: a shared FIFO
    /// helper handed the wrong lock. Each direction must see only its own bytes.
    #[test]
    fn stdin_and_stdout_fifos_stay_independent() {
        setup();
        let ch = ProcessChannel::new();

        assert_eq!(ch.write_stdin(b"STDIN-SIDE"), 10);
        assert!(!ch.has_stdout_data(), "stdin write leaked into the stdout FIFO");
        assert_eq!(ch.stdin_bytes_available(), 10);

        assert_eq!(ch.write_bounded(b"STDOUT-SIDE!"), 12);
        assert!(ch.has_stdout_data() && ch.has_stdin_data());
        assert_eq!(ch.stdin_bytes_available(), 10, "stdout write landed in stdin");

        let mut out = [0u8; 32];
        assert_eq!(ch.read_stdin(&mut out), 10);
        assert_eq!(&out[..10], b"STDIN-SIDE");
        assert!(!ch.has_stdin_data() && ch.has_stdout_data());

        let mut out = [0u8; 32];
        assert_eq!(ch.read(&mut out), 12);
        assert_eq!(&out[..12], b"STDOUT-SIDE!");
        assert!(!ch.has_stdout_data());
    }

    /// Both bounded writers cap at `MAX_BUFFER_SIZE` and short-write rather than
    /// evicting. The stdin half is the regression test for §6 of
    /// `TRIM_FAT_EMBARASSING_DUPLICATIONS.md`: drop-oldest silently deletes
    /// the middle of a byte-faithful stream, and this fix reached only the
    /// stdout copy for as long as the two bodies were separate.
    #[test]
    fn bounded_writers_short_write_and_keep_the_head() {
        setup();
        for stdin_side in [false, true] {
            let ch = ProcessChannel::new();
            let chunk = alloc::vec![0x42u8; 64 * 1024];
            let push = |d: &[u8]| if stdin_side { ch.write_stdin(d) } else { ch.write_bounded(d) };

            let mut total = 0usize;
            while let n = push(&chunk)
                && n > 0
            {
                total += n;
            }
            assert_eq!(total, MAX_BUFFER_SIZE, "stdin_side={stdin_side}");
            assert_eq!(push(b"overflow"), 0, "accepted past the cap");

            // Drop-oldest would have evicted this head to make room.
            let mut head = [0u8; 8];
            let n = if stdin_side {
                ch.read_stdin(&mut head)
            } else {
                ch.read(&mut head)
            };
            assert_eq!(n, 8);
            assert!(head.iter().all(|b| *b == 0x42), "head of the stream was evicted");
        }
    }

    /// `write` keeps terminal scrollback semantics: it is the one FIFO push that
    /// may evict, and it evicts exactly the overflow.
    #[test]
    fn terminal_write_drops_oldest() {
        setup();
        let ch = ProcessChannel::new();
        ch.write(b"OLDEST");

        let chunk = alloc::vec![0x43u8; 64 * 1024];
        for _ in 0..16 {
            ch.write(&chunk);
        }

        // 6 + 16 * 64 KiB is 6 bytes over the cap, so exactly "OLDEST" goes.
        let buffered = ch.read_all();
        assert_eq!(buffered.len(), MAX_BUFFER_SIZE);
        assert!(buffered.iter().all(|b| *b == 0x43));
    }

    /// A single `write` larger than the whole buffer keeps its **tail**, which
    /// is what scrollback means. Previously buried in the copy that also made a
    /// redundant `Vec` of the payload first.
    #[test]
    fn terminal_write_larger_than_buffer_keeps_the_tail() {
        setup();
        let ch = ProcessChannel::new();
        let mut oversized = alloc::vec![0x44u8; MAX_BUFFER_SIZE + 16];
        oversized[MAX_BUFFER_SIZE + 15] = 0x45;

        ch.write(&oversized);

        let buffered = ch.read_all();
        assert_eq!(buffered.len(), MAX_BUFFER_SIZE);
        assert_eq!(*buffered.last().unwrap(), 0x45, "kept the head instead of the tail");
    }

    /// The poller policy is deliberately asymmetric and must stay that way —
    /// see [`ProcessChannel::read_stdin`]. If a future edit "tidies" the two
    /// directions into one, this is what catches it.
    #[test]
    fn stdin_drain_neither_registers_nor_wakes_pollers() {
        setup();
        let ch = ProcessChannel::new();
        let tid = crate::threading::current_thread_id();
        let mut out = [0u8; 16];

        // An empty stdin read must not self-register the reader...
        assert_eq!(ch.read_stdin(&mut out), 0);
        assert!(!ch.is_poller_registered(tid));

        // ...and a stdin drain must not wake stdout waiters, who gained nothing.
        ch.write_stdin(b"data");
        ch.add_poller(tid);
        assert_eq!(ch.read_stdin(&mut out), 4);
        assert!(ch.is_poller_registered(tid), "stdin drain woke a stdout waiter");

        // The stdout side does both.
        assert_eq!(ch.read(&mut out), 0);
        assert!(ch.is_poller_registered(tid), "empty stdout read must self-register");
        ch.write_bounded(b"x");
        assert!(!ch.is_poller_registered(tid), "stdout write must wake and clear");
    }

    /// `check_set_writer` is the only site that nests the two locks it touches
    /// (`buffer` then `pollers`); the FIFO helpers cannot express that shape.
    /// Pin its TOCTOU contract: full → park, drained → proceed.
    #[test]
    fn check_set_writer_parks_only_at_capacity() {
        setup();
        let ch = ProcessChannel::new();
        let tid = crate::threading::current_thread_id();

        assert!(ch.check_set_writer(tid), "empty buffer must not park a writer");
        assert!(!ch.is_poller_registered(tid));

        let chunk = alloc::vec![0u8; 64 * 1024];
        while ch.write_bounded(&chunk) > 0 {}

        assert!(!ch.check_set_writer(tid), "at capacity the writer must park");
        assert!(ch.is_poller_registered(tid));

        let mut sink = alloc::vec![0u8; 4096];
        assert_eq!(ch.read(&mut sink), 4096);
        assert!(!ch.is_poller_registered(tid), "the drain must wake the parked writer");
        assert!(ch.check_set_writer(tid));
    }

    /// The trace formatter, driven directly — no config, no channel. This is
    /// the branch the previous shape could not reach at all: guarding
    /// `trace_transfer` on "is anything registered?" made every host test skip
    /// the trace instead of running it.
    #[test]
    fn trace_snippet_scrubs_and_truncates() {
        let mut out = [0u8; 32];

        assert_eq!(trace_snippet(b"hello world", &mut out), 11);
        assert_eq!(&out[..11], b"hello world", "space and graphic ASCII pass through");

        // Control bytes, DEL and high bytes all become '.'; length is unchanged.
        assert_eq!(trace_snippet(b"a\nb\tc\x00d\x7fe\xfff", &mut out), 11);
        assert_eq!(&out[..11], b"a.b.c.d.e.f");

        // Truncates to the buffer, never panics on oversized input.
        let long = alloc::vec![b'z'; 4096];
        assert_eq!(trace_snippet(&long, &mut out), 32);
        assert!(out.iter().all(|b| *b == b'z'));

        assert_eq!(trace_snippet(b"", &mut out), 0);

        // Every output byte must be valid UTF-8, because `trace_transfer` feeds
        // the result to `from_utf8` and would otherwise print "..." for any
        // payload containing a stray high byte.
        assert_eq!(trace_snippet(b"\xc3\x28\xf0\x9f", &mut out), 4);
        assert!(core::str::from_utf8(&out[..4]).is_ok());
    }

    /// Both registries are one set of generic accessors now; `get` must clone
    /// rather than remove, and `register_system_thread_channel` is the same
    /// registry as `register_channel` (it always was — byte-identical bodies).
    #[test]
    fn registries_insert_get_and_remove() {
        setup();
        let ch = Arc::new(ProcessChannel::new());
        let tid = 0xC0FFEE;

        assert!(get_channel(tid).is_none());
        register_system_thread_channel(tid, ch.clone());
        assert!(Arc::ptr_eq(&get_channel(tid).unwrap(), &ch));
        assert!(get_channel(tid).is_some(), "get must not consume the entry");
        assert!(Arc::ptr_eq(&remove_channel(tid).unwrap(), &ch));
        assert!(get_channel(tid).is_none());

        let state = Arc::new(Spinlock::new(terminal::TerminalState::default()));
        assert!(get_terminal_state(tid).is_none());
        register_terminal_state(tid, state.clone());
        assert!(Arc::ptr_eq(&get_terminal_state(tid).unwrap(), &state));
        assert!(remove_terminal_state(tid).is_some());
        assert!(get_terminal_state(tid).is_none());
    }
}
