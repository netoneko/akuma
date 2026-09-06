# `akuma-pipes`: one pipe table for two kernels

**Status, 2026-09-07: both kernels consume it.** The crate was written and tested
2026-09-06; the wiring pass landed the next day and is recorded in § "What was
wired" below. This document is the finding, the design and the reason the design
is shaped the way it is.

## What was measured

Both kernels had a pipe implementation. They agreed on the 64 KiB cap and on
nothing else.

| | aarch64 | amd64 |
|---|---|---|
| where | `crates/akuma-syscalls-glue/src/pipe.rs`, 499 lines | `amd64/src/pipe.rs`, 175 lines |
| buffer | its own `VecDeque<u8>` + `PIPE_CAPACITY: usize = 65536` | `akuma_pipe::Pipe` (`DEFAULT_CAPACITY = 64 * 1024`) |
| ends | `read_count` **and** `write_count`, both refcounted | one `write_closed` flag |
| waiters | `pollers: BTreeMap<tid, WakeHandle>`, woken on every event | none — callers poll |
| destroy | when both counts reach 0 | explicit `free()`, by the allocator |
| capacity | 64 KiB | 64 KiB |
| table size | unbounded `BTreeMap` | fixed `[Slot; 16]` |

The first row is the one worth pausing on: **`akuma-pipe` — the host-tested leaf
crate that exists precisely to hold these semantics — is used by amd64 only.**
The aarch64 side never adopted it and grew a parallel `VecDeque` with the same
constant written out again. Two implementations of one set of rules, one of them
host-tested and in the wrong kernel.

The consequence is not symmetric. amd64's version is missing every rule the
aarch64 one learned the hard way, and each of those rules exists because of a
specific failure recorded in the source:

- **End reference counts.** A `fork` or `dup` of a pipe end must not make the
  pipe closable by the first `close`. amd64 had no counts at all, so this was
  simply not expressible.
- **EOF is an event.** Losing the last writer has to wake blocked readers.
- **Losing the last *reader* is an event too** — the mirror, and the one that is
  easy to miss. A writer parked on a full buffer can only learn the pipe broke
  by retrying and seeing `read_count == 0`; without the wake it never retries,
  never gets `EPIPE`, never gets `SIGPIPE`, and sleeps forever. Invisible until
  pipes were capped, because an uncapped pipe never blocked a writer.
  `busybox yes | busybox head -n 1` hits it every time.
- **Check-and-register is one step.** The two-step spelling (`read` → empty →
  `add_poller` → sleep) has a TOCTOU window: a write landing between them fires
  its wake with no waiter registered, and the caller sleeps through the event it
  was waiting for.

## The design: wakes are returned, never performed

`PipeTable<W>` is generic over the caller's wake token, and every operation that
can make a waiter runnable hands back a `Wakes<W>` instead of firing it.

That is what lets one implementation serve two kernels — aarch64 fires
`akuma_threading::wake_by_handle`, amd64 fires nothing — but portability is the
lesser reason. **It is the shape the aarch64 code arrived at by debugging**, and
returning the effects makes "act outside the locked section" the only thing a
caller *can* do, rather than a rule each caller has to remember:

- Raising `SIGPIPE` inside the pipe lock self-deadlocked a core. A default
  disposition runs the terminate action inline, which reaches `close_all` →
  `pipe_close_write` → the same lock, IRQs masked, BKL still held. Root-caused
  live 2026-07-24 (`aria2c ... | head -1`).
- Destroying a pipe detaches its AF_UNIX channel, which takes the socket-table
  lock. Taking that while the pipe table is held is the mirror inversion.

`Wakes<W>` is produced with `core::mem::take` on the poller map, so it allocates
nothing: the table hands its allocation to the caller and keeps a fresh empty
map.

### One waiter set, not a reader set and a writer set

`pollers` holds every thread interested in the pipe for any reason, and each
event drains **all** of them so each can re-test its own condition. Splitting the
set would mean deciding at *registration* time which event a waiter cares about,
and `pipe_write_all_blocking` cares about both.

## What exists now

`crates/akuma-pipes` — 389 lines plus 277 of tests.

- `[dependencies]`: `akuma-pipe` only. Not `akuma-threading`; see above.
- `#![no_std]`, `#![forbid(unsafe_code)]`.
- Builds for the host and for `x86_64-unknown-none`. 22 host tests, 0 clippy
  warnings.
