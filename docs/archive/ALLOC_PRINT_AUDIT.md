# Alloc/print audit: raw `StackWriter` use and heap-allocating console output

**Date:** 2026-08-09. **Scope:** `src/` and `crates/akuma-exec/src/` (the only
tree with any hits — no other `crates/*/src` has a Write-impl'd stack buffer,
a `write!`/`writeln!` against one, or heap formatting feeding console output).
**Status:** survey (§1-§6) **remediated** 2026-08-09 — see **§7** for what
landed, what was deliberately kept, and what is still open. §1-§6 are preserved
verbatim as the survey they were; read §7 for current state.

**One line:** the codebase reimplements the same "fixed byte array + `pos` +
`core::fmt::Write`" writer **eight separate times** (`console::StackWriter`,
`exceptions::StaticWriter`, `main::StackBuffer`, `akuma-exec::threading::StackWriter`,
`akuma-exec::process::diag::StackBuf`, `akuma-exec::process::children::LazyDebugWriter`,
`akuma-exec::process::FmtBuf`, plus three anonymous local `struct Buf` in
`akuma-exec::sync`/`bkl`), and roughly **59 of the ~68 in-scope call sites**
construct one of these writers to do a single `write!`/`writeln!` call before
flushing — which is exactly what `safe_print!`/`tprint!` already do in one line.
Two sites, independent of write-call count, genuinely defeat the heap-free-console
goal: **`src/main.rs`'s `memory_monitor`** builds a `DOUBLE-FREE=` marker via
`alloc::format!`/`String::new()` before folding it into an otherwise heap-free
stack buffer, and **`crates/akuma-exec/src/process/stats.rs::dump()`** builds
its entire `[PSTATS]` line — including a per-syscall breakdown loop — as a heap
`String` and hands it straight to `print_str`. Both are described in full below.

## 0. Needs fixing

First of all, a hook is needed to automatically flag this stuff next to clippy.

## 1. Method

```
rg -n 'StackWriter' src crates --type rust
rg -n 'write!\(|writeln!\(' src crates/*/src --type rust
rg -n 'core::fmt::write\(' src crates/*/src --type rust     # free-fn form of write!, missed by the macro grep
rg -n 'print_str' crates/*/src --type rust                   # catches non-write!-based flush call sites
rg -n '\bformat!\(|String::new\(\)|String::from\(' src crates/akuma-exec/src --type rust
```

