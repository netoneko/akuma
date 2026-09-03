# The unexplained `rustc` SIGSEGV: an IRQ between `msr elr_el1` and `eret`

**Status: ROOT-CAUSED + FIXED, 2026-09-03.**
Class: EL0 entry / exception return. Not memory corruption, not OOM, not the heap leak.

## Symptom

An intermittent `SIGSEGV` in `rustc` during the self-host campaign, roughly once
per 150 s of heavy `fork`/`exec` load. It survived every explanation offered for
it across three sessions:

- a different crate each time (`akuma-entry`, `akuma-mmap`, `akuma-syscalls`), so
  not input-specific;
- `[OOM]=0`, `WILD-DA=0`, `WILD-IA=0`, `no lazy region=0`, `PANIC=0`,
  `PMM-UAF=0`, `PMM-RESURRECT=0`, 2.6 GB PMM free, 2.2 G disk free, kernel heap
  flat at 408 MB — every corruption and pressure tripwire clean;
- reported as *"no kernel fault diagnostic at all"*, which is what made it look
  like a silent OOM fall-through and sent the hunt at the three
  `OOM: fall through to SIGSEGV` paths.

It was also blamed, provisionally, on the `drain_retired` terminal gate landed in
the same session — on the theory that delaying retired-process collection had
moved a latent race. **That was wrong, and the A/B to check it was not needed.**
The defect predates every change in that session.

## What actually cracked it

The diagnostic was in the log the whole time. The grep set — `WILD-DA`,
`WILD-IA`, `OOM`, `PANIC`, `PMM-*` — did not include `[Fault]`, which is the
string the fatal path prints:

```
[signal] sig 11 needs sigaltstack but slot 35 has none — re-pending
[IA] pid=2268 far=0x40103930 iss=0xe
[Fault] Instruction abort from EL0 at FAR=0x40103930, ISS=0xe
[Fault]  x0=0x0 x1=0x1458dd630 x2=0x1458dd680 x3=0x1458dd728
[Fault]  x19=0x300c2000 x20=0x206000 x29=0x3e313d50 x30=0x30062b3c
[Fault]  SP_EL0=0x1458dd630 ELR=0x40103930 SPSR=0x0
[Fault] Process 2275 (rustc) SIGSEGV after 0.00s
```

Three facts in that block name the bug outright:

1. **It is an INSTRUCTION abort** (`EC_INST_ABORT_LOWER`), not a data abort. Every
   grep in the hunt was for data-abort diagnostics.
2. **`FAR == ELR == 0x40103930`, a kernel address.** `ISS=0xe` is
   `IFSC=0b001110` — permission fault, level 2 — i.e. the page is mapped but not
   EL0-executable. Userspace was executing kernel text.
3. **`SPSR=0x0`** — EL0t, the *user* SPSR. So the `eret` used a **kernel** `ELR_EL1`
   with a **user** `SPSR_EL1`. That combination is the whole diagnosis.

Resolving the address against the running binary:

```
$ llvm-nm --defined-only -n target/aarch64-unknown-none/release/akuma
40103768 T akuma_el0_entry::enter_user_mode
40103980 T akuma_el0_entry::enter_user_mode_checked
```

`0x40103930` is `enter_user_mode + 0x1c8`, and disassembly says exactly which
instruction:

```asm
40103918: msr  DAIF, x21        ; leave_kernel() restores caller DAIF -> IRQs ENABLED
4010391c: ldr  x8, [x19, #0xf8]
40103920: mov  x30, x19
40103924: ldp  x9, x10, [x19, #0x108]
40103928: msr  SP_EL0, x8
4010392c: msr  ELR_EL1, x20     ; user PC written
40103930: msr  SPSR_EL1, x9     ; <-- FAR/ELR point HERE
40103934: msr  TPIDR_EL0, x10
40103938: ldp  x0, x1, [x30]    ; ... 16 loads ...
40103978: eret
```

## Root cause

`eret` reads `ELR_EL1` and `SPSR_EL1` **live**, at the moment it executes. Any
exception taken between writing them and the `eret` overwrites `ELR_EL1` with the
kernel PC it interrupted, and nothing downstream can tell that the user PC is
gone.