- API: `create`, `clone_ref`, `write`, `read`, `close_write`, `close_read`,
  `check_set_reader`, `check_set_writer`, `add_poller`, `poller_count`,
  `is_poller_registered`, `readable`, `writable`, `counts`, `buffered`,
  `live_count`, `iter`, `poller_tids`.
- Outcome types are explicit rather than tuples: `WriteOutcome::{Wrote,
  BrokenPipe, NoSuchPipe}` (a missing pipe must **not** raise `SIGPIPE`; only a
  reader-less one does), `ReadResult { bytes, eof }`, `CloseResult { destroyed }`.

The tests are the reason the extraction is worth anything — none of them could be
written while the table lived in `akuma-syscalls-glue`, which does not build off
the target. Each pins a rule, and the names say which:
`losing_the_last_reader_wakes_blocked_writers`,
`a_write_that_accepted_nothing_wakes_nobody`,
`buffered_bytes_survive_the_last_writer`,
`check_set_reader_registers_only_when_it_would_block`,
`a_missing_pipe_reads_eof_and_writes_nosuchpipe`.

## What was wired (2026-09-07)

Both modules kept their public API, so their callers did not move.

- **aarch64** — `crates/akuma-syscalls-glue/src/pipe.rs` is now a shim over
  `PipeTable<WakeHandle>`, 253 lines of table logic replaced by 155 of effects.
  All **540** references outside the module are unchanged. What stayed is
  `wake_by_handle`, `send_sigpipe`, `unix_channel_detach`, the
  `PIPE_TRACE_ENABLED` prints, and one `with_table` helper holding the
  `irq::with_irqs_disabled` + `PIPES.lock()` discipline.

  One behaviour did change, and for the better: **wakes now fire outside the
  lock.** The old `pipe_write`/`pipe_read`/`pipe_close_*` drained `pollers` and
  called `wake_by_handle` *inside* the locked, IRQ-masked section. Returning the
  set is what makes firing it outside the only option a caller has.

- **amd64** — `amd64/src/pipe.rs` is a shim over `PipeTable<()>` whose `fire` is
  an empty function, called on every path that produces wakes so that giving
  that scheduler a blocked state (item 3) is a change to one body. The
  `Slot::ends` stopgap is gone, and with it `alloc_pair`/`drop_end`: a `pipe(2)`
  pair and a spawn pipe are now the same allocation, differing only in who
  closes the ends. `MAX_PIPES` stays at 64 as an explicit cap — the `BTreeMap`
  would otherwise grow without bound, and each entry is up to 64 KiB of kernel
  buffer allocated on a userspace request.

### Three real defects the wiring exposed on amd64

Each was unreachable before, because the state that expresses it did not exist:

