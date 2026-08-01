# BKL Phase 7: work plan and agent prompts

Two self-contained prompts, in order. **A** establishes a throughput baseline the campaign
does not currently have; **B** executes Phase 7 against it. B depends on A's output.

Read first, in this order: [`../archive/BKL_PHASE7_AUDIT.md`](../archive/BKL_PHASE7_AUDIT.md)
(what is and isn't blocked, and why), then
[`../archive/BKL_FINE_GRAINED_LOCKING_PLAN.md`](../archive/BKL_FINE_GRAINED_LOCKING_PLAN.md)
§7 (the replan, incl. §7.3's inversion approach), then
[`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) (the playbook
and the load-bearing inventory).

---

## Prompt A — rustc/cargo scaling baseline, before Phase 7 starts

> **Goal.** Establish a *throughput* baseline for BKL work using an in-VM Rust
> compilation workload, at SMP=1/2/4, so Phase 7 has an end-to-end number to move rather
> than only a spin-count proxy.
>
> **Why this workload, and why now.** The campaign has three kinds of evidence today and
> all three have gaps: `contention_spins`/`[BKLPROF]` is a *proxy* (spins, not seconds);
> digests prove correctness but say nothing about speed; and the one throughput number that
> exists (`BKL_MM_CARVE_OUT.md` §4, llama.cpp tok/s) is compute- and mmap-bound and barely
> touches process lifecycle. A `cargo`/`rustc` build hammers exactly the holders that are
> *still BKL-held and un-carved* — `execve` ~22%, `clone` ~10–13%, `openat` ~10% per the
> audit's §1.2 table. It is therefore the first workload that could actually **falsify**
> "the remaining BKL cost doesn't matter."
>
> **The metric that matters is the scaling curve, not a single wall-clock number.** The BKL
> serializes all of EL1, so the question is not "how many seconds" but "does adding cores
> help." Report speedup relative to the SMP=1 `-j1` cell. At minimum run these cells:
>
> | cell | isolates |
> |---|---|
> | SMP=1, `-j1` | serial baseline |
> | SMP=4, `-j1` | does a *single* build get faster with spare cores? (kernel-side overlap only) |
> | SMP=4, `-j4` | does *parallel* compilation scale, or does the BKL eat it? |
> | SMP=2, `-j2` | is the curve linear-ish or does it break between 2 and 4? |
>
> If SMP=4 `-j4` is not meaningfully faster than SMP=1 `-j1`, that number **is** the Phase 7
> justification, and its improvement is the Phase 7 success criterion. Record it as such.
>
> **Setup — already verified present, do not rebuild the disk.** `devbox.img` (the same
> disk the contention regimen uses, so results are directly comparable) contains the apk
> stable toolchain: `/usr/bin/rustc` (134 KB driver shim), `/usr/lib/librustc_driver-209dcae0deb659d4.so`
> (63 MB — this is the big mmap that makes rustc startup ext2-bound), `/usr/bin/cargo`
> (20 MB), `/usr/bin/rustdoc`, plus `cc`/`gcc`. ~3.1 GB free (806,609 free 4 KB blocks).
> `/root` has only `DEVBOX.txt`, so the benchmark crate must be staged.
>
> **Do NOT try to build the Akuma kernel.** The in-VM kernel `cargo build` deadlocks at
> dependency 12/147 on proc-macro2's compiler-probing `build.rs` (cargo futex-blocked,
> orphaned `rustc` probes parked with no syscall). That is a separate known blocker and it
> will eat the session. Use a **dependency-free, proc-macro-free, build.rs-free** crate so
> the measurement is deterministic and can't hit it.
>
> **Harness.** Build `scripts/bkl_rustc_bench/` mirroring `scripts/bkl_smp_regimen/`'s
> structure (`gen_payload.py` / `drive.py` / `analyze.py`) — that harness's conventions are
> load-bearing, reuse them:
> - Stage the crate + a `job.sh` over the host HTTP server on `127.0.0.1:8899`; the guest
>   reaches the host as `10.0.2.2`.
> - `SNAPSHOT=1` on every boot so all cells start from byte-identical disk state.
> - **One kernel binary across all cells** — vary only QEMU `-smp`. Do not rebuild between
>   cells; a rebuild invalidates the comparison.
> - Emit machine-readable timings (`rep,phase,wall_ms`) plus a median, and N≥3 reps per
>   cell. rustc on Akuma is slow and noisy; a single rep is not a measurement.
> - Also run each cell on a `bkl-profile` build so you get **attribution and throughput on
>   the same workload**. That pairing is what the campaign has never had, and it is what
>   tells you which holder to convert to move the wall-clock.
>
> **Workload: `hello.rs`, with and without `std`. No crate, no cargo.** Decided
> deliberately — a bare `rustc` invocation has no dependency graph, no `build.rs`, no
> proc-macro2 exposure, and low enough variance to afford many reps. The std/no_std pair is
> the interesting axis: the std build reads and mmaps far more (`librustc_driver` 63 MB plus
> std metadata) and then forks `cc` to link, while a `#![no_std]` `--emit=metadata`
> invocation does neither. So the pair separates **ext2-read + mmap startup cost** from
> **driver/codegen cost**, and separates **with-linker-exec** from **no-child-exec**. Those
> scale differently under the BKL; conflating them muddies the result.
>
> **The trap to avoid: one `rustc` process cannot show BKL scaling.** A single invocation is
> one process — at SMP=4 there is nothing for it to contend *with*, so that cell measures
> latency (useful, and it is exactly the `openat`/`read`/`mmap` path), not contention. The
> scaling signal requires **N concurrent `rustc` invocations**, which is the same shape the
> existing regimen already uses for `net4`/`read4`. Still just `hello.rs` — no crate needed.
>
> Matrix: `{std, no_std} × {1, 2, 4 concurrent rustc} × SMP={1, 2, 4}`, N≥5 reps per cell
> (they're cheap). The two cells that carry the argument are **SMP=1 × 1-concurrent**
> (serial latency floor) and **SMP=4 × 4-concurrent** (does the BKL eat the parallelism).
> Add a `--emit=metadata` variant to drop the linker exec when you want the child-process
> variable removed.
>
> Suggested sources — keep them byte-identical across all cells and check them into the
> harness:
> ```rust
> // hello_std.rs — full pipeline incl. linking a real binary (forks cc)
> fn main() { println!("hello"); }
> ```
> ```rust
> // hello_nostd.rs — compile with --crate-type=lib --emit=metadata (no linker, no cc exec)
> #![no_std]
> #[panic_handler] fn p(_: &core::panic::PanicInfo) -> ! { loop {} }
> pub fn hello() -> u32 { 42 }
> ```
>
> **Deliverables.**
> 1. `scripts/bkl_rustc_bench/` with a README documenting the exact cells and how to re-run.
> 2. A new `docs/archive/BKL_RUSTC_SCALING_BASELINE.md`: the cell table, the speedup curve,
>    the paired `[BKLPROF]` attribution per cell, and an explicit statement of what would
>    count as Phase 7 success on this metric.
> 3. A row in `docs/README.md`'s task list, and a pointer from
>    `BKL_FINE_GRAINED_LOCKING_PLAN.md` §7.4's success criteria to the curve.
>
> **Report honestly.** If the curve shows the BKL is *not* the bottleneck for this workload
> (e.g. it's ext2-read-bound and cores don't matter), say so plainly — that is a valuable
> result and it would re-order Phase 7's priorities. Do not tune the benchmark until it
> shows what Phase 7 wants to see.
>
> **Harness gotchas that have each cost real debugging time in this repo:**
> - Serial-capture logs contain NUL bytes, so `grep` treats them as binary and silently
>   prints/counts nothing. **Use `awk`.** (`grep -c RECOVERED` returned 0 on a log with 46.)
> - `grep -rn <pat> src crates | head` searches `src/` first, so test-file hits fill the
>   window and production callers get cut off. Drop the `head` or grep the crate directly.
> - Polling host port 2222 is **not** a boot-readiness check — QEMU hostfwd accepts the TCP
>   connection before the guest listener exists. Poll the serial log instead.
> - Nothing else may hold `devbox.img` open (QEMU takes a write lock); check with `lsof`.
> - The devbox sshd execs commands with naive splitting — **no shell compounds** (`;`, `&&`,
>   redirects) in `ssh vm '<cmd>'`. Stage a script and run it.
> - Long `exec` channels get killed by sshd keepalive; use `nohup` + poll for a result file.
> - Never call `job_output`/wait on the QEMU process — it runs forever.

---

## Prompt B — execute Phase 7, starting at 7a

> **Read first:** `docs/archive/BKL_PHASE7_AUDIT.md`, then
> `docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md` §7, then
> `docs/reference/subsystems/locking.md` (the playbook's 7 rules and the "Correctness rules
> learned the hard way" list — every one of those was a real bug, do not rediscover them).
> Then read `docs/archive/BKL_RUSTC_SCALING_BASELINE.md` for the throughput curve you are
> trying to move.
>
> **The framing that matters: Phase 7 is not "delete the BKL."** It is "give six structures
> real locks, then let the BKL wither." The audit's §2 inventory is the actual scope; the
> deletion at the end is bookkeeping. If you find yourself editing
> `rust_sync_el0_handler` in week one, you have misread the phase.
>
> **Start at 7a and stop there for review.** Do not attempt the whole phase in one session.
> 7a is: give `ALARM_QUEUE` (`src/kernel_timer.rs:124`) a real `Spinlock`, and make the
> `critical_section` impl (`kernel_timer.rs:316-354`) per-core instead of using the
> process-global `CS_NESTING`/`CS_SAVED_DAIF` — or drop the `critical_section` dependency
> entirely in favour of the kernel's own `IrqGuard` + a Spinlock. `critical_section` is used
> only in `kernel_timer.rs` (+ `src/tests.rs`), so the blast radius is one file. The payoff:
> `dispatch_irq` for the timer PPI 27 (`src/exceptions.rs:1579` acquires the BKL before it)
> no longer needs the BKL, which is the substance of `irq/sched`'s ~21–23%.
>
> Note *why* the global nesting counter is worse than useless under SMP: core A's `acquire`
> increments the same counter core B's `release` decrements, so a concurrent pair can
> restore DAIF while a critical section is still open. The BKL is currently hiding that.
>
> **Ordering, and why it is not contention rank.** 7a → 7b (`ppoll`/`epoll_*`, which also
> still calls `smoltcp_net::poll()` BKL-held at `src/syscall/poll.rs:925` — the §20
> `netpoll_drain` carve is the precedent) → 7c (re-measure the carved residual; `sys_openat`
> at ~10% for a *converted* syscall means the window starts too late or the re-acquire costs
> more than expected — measurement first, not code) → 7d (`THREAD_CONTEXTS` +
> `Process::context` ownership) → 7e (process table) → 7f (wither). `execve` (~22%) and
> `clone` (~10–13%) outrank all of 7a–7d but go **last**: they have no inner lock, and
> converting them before the process table has a locking story means building on the thing
> that needs replacing.
>
> **For 7f, do not remove the BKL — invert its default** (plan §7.3). Change
> `rust_sync_el0_handler` from "always acquire" to "acquire unless this syscall is on the
> converted list," land the list **empty** (byte-identical behaviour, a no-op commit), then
> move syscalls across one at a time. This keeps every step bisectable, preserves the
> per-syscall kill switch that every prior phase relied on, and makes `KernelLock` /
> `reconcile_for_spsr` / the dropped-window ledger / all five guards *provably* dead code at
> the end.
>
> **`reconcile_for_spsr` and the dropped-window ledger MUST survive the entire traversal.**
> A converted syscall is exactly a permanently-open dropped window, and the ledger's
> invariant is what makes the mixed converted/unconverted state safe. Deleting them early is
> the single most likely way to get this phase wrong. The plan's original task 2 ("remove
> `reconcile_for_spsr` logic") is superseded for this reason.
>
> **Hard don'ts, each with a citation:**
> - **Don't build `PROCESS_TABLE_LOCK`.** `BKL_PROCESS_CARVE_OUT.md` §9.2 rejected it — a
>   new coarse lock held across millisecond-scale work is the anti-pattern
>   `locking.md` warns about. Extend `lookup_process_shared`'s `&self` + `Process::as_lock`
>   pattern (`process/children.rs:341`, already carrying the M5b BKL-free fault path)
>   instead.
> - **Don't treat 7e as an accessor refactor.** `Process` has ~40 fields and locks for about
>   a third; ~25 are plain fields mutated through `&mut Process` (`cwd`, `brk`, `state`,
>   `exit_code`, `context`, …). Group the fields → lock each group or prove it
>   single-writer → *then* convert the ~274 sites. The accessor edit is the tail.
> - **Don't forget the free path.** `unregister_process` (`process/table.rs:63`) drops the
>   `Box`, and peer cores free *other* PIDs' `Process` at `process/mod.rs:1116`/`:1209`/`:241`.
>   `lookup_process`'s stated safety argument covers self-teardown only. Needs epoch/RCU or
>   the cooldown pattern `reclaim_terminated_slots` uses.
> - **Don't trust any percentage in the docs without re-measuring.** This campaign has been
>   wrong twice by asserting instead of measuring: the Phase 0 "~70% scheduler/IRQ" estimate,
>   and Phase 3's "the parent's page tables have no lock" (`as_lock` was right there —
>   §9.1). Re-run the regimen; never compare absolute spin counts across sessions, only
>   shares and ranks within one run.
>
> **The bar for each sub-phase** (all three, not a subset):
> 1. Host tests for the lock logic in the owning crate (deterministic; the ticket-accounting
>    bug was caught this way in seconds after weeks of it hiding in logs).
> 2. A **boot self-test in `src/process_tests.rs`** driving the real entry point — kernel
>    changes need kernel tests, not just e2e checks. Follow `test_mm_bkl_drop` /
>    `test_drivers_bkl_drop` for shape, and `test_no_bkl_ticket_recoveries` for the
>    counter-tripwire shape.
> 3. A same-binary A/B at SMP=4 on the contention regimen (toggle **in source**, keep the
>    feature set byte-identical — playbook rule 5), reporting: `[BKLPROF]` shares over the
>    *workload windows* via `analyze_workload.py --auto`, 6/6 digests exact, and **0**
>    `[BKL] stuck` / `RECOVERED` / PANIC / WILD / SPURIOUS / stale dropped-window heals.
>    Plus the Prompt-A scaling cell for anything expected to move throughput.
>
> `RECOVERED` is now a test assertion, not a log line — `sync::kernel_lock_recoveries()` +
> `test_no_bkl_ticket_recoveries`. If it goes non-zero, you broke the acquire/`now_serving`
> pairing; that counter exists because 46-per-workload-window sat in logs looking benign for
> days. Note the *opposite*-sign wedge (`next_ticket == now_serving + 5`, all cores spinning,
> `owner == 0`) noted at `crates/akuma-exec/src/sync.rs` ~line 490 is a **separate and still
> open** bug — if you see it, that's the one.
>
> **Working conventions for this repo** (from CLAUDE.md and standing feedback):
> - **Never `git commit` or `push`** — parent repo or submodules. Leave work uncommitted.
>   Run clippy (all three configs: `--release`, `release-smp-shared --features smp-shared`,
>   `release-smp-shared --features devbox-smoltcp,no-tests,bkl-profile`) and the host test
>   suite so the user's commit is clean.
> - Use `crate::safe_print!` (heap-free, secondary-safe), not `console::print`.
> - No milestone tags (`R1`, `M5c`, `7a`) in identifiers — descriptive names, milestone in
>   comments only.
> - Don't propose edits to `CLAUDE.md`; doc updates go in `docs/`.
> - New docs get a "Background" footer linking the `archive/` originals, and a row in the
>   relevant triage matrix (`docs/README.md`, `docs/runbooks/README.md`).
> - Reference docs carry a stability grade (A/B/C) — check it before trusting one.

---

## Background

- [`../archive/BKL_PHASE7_AUDIT.md`](../archive/BKL_PHASE7_AUDIT.md) — the audit these
  prompts execute against.
- [`../archive/BKL_FINE_GRAINED_LOCKING_PLAN.md`](../archive/BKL_FINE_GRAINED_LOCKING_PLAN.md)
  §7 — the replanned phase; §7.3 is the inversion approach.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) — playbook,
  syscall→lock map, and the load-bearing inventory.
- [`selfhost-kernel-build.md`](selfhost-kernel-build.md) — the *other* in-VM Rust build
  (nightly toolchain, separate 8 GB disk). Prompt A deliberately does not use it.
- [`debug-smp.md`](debug-smp.md) — BKL wedge procedures.
