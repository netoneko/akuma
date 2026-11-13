# PHASE-2-FAULTNAME — research (R1, written before any code)

**Gate G6** (`denver/docs/REQUIREMENTS.md`): the workload's death after the G1 fix was a raw
`signal=11` with nothing on stdout — the row demanded "kernel-side fault naming: the
user-SIGSEGV delivery path prints rip+cr2 with the `[Fault]` line". This artifact was written
after re-reading the evidence the row was built from, and it corrects that row's central claim
before the gate runs (the runbook's lesson-defect rule: quote it, fix the living lesson).

## Finding 0 — the fault is already named; what failed was the capture (and the quote)

The PHASE-2-WX records (LOOP_LOG, RESULT, the G6 row) say the kernel "prints
`[Fault] slot N killed (SIGSEGV)` **without an address**". **That is a misquote of the console
census.** The census itself — `/root/denver/w/vmrun-diag.log`, from the WX gate's diagnostic
boot of the same `denver-wx` binary (`[KVM] linux: loading akuma-vm guest /root/denver/w/denver-wx`,
uname `0.1.0-8a658723-denver`) — carries, one line above the slot line:

```
[Fault] ring3 PageFault rip=0xffffffffff600800 rsp=0x00006000014fe768 cr2=0xffffffffff600800 code=0x14 -- task killed
[Fault] slot 5 killed (SIGSEGV)
```

So the death *was* named at address level all along: `rip == cr2 == 0xffffffffff600800`,
`code=0x14` = user-mode (`bit 2`) instruction fetch (`bit 4`) on a not-present page —
HotSpot **jumped to the legacy vsyscall page and fetched from it**, which is exactly G3's
mechanism, and exactly the address the metal `hs_err` captured in phase 1
(`SIGSEGV at pc=0xffffffffff600800`, RIP inside the unmapped page). Two machines, one address,
one mechanism.

Why the records got it wrong: the gate capture (`denver/gates/PHASE-2-WX.log`) holds only
ssh-side output — the probe scripts never capture the guest console — and the census was
quoted selectively (the slot line, not the ring3 line above it). The "unnamed SIGSEGV" was
true only in the *module* sense (no maps to turn the address into a module — G4), and the
re-rank that inserted G6 (diagnostics before guesses) stays valid either way. What G6 actually
needed was: **the gate capture carries the naming line first-hand**, plus the address→module
step. Lesson defect filed on L2 (capture rule + full-block quoting), fixed this session.

**Kernel-side capability: zero new work.** The ring3 naming line has existed since the kernel
file's birth — `git log -L 485,510:crates/akuma-tokio-kernel-amd64/src/lib.rs` shows it in
`aafbb973` (the tokio kernel's first commit), unchanged through the WX gate.

## Crates, files, functions found (named)

| piece | path | what it is |
|---|---|---|
| the naming line | `crates/akuma-tokio-kernel-amd64/src/lib.rs` ~L488 (`page_fault`, ring3 arm) | `[Fault] ring3 {kind} rip= rsp= cr2= code= -- task killed`, then `[Fault] slot N killed (SIGSEGV)` + `child::task_die_segv()` |
| the deeper dump (considered, rejected) | same file ~L1158–1185 | `--features trace-err`: `MM_EVENTS` (a 128-entry *slab-history* ring: munmap/MAP_FIXED/mremap/mmap kinds) + `mm_dump_for(cr2)` prints slabs **whose event ranges cover** the fault VA |
| trace-err relay | `crates/akuma-tokio-kernel-amd64/Cargo.toml` ← `akuma-tokio-kernel` ← `akuma-jvm-kernel` | relayed to the front crate but **not** to `denver` (`denver/Cargo.toml` carries only `wx-relaxed`) |
| the console stream | `denver/vm-probe.sh` ~L48 | `herd vm-run "$SPEC" > "$VMLOG" 2>&1 &` — the console already lands in `$W/vmrun.log`; nothing streams it into the gate capture today |
| the census evidence | `/root/denver/w/vmrun-diag.log`, `vmrun-diag2.log` | the WX gate's diagnostic boots (kept untracked in the rig dir, quoted here first-hand) |
| the metal corroboration | `hs_err_pid*.log` (repo root, phase-1 evidence) | `SIGSEGV at pc=0xffffffffff600800` — the vsyscall fallback, metal kernel 0.0.9 |
| the probe vocabulary | `denver/probes/jvmprobe.c` (`java` arm) | `JVM INFO exit=-1 signal=11 bytes=0` + `JVM RED signal=11 first=""` — the death signal is captured; the *fault* is console-side, by design of the kernel's print path |
| the stamp shape to copy | `denver/probe-method.sh` | `T0=$(date +%s)`, per-step `seconds=N` (slew-clamped, both epochs logged — L4 Do 3) |

## Precedents (R9) — searched docs/archive, docs/reference, the sibling kernels, git history

1. **The attempt doc** (`git show d7878378:docs/archive/TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md`):
   its blocker 3 died with `[Fault] EL0 ec=0x24 elr=0x000060000089060c far=0x0 esr=0x92000146`
   — the aarch64 sibling *names fault addresses*. Its "Not found" section names the exact gap
   ("/proc/<pid>/maps is empty on this kernel") and its declared next step is "a fault report
   that names the module (**maps content, or the fault handler printing the VMA**)". The amd64
   tokio kernel has satisfied the second form at address level since birth; the first form
   (maps) is G4's gate, not this one.
2. **The metal's own discipline** (`crates/akuma-exceptions/src/lib.rs` ~L5078, aarch64 metal):
   `[SIGSEGV-HEAP] pid={} far={:#x} elr={:#x} iss={:#x}` under the comment "Log all SIGSEGV
   deliveries to understand crashes" — fault-address naming on the console is the established
   debugging channel in this tree, not a new invention.
3. **`git log -L` on the amd64 handler** (above, `aafbb973`): the capability predates every
   denver gate — "close the gap" here means *prove it in a capture*, not write a print.
4. **The runbook's own gate rule** (`denver/docs/runbooks/AUTORESEARCH.md`): "`denver/gates/
   <gate>.log` — captured probe output, the only thing that judges a gate." A capability that
   exists but never lands in a capture is, by this house rule, not evidence.
5. **`probe-method.sh`'s grep -a note** (taoxichuan gates.md rule): console logs carry control
   bytes — any grep over console-derived text must be `-a`-safe or tolerant.
6. **trace-err's own docs** (`lib.rs` ~L1158): the ring exists so "a fault can name the
   *unmap/replace* that took its page away" — a slab-history forensics tool. For a fetch at
   the vsyscall page (never mapped, events long evicted) it prints nothing: enabling it would
   change the kernel binary away from the proven `denver-wx` rig and add hang-dump/lap console
   noise, for no verdict evidence. Rejected (R5: budget for what the gate greps).

## Decisions

- **D17 — zero kernel/crate LOC this session.** The naming capability predates the gate
  (`aafbb973`); G6's real gap was capture + interpretation. No feature is added, no manifest
  touched, no `.rs` file changed. R6/R7 trivially hold; the R6 check reduces to *not building
  anything* (the tree is untouched).
- **D18 — the gate runs the proven WX rig unchanged**: `/root/denver/w/denver-wx` +
  `vm-wx.spec` (D15's second-rig rule; a rebuild would be a third variable for no delta). The
  capture proves rig identity two ways: `uname` must say `0.1.0-8a658723-denver` (the exact
  WX-gate hash; a mismatch is `RESULT FAIL: rig changed`, not a warning), and the `rwx` arm
  must print `RWX PASS rw_to_rx=42` (the wx-relaxed policy active).
- **D19 — the probe is `denver/probe-faultname.sh`** (probe-wx.sh's shape) with three
  additions it owns from the lesson debts:
  1. **epoch stamps per step** (L2 debt 2): `T0=$(date +%s)`, each step logs `epoch=N
     elapsed=M`, durations slew-clamped (L4 Do 3);
  2. **console capture with attribution** (Finding 0's fix): byte offset into `$W/vmrun.log`
     before the java arm; after it (+2 s flush), `tail -c +OFF` re-reads the window and greps
     `-a`-tolerantly for the ring3 line and the slot line. Expected: `rip == cr2 ==
     0xffffffffff600800`, `code=0x14`;
  3. **module interpretation, ABI-level**: rip/cr2 at `0xffffffffff600800` is the Linux legacy
     vsyscall page — a constant of the ABI, corroborated by the metal `hs_err` (precedent 1's
     G3 evidence). The verdict line names it `module=legacy-vsyscall-page`; any other rip is
     `module=unmapped` + a recorded surprise (the blocker moved).
  Pass condition (what the gate greps, in order): rig identity → `tokioprobe` `RESULT PASS`
  (platform sanity, phase-0 convention) → `RWX PASS rw_to_rx=42` → the java arm's death
  (`JVM RED signal=11`, expected; the death is the subject) → the console window's
  `[Fault] ring3 PageFault rip=0x… cr2=0x…` + `slot N killed (SIGSEGV)` → `FAULT PASS` →
  `RESULT PASS`. A java arm that *passes* (no fault occurs) is `RESULT FAIL: nothing to name`
  — a surprise that would re-open the ladder, recorded, not smoothed over.
- **D20 — re-rank after the expected pass: G3 (PHASE-2-VDSO) next**, then G2, G4, G5. The
  guest-proven death is the vsyscall fetch — G3's mechanism with guest evidence now, not just
  metal suspicion (the criterion REQUIREMENTS itself used when it moved G3 behind G4: "the
  guest never got past G1, so this is a suspect with metal evidence"). Closing `getcpu` (G2)
  cannot unblock a ladder whose first blocker is the vDSO fallback; G4 (maps) stays after G2
  because the next unknown (the attempt's blocker-3 NULL store) only arrives once the JVM
  initializes further, and the auxv arm already proves the vDSO verdict without maps.

## Composition plan and new-LOC estimate (R5 budget)

1. `denver/docs/research/PHASE-2-FAULTNAME.md` — this artifact. ~90 LOC.
2. `denver/probe-faultname.sh` — the runner (probe-wx shape + stamps + console window +
   interpretation). ~110 LOC, shell only (`sh -n` checked; dry-run parsing verified against
   `vmrun-diag.log` on busybox grep/sed before booting).
3. Docs: REQUIREMENTS G6 row correction + closure + re-rank; LOOP_LOG gate record; RESULT
   session record; L2 defect fix. Kernel/crate delta: **0 LOC**.

**Risks.** The console line could lag the ssh return (mitigated: 2 s flush wait; the kernel
prints before `task_die_segv` reaps, so the line is written first by construction); `[E2-EOF]`
spam widens the window (bounded: 32 lines in the prior census; greps are anchored on the
`[Fault]` tokens, not line numbers); host clock slew (clamped, both epochs in the log); the
`slot` line's N varies with boot ordering (not gated on); the fault could take a different
vector in some boot (then the window lacks the ring3 line → honest `RESULT FAIL` naming what
was seen instead — the capture still names the new shape).
