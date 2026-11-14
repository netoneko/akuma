# PHASE-2-MEMINFO — serve `sysinfo(2)` with the guest's real RAM (the heap-sizing feed)

**Gate:** PHASE-2-MEMINFO · **Date:** 2026-11-14 · **Session:** 14 (goose)
**Feeds:** REQUIREMENTS G7 (re-ranked 2026-11-14, post-NULLSTORE and post-HEAPRO);
the datum already measured twice (`SYSINFO INFO errno=38` — `denver/gates/PHASE-2-NULLSTORE.log`
on the merged tree, `denver/gates/PHASE-2-HEAPRO.log` on the heapro kernel); the HEAPRO
record's open capture problem ("the new first fatal is uncaptured — the next pass opens
with a capture problem (longer window / output recovery), not a fix").

## The gate's question (G7)

`sysinfo(2)` is served nowhere in the denver (tokio-lineage) kernel: musl's
`sysconf(_SC_PHYS_PAGES)` performs `sysinfo` (x86_64 nr **99**), gets `-ENOSYS`, and
HotSpot's `Arguments::set_heap_size` computes a too-small MaxHeap — the JVM exits
*cleanly* (`exit=1`, no signal) with `Too small maximum heap` (the PHASE-2-VDSO-era
java arm: `JVM INFO exit=1 signal=0 bytes=66`). The census recorded `[nosys] nr=99`
in the java arm's window. On the merged tree the presumption "main served it over"
was disproven (NULLSTORE D35's arm): `SYSINFO INFO errno=38` — twice now, the second
time on the heapro kernel (c921d625). This pass is the **implementation** pass with
its datum already recorded.

## Where the pieces are (named, with paths — R1)

- `crates/akuma-syscalls-linux/src/proc.rs` — `pub struct Sysinfo` (112 bytes,
  `repr(C)`, offsets asserted by `sysinfo_procs_and_totalhigh_straddle_the_alignment_hole`,
  all-112-bytes-initialised asserted by `defaulted_sysinfo_has_no_uninitialised_bytes`),
  `Sysinfo::to_bytes()` written through `offset_of!`. **Reusable as-is; no new pure
  logic needed.**
- `crates/akuma-syscalls-glue/src/proc.rs` — `pub(super) fn sys_sysinfo(info_ptr)`:
  validates the user pointer, fills `uptime` (`akuma_primitives::clock::uptime_us`),
  `totalram`/`freeram` (`akuma_exec::pmm::total_count()/free_count()` × 4096 — real
  frames), `procs` (`akuma_exec::process::process_count()`), `mem_unit=1`, and
  `write_user_val`s the struct. **Reusable as-is.**
- `crates/akuma-syscalls-glue/src/lib.rs:1114` — `nr::SYSINFO => proc::sys_sysinfo(args[0])`,
  unconditional in `handle_syscall`'s asm-generic-numbered table.
- `crates/akuma-syscalls-abi/src/lib.rs:678` — `Syscall::Sysinfo`, x86_64 nr 99,
  asm-generic nr via `nr::SYSINFO`; `to_aarch64()` is the translation the tokio kernel
  already uses for the signal family.
- `crates/akuma-tokio-kernel-amd64/src/lib.rs` — `dispatch_inner` → `dispatch_after_fold`
  → the x86_64 match; nr 99 falls to `other => { nosys_note(other); e(ENOSYS) }`.
  The in-tree translation shape: `RT_SIGSUSPEND`/`RT_SIGPENDING`/`TKILL`/`KILL`/`TGKILL`
  arms forward into `akuma_syscalls_glue::handle_syscall` with `.to_aarch64()` numbers.
- `crates/akuma-runtime-tokio/src/lib.rs:233` — `shell()` classifies `S::Sysinfo` into
  the shell call set, but line 176 only routes it under the tokio-runtime **`shell`
  feature, which the denver gate kernel does not enable** — one feature flipping
  20+ calls (sethostname, getrusage, prlimit64, getrlimit, …), far more than this
  gate's one number, and those are later gates' territory.

## D52 — root cause: the tokio lineage never grew the metal kernel's one-line translation

The metal kernel translates it (`crates/akuma-kernel-amd64/src/usermode/dispatch.rs:1305`):
`Syscall::Sysinfo => to_glue(call, [a1, 0, 0, 0, 0, 0])`. The tokio lineage's
`akuma-runtime-tokio` reclassified sysinfo into `shell()` — a set a JVM-facing kernel
build deliberately doesn't enable — and nothing else in the dispatch match names nr 99.
So the number reaches the default arm and ENOSYSes, exactly as the census and both
gate logs measured.

## R9 precedents searched

- `crates/akuma-kernel-amd64/src/usermode/dispatch.rs:1305` — **the metal precedent**:
  sysinfo translated to glue, one arm, arguments passed through.
- `crates/akuma-syscalls-glue/src/lib.rs:1114` + `proc.rs` `sys_sysinfo` — the merged
  lineage serves it through this table (the merged tree measured errno=38 only because
  **denver's** dispatch never forwards nr 99 there — the fusion is one arm away).
- `docs/archive/BUG_FIX_LIST.md` — sysinfo was among the 20 syscalls added for bun;
  the extraction history records two past defects: hardcoded 256 MB total RAM with a
  wrong `mem_unit` offset (fake numbers break sizing consumers), and the `[u8; 112]`
  byte-writes leaking 4 bytes of kernel stack through unnamed tail padding (fixed by
  the named `_f` field + tests this pass reuses).
- `docs/archive/TAOXICHUAN_B10_SYSCALL_CENSUS.md:63` — the sibling lineage's census
  serves sysinfo in its `runtime` set (25 of 38); the tokio lineage's `shell()`
  classifier is where it parked instead.
- `shanghai/`, `taoxichuan/` sources — no sysinfo serving of their own to copy
  (grep: only the census doc); the glue translation is the lineage's shared answer.
- `akuma-primitives/src/clock.rs` — `uptime_us()` returns **0 when no hook is
  registered** (documented "unknown", never panics); whether the tokio kernel
  registers it via `akuma_exec::runtime::register` is a known-unknown — the probe
  records the value; only `totalram=0` REDs (a sizing consumer divides by it).

## D53 — the fix: one translated arm behind `sysinfo` (off by default, R6)

In `crates/akuma-tokio-kernel-amd64/src/lib.rs`, next to the signal-family
translations (cfg'd):

```rust
#[cfg(feature = "sysinfo")]
akuma_syscalls_abi::x86_64::SYSINFO => akuma_syscalls_glue::handle_syscall(
    akuma_syscalls_abi::Syscall::Sysinfo.to_aarch64(),
    &[fr.arg1(), 0, 0, 0, 0, 0],
),
```

R4 shape: the arm is ABI glue only (translate the nr, pass arg0); every decision
(what the fields mean, the struct layout, the user copy) lives in glue/`akuma-syscalls-linux`,
host-tested there. Real RAM comes from `akuma-pmm`'s frame counters — the spec's
`ram_mib=3072` should surface as `totalram ≈ 3.2 GB`, not a hardcoded constant
(the BUG_FIX_LIST lesson). Feature relay 1:1, the `lazy-release-sweep` shape:
`akuma-tokio-kernel-amd64` ← `akuma-tokio-kernel` ← `akuma-jvm-kernel` ← `denver`.
R6 evidence: default build byte-compared before/after the change (the cfg'd arm
must add nothing when off); gate feature set builds; `akuma-syscalls-linux`'s
existing sysinfo host tests run on the musl target (the E0463 lesson: `--target
x86_64-unknown-linux-musl`, never bare).

## D54 — the probe: `denver/probe-heapro.sh`'s shape, SYSINFO now JUDGED

New rig `denver-meminfo` + `vm-meminfo.spec` (the heapro spec's disk and port, new
kernel path; `denver-jdk.img` already carries the jvmprobe with the sysinfo and
lazyrel arms — no image re-injection). Pass conditions, in order:

1. Rig identity: uname says denver and **NOT** any prior binary — `72be5c55`
   (nullfix), `800e8c80` (nullstore), `f0dcacb6` (procfs), `3531cf69` (vdso/merged),
   `c921d625` (heapro); empty constant aborts (staging check);
2. tokioprobe RESULT PASS (platform sanity);
3. `RWX PASS rw_to_rx=42` (rides); 4. MAPS PASS + AUXV PASS (rides);
5. MMARMS PASS incl. `rwx call=42` (rides);
6. **THE GATE: `SYSINFO PASS` with `totalram>0`, `freeram>0`, `mem_unit=1`;
   `uptime` recorded (0 acceptable = no clock hook — a known-unknown, now measured).
   `SYSINFO INFO` (ENOSYS or other errno) or `SYSINFO RED` = RESULT FAIL.**
7. regpreserve ≥ 20 REGPASS, zero REGRED/CRASH (rides);
8. LAZYREL PASS anon-release-kills AND file-release-kills, zero RED/CRASH (rides);
9. **The java arm MOVED** (D37/D41 honesty): the console window carries NO fault at
   the old rip `60000067cd7a` — a kill there is an honest FAIL. This gate also
   opens the capture problem: the window is **30 s** (heapro's 10), and
   `/tmp/java-out.log` is recovered **twice** (at +30 s and +60 s). Version output
   = `JAVA COMPLETE` (the G7 payoff); a recovered `Too small maximum heap` with
   `SYSINFO PASS` standing is recorded as the next gate's datum (a sizing consumer
   reading something else); a *new* fault rip is the next gate's data.

`RESULT PASS` = 1–6 AND 7 AND 8 AND not-9-failed.

## Composition plan and new-LOC estimate (R5 budget)

1. this artifact (~110 LOC);
2. the arm in `akuma-tokio-kernel-amd64/src/lib.rs` (~8 LOC with comment);
3. feature lines in four Cargo.tomls (~14 LOC with comments);
4. `denver/probe-meminfo.sh` (~215 LOC, mostly the heapro shape);
5. rig staging (`denver-meminfo`, `vm-meminfo.spec`), gate log, RESULT/LOOP_LOG records.
No new pure logic, no image re-injection — composition only.

## Experiment proposal (the one-per-phase rule)

None new — PHASE-2-NULLSTORE's recorded proposal (knob `GOOSE_TOOL_CALL_CUTOFF` at
default, judged by this session's `final_ctx` in COST_LEDGER.md) stands for the
phase; the operator owns applying it.
