# Rust Toolchain on the Devbox — Open Issues

Tracks open issues found while exercising Rust toolchains on the `devbox`
profile. Two **separate toolchain installs** are in play here, don't conflate
them:

- **Self-hosted nightly** (`aarch64-unknown-linux-musl`, installed under
  `/usr/local` by `overlays/devbox/bootstrap.sh` step 7) — `rustc
  1.98.0-nightly`. §1 below.
- **Alpine apk package** (`aarch64-alpine-linux-musl`, installed under `/usr`
  by `overlays/devbox/bootstrap.sh` step 6 alongside `git`) — `rustc 1.96.0
  (Alpine Linux Rust 1.96.0-r0)`. §2 below.

These are an **investigation report, not a fix** — written so the next
session can pick up either one without re-deriving the repro.

## 1. Self-hosted nightly `cargo` crash (`/usr/local/bin/cargo`)

### Status

| Component | State |
|---|---|
| `rustc --version` | ✅ works |
| `cargo --version` | ❌ crashes the kernel's exception handler; process killed, SSH session gets `Hangup` (exit 129) |
| VM survives the crash | ✅ yes — thread is recycled, subsequent SSH connections still work. Not the same class as the documented curl/DNS EC=0x20 "spins forever" wedge (`overlays/devbox/README.md` Backlog) |
| Root cause | ⚠️ **not confirmed** — best-evidence hypothesis below, needs a targeted repro to confirm |

### Symptom

Over SSH into the devbox (`ssh -p 2223 root@localhost`, `export PATH=/usr/local/bin:$PATH`):

```
$ rustc --version
rustc 1.98.0-nightly (c397dae80 2026-07-02)

$ cargo --version
Hangup
```

Kernel log (QEMU `-serial mon:stdio`) right before the crash:

```
[T65.86] [syscall] execve(path="/usr/local/bin/cargo", args=["cargo", "--version"]) PID 14
[T65.89] [mmap] pid=14 fd=3 file=/usr/lib/libgcc_s.so.1 off=0 len=0x31000 = 0x30100000 (lazy-file, 7 regions)
[T65.89] [mprotect] pid=14 owner=14 addr=0x3012f000 len=0x1000 prot=0x1
[T65.93] [mprotect] pid=14 owner=14 addr=0x11da0000 len=0x150000 prot=0x1
[T66.01] [IA-DP] file region: fault_va=0x1167fc20 seg_va=0x10000000 filesz=0x1d97fac file_off=0x0
[T66.05] [IA-DP] file region: fault_va=0x10ee1ee8 seg_va=0x10000000 filesz=0x1d97fac file_off=0x0
[T66.07] [IA-DP] file region: fault_va=0x11022cd8 seg_va=0x10000000 filesz=0x1d97fac file_off=0x0
[T66.10] [IA-DP] file region: fault_va=0x1101ee40 seg_va=0x10000000 filesz=0x1d97fac file_off=0x0
[Exception] Unknown from EL0: EC=0x0, ISS=0x0
  Thread=12, ELR=0x1101eea0, FAR=0x1101ee40, SPSR=0x800
  TTBR0=0x15000063517000, SP=0x60575830
[Cleanup] Thread 12 recycled after 10056us cooldown
```

`cargo` (dynamically linked, ~47 MB binary at `/usr/local/bin/cargo`) demand-pages
its own text segment via the lazy-file-region mechanism (`IA-DP`), then faults at
the address it just paged in, with ESR exception class `EC=0x0` — architecturally
"Unknown reason". `exceptions.rs`'s dispatch (`src/exceptions.rs` ~line 3467
onward) only special-cases `EC_SVC64`, `EC_DATA_ABORT_LOWER`,
`EC_INST_ABORT_LOWER`, `EC_MSR_MRS_TRAP` (`0x18`), and `EC_BRK_AARCH64`; anything
else — including `0x0` — falls into the catch-all `_ =>` arm at line ~3467, which
logs `[Exception] Unknown from EL0: ...` and kills the process (`return_to_kernel(-1)`).
That catch-all is why the process dies cleanly instead of the kernel wedging.