`enter_user_mode` (`crates/akuma-el0-entry/src/lib.rs`) ran that whole sequence
with **IRQs enabled**. It is reached from ordinary kernel code — initial process
launch and `execve` — not from a trap, so it does not inherit an exception
entry's mask; and `akuma_bkl::bkl::leave_kernel()`, called immediately above the
`asm!` block, ends by *restoring the caller's DAIF* (`msr DAIF, x21` at
`0x40103918`), which re-enables IRQs on every path into the block.

The observed crash is the narrow window — one instruction wide:

```
msr elr_el1, x20    ; ELR_EL1 = user PC
<TIMER IRQ>         ; ELR_EL1 := 0x40103930 (the next insn), SPSR_EL1 := EL1h
msr spsr_el1, x9    ; SPSR_EL1 := 0 (EL0t) — overwrites the IRQ's EL1h
...                 ; GPR restore
eret                ; PC := ELR_EL1 = 0x40103930, PSTATE := EL0t
```

EL0 then fetches an instruction from kernel text → instruction abort, permission
fault, `FAR == ELR ==` the address of the `msr spsr_el1` in that very block. That
reproduces all three observables, including `SPSR=0x0`: the user SPSR write lands
*after* the IRQ, so the SPSR is repaired while the ELR is not.

**The wider window is worse.** An IRQ after `msr spsr_el1` but before the `eret`
(18 instructions, so ~18x more likely) leaves `SPSR_EL1 = EL1h` as well. The
`eret` then returns **to EL1** at that kernel PC with `DAIF.I` masked, landing back
in the register-restore tail, which runs to the same `eret` again with `ELR_EL1`
unchanged — an uninterruptible EL1 loop. That is a silent hang at 100 % CPU with
no console output and no signal, not a SIGSEGV. Any unexplained wedge on a
fork/exec-heavy workload is a candidate for this arm.

### Why it looked like it had no diagnostic

Two independent reasons, worth separating:

- **`try_deliver_signal` runs BEFORE every `[Fault]` print**, on both the data-
  and instruction-abort paths. Rust's std installs a `SIGSEGV`/`SIGBUS` handler in
  every process (for stack-overflow reporting), so for a *mature* Rust process the
  kernel delivers the signal and prints **nothing at all**. This crash printed only
  because it hit `rustc` **`after 0.00s`** — a freshly-`execve`'d process entering
  userspace for the very first time, before std had installed anything.
- Because the process dies at its first instruction, there is no corruption to
  find anywhere: the fault happens *before* the program runs. Every
  memory-integrity tripwire is correctly silent.

### Two independent corroborations from the same logs

**1. `[EUM POISON]` never fired — 0 hits in every log in `logs/`.** That tripwire
prints whenever `enter_user_mode` is handed a `ctx.pc >= 0x4000_0000`, so the
kernel address in `ELR_EL1` was *not* in the context: it appeared **after**
`enter_user_mode` read `ctx.pc` into `x20`. The only thing between that read and
the `eret` is this window. It also rules out the "poison minted upstream in
`update_thread_context`" hypothesis the tripwire was originally added for.

**2. The signature is old, and it recurs at a handful of fixed addresses.** Six
occurrences across five logs, every one an EL0 instruction abort at a kernel VA
with `iss=0xe`:

| log | date | hits |
|---|---|---|
| `base_verify.log` | 2026-08-15 | 1 |
| `exp_live8_boot.log` | 2026-09-03 | 1 |
| `campaign2g_v2.log` | 2026-09-03 | 1 |
| `campaign_final.log` | 2026-09-03 | 1 |
| `campaign4g_fixed.log` | 2026-09-03 | 2 |

Addresses: `0x40105398` (three times, three different pids in three different
logs), `0x40103930`, `0x40103df4`, `0x401f93f4`. A *small set* of repeated kernel
addresses is the signature's tell — each is the instruction following a
`msr ELR_EL1` in whichever build produced that log, so it repeats exactly within a
build and moves between builds.

**The 2026-08-15 hit settles the attribution question outright**: the defect is at
least 19 days older than the `drain_retired` terminal gate and the
`CACHE_CHUNK_BYTES` change that were provisionally suspected of having made it
more frequent. The planned A/B against the pre-change commit is unnecessary.

Grep for it in any log with:

```
grep -aE "\[IA\] pid=[0-9]+ far=0x4[0-9a-f]{7}" *.log
```

### Why the frequency looked load-dependent

The window is two instructions of a path taken once per `exec`. A `cargo clean` +
full rebuild is tens of thousands of `exec`s against a 1 ms timer tick on 4 cores,
which is exactly the regime that turns a ~1 ns window into a once-per-few-minutes
event. Nothing about the earlier heap or drain changes moved it.

