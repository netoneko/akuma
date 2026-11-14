# PHASE-2-GETCPU — serve `getcpu(2)` with the caller's core (the raw-syscall gap)

**Gate:** PHASE-2-GETCPU · **Date:** 2026-11-14 · **Session:** 15 (goose)
**Feeds:** REQUIREMENTS G2 (next on the standing rank after PHASE-2-MEMINFO GREEN);
the datum measured since PHASE-1-METHOD (`GETCPU RED ENOSYS` on **both** kernels,
jvmprobe's raw-syscall arm) and the PHASE-2-VDSO census (`[nosys] nr=168` in the
java arm's window). G3's close **softened** the JVM-side half: HotSpot's wrapper
tolerates the legacy vsyscall stub's -38 (`cmp $-1`) and reads the pre-zeroed
cpu → 0, so the JVM no longer exits on getcpu — but the **raw arm still REDs for
direct callers**: musl's `sched_getcpu()` performs nr 168 and returns -1 there.
This is the java-free arm on the ladder (the judged arm never runs java).

## The gate's question (G2)

`getcpu(2)` (x86_64 nr **168**, asm-generic nr **168** — the tables agree) is
served nowhere in the tokio lineage: the number falls to the default arm
(`nosys_note`, ENOSYS). The probe arm exists and is already on the image
(`jvmprobe getcpu` → `GETCPU PASS cpu=N node=M` / `GETCPU RED ENOSYS`,
`denver/probes/jvmprobe.c:47-64`) — no image re-injection; the gate is judged by
flipping its line.

## Where the pieces are (named, with paths — R1)

- `denver/probes/jvmprobe.c:51-64` — `arm_getcpu()`: raw `syscall(168, &cpu,
  &node, NULL)`, prints the gate's named line. **Reusable as-is.**
- `crates/akuma-syscalls-linux/src/nr.rs` — the asm-generic table (abi's `nr`
  is `use akuma_syscalls_linux::nr`, `akuma-syscalls-abi/src/lib.rs:116`).
  No `GETCPU` row; neighbours `PRCTL=167`, `GETRUSAGE=165` (line 225-226).
- `crates/akuma-syscalls-abi/src/lib.rs:678` — the macro row shape:
  `Sysinfo => SYSINFO = 99, nr::SYSINFO;` (x86_64 literal + asm-generic name;
  one row generates the `Syscall` variant, both nr consts, `from_x86_64`,
  `to_x86_64`/`to_aarch64`). A `Getcpu => GETCPU = 168, nr::GETCPU;` row is
  the whole ABI change — 168 agrees across tables, so `to_aarch64()` is
  identity (the aarch64 getcpu nr is 168 too: the taoxichuan record says so).
- `crates/akuma-syscalls-glue/` — **no getcpu anywhere** (grep). The arms'
  user-access helpers are `validate_user_ptr` (`lib.rs:476`) and
  `copy_to_user` (reached bare under `use super::*;` — the `cred.rs:112-117`
  pattern: validate → copy → `EFAULT`). `EFAULT` resolves through
  `super::*` exactly as `cred.rs`/`aio.rs` do.
- `crates/akuma-tokio-kernel-amd64/src/lib.rs` — the x86_64 dispatch match;
  the `sysinfo` arm (lines 1780-1784) is the committed shape to clone
  (`#[cfg(feature = "sysinfo")]` arm next to the signal-family translations).
  `use akuma_ring3_amd64 as ring3;` (line 67) — `ring3::cpu_index()`
  (`akuma-ring3-amd64/src/lib.rs:164`) is already the kernel's current-core
  source: trap stacks (line 191), `TASKS.current` (line 1236), sched
  (`sched.rs:70-71`). The dispatch match already calls glue functions
  directly (`TKILL => akuma_syscalls_glue::signal::sys_tkill(...)`), so the
  arm can hand `cpu_index()` to glue as a parameter — no hook (R7).
- Feature relay — the `sysinfo` shape in all four Cargo.tomls
  (`crates/akuma-tokio-kernel-amd64` ← `akuma-tokio-kernel` ←
  `akuma-jvm-kernel` ← `denver`), each one line + comment.

## R9 precedents searched

- `docs/archive/TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md:31-32,83` — **the shape's
  precedent**: the aarch64 attempt's blocker 2 was getcpu ENOSYS (HotSpot
  `os::init_2` exits on it there); fix was "a `getcpu` arm in
  `akuma-tokio-kernel-aarch64/src/syscall.rs` (write `cpu_index()` and node 0
  with `copy_to_user`, `EFAULT` on failure)". It unblocked the JVM there and
  was "correct on its own and could land independently" — but it was a
  throwaway, never committed. `crates/akuma-tokio-kernel-aarch64/` exists
  in-tree and serves **no** getcpu (grep empty). This pass lands that
  recorded shape on the amd64 half, decisions host-tested (R3).