### Leading hypothesis: QEMU HVF traps physical-timer (`CNTP_*`) register access as a bare `EC=0x0`

`EC=0x0` already has a confirmed, documented precedent **in this exact codebase**,
for a different code path:

- `src/timer.rs` (`read_counter`, ~line 110): *"the physical timer/counter is
  owned by the hypervisor under QEMU HVF and trapping to it faults the guest
  (EC=0x0); the virtual timer works under HVF, TCG, and bare-metal EL1 alike."*
- `src/main.rs` (~line 880): *"The physical timer (CNTP/PPI 30) is not used — it
  is inaccessible to the guest under QEMU HVF (programming it faults with
  EC=0x0)."*

Those are about the **kernel** (EL1) avoiding `CNTP_*` and using `CNTV_*`
instead. The mechanism they describe — QEMU/HVF's nested-virtualization layer
denying access to the physical timer/counter and delivering a bare, unclassified
`ESR_EL1` (both `EC` and `ISS` reading as `0`) rather than the architecturally
normal trap — is not EL-specific. If **userspace** code executes an `MRS`/`MSR`
against a `CNTP_*` register (`CNTP_CTL_EL0`, `CNTP_TVAL_EL0`, `CNTP_CVAL_EL0`,
`CNTPCT_EL0`), the same HVF quirk would plausibly produce the same bare `EC=0x0`
from EL0 — matching our log exactly, including `ISS=0x0` (a real, architecturally
decoded `EC_MSR_MRS_TRAP` would carry non-zero `ISS` fields identifying the
register and destination `Xt`; ours has none, consistent with HVF handing back
an empty `ESR_EL1` rather than a decodable trap).

Supporting detail: `exceptions.rs`'s `EC_MSR_MRS_TRAP` handler (`src/exceptions.rs`
~line 3409) only emulates `CTR_EL0` reads specifically; anything else (including a
real, correctly-classified `CNTPCT_EL0` trap) falls through to `else { 0 }` and
returns a *harmless* dummy `0` — it does **not** crash. That rules out "the kernel
doesn't emulate `CNTP` reads" as the direct cause; the crash requires the
malformed/bare `EC=0x0` path specifically, which bypasses `EC_MSR_MRS_TRAP`
handling entirely and lands in the generic catch-all instead.

**Why `cargo` and not `rustc`:** both are dynamically linked against
`libstd-*.so` and go through the same `PT_INTERP` path (confirmed: a later,
slower `cargo build`-style rustc invocation also mmaps
`libstd-4c2645ca1464cad6.so`), so "dynamic linking" alone isn't the
differentiator. Something `cargo` does early in its own startup (before
`--version` even prints) that `rustc`'s startup doesn't — a timing check,
jobserver/thread setup, or a libgcc_s unwind-info init path — is the most likely
place a stray `CNTP_*` read would come from. Not yet pinned to a specific
instruction/call site.

**Why the documented self-hosting milestone's `cargo build --release` (June
2026, also under HVF — see `docs/AKUMA_SELF_HOSTING.md` line ~160) didn't hit
this:** unconfirmed, but the most likely explanation is toolchain drift, not
environment drift. That milestone pinned whatever nightly was current in June
2026; `overlays/devbox/bootstrap.sh` step 7 downloads whatever nightly is
*currently* live from `static.rust-lang.org` (as of this writing,
`1.98.0-nightly (c397dae80 2026-07-02)`). A newer `cargo` or one of its vendored
dependencies could easily have picked up a new timing/instant call on the
`aarch64-unknown-linux-musl` path between June and July that the older nightly's
`cargo` didn't execute.

### What would confirm/fix this

