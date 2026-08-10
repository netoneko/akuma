# Console (UART) I/O

PL011 UART driver and the kernel's console output/input chokepoint. Source:
`src/console.rs`.

> **Stability: B (watch).** Dormant since March 2026 (`extract akuma-exec`).

## UART driver

`Uart` (`:19-57`) wraps the PL011 registers at `akuma_exec::mmu::DEV_UART_VA`
(remapped VA for physical `0x0900_0000`): `write`/`read` hit the data
register (`DR_OFFSET`), `has_data()` checks the flag register's `RXFE` bit.
One `static UART: Uart` (`:60`) instance; there is no lock around it.

## Output: the `emit()` chokepoint

Every print helper (`print`, `print_char`, `print_hex`, `print_dec`,
`print_u64`, `StackWriter::flush`) funnels through `emit(bytes: &[u8])`,
which writes the UART directly with IRQs disabled on the current core
(`irq::with_irqs_disabled`) so a timer preemption can't interleave two
*threads on the same core's* output mid-message.

> **Cross-core serialization (shipped 2026-08-11):** a `Spinlock<()>` +
> owner-core-ID reentrancy guard around the loop body is **default-on in
> `release`** (cfg `kernel_console_lock`, gated by `OPT_LEVEL != "z"` in
> `build.rs`). With it on, the whole per-call byte sequence is atomic
> across cores, and a panic / sync exception landing while this core
> already holds the lock takes the reentrant fast path instead of
> self-deadlocking. Off in size/extreme (single-core targets); opt out
> in `release` with `CONSOLE_LOCK=0`, force-on in size with
> `CONSOLE_LOCK=1`. Background and verification:
> `docs/archive/UART_SMP_INTERLEAVE_FIX.md`.

## Formatting without heap allocation

`print_hex`/`print_dec`/`print_u64` (`:107-165`) format directly into a
fixed-size stack buffer, no `alloc`. `StackWriter<const N: usize>`
(`:173-209`) implements `core::fmt::Write` over a stack buffer for panic-safe
`write!()`-style formatting; `safe_print!`/`tprint!` (`:219-241`, exported
macros) wrap it — `tprint!` additionally prepends a `[T<secs>.<cs>]` uptime
timestamp read from `crate::timer::uptime_us()`. These exist specifically so
kernel logging can't itself panic from an allocation failure while already
handling a fault.

## Printing rules (required)

**All kernel console output must go through `safe_print!` or `tprint!`.** The
heap must never be touched on a path that ends at the console. This is a hard
rule, not a style preference: the console is what you have left when the
allocator is the thing that broke, and a diagnostic that needs a healthy heap
to report allocator health is the wrong shape for the job. `ALLOC_PRINT_AUDIT.md`
found five violations of it, one of them inside the sync-EL1 crash handler,
where an `alloc::format!` one call-graph hop away could have re-entered the
TALC lock and hung the handler mid-dump.

Concretely:

- **Do** use `crate::safe_print!(N, "fmt\n", args…)` — or `tprint!` when you
  want the `[T<secs>.<cs>]` uptime stamp. Both exist in `src/` (`console.rs`)
  and in `crates/akuma-exec` (`threading/mod.rs`, `#[macro_export]`ed, so
  `crate::safe_print!` resolves from any module in that crate).
- **Don't** hand-roll a `struct Buf([u8; N], usize)` + `impl core::fmt::Write`
  to do a single `write!` before flushing. That *is* the macro body, unrolled.
  The codebase had eight such reimplementations; the audit's remediation
  removed the redundant ones.
- **Don't** write `pub fn foo() -> alloc::string::String` for a "format some
  counters into one line" helper. The owned `String` return type is the tell —
  every caller discards it after one read, and it forces an allocation on a
  console path. Take `w: &mut dyn core::fmt::Write` and write into the
  caller's stack buffer instead. `pmm::dp_counters_line`,
  `file_page_cache::stats_line`, and `syscall::mem::dontneed_audit_line` are
  the worked examples.
- **Don't** assume `format_args!` is the risk. It isn't: constructing
  `Arguments` and rendering it via `core::fmt::write` into a stack sink is
  heap-free by construction, padding included. The risk is always an
  *argument expression* that allocates before formatting starts.

Two narrow exemptions, both established by the audit rather than assumed:

1. **Genuinely variable-cardinality output** — a loop over a runtime-sized
   collection can't collapse into one macro call. Build it into a fixed stack
   buffer anyway (`console::StackWriter`, or `akuma-exec`'s `process::FmtBuf`,
   which borrows the caller's buffer), never a `String`. See
   `process::stats::dump()`'s per-syscall breakdown and
   `children::lazy_region_debug`'s region list. A multi-`write!` buffer flushed
   once is also what keeps a multi-field line from being interleaved by another
   core's output mid-message.
2. **Paths that may run with no runtime registered** — `sync.rs`'s
   `log_kernel_lock_stuck`/`log_kernel_lock_recovered` probe
   `runtime::is_registered()` before printing, because host unit tests drive
   `KernelLock` directly. `safe_print!` resolves `runtime()` unconditionally, so
   these keep the explicit build-then-guard-then-flush shape. A diagnostic must
   never be the thing that panics.

Boot self-tests (`src/tests.rs`, `src/process_tests.rs`, `src/shell_tests.rs`)
are out of scope — they run in ordinary thread context at boot, never inside
IRQ/lock/allocator-reentrant code, and their `format!` calls mostly build test
fixture paths rather than console content. Likewise `/proc` file *contents*
(`src/vfs/proc.rs`, `src/syscall/log.rs`): heap allocation there is normal VFS
behavior, not a print-path violation.

Nothing enforces this mechanically yet — there is no clippy-adjacent hook, and
all five violations were found by hand. Until one exists, this section is the
enforcement.

## Input

`has_char()`/`getchar()` (`:244-251`) are the non-blocking read primitives
(check `has_data()` before calling `getchar`). `getchar_blocking()` and
`read_line()` (`:255-277`, both `#[allow(dead_code)]`) are unused convenience
wrappers kept for callers outside the normal async SSH input path.

## Background

- `docs/archive/ALLOC_PRINT_AUDIT.md` — the survey behind "Printing rules"
  above: every writer reimplementation and heap-on-console-path violation in
  `src/` and `crates/akuma-exec/src/`, with §7 recording the remediation.
- `docs/archive/SERIAL_TRACE_TRAFFIC_AUDIT.md` — the other half of console
  discipline: *volume*. Heap-free prints still saturate a 115200-baud UART and
  serialize every logging core on the console lock; per-event traces need a
  config flag with a live reader.
- `docs/archive/MULTIKERNEL.md` (§8.2) — the removed one-kernel-per-core
  design's per-core SPSC console ring, which `emit()` used to route through
  before the multikernel was removed (`docs/archive/TRIM_FAT_MULTIKERNEL.md`).