## The fix

`crates/akuma-el0-entry/src/lib.rs` — mask IRQs as the **first instruction** of the
`asm!` block, ahead of `msr elr_el1`:

```asm
msr daifset, #2
isb
```

`#2` (the I bit) matches `msr daifset, #2` in the SVC epilogue in
`crates/akuma-exceptions/src/lib.rs`, the sibling EL0 return that always had it —
it enables IRQs for the handler with `msr daifclr, #2` and re-masks them before
touching `SPSR_EL1`/`ELR_EL1`. FIQ and SError are routed to
`default_exception_handler` and unused. The mask does not leak past the `eret`:
`eret` restores PSTATE from `SPSR_EL1`, and every context this kernel builds sets
`spsr = 0`, so userspace resumes with interrupts unmasked.

It must come **after** `leave_kernel()`, not before — that call ends with a DAIF
*restore* from a saved register, which would undo a mask placed ahead of it.

Verified in the emitted binary rather than the source:

```asm
4021c634: msr  DAIF, x21      ; leave_kernel()'s restore
4021c644: msr  DAIFSet, #0x2  ; the fix
4021c648: isb
4021c64c: msr  SP_EL0, x8
4021c650: msr  ELR_EL1, x20   ; now inside the masked region
4021c654: msr  SPSR_EL1, x9
4021c69c: eret
```

`enter_user_mode` was the **only** `eret` in the tree without the mask. The tree
has six: this one `asm!` `eret` plus five in the vector table's `global_asm!` in
`crates/akuma-exceptions/src/lib.rs`. Audited mechanically by walking back from
each `eret` to its label, collecting every `daifclr`/`daifset`/`msr daif` and every
`msr elr_el1`/`msr spsr_el1`:

| `eret` | DAIF writes between label and `eret` | verdict |
|---|---|---|
| `default_exception_handler` | none | masked from hardware entry throughout |
| `sync_el1_handler` | none | ditto (rewrites `SPSR` only, not `ELR`) |
| `sync_el0_handler` | `daifclr #2` → handler → `daifset #2` → restore `SPSR`/`ELR` | correct, and the model for the fix |
| `irq_el0_handler` | none | masked throughout |
| `irq_handler` (EL1) | none | masked throughout |
| `enter_user_mode` | **none, and not entered from a trap** | **the defect** |

The four "none" rows are safe precisely *because* an exception entry masks DAIF in
hardware and they never unmask. `enter_user_mode` is the one EL0 return reached
from ordinary kernel code, so it inherits nothing — and `leave_kernel()` actively
*restores* the caller's DAIF right above it.

## Guards added

**1. `test_el0_eret_masks_irqs`** (`src/process_tests.rs`, boot suite, not
SMP-gated — the tick that wins the race exists at SMP=1). The race is two
instructions wide and cannot be provoked on demand, so the test asserts the
*invariant* against the emitted instruction stream: walk `enter_user_mode` to its
first `msr ELR_EL1, xN`, tracking `PSTATE.I` across every DAIF write
(`DAIFSet`/`DAIFClr` immediate forms and the `msr DAIF, xN` register restore),
and require I to be masked there and still masked at the `eret`. It holds however
the compiler schedules the block, and reports **INCONCLUSIVE** rather than FAILED
if the `msr ELR_EL1` anchor is absent, so a codegen change that relocates the
`asm!` makes the check inapplicable instead of falsely red.

**The test was verified against the defect, not only against the fix.** Its first
version PASSED on a mask-removed build — vacuously, because the `msr DAIF, xN`
encoding constant was wrong: `DAIF` is `S3_3_C4_C2_`**`1`** (op2=1), so the base is
`0xd51b4220`, while `0xd51b4200` (op2=0) is `NZCV`. With that constant the
`leave_kernel()` restore went undetected, I read as still-masked at the
`msr ELR_EL1`, and the pre-fix kernel passed. Corrected, the mask-removed build
reports:

```
[Test] el0_eret_masks_irqs FAILED: msr ELR_EL1 at word 113 with I masked=false, still masked at eret=false
```