1. **Confirm the register.** Patch the `_ =>` catch-all in `src/exceptions.rs`
   (~line 3467) to also disassemble/print the 4 bytes at `ELR-4`..`ELR` (the
   trapping instruction itself) before killing the process. An `MRS`/`MSR`
   encoding targeting `CNTP_CTL_EL0` (`op0=3 op1=3 crn=14 crm=2 op2=1`),
   `CNTP_TVAL_EL0` (`crm=2 op2=0`), `CNTP_CVAL_EL0` (`crm=2 op2=2`), or
   `CNTPCT_EL0` (`crn=14 crm=0 op2=1`) would confirm the hypothesis directly.
2. **Cheap experiment:** boot the same devbox image with `-accel tcg` instead of
   HVF (edit `overlays/devbox/run.sh`'s `cargo run` invocation, or run the
   underlying `qemu-system-aarch64` command by hand with `-accel tcg`) and retry
   `cargo --version`. Per the `timer.rs` comment, TCG does *not* have this
   quirk for `CNTP`; if the crash disappears under TCG, that's strong
   confirmation (at the cost of much slower emulation — not a real fix, just a
   diagnostic).
3. **If confirmed:** the real fix is a generic one, not `cargo`-specific — add
   `CNTP_CTL_EL0`/`CNTP_TVAL_EL0`/`CNTP_CVAL_EL0`/`CNTPCT_EL0` to the
   `EC_MSR_MRS_TRAP` emulation table in `exceptions.rs` (return a synthesized
   value, e.g. proxy through the already-working `CNTV_*` equivalents) so any
   userspace binary that happens to probe the physical timer degrades
   gracefully instead of dying — mirroring how the kernel already routes its
   own timer usage through `CNTV` instead of `CNTP`. That only helps if the
   trap is ever classified as `EC_MSR_MRS_TRAP` in the first place; if HVF
   really does hand back a bare `EC=0x0`/`ISS=0x0` with no decodable fields (as
   the evidence above suggests), then the fix may need to live in the `_ =>`
   arm itself: detect "`ELR` points at an `MRS`/`MSR` instruction referencing a
   `CNTP_*` register" by decoding the raw instruction bytes (since `ISS` gives
   nothing to go on here), and handle it the same way `EC_MSR_MRS_TRAP` does.

### Repro

```
cd /Users/netoneko/github.com/netoneko/akuma
./overlays/devbox/run.sh   # boot; wait for "[herd] Started sshd" in the log
ssh-keygen -R "[localhost]:2223"   # only needed after a rebuild (new host key)
ssh -o StrictHostKeyChecking=no -p 2223 root@localhost
export PATH=/usr/local/bin:$PATH
cargo --version   # crashes; rustc --version works fine
```

**Caution:** the devbox VM is single-core (`-smp 1`) and 4 GB RAM. `rustc` alone
needs ~2 GB+ per `docs/RUST_TOOLCHAIN.md`. Running two compiler invocations
concurrently (e.g. a hung SSH client retried while the first invocation is still
running server-side) can starve `sshd` of CPU entirely and make the VM look
wedged even though it isn't crashed — observed firsthand during this
investigation. Wait for one SSH command to fully return before starting
another, and use a generous client-side timeout (rustc/cargo invocations can
legitimately take minutes on this hardware).

## 2. Alpine apk `rustc` — Scudo heap corruption during release/LTO build

