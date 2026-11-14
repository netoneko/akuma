# PHASE-2-NULLSTORE — research (R1, written before any code)

**Gate:** PHASE-2-NULLSTORE — G9 (`denver/docs/REQUIREMENTS.md`): the merged tree's
`java -version` dies mid-init with `[Fault] ring3 PageFault rip=0x000000003005fc41
rsp=0x00006fffffffba60 cr2=0x0 code=0x6` (user-mode write to the NULL page). G4's
PROCFS pass closed with maps/auxv serving, but the NAME arm's attribution could not
cross the guest's sshd — java startup starves sshd within seconds and it never
recovers (RESULT.md, session 8). G9's first job, per the REQUIREMENTS row: capture
the rip's owning module by a console-side path, then the fix behind an
off-by-default feature.

## Finding 0 — the rip is already symbolizable host-side; the gate is confirmation, not a guess

The kernel places a dynamic image's interpreter at a fixed constant:
`akuma-elf-exec::INTERP_BASE = 0x3000_0000` (`crates/akuma-elf-exec/src/lib.rs:51`,
`AT_BASE` = the same value — the loader's `at_base`). The guest JVM is dynamic
(`readelf`: `/opt/jdk/bin/java` is ET_DYN with `INTERP /lib/ld-musl-x86_64.so.1`;
extracted from the rig image with debugfs — the VDSO pass's injection recipe, read
side). ld-musl (670 312 B) segments: RX text at vaddr `0x14000..0x6c59f`.

Therefore `rip = 0x3005fc41` − `0x30000000` = `0x5fc41` lands **inside ld-musl's
executable segment**, and its dynsym bracket says the containing function is
**`fstatat`** (entry `0x5fb2a`, size 442 → `0x5fb2a..0x5fce2`; the next candidate,
`+0x10000` base shift, lands in `lgamma_r`, a function with no plausible
store-to-NULL). The instruction at `0x5fc41` is:

```
5fc31: xor %eax,%eax
5fc33: mov $0x12,%ecx          # 18 qwords = 144 B = musl's sizeof(struct stat)
5fc38: mov %r8,%rdi            # r8 = the caller's `struct stat *buf`, saved at entry
5fc41: rep stos %rax,(%rdi)    # <-- THE FAULTING INSTRUCTION
5fc44+: movups %xmm0,(%r8)...  # then the field-by-field copy-out from the stack st
```

musl's `fstatat` runs the stat syscall into a stack `struct stat`, and on success
zero-fills then fills the *caller's* buffer. `cr2=0x0`, `code=0x6` (user write) and
this instruction say one thing: **`rdi` (= the caller's `buf`) was exactly NULL when
a stat-family call SUCCEEDED.** The kernel did not fault; userspace stored through
a NULL pointer the caller passed. So the name is: **the G9 death is a NULL
`struct stat *` reaching musl `fstatat` during JVM init** — the interesting question
moves to *who passed NULL and why*, which is a syscall-ring question (Finding 3),
not a maps question. The console-side capture still pins the range first-hand
(D33), because a host-side ELF argument must be *confirmed on the rig*, not assumed
(the FAULTNAME lesson: a constant that can lie must be checked against the capture).

## Finding 1 — the console is the only live channel during java; attribution must print kernel-side

Session 8's data (RESULT.md, gate log): java + the 2 ms maps poll starve sshd within
seconds; nine reconnect stalls; the arm's stdout never crosses the wire. A slower
poll does not remove the starvation (java itself suffices, reproduced standalone).
So the NAME arm's verdict moves to the console (`$W/vmrun.log`), which the
FAULTNAME pass proved reliably captures fault lines. The kill path is
`crates/akuma-tokio-kernel-amd64/src/lib.rs` `fault_trap_locked` (the
`[Fault] ring3 … -- task killed` print at lib.rs:343). It has `Trap.regs` — the
**full GPR file** (`akuma-trap-amd64::TrapRegs`, r15..rax) — and `copy_from_user`
(a fault-path-safe reader already used by `fault_code_dump`), so a bounded
diagnostic print there can carry: the map rows around rip, the GPRs, and stack
qwords. The walk is already behind the runtime hook
(`akuma_exec::process::maps_rows::pid_map_rows_of_kernel_owned_roots(pid)`); the
pid at fault time comes from `pid_for_slot(exec_twin::current_slot())` (exported by
`child.rs:14`). Lock order is the normal one: `fault_trap` takes the BKL for user
faults (lib.rs:292), and every syscall path takes process locks *under* the BKL —
the walk's `with_process`/AS-lock acquisitions are the same order the syscall path
already produces. The dying task's tables are live (its own cr3), so the
freed-tables hazard the walk's l0 re-check guards does not apply to *self*.

## Finding 2 — the kernel-side suspects are testable WITHOUT java (sshd survives)

Taoxichuan's blocker-3 tree walk (`docs/archive/TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md`,
suspects 1–2) named the mmap error convention and the mprotect-over-PROT_NONE
commit path as the NULL-store candidates. Both are contract facts a guest C probe
can check while sshd is healthy (no java in the room):

1. `mmap` failure convention: `mmap_arm`'s error paths return `e(EINVAL/ENOMEM)`
   (`crates/akuma-tokio-kernel-core/src/mm.rs:19`, two's-complement negative), and
   the machine dispatch returns that value raw (lib.rs:1598). A probe arm asserts
   failures arrive as `MAP_FAILED` (−errno), **never 0** — the taoxichuan
   suspect-1 shape (musl handing HotSpot a NULL cache base).
2. The code-cache commit shape: reserve anon PROT_NONE 48 MiB, `mprotect` a
   sub-range to RW (write it), another to RX (call it), and — under wx-relaxed —
   RWX. Suspect-2's commit-in-place contract, directly exercised.
3. `sysinfo(2)` (G7's demoted verification, free to record): served or ENOSYS —
   the merged tree presumed it served over by main's changes; this arm makes the
   presumption measured.

The current `mmap_arm` reading says the conventions are right, which is exactly why
the arms belong in the probe: an argument "it looks right in source" is a
hypothesis until the rig says it (the WX pass's lesson — the policy doc said one
thing, the rig's RWX arm said the baseline moved).

## Finding 3 — the evidence for *who passed NULL* already has machinery: `trace-err`

`--features trace-err` (off by default, relayed 1:1 through
`akuma-jvm-kernel`; NOT yet relayed in `denver/Cargo.toml` — D34 adds the line)
records every syscall into a ring `(slot, nr, a1, a2, a3, RESULT, rip)`
(`crates/akuma-tokio-kernel-amd64/src/lib.rs:1379 trace_ring`, ring at :1302,
`ring_dump_slot` at :1409) and already prints per-slot dumps on tkill and failed
exec. A stat-family call's buffer argument **is `a2`** (`stat`/`lstat` nr 4/6,
`newfstatat` 262), so the ring dump at the kill path will show the lethal call with
its arguments — and, crucially, **the return values of the syscalls just before it**
(mmap/brk/mprotect results — the upstream NULL-maker, if the chain runs through one).
This is the metal's `strace_nr=` facility's tokio twin, and the FAULTNAME pass's
"death window" method applied first-hand.

## Precedents (R9) — searched docs/archive, docs/reference, the sibling kernels

1. `docs/archive/TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md` — the blocker-3 suspects
   (mmap convention, mprotect commit, faults-not-reaching-handlers) and its
   suggested order: "a fault report that names the module … then a Linux A/B". This
   gate is that report, kernel-side.
2. `docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md` Fixes 10–13 — the metal's
   method for exactly this class: make the failure readable (VMA-aware reports,
   `/proc` facts) before touching the workload; blocker 3 turned out to be a
   contract fix (`CLOCK_REALTIME` frozen), not a workload bug. Same shape expected
   here: a kernel contract detail, read through the dying caller's eyes.
3. `docs/archive/AKUMA_AMD64_THREAD_LIFECYCLE.md` (`segvchild` case C) — a
   `clone`-thread NULL-store investigation on the metal; the fault-report-first
   reflex's local precedent.
4. The FAULTNAME pass (D17/D18 + research): console-window capture, `-a`-tolerant
   greps, python-checked rip constants, "a probe constant that can lie is a broken
   probe". The PROCFS pass (D27–D32): the walk this gate reuses, the
   `procfs-diag` feature shape, the NAME arm and its sshd limit. The VDSO pass
   (D21/D22): off-by-default feature relayed 1:1 down the stack, gate kernel rides
   proven policies, debugfs staging.
5. `akuma-kernel-amd64/src/usermode.rs:155` — the metal's `strace_nr=` per-syscall
   trace; tokio's `trace-err` ring is its counterpart (no new facility needed).

## Decisions

- **D33 — console-side attribution at the ring3 kill path, machine crate, behind
  `procfs-diag` + `trace-err`.** New `fault_attribution(t, cr2, code)` called in
  `fault_trap_locked`'s ring3 kill block: prints (a) `[Fault] ring3 map` — the map
  rows covering/near rip, from the registered walk with the dying task's pid
  (bounded: containing row + nearest below + nearest above; absence recorded
  honestly); (b) `[Fault] ring3 regs` — rip/rsp/cr2/code + rax rbx rcx rdx rsi rdi
  rbp r8–r15 (the `rep stos` register file: rdi is the NULL candidate, r8 its
  provenance); (c) `[Fault] ring3 stack` — 8 qwords at rsp via `copy_from_user`
  (return addresses for the caller identification); (d) under `trace-err`, the
  dying slot's syscall ring (`ring_dump_slot(slot, 40)` — the `nr == 200` tkill
  precedent). Under `trace-err` the existing `fault_code_dump`/`mm_dump_for` ride
  unchanged (instruction bytes at rip = the `rep stos` confirmation). Metal
  untouched; default builds byte-identical (both features off; R6).
- **D34 — `trace-err` relays through `denver`** (the one-line feature relay in
  `denver/Cargo.toml`, the D29 shape). The gate kernel is `--features
  wx-relaxed,vsyscall,procfs-diag,trace-err` — all four default-off, gate-only
  (D30's shape: new binary `denver-nullstore` + `vm-nullstore.spec`, staged by
  size; proven binaries byte-put).
- **D35 — jvmprobe gains two java-free contract arms.** `mmaparms`: mmap failure
  convention (len=0 → EINVAL; oversize → MAP_FAILED, never 0), PROT_NONE
  reserve → mprotect RW commit → write → mprotect RX → call, and an RWX write+call
  under wx-relaxed; prints `MMARMS PASS/RED <detail>`. `sysinfo`: `sysinfo(2)`
  served or `SYSINFO INFO errno=N` (G7's feed, recorded not judged here). Both run
  while sshd is healthy — the arms' verdicts cross the wire. The existing `name`
  arm stays for the java arm's ride-along, and additionally best-effort writes its
  last maps snapshot to `/tmp/name-maps.log` (recovered if sshd ever answers; never
  required by the verdict).
- **D36 — the probe is `denver/probe-nullstore.sh`** (D26/D31 shape). Pass
  conditions, in order: (1) rig identity (`denver`, not `f0dcacb6` (procfs), not
  `3531cf69` (vdso); empty-constant abort — the FAULTNAME lesson); (2) tokioprobe
  `RESULT PASS`; (3) `RWX PASS rw_to_rx=42`; (4) `MAPS PASS` + `AUXV PASS`
  (procfs-diag alive); (5) `MMARMS PASS` (the contract facts, D35); (6) `SYSINFO`
  line recorded (PASS or INFO — G7 owns the verdict); (7) **NAME PASS**: the java
  arm's console window carries the fault at the python-checked rip `0x3005fc41`
  **and** a `[Fault] ring3 map` row whose range covers it **and** the probe's
  host-side arithmetic prints the module line (`ld-musl+0x5fc41 fstatat` — the
  constants from Finding 0, python-checked against the captured range); the
  `[Fault] ring3 regs`/`stack`/`[ring]` lines are recorded evidence for the fix
  decision, not pass conditions. (8) `RESULT PASS` = 1–6 ∧ 7. A *different* rip
  with a covering map row is `NAME SURPRISE` → the re-rank's data, gate FAILs
  honestly (the constant moved; the diagnostic still worked).
- **D37 — the fix lands in THIS gate only if the ring names a kernel contract
  defect with a small, precedented fix** (the Chromium Fixes 10/11 shape: a
  syscall answering a contract detail wrong). Otherwise the gate's GREEN is the
  *named* blocker (conditions 1–7) and the fix is the next pass's opening job with
  the ring evidence attached — recorded here so the verdict cannot silently drift
  from "named" to "guessed". Any fix goes behind an off-by-default feature relayed
  1:1 (R6), and the java arm must *move* (a new, different fault line or progress)
  to count as fixed, never to silently die the same way.

## Composition plan and new-LOC estimate (R5 budget)

1. this artifact (~150 LOC).
2. `crates/akuma-tokio-kernel-amd64/src/lib.rs`: `fault_attribution` (~55 LOC,
   cfg'd `procfs-diag`) + one call site + `ring_dump_slot` call under `trace-err`
   (~4 LOC).
3. `denver/Cargo.toml`: the `trace-err` relay line (~3 LOC).
4. `denver/probes/jvmprobe.c`: `arm_mmaparms` + `arm_sysinfo` + the name-arm
   /tmp dump (~90 LOC).
5. `denver/probe-nullstore.sh` (D36 shape, ~200 LOC), rig staging lines.
6. `denver/gates/PHASE-2-NULLSTORE.log`, `denver/RESULT.md`, LOOP_LOG entry.

## Experiment proposal (the one-per-phase rule)

Knob: `GOOSE_TOOL_CALL_CUTOFF` left at default (10) — this session's biggest single
tool outputs were the maps/gate-log greps; measure `final ctx` in COST_LEDGER.md
against PHASE-2-PROCFS's 123k to judge whether the cutoff summarizes well on GLM
before the operator ever enables it. (Recorded, not applied — the operator owns it.)