word 113 being `+0x1c4`, the `msr ELR_EL1` the disassembly names. **Boot the
mask-removed build and require FAILED before trusting this test** — the same
lesson as `SMP_ADOPTED_IDLE_SLOT_CLOBBER.md`, where boot-suite stack assertions
were vacuous for a comparable reason. A linear scan also cannot see a callee's
DAIF writes, so `bl`/`blr` counts as a possible unmask: the mask must be the last
DAIF-relevant instruction before `msr ELR_EL1`, calls included.

**2. `[ERET-CLOBBER]`** (`crates/akuma-exceptions/src/lib.rs`, EL0 instruction-abort
handler). Printed **unconditionally and before the signal-delivery attempt** —
which is precisely where this class went dark — whenever an EL0 instruction fetch
faults at an address inside the kernel identity range:

```
[ERET-CLOBBER] pid=… as_owner=… EL0 fetch at kernel VA far=0x… elr=0x… iss=0x… far_eq_elr=true — eret used a kernel ELR_EL1
```

No user mapping is ever placed in that range (`ProcessMemory` refuses allocations
overlapping it), so a userspace PC there cannot be a value userspace computed: it
came from an `eret` that used a kernel `ELR_EL1`. `far_eq_elr` separates this
shape (the PC *is* the faulting address) from a wild indirect branch that merely
happens to target the range.

## Verification, and the SECOND class it exposed

Post-fix, the user's own reproducer was run against a 2 GB `devbox-smoltcp` VM:

```
free -h && cargo clean && ./scripts/build_devbox_smoltcp.sh
  from /src/github.com/netoneko/akuma
```

Result over 7 iterations: **`ERET-CLOBBER=0`, kernel-VA instruction aborts = 0** —
this defect's signature is gone. Iterations 1-2 built clean (52 s, 58 s).
(Iteration 7 ended in an ssh-level error that coincided with the harness tearing
the VM down, so it is not evidence either way.)

**But iterations 3-6 still failed, from a different cause.** Anyone
picking this up must not read that as the fix having failed. The two are
distinguishable at a glance, and the distinction is the whole point of adding a
resolved-address print:

| | this defect (FIXED) | the remaining class (OPEN) |
|---|---|---|
| `ISS` | `0xe` — permission fault, L2 | `0x6` — translation fault, L2 |
| `FAR` | **kernel** VA (`0x4010…`), inside the EL0-return sequence | **user** VA (`0x6024704`, `0x327aae0`, `0x8c5720`) |
| `WILD-IA` | silent (the VA is not a user mapping) | **fires**, with `[DP] no lazy region for inst FAR=` |
| age of process | `after 0.00s` — dies at its first instruction | `after 0.15s` / `4.54s` — dies mid-run |
| cause | kernel clobbered `ELR_EL1` | userspace branched through a garbage pointer |

**Correction — iteration 6 was memory exhaustion, not file corruption.** The first
reading of this record blamed `cc: fatal error: cannot execute '.../collect2':
posix_spawn: Exec format error` on bad file content. The console says otherwise,
two lines earlier:

```
[PSTATS] PID 4443 (.../collect2) 0.00s: 18 syscalls  pmm=333free/524288tot
[syscall] execve: replace_image failed for .../collect2: Failed to load ELF: Mapping…
```

**`pmm=333free/524288tot`** — 333 free physical pages, 1.3 MB of a 2 GB box. The
ELF load could not map the image, `execve` returned the failure, and gcc reported
it as `Exec format error`. The binary on disk was fine. Anything that presents as
`Exec format error`, a failed `execve`, or an `[OOM]` on this box should be checked
against `pmm=…free` in the nearest `[PSTATS]` line **before** being called
corruption.

The `[OOM]` in the same window has the same cause rather than being a heap-cap or
fragmentation problem: `[ALLOC FAIL] requested=2370248 heap_total=289MB
heap_used=279MB (96%)` failed because the heap grows *from PMM*, and PMM had 333
pages. It is not independent evidence about `CACHE_CHUNK_BYTES`.