Found 2026-07-05 while verifying the fork/socketpair fixes below against a
real crate ([netoneko/teddy](https://github.com/netoneko/teddy)) with the
**Alpine apk toolchain** (`/usr/bin/rustc`, `1.96.0 (Alpine Linux Rust
1.96.0-r0)`, `/usr/bin/cargo` — installed by `overlays/devbox/bootstrap.sh`
step 6, *not* the self-hosted nightly in §1 above). This is a **separate
toolchain and a separate symptom** from the §1 `cargo --version` crash — do
not conflate them.

### Status

| Component | State |
|---|---|
| `git clone` (real repo, over rump DNS) | ✅ works — see `docs/OPTIONAL_SMOLTCP.md` |
| `cargo build` (debug, no LTO) | ✅ got well into compiling the crate's own binaries (only warnings) before the session ended; not confirmed complete |
| `cargo build --release` (LTO + `codegen-units=1`) | ❌ `rustc` aborts (`SIGABRT`) compiling a dependency (`rustix`) with a Scudo allocator integrity error |
| Root cause | ⚠️ **not investigated** — found at the very end of a long debugging session, deferred to next time |

### Symptom

```
Scudo ERROR: corrupted chunk header at address 0x000170804610: chunk header is
zero and might indicate memory corruption or a double free
error: could not compile `rustix` (lib)

Caused by:
  process didn't exit successfully: `rustc --crate-name rustix --edition=2021
  ... -C opt-level=z -C linker-plugin-lto -C codegen-units=1 ...`
  (signal: 6, SIGABRT: process abort signal)
```

Scudo is Alpine/musl's hardened heap allocator; "corrupted chunk header" is its
own internal consistency check firing, not a kernel-reported fault — this is
`rustc` itself corrupting its own heap (or Scudo mis-detecting corruption)
partway through LTO codegen for `rustix`.

### What's known so far

- **Not the fork/socketpair bugs** fixed the same session (see
  `docs/OPTIONAL_SMOLTCP.md`'s socketpair/CLOEXEC sections) — those are
  confirmed fixed independently: a minimal `Command::new("/bin/true").output()`
  repro, and `rustc` invoking its own linker directly, both complete cleanly
  with no panic.
- ~~**Likely LTO/codegen-units=1-specific.**~~ **DISPROVEN 2026-07-05.** With
  the box bumped to 6 GB (`DEVBOX_MEMORY=6144`), a plain `cargo build` (debug,
  no LTO) **also** aborts with the same Scudo corruption compiling `rustix` —
  same "chunk header is zero" signature. The earlier session's "debug got much
  further" was a misread: that run was killed before it ever reached `rustix`,
  not because debug was immune. So this is **not** LTO-specific and **not**
  memory-pressure (6 GB free, kernel `pmm` reports ~5.4 GB free at crash time).
- **Non-deterministic.** On a second run the debug build compiled `rustix`
  cleanly and got into `teddy`'s own binaries before the session hit the
  unrelated §3 freeze. So the corruption is a race, not a guaranteed fault.
- **Not yet root-caused.** With LTO and memory pressure ruled out, remaining
  candidates: a genuine Scudo/musl bug, or a subtle kernel memory-safety bug
  (CoW / brk / mmap) that intermittently zeroes a userspace heap page so
  Scudo's header reads as zero. No kernel-side panic or exception was logged
  before the abort. The "chunk header is zero" signature specifically is
  consistent with a page that should hold dirty allocator metadata being
  served as a fresh zero page — worth a targeted look at the brk/CoW paths.
  **Note:** this is now lower priority than §3 — the §3 freeze is the actual
  build blocker (you can't finish a compile if the box wedges first).

### CPU-starvation caveat — now believed to be §3, not starvation

~~While waiting on the debug (non-LTO) build, the devbox VM's console output
went silent for 13+ minutes while the QEMU host process stayed pegged at
~100% CPU…~~ **Re-investigated 2026-07-05:** the "silent VM at 100% CPU"
symptom is **not** scheduling starvation — it is a hard timer/scheduler freeze
(see §3 below). The original session's interpretation ("concurrent CPU-bound
work starves sshd") was wrong; it just happened to coincide with a build. With
diagnostic `[TMR]` logging added (see §3), the freeze is reproducible on
demand: the timer IRQ stops firing entirely mid-`execve`, and the box does not
recover.

### Next steps

1. **§3 is fixed (appears) — no longer the blocker.** The `teddy` debug and
   release builds now complete without freezing (6+ stress runs verified).
2. **Scudo §2 workaround confirmed:** `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
   CARGO_PROFILE_RELEASE_LTO=false cargo build --release` completes cleanly
   (2m 29s, 594 KB `teddy` binary that runs). The debug build also completes
   cleanly when Scudo doesn't race-hit (3× verified). The corruption remains
   non-deterministic with the stock `lto=true, codegen-units=1` profile, so
   the workaround is the practical path until §2 is root-caused.
3. **Root-causing §2** is now lower priority — the workaround unblocks real
   development. If revisited: the "chunk header is zero" signature points at
   a page that should hold dirty allocator metadata being served as a fresh
   zero page; audit the brk expansion and CoW paths for a race that zeroes a
   live heap page under heavy allocation.

## 3. Scheduler/timer freeze during `execve` under load — APPEARS RESOLVED

Found 2026-07-05 while retrying the §2 build with `DEVBOX_MEMORY=6144`. This is
a **kernel bug**, not a toolchain bug, and it was what actually prevented
compiling `teddy` to completion.

### Status

| Component | State |
|---|---|
| Symptom | VM goes 100%-CPU busy-spin, fully unresponsive (SSH banner-exchange timeout), never recovers |
| Timer IRQ | ❌ **stops firing entirely** at the freeze point (before the fix) |
| Preemption-disable leak | ✅ ruled out — `p=0` on every `[TMR]` line up to the freeze |
| Fix applied | `sgi_scheduler_handler_with_sp` uses `POOL.try_lock()` instead of blocking `POOL.lock()` (`threading/mod.rs:2257`) — an IRQ-context spinlock deadlock can no longer hang the box |
| Verified | ✅ **6+ stress runs with zero freezes**: 3× clean debug builds, 1× release build (LTO), 1× release build (codegen-units=16), 1× 300-spawn stress loop, all with aggressive concurrent SSH polling. Timer stayed alive throughout. |
| Root cause certainty | ⚠️ **medium** — the try_lock never actually contended (`SGI_skips=0` across all runs), so the exact freeze mechanism is not 100% confirmed. The fix is a correct safety improvement regardless (preventing IRQ-context spinlock deadlock is sound even if it wasn't the precise trigger). The freeze may have been a timing-sensitive race that the slightly different code path shifted off. |

### Smoking-gun evidence

Added diagnostic `[TMR]` logging to `src/timer.rs`'s tick handler: interval
bumped from 200 000 ticks (~33 min, useless) to 1 000 ticks (10 s), and the
line now also prints `p=<preemption-disabled-count>` for the current thread via
the new `threading::preemption_disabled_count(tid)` helper. Reproduced with
`teddy`'s debug build (deps cached → jumps straight to the heavy phase):

```
[TMR] t=17000 T=23 p=0 f=0     ← timer still alive, thread 23, preemption enabled
[TMR] t=18000 T=18 p=0 f=0     ← last TMR line ever printed
...                             ← timer never fires again; VM frozen
```

Last kernel log lines at the freeze (QEMU host at 100% CPU, not WFI — a
busy-spin in EL1):

```
[T189.47] [syscall] execve(path="/bin/busybox", args=["cat", "/root/build_done"]) PID 395
[FORK-DBG] replace_image: loading ELF
[FORK-DBG] replace_image: ELF loaded, deactivating old AS
[FORK-DBG] replace_image: deactivating
[FORK-DBG] replace_image: swapping AS
[FORK-DBG] replace_image: AS swapped     ← LAST LINE; timer dies here
```

Key facts, all confirmed against the host log:

- The freeze lands **inside `replace_image`** (execve), right after the
  address-space swap (`crates/akuma-exec/src/process/image.rs:44`). It is
  triggered by **concurrent process spawning** — in the repro it was a trivial
  polling `cat`/`tail` SSH command racing with the build's own rustc spawns.
  Many execves complete fine; one eventually wedges.
- **`p=0`** on every TMR line: preemption was never disabled when the timer
  was still firing. This is **not** a leaked `disable_preemption()` (the
  original hypothesis).
- **QEMU stays at 100% CPU**: the guest is in a tight spin, not `wfi`. And
  since `p=0`, it isn't a thread that just never yields — it's the timer IRQ
  itself no longer being delivered/handled.
- **Single-core (`-smp 1`)**: the thread that the timer interrupts is the same
  thread that holds whatever lock the timer/SGI handler wants.

### Leading hypothesis: spinlock deadlock — timer IRQ fires while execve holds `POOL`

The timer path is a two-trap sequence per tick:
1. PPI 27 (timer) → `rust_irq_handler_with_sp` → `dispatch_irq(27)` →
   `timer_irq_handler` → re-arms CNTV, then `trigger_sgi(SGI_SCHEDULER)`.
2. SGI 0 → `rust_irq_handler_with_sp` → `sgi_scheduler_handler_with_sp` →
   **`POOL.lock()`** (`threading/mod.rs:2257`).

On AArch64, IRQs are masked on exception entry. So while inside step 2's
`sgi_scheduler_handler_with_sp`, **PSTATE.I is set** — no further IRQs land
until the handler returns / ERETs. If that handler spins on `POOL.lock()`,
it spins with IRQs masked, and the next timer tick is never delivered.
That matches "timer stops firing" + "100% CPU busy-spin" exactly.

The deadlock requires the **same single core's** current thread to already
hold `POOL` when the timer fires. `do_execve` (`src/syscall/proc.rs:597`) does
**not** wrap `replace_image` in `disable_preemption()`, so a timer tick can
land mid-execve. The question to confirm next: does anything on the execve
path (or a nested fault it takes while swapping the AS — e.g. a demand-page
fault on the new ELF that re-enters the scheduler) hold `POOL.lock()` at the
moment the timer fires?

Notably the codebase is already aware of this class: `threading/mod.rs:316`
("Taking `POOL.lock()` there self-deadlocks the single CPU if the …") and
`threading/mod.rs:2632` ("where taking `POOL.lock()` could self-deadlock a
nested fault") are existing comments warning about exactly this pattern. The
execve path looks like a third instance that wasn't covered.

### What's already ruled out

- **Preemption-disable leak** — `p=0` on every TMR line up to the freeze.
- **CNTV register corruption** — `timer_irq_handler` defensively re-enables
  `CNTV_CTL_EL0` on every tick (`src/timer.rs:58-62`); the freeze is upstream
  of that (the handler stops running), not a dead timer register.
- **The build itself / rustc** — the repro freeze was a `cat`/`tail`
  polling command's execve, not rustc. The build just creates the load that
  makes the execve race window hit.
- **Memory pressure** — 6 GB box, ~5.4 GB free at freeze.

### Fix applied

Made `sgi_scheduler_handler_with_sp`'s `POOL` acquisition a `try_lock`
(`threading/mod.rs:2257`): if POOL is already held, the handler returns 0 ("no
switch this tick") instead of spinning. This is sound because the scheduler SGI
is best-effort preemption — skipping one 10 ms tick just means the current
thread runs slightly longer; the next tick retries. An IRQ-context spinlock
deadlock (timer interrupts the POOL holder on the same single core → SGI handler
spins on POOL with IRQs masked → timer never fires again) can no longer hang the
box. A rate-limited `[SGI] POOL contended, skipped N ticks (tid=T)` log fires
every 1000 skips so a future contention hotspot is visible.

**Honest caveat:** across 6+ post-fix stress runs the try_lock **never
contended** (`SGI_skips=0`), so the try_lock is not provably the thing that
fixed it — the freeze may have been a timing-sensitive race shifted by the
slightly different code path, the extra `[TMR]` serial I/O, or simple
non-determinism. The change is kept regardless: it is a correct robustness
improvement (IRQ-context spinlock deadlock prevention) independent of whether
it was the exact trigger.

### Remaining work (lower priority)

- **Pin the exact race.** The try_lock skip counter never fired, so the true
  freeze mechanism is still unidentified. If the freeze recurs, add an
  instrumentation print at the top of `sgi_scheduler_handler_with_sp` (before
  POOL) to confirm the handler is even entered, and check whether
  `kernel_timer::on_timer_interrupt()` (called earlier in the timer IRQ path)
  holds a lock that execve also needs.
- Consider whether the two-trap-per-tick design (timer PPI 27 → SGI 0) is
  necessary on a single-core box — doing the scheduling decision inline in the
  timer IRQ handler would halve the IRQ-masked windows.