1. **`write_pipe` span forever against a dead reader.** With one `write_closed`
   flag and no `read_count`, a full buffer and a reader-less pipe were the same
   observation — `Some(0)` — and `write_pipe`'s retry loop treats that as "try
   again". `busybox yes | busybox head -n 1` hangs the shell. `write` now
   answers `None` for a broken pipe and the loop returns `EPIPE` (added to
   `fd::errno`; this target has no signal machinery, so `EPIPE` is the whole of
   Linux's answer that applies). Pinned by a new boot check,
   `redirect: \`yes | head -n 1\` terminates`.
2. **A pipe freed under a live writer.** `release(PipeRead)` freed the slot
   outright for a spawn-owned pipe. Subsequent writes by the child landed in a
   cleared buffer and reported success. Now `close_read` drops one count and the
   pipe is destroyed only when both reach zero.
3. **`poll` reported POLLOUT on a pipe with no readers.** Left as-is
   deliberately — Linux answers the same shape (`POLLERR` on the write end,
   writable to `select`), and reporting "not ready" would turn a broken pipe
   into a permanent wait. The crate's `writable`/`readable` are taken verbatim
   here, which is a **deliberate divergence** from the AArch64 `pipe_can_read`'s
   existence guard; both sides carry the note.

### Two changes to the crate itself

- `read` no longer wakes on a zero-byte read. `Read(0)` is reachable only for an
  empty `out` (`read(fd, buf, 0)`): it drains nothing, so it makes no room and
  there is nothing for a parked writer to re-test — the same rule `write`
  already applied to `Wrote(0)`. It falls through to the EOF arm, which is what
  the AArch64 implementation did.
- `destroy(id)` was added, for a lifetime an allocator manages by hand: a
  spawned child's stdin pipe is read *by number* rather than through a
  descriptor, so its read end never closes and refcounting alone would never
  free it. `waitpid` is what knows the child is gone. A `pipe(2)` pair must not
  come through it.

27 host tests now, up from 22.

## Two findings about `akuma-threading`, from the same investigation

Recorded here because both were discovered while answering "why not just reuse
the aarch64 pipes", and both outlive this extraction.

### 1. It is already ported to x86_64 — but cooperatively only

`proposals/AKUMA_THREADING_ARCH_PORTABILITY.md` § Status: done 2026-09-05.
`Context` is `cfg(target_arch)`-gated and `spawn_fn` / `spawn_system_thread_fn` /
`yield_now` have boot-verified x86_64 arms.

What is **not** ported is the interrupt-driven switch, and that is exactly what
`schedule_blocking` needs:

```rust
voluntary_schedule_flag().store(true, Ordering::Release);
(runtime().trigger_sgi)(0);            // GIC software-generated interrupt
loop { ... akuma_cpu::park::wfi(); }   // wait to be preempted, then woken
```

The SGI handler is what switches a WAITING thread out. The proposal says that
machinery (`setup_fake_irq_frame`, `sgi_scheduler_handler_with_sp`) is
deliberately not wired for x86_64 — it builds an AArch64 fake-IRQ-return frame
with no x86 analogue. Independently, amd64's own picker has no blocked state:
`State` is `Unused | Reserved | Runnable | Finished`.

So real park/resume on amd64 is two pieces, not a rewrite: **a blocked state the
picker skips**, and **`schedule_blocking` routed through the cooperative x86
switch** instead of `trigger_sgi` + `wfi`. Tractable, because that switch is a
plain function-call switch that can switch out directly.

### 2. There are three context-switch paths, and two of them disagree

1. `akuma-threading`, AArch64 — SGI / fake-IRQ-frame.
2. `akuma-threading`, x86_64 — `akuma_threading_x86_switch_context`, cooperative,
   added 2026-09-05.
3. `amd64/src/sched.rs` — `switch_context`, cooperative. **The one amd64 runs.**

2 is a port of 3, and the port dropped a fix:

```
amd64/src/sched.rs:        pushfq   push rbp rbx r12-r15  ...  popfq  ret
akuma-threading (x86):              push rbp rbx r12-r15  ...         ret
```

`amd64/src/sched.rs`'s own comment says why the flags are saved: *"a syscall
(interrupts off) that yields to a kernel task (interrupts on) must come back with
them off, and it did not until Stage U — the resumer's state leaked into the
resumed."* The copy silently reintroduces that. **Fix `akuma-threading`'s switch
before anything adopts it**, or the scheduler fold inherits an interrupt-flag
leak that presents as an unrelated intermittent hang.

## Verification

**Use the RAM root and Firecracker.** Do not verify this against
`root=/dev/sda1`: that configuration has a separate, open, unrelated failure —
Akuma cannot read files off the ext2-on-USB root (`[SSH Keys] WARNING: cannot
read /etc/sshd/authorized_keys`, plus `ls`/`cat`/`dmesg` hanging), and mixing it
in makes every pipe result unreadable.

- `cargo test -p akuma-pipes --target <host>` — the rules.
- `amd64/run.sh` (QEMU/TCG, local) and `hpbox.firecracker(vcpus=1|4)` — the boot
  suite, which since 2026-09-06 includes `redirect_test`: `>`, `>>`, `cmd | cmd`
  and a 12-stage pipeline driven through the real `busybox ash`.
- aarch64: the existing `kernel_tests` for pipes, which are the oracle for the
  shim — `test_pipe_close_read_wakes_blocked_writer`,
  `run_pselect6_registers_waker_test`, `test_sigpipe_terminate_no_deadlock`.

## Background

- `proposals/AKUMA_THREADING_ARCH_PORTABILITY.md` — the x86_64 port of
  `akuma-threading`, its status, and what it deliberately left stubbed.
- `docs/archive/AKUMA_THREADING_X86_SWITCH.md` — the cooperative switch this
  document reports a missing `pushfq`/`popfq` in.
- `docs/archive/AKUMA_AMD64_STREAMLINING.md` §11.8 — `dup2`/`pipe2` reaching
  userspace, which is what made amd64's pipes carry real user traffic and
  exposed how much smaller its implementation was.
- `docs/reference/subsystems/syscalls/poll.md` — `akuma-net-yarn`, the
  established precedent in this tree for a crate that owns a state machine and
  lets its callers supply the effects.