What remains genuinely unexplained is narrower: the wild **indirect branch**
(iteration 3's `[WILD-IA] FAR=0x6024704` with `[DP] no lazy region for inst`, and
iteration 5's `ld terminated with signal 11`). Those may themselves be OOM in
disguise — the three `OOM: fall through to SIGSEGV` paths deliver a signal and
print nothing — which is exactly why those three sites deserve the print. Four
`[E2-EOF]` lines are a separate, small signal. `PMM-UAF=0`,
`PMM-RESURRECT=0`, `PANIC=0` throughout.

The remaining class is the already-open garbage-function-pointer family
(`CARGO_HEAP_NULL_RC.md`, `TRAMPOLINE_STALE_PROCESS_RELR.md`, and `SELFHOST_ZERO_PAGE_HUNT.md`). One **new** datum for that hunt, from the `SPSR` this run
captured:

```
[Fault]  SP_EL0=0x3e315360 ELR=0x6024704 SPSR=0x80000800
[Fault]  x19=0xcce27a30 x20=0x4a578eabd4b9748 x29=0xccdd8200 x30=0x33a98664
```

`SPSR` bits[11:10] = `0b10` is `PSTATE.BTYPE` = **BLR** — so the faulting PC was
reached by an indirect *call* through a register, not by a corrupted `ret` (which
leaves BTYPE `0b00`) and not by falling off the end of a function. That narrows it
to a corrupted function pointer — vtable, GOT or closure slot — and `x30` holding a
valid return address corroborates it. Note also `x20=0x04a578ea_bd4b9748` against
`x3=0xbd4b9748`: `x3` is exactly `x20`'s low 32 bits, the shape of a pointer read
out of a misaligned or wrong-width slot. Not chased here.

Disk and memory were ruled out as the cause of those three: 1.05 GB free (83 %
used) and 600 MB free RAM at the time. Worth noting the `du -sx /src` = 331 MB vs
`df` used = 5.2 GB gap, which is the separate ext2 unlink leak
(`EXT2_UNLINK_INODE_BLOCK_LEAK.md`) — it will eventually turn this same loop into
ENOSPC, so `e2fsck` the image before drawing conclusions from a long campaign.

## Loose ends, deliberately not chased here

- **`[signal] sig 11 needs sigaltstack but slot 35 has none — re-pending`**, one
  line above the crash, is a *different* thread's event (slot 35 vs. the faulting
  process). Unrelated to this root cause; worth its own look.
- **The `[Fault]` block is unreachable for any process with a handler
  registered** — i.e. every mature Rust binary. This crash was visible only by the
  accident of dying at 0.00 s. A one-line unconditional pre-delivery print on the
  SIGSEGV paths (the shape `[ERET-CLOBBER]` now has) would end that blindness in
  general; not done here to keep this change to the defect.

## Not the bug: the `SIGILL` storm

`cargo clean` prints bursts like

```
[Exception] Unknown from EL0: EC=0x0,  ISS=0x0 ELR=0x112b2b60 — delivering SIGILL
[Exception] Unknown from EL0: EC=0x1d, ISS=0x0 ELR=0x112b2b48 — delivering SIGILL
```

These are **benign** and were a red herring recorded as undiagnosed. They are
userspace **CPU feature probes**: code that deliberately executes an unsupported
instruction inside a registered `SIGILL` handler to detect support (OpenSSL's
`OPENSSL_cpuid_setup` armcaps, statically linked into nightly cargo via its
git/curl stack). Two independent confirmations:

- The print sits **before** `try_deliver_signal`, and the fatal path prints two
  further lines (`Thread=…`, `TTBR0=…`). Bursts with nothing between them were all
  delivered to a handler, and the process continued.
- `[EXCC]` counts them for the whole boot: `e0.0x0=12 e0.0x1d=8` in 180 s, against
  `e0.0x15=24487885` SVCs. Twenty events is a probe sequence, not a storm.

`EC=0x1d` is SME access trapped — on Apple Silicon under HVF the CPU advertises
SME while the kernel leaves `CPACR_EL1.SMEN` clear, so an SME probe traps rather
than being undefined. `EC=0x0` is the genuinely-unallocated encodings (SVE etc.).

## Background

- Crash log: `logs/campaign_final.log` (lines 940-945), `logs/campaign2g_v2.log`,
  `logs/campaign4g_fixed.log`.
- Sibling EL0 return that always masked: `crates/akuma-exceptions/src/lib.rs`,
  `sync_el0_handler` epilogue.
- The heap leak fixed in the same session, and correctly: 
  [`SELFHOST_KERNEL_HEAP_LEAK.md`](SELFHOST_KERNEL_HEAP_LEAK.md).
- Earlier members of the "odd ELR / instruction abort" family, now worth
  re-reading against this cause:
  [`TRAMPOLINE_STALE_PROCESS_RELR.md`](TRAMPOLINE_STALE_PROCESS_RELR.md).