- `docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md:801` — "Akuma does not answer
  `getcpu`" (the benchmark-bucket note; the gap is tree-old, not denver-new).
- `docs/archive/BUG_FIX_LIST.md:2117` — x86_64 **300** is `getcpu`: the
  private-number-space collision note; also the hardcoded-256MB lesson, which
  this gate's decision honors — the cpu is the **caller's** core from
  `cpu_index()`, never a constant.
- `crates/akuma-sched-amd64/src/platform.rs:48,231` and
  `crates/akuma-mmu/src/lib.rs:1174` — `cpu_index()` is the machine state the
  kernel itself trusts ("instead of guessing"); affinity (`can_run`) is
  decided from it.
- `shanghai/`, `taoxichuan/`, `docs/reference/` — grep empty (no getcpu
  content to copy); the sysinfo precedent (MEMINFO D52/D53) supplies the
  relay/arm shape, glue's `SCHED_GETAFFINITY` arm (`lib.rs:1056-1099`) the
  validate/copy/errno shape.

## D55 — the fix: decision in a pure crate, thin arm in the machine half

1. **Pure decision + host tests** — new `crates/akuma-syscalls-linux/src/sched.rs`:
   `pub fn cpu_node(cpu: u32) -> (u32, u32) { (cpu, 0) }` — the caller's core,
   node 0 (single-NUMA guest; the taoxichuan shape). Tests: answers the
   caller's core (never a constant — the BUG_FIX_LIST lesson), node is always
   0, core 0 works. `pub mod sched;` in the linux crate's lib.rs.
2. **Glue arm** — new `crates/akuma-syscalls-glue/src/sched.rs`:
   `pub fn sys_getcpu(cpu_ptr: u64, node_ptr: u64, current_cpu: u32) -> u64` —
   validates each **non-NULL** pointer (Linux: either output may be NULL) for
   4 bytes, copies `cpu_node(current_cpu)` out through `copy_to_user`,
   `EFAULT` on any failure, else 0. The `tcache` argument (arg3) is ignored —
   Linux has ignored it since 2.6.24 (recorded, not implemented).
3. **Machine-half arm** (R4 thin: ABI glue + machine state only), next to the
   sysinfo arm in `akuma-tokio-kernel-amd64/src/lib.rs`:
   ```rust
   #[cfg(feature = "getcpu")]
   akuma_syscalls_abi::x86_64::GETCPU => akuma_syscalls_glue::sched::sys_getcpu(
       fr.arg1(), fr.arg2(), ring3::cpu_index() as u32,
   ),
   ```