then manual read of every hit's enclosing function to check (a) whether it's a
`core::fmt::Write` impl or a call against one, (b) how many `write!`/`writeln!`/
`core::fmt::write` calls happen before the buffer is flushed, and (c) whether
the ultimate destination is the console/log (`console::print`, `runtime().print_str`)
or something else entirely (a VFS file's byte content, a filesystem path string,
an `Err` value later translated to an errno, a `Display` impl's `Formatter`).

**Excluded as out of scope** (matched the greps, not the goal this audit checks):

- Plain `impl Display for E { write!(f, ...) }` (`src/audio.rs:42-45`,
  `src/block.rs:59-63`, `src/rng.rs:101-104`, `src/kernel_timer.rs:53`,
  `src/shell/commands/fs.rs:493`, `src/syscall/mod.rs:1148`,
  `crates/akuma-exec/src/elf/types.rs:105-112`) — standard `Display` shims writing
  into the caller-supplied `Formatter`, not a console path.
- `src/vfs/proc.rs:516-586` and `src/syscall/log.rs:71-78` — `String`/`writeln!`
  building **the byte content of a `/proc` file** returned from `read_file()`,
  consumed by whatever process calls `read()` on it. Never touches the console;
  heap allocation here is normal VFS behavior, not a print-path violation.
- `src/tests.rs:1093` and `src/process_tests.rs` (130 `format!` sites) — boot
  self-test code. `tests.rs:1093`'s `write!(long, ...)` loop is *testing* heap
  `String`/realloc behavior on purpose and only prints `long.len()` (an integer).
  `process_tests.rs`'s `format!` calls build filesystem paths for test fixtures
  (`fs::remove_file(&format!(...))`), not console content. Matches the
  `#[cfg(test)]`-style exemption: these run in ordinary thread context at boot,
  never inside IRQ/lock/allocator-reentrant code.
- `src/shell_tests.rs` (19 `format!(...)` sites piped through a local `log()` →
  `console::print()`) — also boot self-test output, same exemption. Flagged here
  only for completeness; not itemized below since it's judged out of scope by
  the same test-path clause, not overlooked.
- `src/rump_proxy.rs` (9 `format!` sites) — build `Result<_, String>` error
  values later translated to a NetBSD→Linux errno; never printed.
- I/O `Write` impls unrelated to console formatting: `TlsStream`, `VecWriter`,
  `TcpStream`, `FileWriter`, `SshChannelStream` (network/file/SSH channel
  streams, not stack print buffers).
- Two bare `print_str("static string\n")` calls with no formatting at all
  (`crates/akuma-exec/src/mmu/mod.rs:349`, `process/children.rs:395`) — nothing
  to convert; already minimal.

## 2. Writer infrastructure inventory

| Type | Defined at | Role |
|---|---|---|
| `console::StackWriter<N>` | `src/console.rs:173` | Backs `safe_print!`/`tprint!`. Canonical. |
| `exceptions::StaticWriter` | `src/exceptions.rs:2667` | Local reimplementation, fixed 256B, used only by the two functions in §3.1. |
| `main::StackBuffer` | `src/main.rs:1740` (local to `memory_monitor`) | Local reimplementation, 384B; genuinely used for multi-write atomic lines (§3.3), and adjacent to this audit's one heap violation. |
| `akuma-exec::threading::StackWriter<N>` + its own `safe_print!` | `crates/akuma-exec/src/threading/mod.rs:35,63` | The crate's own console macro, mirroring `src/console.rs` — but **module-scoped, not exported crate-wide** (no `#[macro_export]`/`pub(crate) use`), so no other file in the crate can call it. This is the direct cause of §3.2/§3.4/§3.5/§3.6/§3.7 below: every other subsystem in the crate had to hand-roll its own writer instead of reusing one. |
| `process::diag::StackBuf<N>` | `crates/akuma-exec/src/process/diag.rs:96` | Local, used by 2 call sites (§3.4). |
| `process::children::LazyDebugWriter<N>` | `crates/akuma-exec/src/process/children.rs:1039` | Local, used by 2 call sites, one of them genuinely warranted (§3.5). |
| `process::FmtBuf<'a>` (`pub(crate)`) | `crates/akuma-exec/src/process/mod.rs:80` | The one shared/reusable writer in the crate (borrows caller's `buf`/`pos` instead of owning them); reused from `signal.rs` and `table.rs`. Invoked via the free function `core::fmt::write(&mut FmtBuf{..}, format_args!(...))` rather than the `write!` macro — same semantics, just not grep-visible as `write!(`. 17 call sites, all single-shot (§3.6). |
| Anonymous local `struct Buf([u8;96], usize)` × 3 | `crates/akuma-exec/src/sync.rs:833,876,1043`, `bkl.rs:361` | Ad hoc, redefined per call site instead of reusing `FmtBuf`/`StackWriter` (§3.7). |

Eight independent implementations of the identical ~15-line pattern. None of
this is a correctness bug — every one is heap-free and functionally sound —
but it is exactly the "sweep the rest of src/" consistency debt this audit was
asked to quantify, and it's the reason the "convertible" sites below multiplied
past `src/console.rs`'s own macros: most of `crates/akuma-exec` simply has no
`safe_print!`/`tprint!` to convert *to*.

## 3. Findings

### 3.1 `src/exceptions.rs` — `StaticWriter`, 34 sites, all NOT WARRANTED

Two functions, `rust_sync_el1_handler` and `log_memory_stats_on_crash`, share one
`StaticWriter` and repeat `let _ = writeln!(w, "...", ...); w.flush();` as a unit,
over and over — literally the `safe_print!`/`tprint!` macro body, unrolled by hand.
Confirmed by direct inspection: every occurrence has exactly one `writeln!` before
its `flush()`, and the same functions already call `safe_print!` directly for
their fixed-string branches (e.g. `exceptions.rs:2523-2524`, `2591`, `2601`,
`2623`), so the `StaticWriter` calls are inconsistent leftovers, not a
deliberate choice.

| Lines | Excerpt | Fix |
|---|---|---|
| 2483, 2485, 2487, 2489, 2499, 2501, 2509, 2517 | `writeln!(w, "[Exception] Sync from EL1: EC={ec:#x}, ISS={iss:#x}")` etc. — register dump preamble | `crate::safe_print!(N, "...\n", ...)` per line, same format string |
| 2548, 2550, 2552, 2555, 2558 | conditional hint/kill-path lines inside the `EC==0x25` branch | same |
| 2595, 2597, 2607, 2614, 2619, 2629, 2637 | page-table-walk forensics (`Expected boot_ttbr0`, `L0[0] entry`, `L1[0] entry`, ...) | same |
| 2721, 2732, 2740, 2751, 2762, 2766, 2786, 2789, 2796, 2800, 2805, 2812, 2820, 2828 | `log_memory_stats_on_crash`'s heap/PMM/thread/process dump | same |

None of these loop over a runtime-sized collection — every field is a single
scalar or a handful of named registers known at the call site — so each
collapses to one `safe_print!` (or `tprint!`, see §4 recommendation). This is
the single largest cluster in the audit by line count.

### 3.2 `src/main.rs:159-161` — panic handler, 1 site, NOT WARRANTED

```rust
let mut buf = console::StackWriter::<256>::new();
let _ = write!(buf, "{}", info.message());
console::print(buf.as_str());
```

Single `write!`, immediately printed. `safe_print!` was built for exactly this
call site (the doc comment at `console.rs:211-217` even shows a comparable
usage) — but `#[panic_handler]` can't call the macro conveniently for this
particular case because it needs the string un-terminated
(`console::print("\n")` follows separately). Trivially:
`crate::safe_print!(256, "{}\n", info.message());`, dropping the standalone
`console::print("\n")` at line 163.

### 3.3 `src/main.rs::memory_monitor` — `StackBuffer`, WARRANTED (with one carve-out, see §3.8)

`StackBuffer` (`main.rs:1740`) is built once per report cycle and accumulates
several **conditionally-present** fields — base stats, then `| reclaimed=...`
only if `reclaimed_pages > 0`, `| quar=...` only if the UAF quarantine is armed,
`| spans: ...` only if a span report is non-trivial — before a single
`console::print(buf.as_str())` (`main.rs:1818-1855`). This genuinely needs
multiple `write!` calls into one buffer: the fields aren't fixed-cardinality,
and folding them into one flush also gets the same interleaving-safety benefit
`console::emit()` documents (one flush = no other thread's line can land in the
middle). The `[SSH]` block right after it (`main.rs:1865-1924`) is the same
shape — an optional second `writeln!` (`STALL DETAIL`) appended to the first
before one `console::print`. Both are **WARRANTED**. The one exception inside
this same function is a real heap violation — see §3.8.

### 3.4 `crates/akuma-exec/src/process/diag.rs` — `StackBuf`, 2 sites, NOT WARRANTED

- `log_slow_lock` (line 39, `writeln!` at 42): `[PTLOCK] {}: held {}us`, 2 scalar args.
- `log_borrow_alias` (line 85, `writeln!` at 88): `[BORROW-ALIAS] pid={} count={}`, 2 scalar args.

Both single-shot. Fix: replace the `StackBuf::new()` / `writeln!` / `flush()`
triplet with one macro call once a crate-wide print macro exists (see §4) —
e.g. `akuma_safe_print!(128, "[PTLOCK] {}: held {}us\n", caller, elapsed_us);`.

### 3.5 `crates/akuma-exec/src/process/children.rs::lazy_region_debug` — `LazyDebugWriter`, split verdict

- Line 1068 (no-process-entry branch): `writeln!(w, "[DP] lazy miss: pid={} va={:#x} no process entry", pid, va)` — single call, **NOT WARRANTED**.
- Lines 1076-1082 (has-process branch): `write!(w, "...regions={} [", ...)`, then
  `g.for_each_debug(|sv, sz| { ...; write!(w, "{:#x}+{:#x}", sv, sz); })` over a
  **runtime-sized** lazy-region list, then a closing `w.write_str("]\n")` —
  **WARRANTED**: this is precisely the "unknown count, iterating a runtime-sized
  collection" case the task brief calls out as the one kind of loop that can't
  collapse into a single macro call.

### 3.6 `crates/akuma-exec/src/process/mod.rs` (+`signal.rs`, `table.rs`) — `FmtBuf`, 17 sites, all NOT WARRANTED

Every occurrence is `core::fmt::write(&mut FmtBuf{buf:&mut buf, pos:&mut pos}, format_args!(...))`
followed immediately by `if let Ok(s) = core::str::from_utf8(&buf[..pos]) { (runtime().print_str)(s); }`
— one format call, one flush, checked programmatically (no site has more than
one `core::fmt::write` before its matching `print_str`):

`process/mod.rs:875` (`[RUN-REFUSED]`), `1022` (`[EUM POISON]`), `1230` (`[KTG]`),
`1252` (`[KTG-MISMATCH]`), `1373` (`[KTG-STALE]`), `1446` (`[KTG-STALE-CH]`),
`1533` (`[PROC-ORPHAN]`), `1567` (`[ORPHAN-KILL]`), `2205` (`[FORK-DBG]`),
`2557` (`[FORK-COW]`), `2595` (`[FORK-DBG] step4`), `2640` (`[FORK-DBG] mmap region`),
`3422` (`[TRAMP-MISMATCH]`), `3436` (`[FORK-DBG] trampoline ENTRY]`),
`3461` (`[TRAMP]`); `process/signal.rs:186` (`[kill] ... stale tid`);
`process/table.rs:192` (`[unregister] ... stale tid`).

`FmtBuf` is at least `pub(crate)` and genuinely shared (unlike the other
duplicated writers), so the type itself is fine — the boilerplate around each
call is what's redundant. Fix per site:
`crate::safe_print!(<same size>, "<same format string>\n", <same args>);` (once
that macro exists crate-wide, §4) — 3 lines collapse to 1, seventeen times over.

### 3.7 `crates/akuma-exec/src/sync.rs`, `bkl.rs` — anonymous `struct Buf`, 4 sites, all NOT WARRANTED

- `sync.rs:831-853` `log_kernel_lock_stuck`: `[BKL] stuck: owner={} waiter={} tag={} (aff0+1)`.
- `sync.rs:873-887` `log_kernel_lock_recovered`: `[BKL] RECOVERED ({kind}) by core {me} (aff0+1)`.
- `sync.rs:1035-1054` `log_write_lock_stuck`: `[RWLOCK] write lock stuck: state={:#x} readers={} writer_bit={}`.
- `bkl.rs:355-373` `note_preserved_window`: `[BKL] dropped window preserved across IRQ x{n}`.

Each redefines its own 10-line `struct Buf([u8; 96], usize)` + `impl Write`
*inline inside the function*, purely to do one `writeln!` — the most redundant
form in the whole survey, since these four don't even reuse `FmtBuf`, which is
`pub(crate)` and already visible from `sync.rs`/`bkl.rs`. Fix: same pattern as
§3.4/§3.6.

### 3.8 HEAP VIOLATION — `src/main.rs:1810-1822`, `memory_monitor`

```rust
let dfree = pmm::double_free_count();
let dfree_marker = if dfree > 0 {
    alloc::format!(" | DOUBLE-FREE={dfree}")     // <-- heap alloc
} else {
    alloc::string::String::new()                  // <-- heap alloc (empty, but still a String)
};
...
let _ = write!(buf, "...{}", ..., dfree_marker);   // folded into the otherwise-heap-free StackBuffer
```

`memory_monitor` is otherwise the model citizen described in §3.3 — a
purpose-built stack buffer, explicit comments about avoiding heap allocation,
several genuinely-conditional fields appended without ever touching the heap.
This one field is the exception, and it's avoidable with the exact pattern the
function already uses two lines later for `reclaimed`/`quar`/`spans` (`main.rs`
`1827-1852`, all `write!(buf, " | ...")` directly, no intermediate `String`):

```rust
// replace the dfree_marker String entirely; drop it from the big write! format
// string/arg list, then after that write! do:
if dfree > 0 {
    let _ = write!(buf, " | DOUBLE-FREE={dfree}");
}
```

This is a genuine, if low-severity, violation: `memory_monitor` runs as a
normal async task (not IRQ/lock context), so the immediate risk is low, but
it's precisely the kind of drift this print discipline exists to prevent —
and doubly ironic here because the double-free counter this code is reporting
*is itself* a PMM/allocator health signal; a monitor whose own reporting path
depends on the heap being healthy is the wrong shape for that job.

### 3.9 HEAP VIOLATION — `crates/akuma-exec/src/process/stats.rs::dump()`, lines 85-110

```rust
let mut top = String::new();                                  // heap alloc
for (i, (nr, count, time)) in entries.iter().enumerate() {
    ...
    let _ = write!(&mut top, "{}={}({}ms)", sname, count, time_ms);  // heap growth per entry
    if i >= 9 { break; }
}
...
let msg = format!(                                             // heap alloc, full [PSTATS] line
    "[PSTATS] PID {} ({}) {}.{:02}s: {} syscalls ({}/s) in_kernel={}ms pmm={}free/{}tot retired={}/{}p pgfault={}({}pg) | {}\n",
    pid, name, secs, frac, total, rate, total_time_ms,
    pmm_free, pmm_total,
    table::retired_process_count(), crate::process::reclaim::retired_pages_pending(),
    pf, pf_pg, top,
);
(runtime().print_str)(&msg);                                    // straight to console
```

This is the most serious finding in the survey: a `String` grown in a loop and
a second `format!`-built `String`, both feeding straight into `print_str`. It
is reached from `dump_running_process_stats()` (`stats.rs:114-130`), a
periodic sweep over every process that has run >10s, gated only by
`PROCESS_SYSCALL_STATS_ENABLED` — a diagnostics feature meant to be safe to run
under real load, including exactly the memory-pressure conditions where a
heap-free console path matters most.

The `top` loop *is* a genuinely variable-cardinality case (0-10 syscalls,
runtime-sorted) — so the fix isn't "collapse to one macro call", it's "do the
same loop into a real stack buffer instead of a `String`": build `top` with
`core::fmt::Write` against a local fixed-size writer (e.g. reuse `FmtBuf` or
add one sized ~192B, matching the existing `[PSTATS]` line lengths seen in
practice), then either write the `[PSTATS]` prefix into the *same* buffer
before the loop and flush once, or `tprint!`/`safe_print!` the prefix and the
already-built `top` slice as one final format argument. Either way, zero heap
touches the console path.

## 4. Cross-cutting recommendation (not applied, out of scope for this doc)

`crates/akuma-exec` has no crate-wide equivalent of `safe_print!`/`tprint!` —
only `threading::safe_print!`, module-private. That single gap is the direct
cause of §3.4, §3.6, and §3.7 (23 of the 59 NOT WARRANTED sites): every other
module had no macro to reach for and hand-rolled a writer instead. Exporting
`threading::safe_print!` crate-wide (`pub(crate) use` from `lib.rs`, or moving
the macro to a top-level module) would let all of them collapse to one line
each without inventing anything new. This is a mechanical follow-up, not
attempted here per the task's read-only scope.

## 5. Summary counts

| Category | Count | Notes |
|---|---:|---|
| Distinct writer-type reimplementations found | 8 | `console::StackWriter` is canonical; the other 7 are redundant with it or each other |
| Call sites, WARRANTED (genuine multi-write or infra) | 5 | `console.rs` infra, `threading/mod.rs` infra, `main.rs::memory_monitor`'s two multi-field blocks, `children.rs`'s lazy-region loop |
| Call sites, NOT WARRANTED (single write, convertible) | 59 | 34 `exceptions.rs` + 1 `main.rs` panic handler + 2 `diag.rs` + 1 `children.rs` + 17 `FmtBuf` (`mod.rs`/`signal.rs`/`table.rs`) + 4 `sync.rs`/`bkl.rs` |
| **HEAP VIOLATIONS** (heap alloc feeding console output) | **2** | `main.rs::memory_monitor` (`DOUBLE-FREE` marker); `akuma-exec::process::stats::dump()` (entire `[PSTATS]` line + per-syscall loop) |

No heap violations were found in any exception/fault/lock-diagnostic path
itself — `exceptions.rs`'s 34 sites and `sync.rs`/`bkl.rs`'s 4 are all
needlessly verbose but **already heap-free**, just not macro-consolidated.
The two real heap violations are both in periodic/on-demand diagnostics
(`memory_monitor`'s 10s tick, `stats::dump()`'s >10s-runtime sweep) rather than
interrupt or lock-holding contexts, which matches this codebase's history: the
dangerous class (heap alloc *inside* an unsafely-reentrant context) shows up
zero times in this survey. That's a meaningfully clean result on the
higher-severity half of the question this audit was asked to answer — the
debt that does exist is entirely consistency/verbosity debt (§2-§3), not the
kind that has previously caused wedges in this codebase.

## 6. 2026-08-09 addendum: `format_args!` verdict, log-backend verdict, and 3 call-graph-hop heap violations

Follow-up pass, same read-only scope, prompted by a direct question: is
`format_args!` itself safe in kernel code, what does `log::debug!`/`warn!`/
`info!` actually do at runtime, and did §3 miss anything by only grepping the
*text* of `write!`/`writeln!` call sites rather than following what their
*arguments* do. It found three new heap violations, all one call-graph hop
away from a heap-free-looking `writeln!`/`safe_print!` site already covered by
§3.1 and §3.3 above.

### 6.1 `format_args!` verdict

`format_args!` builds a `core::fmt::Arguments` — a `core`-crate value that
borrows its inputs; constructing it never touches the heap. Rendering it is
what decides: `core::fmt::write(&mut sink, args)` — the free-function form
`FmtBuf`/`as_trace`/`StackWriter` all use — calls into `core::fmt`, not
`alloc::fmt`, so the formatting machinery itself (including width/precision
padding, which pads by issuing extra `write_str` calls to the sink, not by
building an intermediate `String`) is heap-free by construction, regardless of
sink. `alloc::format!`/`.to_string()`/`String::push_str` are a different
code path (`alloc::fmt::format`, which does `String::with_capacity` then
`write!` into it) — heap alloc is a property of *that macro*, not of
`format_args!`/`core::fmt::write`. Confirmed by direct inspection: no custom
`Display`/`Debug` impl in `src/` or `crates/akuma-exec/src/` allocates inside
its own `fn fmt` body (checked every `impl ... for` block with a `fn
fmt(&self, f: &mut Formatter)` signature) — every kernel-print-relevant impl
is a plain `write!(f, "...", scalar_fields)` shim, so rendering an existing
value never grows the heap. **The actual risk is never `format_args!` or the
sink — it's an argument expression that itself allocates before formatting
ever starts** (typically a helper function that returns `String`). That's
exactly what §6.3 below found three instances of, all missed by §3 because its
greps matched `write!`/`writeln!`/`StackWriter` *usage sites* and manually
verified their format strings' inline arguments were scalars, without
following a plain-looking identifier argument (`dp_counters_line()`,
`stats_line()`, `dontneed_audit_line()`) back to its own function body.

### 6.2 Log-backend verdict

`log::debug!`/`warn!`/`info!`/`trace!` appear at **102 call sites** across
`crates/akuma-ssh/src` (message.rs, packet.rs, config.rs, session.rs,
transport.rs), `crates/akuma-net/src` (smoltcp_net.rs, http.rs),
`crates/akuma-exec/src` (process/mod.rs, process/signal.rs) — all of them
kernel-context code, reachable from SSH sessions, net polling, and
fork/kill/exit paths. **None of them ever renders or allocates, because no
logger is installed**: `rg -n "set_logger|impl log::Log|dyn Log"` across
`src/` and every `crates/*/src` returns zero hits, and
`rg -n "set_max_level"` is likewise empty repo-wide. The `log` crate's
macros expand to `if level <= STATIC_MAX_LEVEL && level <= log::max_level() {
format_args!(...) -> record -> logger }`; with no `set_logger`/`set_max_level`
call anywhere, `log::max_level()` stays at its default, `LevelFilter::Off`,
for the life of the kernel. `Off` is below every `Level` variant, so the
runtime check fails unconditionally, before `format_args!` is even
constructed — the argument expressions inside every `log::debug!` etc. call
are never evaluated. Net effect: **zero heap risk** (nothing ever formats),
but also **zero output** — these 102 sites are dead instrumentation that read
as live logging. (`crates/akuma-net/Cargo.toml:72` sets
`features = ["max_level_off"]` on its own `log` dependency, which compiles its
call sites out entirely at the `STATIC_MAX_LEVEL` compile-time bound — the
other three crates' `Cargo.toml`s set `default-features = false` with no
`max_level_*` feature, so their sites compile in but are runtime-dead for the
reason above. Either way, the effect at runtime is identical: silence.) This
is a correctness/observability gap, not a safety one — noted here because the
task asked what the log backend does with the record, and the answer is
"nothing, there is no backend."

### 6.3 New heap violations found (call-graph hop, not visible at the `write!`/`writeln!` site itself)

Three helper functions each build a heap `String` via `alloc::format!` purely
to hand a "one-line summary" back to a caller that immediately feeds it into
an otherwise heap-free `safe_print!`/`writeln!` — identical in shape to the
already-documented §3.8/§3.9 violations, except the `String::new()`/`format!`
call is one function away from the print site, so it didn't show up when §3
checked each `write!`/`writeln!`'s *inline* arguments for scalars.

| Site (String built) | Site (fed to print) | Context | Severity | Why |
|---|---|---|---|---|
| `src/pmm.rs:947-957` `dp_counters_line()` — `alloc::format!(...)` at line 948 | `src/exceptions.rs:2828` — `writeln!(w, "    DP pages (global): {}", crate::pmm::dp_counters_line())` inside `log_memory_stats_on_crash` | Sync-EL1 exception/**crash handler** (`rust_sync_el1_handler` → `log_memory_stats_on_crash`) | **HIGH** | This is the one dangerous-context hit in the whole survey. The surrounding code (§3.1; comment block `exceptions.rs:2661-2663`, "Static buffer formatting for crash handlers (no heap allocation)") exists specifically so this handler never needs the heap — and a comment right above it at `exceptions.rs:2446-2452` already documents the exact failure class this violates: *"That function acquires POOL lock inside `with_irqs_disabled`. If an EL1 data abort fires while POOL lock is already held (e.g. during context switch), the lock acquisition would deadlock."* The TALC heap lock is exposed to the identical risk: if the fault that triggered this handler occurred while a core already held the allocator's lock (e.g. a fault inside `alloc`/`dealloc` itself, or heap corruption), `dp_counters_line()`'s `alloc::format!` re-enters that same lock and the crash handler hangs instead of printing — losing every diagnostic line after it, including the ones already-heap-free (`writeln!` at 2812, 2820, etc. right next to it). |
| `src/file_page_cache.rs:276-285` `stats_line()` — `alloc::format!(...)` at line 277 | `src/main.rs:1531` — `crate::safe_print!(192, "{}", crate::file_page_cache::stats_line())` | `memory_monitor`'s 30s `[FSCACHE]`/page-cache diagnostics tick (same function as §3.3/§3.8) | LOW-MEDIUM | Same class as §3.8/§3.9: normal async-task context, not IRQ/lock-held, so immediate risk is low — but it's the same "diagnostics whose own reporting path needs a healthy heap" irony §3.8 already calls out, and it sits three lines away from `dontneed_audit_line()` below, in the exact same block. |
| `src/syscall/mem.rs:127-134` `dontneed_audit_line()` — `alloc::format!(...)` at line 129 | `src/main.rs:1536` — `crate::safe_print!(128, "{}", crate::syscall::mem::dontneed_audit_line())` | Same `memory_monitor` tick, immediately after the `stats_line()` call above | LOW-MEDIUM | Same as above. Both this and `stats_line()` return `alloc::string::String` from a function whose only caller is a `safe_print!("{}", ...)` — the `String` return type is itself the tell; a heap-free version would take `&mut dyn core::fmt::Write` (the `FmtBuf`/`StackWriter` pattern) instead of returning an owned buffer. |

All three follow the same anti-pattern: `pub fn foo() -> alloc::string::String`
where the function's only job is "format some counters into one line for the
console" — the `String` return type forces heap allocation even though every
caller immediately discards the `String` after one read. Fix (not applied,
per this doc's read-only scope): change the signature to
`pub fn foo(w: &mut dyn core::fmt::Write)` (or reuse `crate::console::StackWriter`/
`akuma_exec::process::FmtBuf` directly at the call site) and `write!` the same
format string into the caller's stack buffer instead of returning an owned
`String`. `pmm::dp_counters_line()` is the one to fix first — it is the only
one of the three reachable from the crash handler.

### 6.4 Sites checked and ruled benign (not added to the table above)

- `src/timer.rs:293-304`/`314-318` `to_iso8601()`/`utc_iso8601()` — `alloc::format!`
  building an ISO-8601 timestamp `String`. Only caller is `src/main.rs:922`,
  a one-time boot-init print (`console::print(&timer::utc_iso8601())`) before
  threading/scheduling exists. Heap is guaranteed healthy at that point;
  trivial.
- `src/console.rs:270` `read_line()`'s echo — `print(&(c as char).to_string())`,
  one `String` alloc per typed character. `#[allow(dead_code)]`-marked
  (unused), blocking-input-loop function, not console-output-formatting in
  the sense this audit tracks, and not reachable from any dangerous context.
  Wasteful if ever wired back up (`safe_print!(4, "{}", c as char)` would be
  heap-free) but not a violation today.
- `src/syscall/term.rs:351` `sys_set_cursor_position`'s `format!("\x1b[{row_1};{col_1}H")` —
  syscall body, ordinary thread context (not IRQ/lock-held), writes to a
  process I/O channel, not the kernel console. Matches this doc's existing
  "syscall bodies in normal context are lower severity" carve-out; not
  itemized as a violation.
- `src/syscall/container.rs:55` `String::from(name)` — stored into a
  `BoxInfo` struct field, not fed to any print/format path at all; irrelevant
  to this audit's question, mentioned only because it matched the grep.
- All Drop impls in `src/` and `crates/akuma-exec/src/` (`Process`,
  `UserAddressSpace`, `SharedFdTable`, `LifecycleGuard`, `PreemptGuard`,
  `IrqGuard` ×2, `ProcessBklGuard`, `ForkInProgressGuard`,
  `DroppedWindowPause`, the four `*BklGuard`s in `src/syscall/*.rs`, and the
  four fault-guards defined inline in `src/exceptions.rs`) — checked each
  Drop body for `format!`/`String::`/`.to_string()`/`Vec::new()`/print calls.
  The two that print at all (`UserAddressSpace::drop` via `as_trace`,
  `Process::drop` via the `FmtBuf` pattern) both use the heap-free patterns
  from item 3 of this task; every other Drop impl has no formatting/print
  path whatsoever. No teardown-time heap violation found.
- `crates/akuma-exec/src/allocator.rs` (`alloc_error_handler`) and
  `src/allocator.rs:499-514` — the actual OOM path uses `safe_print!` only,
  no `String`/`format!`. Clean.
- Scheduler/thread-switch code (`crates/akuma-exec/src/threading/*`,
  no dedicated `sched*`/`switch*` files exist in this codebase) — the only
  `alloc::format!` hits are in `threading/types.rs:323,333`, both
  `Result<_, String>` boot-time stack-size validation errors (never printed
  to console, matches the existing audit's `rump_proxy.rs`-style exemption).
  IRQ entry (`src/irq.rs`) has zero `format!`/`String`/print hits at all.

### 6.5 Updated summary

Adding this pass's results to §5's table: **HEAP VIOLATIONS is now 5** (2 from
§3 + 3 from §6.3), of which **1 is HIGH severity** (`pmm::dp_counters_line()`
reached from the crash handler, §6.3 row 1) and **4 are LOW/benign-context**
(the original §3.8/§3.9 pair, plus §6.3's `stats_line()`/`dontneed_audit_line()`
pair — all four in periodic/on-demand diagnostics, not IRQ/lock-held code).
§3's closing claim that "no heap violations were found in any exception/fault
... path itself" no longer holds in full: `exceptions.rs` itself still builds
no `String` directly (confirmed again, §6.4), but one of its `writeln!` calls
now is shown to *indirectly* allocate through `pmm::dp_counters_line()` — the
first and only violation in this audit reached from inside the sync-EL1
crash-handler call tree. `format_args!`/`core::fmt::write` are exonerated: at
no point does rendering into a stack buffer allocate, with or without
width/padding specifiers. The `log` crate's 102 kernel-context call sites are
exonerated on the heap question for a different reason — they never run at
all — but are flagged as dead instrumentation, a distinct, non-heap
observability gap.

## 7. 2026-08-09 remediation: what landed

The survey above (§1-§6) was read-only. This section records the fix pass that
followed it, on branch `fix-alloc-print`. **All 5 heap violations are closed**
(§7.1); the writer-duplication debt is reduced from 8 implementations to 6
(§7.2); §4's blocking prerequisite is done (§7.3). Two items remain open
(§7.5). Verification is in §7.6.

### 7.1 All 5 heap violations closed

| # | Site | Severity (§6.5) | Fix |
|---|---|---|---|
| §6.3 r1 | `src/pmm.rs::dp_counters_line()` | **HIGH** — only violation reachable from the sync-EL1 crash handler | Signature changed from `-> alloc::string::String` to `(w: &mut dyn core::fmt::Write)`. The crash handler now passes its existing `StaticWriter` straight in (`exceptions.rs:2788-2791`), so the line is rendered into the handler's own stack buffer and the allocator is never entered. This is the fix §6.3 named as "the one to fix first". |
| §3.9 | `akuma-exec::process::stats.rs::dump()` | LOW ctx / most serious by shape | The `String`-grown-in-a-loop `top` breakdown now formats into a fixed 224 B stack buffer through the shared `FmtBuf`, and the outer `format!`+`print_str` pair collapses to one `crate::safe_print!(384, …)`. Zero heap on the `[PSTATS]` path. Took §3.9's recommended shape (loop into a real stack buffer, since the 0-10 entry cardinality is genuinely variable) rather than a macro collapse. |
| §3.8 | `src/main.rs::memory_monitor` `DOUBLE-FREE` marker | LOW | The `alloc::format!`/`String::new()` pair is gone; `dfree` is dropped from the main `write!` arg list and appended conditionally (`if dfree > 0 { write!(buf, " \| DOUBLE-FREE={dfree}") }`) — exactly the pattern the same function already used for `reclaimed`/`quar`/`spans`, as §3.8 proposed. |
| §6.3 r2 | `src/file_page_cache.rs::stats_line()` | LOW-MEDIUM | Same signature change as `dp_counters_line`: `(w: &mut dyn core::fmt::Write)`. Caller in `main.rs` supplies a `console::StackWriter::<192>`. |
| §6.3 r3 | `src/syscall/mem.rs::dontneed_audit_line()` | LOW-MEDIUM | Same, with a `StackWriter::<128>` at the `main.rs` call site. |

The three `-> String` helpers all took §6.3's prescribed fix — *"change the
signature to `pub fn foo(w: &mut dyn core::fmt::Write)` … and `write!` the same
format string into the caller's stack buffer instead of returning an owned
`String`"* — so the `String` return type that §6.3 called "itself the tell" no
longer appears on any console-feeding helper in the tree.

### 7.2 Writer reimplementations: 8 → 6

Removed outright, their call sites converted to `safe_print!`:

- `process::diag::StackBuf<N>` (§3.4) — type deleted; both call sites
  (`[PTLOCK]`, `[BORROW-ALIAS]`) are one-liners now.
- All three anonymous `struct Buf([u8;96], usize)` in `sync.rs`/`bkl.rs` (§3.7)
  — the most redundant form in the survey, each a ~10-line `impl Write`
  redefined inside a function to do one `writeln!`. All four call sites fixed:
  `log_write_lock_stuck` and `bkl::note_preserved_window` became `safe_print!`;
  `log_kernel_lock_stuck` and `log_kernel_lock_recovered` now reuse the
  crate-shared `FmtBuf` instead of defining their own.

Deliberately **kept** (each for a reason the survey itself established):

| Type | Why it survives |
|---|---|
| `console::StackWriter<N>` | Canonical; backs `safe_print!`/`tprint!`. |
| `threading::StackWriter<N>` | Backs `akuma-exec`'s own `safe_print!` (§7.3). |
| `exceptions::StaticWriter` | Down to **one** remaining use — the crash handler's sink for `dp_counters_line(&mut w)` (§7.1). Keeping it is what makes that HIGH fix heap-free; deleting it would have forced the helper back to returning an owned buffer. |
| `main::StackBuffer` | §3.3 WARRANTED — genuinely multi-write, conditionally-present fields folded into one flush for interleaving safety. |
| `process::FmtBuf<'a>` | The one shared writer; now the sink for the two `sync.rs` lock diagnostics and `stats.rs`'s `top` loop. |
| `children::LazyDebugWriter<N>` | §3.5 WARRANTED half only — the runtime-sized lazy-region loop. Its single-shot branch (the `no process entry` line) was converted. |

`log_kernel_lock_stuck`/`log_kernel_lock_recovered` are the two sites that kept
`FmtBuf` rather than becoming `safe_print!`, and the reason is load-bearing:
both probe `runtime::is_registered()` *before* printing, because host unit tests
drive `KernelLock` directly with no runtime registered, and `safe_print!`
resolves `runtime()` unconditionally. A diagnostic must never be the thing that
panics — so these keep the explicit build-then-guard-then-flush shape.

### 7.3 §4's cross-cutting prerequisite: done

§4 identified the crate-wide macro gap as the *direct cause* of §3.4/§3.6/§3.7
(23 of 59 sites). `threading::safe_print!` is now `#[macro_export]`ed, and
`StackWriter`/`new`/`flush` are `pub(crate)`, so the macro is reachable as
`crate::safe_print!` from any module in `akuma-exec` — not just `threading` and
its descendants. Every §3.4/§3.6/§3.7 conversion below depends on this landing
first.

Converted to `safe_print!` on top of it: **18** sites in `process/mod.rs`
(§3.6's 15 `FmtBuf` call sites plus 3 adjacent), 1 in `signal.rs`, 1 in
`table.rs`, 2 in `diag.rs`, 1 in `children.rs`, 1 in `sync.rs`, 1 in `bkl.rs`.
In `src/`: **33** of §3.1's 34 `exceptions.rs` `StaticWriter` sites (the 34th
is the crash handler's `dp_counters_line` sink, §7.2), and the §3.2 panic
handler, which collapsed to `crate::safe_print!(256, "{}\n", info.message())`
with the trailing standalone `console::print("\n")` dropped as §3.2 specified.

### 7.4 Two bugs found while verifying this pass

Neither is an alloc/print defect; both were surfaced by the verification run
(§7.6) and are recorded here because this pass is where they were found.

- **`test_poll_bkl_drop` hung the whole SMP=4 boot.** The test called
  `handle_syscall(nr::PPOLL, &[0, 0, 0, 0, 0, 0])` expecting an early return.
  `ppoll(NULL, 0, NULL, …)` is how musl implements `pause()`, so once the
  "`nfds == 0` is NOT nothing-to-do" fix landed, `sys_ppoll` correctly blocked
  on it forever and the boot self-test suite never reached SSH. The test was
  stale, not the kernel: it now passes a zero `timespec` so it stays the
  non-blocking probe of the entry path it was always meant to be. This hang
  reproduces on a clean `db022be` and is *fixed* by this branch.
- **`scripts/lockprobe.py` could never find `KERNEL_LOCK`.** Its symbol
  window was `IMG_LO..IMG_HI = 0x40100000..0x40400000`, but the kernel's
  `.bss` outgrew 3 MB and `KERNEL_LOCK` now sits at ~`0x404ce0b8`, so every
  symbol in it was filtered out and the tool aborted with a misleading
  `KERNEL_LOCK not found — wrong ELF?`. Every automatic storm capture in
  `j4_selfhost_campaign.py` had been failing this way. `IMG_HI` raised to
  `0x40900000` with a comment on why it must cover `.bss`.

### 7.5 Still open

- **§0's clippy-adjacent hook is not written.** Nothing mechanically prevents
  the next `-> String` console helper from landing. The three signatures fixed
  in §7.1 were all found by hand, twice (§3 missed them; §6.3 caught them only
  by following arguments one call-graph hop). This remains the highest-value
  follow-up in the doc.
- **§6.2's 102 dead `log::` call sites** are untouched. They are heap-safe
  (no logger is installed, so nothing ever formats) but they are also silent —
  an observability gap, not an alloc one, and out of scope for this pass.

### 7.6 Verification

- `cargo build` clean on `release`, `release-smp-shared --features smp-shared`,
  and `size`. Clippy clean on `release-smp-shared`. Host tests 525/525.
- SMP=4 boot self-test suite: 273 PASSED, 1 FAILED —
  `thread_slot_reclaim_on_spawn` (`hot_reclaim=44`), the documented
  pre-existing failure (`J4_HANG_LIVE_AUTOPSY.md` §211,
  `KTG_STALE_TID_EXIT_STAMP_J4_HANG.md` §263).
- `-j4` self-host campaign (`scripts/j4_selfhost_campaign.py`, SMP=4, 14 GB):
  A/B'd against `db022be` + only the §7.4 test fix (baseline cannot boot
  without it). Both sides produced the same shape — one round that does not
  finish inside budget, one GREEN at 206 s. No regression attributable to this
  pass.
- **Not** build-verified on `extreme-size`: that profile fails to compile at
  `db022be` too (17 `E0433`s, `file_page_cache`/`container` unresolved, mostly
  in files this branch never touches). Pre-existing feature-gating breakage,
  tracked separately.

## Background

- `src/console.rs:211-241` — `safe_print!`/`tprint!` definitions and doc comments.
- `docs/archive/PAGE_TABLE_UAF_BKL_STORM.md` §1 — `src/main.rs:1546-1562`'s
  per-core exception-entry counter, cited in the task that produced this audit
  as the precedent for "a loop over a compile-time-constant cardinality
  (`MAX_CORES`) collapses to one `tprint!` call, not a raw `StackWriter` loop."
- `docs/reference/subsystems/config-flags.md` — `PMM_UAF_QUARANTINE`,
  `PROC_SYSCALL_LOG_MAX_ENTRIES`, and other flags referenced by the excluded
  `/proc`-content sites in §1.