4. **nr const only — deliberately NO `Syscall` row** — `pub const GETCPU: u64 = 168;`
   in `nr.rs` (x86_64 168 is getcpu too, the pidfd rule). **Attempt-1 lesson
   (mechanism named, `PHASE-2-GETCPU.attempt1-FAIL.log`):** the first cut added
   the full ABI row (`Getcpu => GETCPU = 168, nr::GETCPU;`) and the gate REDd
   ENOSYS on the correct new binary — because `akuma-syscalls-glue-amd64`'s
   dispatch decodes through `Syscall::from_x86_64(nr)` and forwards
   table-named numbers into glue's `handle_syscall`, which has no getcpu arm:
   the row HIJACKED nr 168 into an ENOSYS before the tokio match ever saw it
   (the pre-row census `[nosys] nr=168` proves the old path reached the
   match's default arm). The pidfd precedent is written down for exactly this
   (`akuma-runtime-tokio`'s `shared_numbers_agree` test: "the abi table names
   neither" — calls served outside glue's table carry an `nr` const, not a
   `Syscall` row).
5. **Feature relay 1:1** — `getcpu = []` → `["akuma-tokio-kernel-amd64/getcpu"]`
   → `["akuma-tokio-kernel/getcpu"]` → `["akuma-jvm-kernel/getcpu"]`, the
   `sysinfo` comments' shape.
6. **Scope note (recorded, not judged):** the legacy vsyscall stub at
   `0xffffffffff600800` keeps answering ENOSYS — HotSpot's own path is
   unchanged (its wrapper reads the pre-zeroed cpu either way). This gate
   serves the **raw** syscall; the stub's semantics are G3's recorded
   behavior and are not touched.

**R6 evidence:** default build's `.text` byte-compared before/after
(objcopy `--only-section=.text`; pre-change snapshot saved at
`/tmp/denver-default-pre-getcpu` — the cfg'd arm must add nothing when off);
the gate feature set builds (`--features ...,sysinfo,getcpu`); linux crate
host tests green on the musl target (the E0463 lesson: `--target
x86_64-unknown-linux-musl`, never bare). **Build fact (LOOP_LOG-recorded
machine note):** `/usr/local/rust/bin` nightly first in PATH (the driver does
this; a bare shell gets Alpine rustc and E0463 on `x86_64-unknown-none`).

## D56 — the probe: `denver/probe-meminfo.sh`'s shape, GETCPU now JUDGED

New rig `denver-getcpu` + `vm-getcpu.spec` (the meminfo spec's disk and port,
kernel path changed; the jdk image already carries jvmprobe's getcpu arm —
no re-injection). Staging: default-first/gate-last into the shared output
path (the LOOP_LOG machine note), gate binary sized ≈ +29 KB like meminfo's.
Pass conditions, in order:

1. Rig identity: uname says denver and **NOT** any prior binary — `72be5c55`
   (nullfix), `800e8c80` (nullstore), `f0dcacb6` (procfs), `3531cf69`
   (vdso/merged), `c921d625` (heapro), `24398b99` (meminfo); empty constant
   aborts (staging check);
2. tokioprobe RESULT PASS (platform sanity);
3. RWX PASS rw_to_rx=42 (rides); 4. MAPS PASS + AUXV PASS (rides);
5. MMARMS PASS incl. `rwx call=42` (rides);
6. **THE GATE: `GETCPU PASS cpu=<0..3> node=0`** — the spec has `vcpus=4`,
   so cpu must be in range and node must be the pure crate's 0;
   `GETCPU RED ENOSYS` (the arm didn't take) or any other RED = RESULT FAIL.
7. regpreserve ≥ 20 REGPASS, zero REGRED/CRASH (rides);
8. LAZYREL PASS anon-release-kills AND file-release-kills (rides);
9. **The java arm MOVED** (riding datum, D37/D41 honesty — not judged
   further): no fault at the old rip `60000067cd7a` in the 30 s window;
   `[nosys] nr=168` absence recorded (musl's sched_getcpu path through the
   raw syscall is the thing this gate changed; HotSpot's stub path is not);
   `/tmp/java-out.log` recovered twice (+30 s/+60 s) — the phase-3 capture
   datum, fed forward unread (`JAVA COMPLETE` if a version line appears).

`RESULT PASS` = 1–6 AND 7 AND 8 AND not-9-failed.

## Composition plan and new-LOC estimate (R5 budget)

1. this artifact (~110 LOC);
2. `nr.rs` const + abi row (~4 LOC);
3. `akuma-syscalls-linux/src/sched.rs` (~28 LOC with tests) + `pub mod` line;
4. `akuma-syscalls-glue/src/sched.rs` (~30 LOC) + `pub mod` line;
5. the arm in `akuma-tokio-kernel-amd64/src/lib.rs` (~10 LOC with comment);
6. feature lines in four Cargo.tomls (~16 LOC with comments);
7. `denver/probe-getcpu.sh` (~230 LOC, the meminfo shape);
8. rig staging (`denver-getcpu`, `vm-getcpu.spec`), gate log, RESULT/LOOP_LOG
   records.
No new pure logic beyond the one-line decision, no image re-injection —
composition only.

## Experiment proposal (the one-per-phase rule)

None new — PHASE-2-NULLSTORE's recorded proposal (knob
`GOOSE_TOOL_CALL_CUTOFF` at default, judged by later sessions' `final_ctx` in
COST_LEDGER.md) stands for the phase; the operator owns applying it.

## D57 — the diagnosis trail, three REDs against a provably sound dispatch [SUPERSEDED by D58 — the "feature-drift" theory below was wrong; kept as the record of how the trail actually went]

Attempt-2 (11:45:20, preserved as `PHASE-2-GETCPU.attempt2-FAIL.log`) and this session's
attempt-3 (preserved as `PHASE-2-GETCPU.attempt3-FAIL.log`) both REDd ENOSYS, and the
session's first theory — that the 11:54 staged binary lacked the `getcpu` feature — died
when a rebuild WITH the feature (verified by symbol, then by a marker build) still REDd.
What the trail actually established, each step dispositive:

- **Dispatch-path audit**: exactly one dispatcher is installed
  (`ring3::enable_syscalls(dispatch)`, `lib.rs:1935`; the stub's `DISPATCH` is an
  AtomicUsize written once); `glue_amd64::dispatch` answers `None` for undecodable 168
  (legacy match 7/23/33/96/201, then `Syscall::from_x86_64(168)?`); `legacy::fold` has no
  168 (`NUMBERS` = 19 x86_64 legacy numbers, checked); `serve_shared` claims only pidfd
  424/434; `sys_getcpu` returns only 0 or EFAULT. A compiled-in arm cannot answer ENOSYS.
- **Static proof on the built binary**: the match compiles to a jump table over `nr-9`;
  entry 159 (nr 168) held the getcpu arm block verbatim (`mov %gs:0x18,%edx` —
  `ring3::cpu_index()` — then `call sys_getcpu`); the nr-99 entry held sysinfo's block as
  the control. The routing was correct in every binary that REDd.
- **Marker experiment** (prints inside the feature-gated arm and at boot, R6-clean):
  boot marker `[gcpu-build]` appeared in the console; the arm markers never did, and the
  RED persisted — the syscall answers -38 **before the match**, with no `[err]`/`[nosys]`
  trace for nr 168 anywhere, on a live, flushed console.

The trail's own errors, kept as warnings: (1) a stale R6 snapshot
(`/tmp/denver-default-pre-getcpu`, 11:27) was from the attempt-1 tree that still carried
the abi `Getcpu` ROW — its `.text` differs from every post-row build, so the first
"R6 FAIL" was the comparator's fault, not the change's; the honest compare is
stash→build→pop→build (see D58). (2) The console's `[nosys] nr=309` + `[err] s4 nr=309
-38` lines in the attempt-3 boot were read as sshd noise. They were the gate.

## D58 — the real root cause: the arm keyed on the asm-generic number; x86_64 getcpu is 309

**D55's premise "x86_64 nr 168, asm-generic nr 168 — the tables agree" was FALSE.** The
x86_64 Linux number for `getcpu(2)` is **309** (`arch/x86/entry/syscalls/syscall_64.tbl`;
musl's `<sys/syscall.h>` names it `SYS_getcpu`=309 on x86_64); 168 is the asm-generic /
aarch64 number. The two tables **disagree** for getcpu — it predates the unified-number
era, so the pidfd rule ("everything since ~5.1 shares one number") does not apply.

Measured, three ways: the image's `/bin/jvmprobe` getcpu arm performs `mov $0x135,%edi` =
309 before `call syscall` (disassembled from the extracted image binary); the gate's
console carried `[nosys] nr=309` + `[err] s4 nr=309 -38` exactly when the gate REDd; and
`BUG_FIX_LIST.md:2117` already warned that the x86_64 300-range is occupied (its own text
says "300 is getcpu" — 309; finit_module=313 is correct there). Every RED since
PHASE-1-METHOD was the default arm honestly ENOSYSing the **real** getcpu call while the
kernel's answer sat keyed on a number no x86_64 caller performs. The phase-1 datum
"`GETCPU RED ENOSYS` on both kernels" was a true measurement of a real gap — the gap was
just never where the arm looked for it.

**The fix (composition, ~14 LOC incl. comments):**
1. `akuma-syscalls-abi::x86_64` gains a hand-written, ROW-FREE const:
   `pub const GETCPU: u64 = 309;` — row-free per D55's own attempt-1 lesson (a `Syscall`
   row would route 309 into `glue_amd64::dispatch`'s decode → glue's table → ENOSYS
   hijack). `nr::GETCPU`=168 stays: it is the asm-generic/aarch64 truth the aarch64
   halves use.
2. The amd64 arm keys on `akuma_syscalls_abi::x86_64::GETCPU`; comment rewritten with the
   correction. Glue, the pure crate, the relay, the probe: untouched.

**R6 evidence (the D49 standard, done right this time):** default build `.text`
sha256 before/after via stash→build→pop→build — **identical**
`94da2dbf0f19c44814e2b4c9…` (the earlier mismatch was the stale 11:27 snapshot from the
abi-row tree; a cross-commit snapshot is not a comparator). Gate feature set builds;
linux-crate sched host tests unaffected (pure crate untouched).

**Verdict:** `sh denver/probe-getcpu.sh` → `denver/gates/PHASE-2-GETCPU.log` —
**RESULT PASS**, wall 83 s: `GETCPU PASS cpu=1 node=0` (the caller's core, in the spec's
0..3 range, node 0), tokioprobe, RWX `rw_to_rx=42`, MAPS+AUXV, MMARMS incl. `rwx call=42`,
regpreserve REGPASS x20 zero REGRED, LAZYREL both arms, java arm moved — no return to the
old death, no fault at all in the window, output again unrecoverable over ssh (the
phase-3 capture problem stands, fed forward unread). G2 **closed**.
