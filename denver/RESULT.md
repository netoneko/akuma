# Denver — result

The experiment record: every gate with command + captured output, every intervention, wall clock
and token spend (`denver/docs/archive/KERNEL_HOWTO_BUILDING_CUSTOM_PLATFORM_KERNEL.md`,
"What a result must look like"). Captured probe output lives in `denver/gates/<gate>.log` — the
only thing that judges a gate.

**Rig:** branch `denver` on the bare-metal ryzen booted into Akuma (`Akuma akuma-ryzen 0.0.9
886dabf-release-smp-shared`, x86_64); model glm-5.3-flash via goose (`denver/autoresearch.sh`).
Toolchain on this box: standalone `/usr/local/rust` cargo 1.101.0-nightly (no rustup — the
repo's `rust-toolchain.toml` pin is not enforced here; the pin's own comment says it exists for
Mac-vs-ryzen parity, and the shanghai stack was built and metal-proven with this same compiler).
`aarch64-unknown-none` std is not installed and not installable without rustup — the aarch64
default build was never runnable on this host; the R6 default-build surface checked here is the
x86_64 + host one. Machine facts, not regressions; recorded once here.

---

## Gate PHASE-0-SCAFFOLD — session 1, 2026-11-13 17:25–18:05 UTC — NOT GREEN (escalated)

**Goal:** `denver/` + `akuma-jvm-kernel/` exist; Denver boots tokio; `uname` says denver;
feature parity with the pass-through. Probe: tokioprobe (`RESULT PASS` marker, the driver's
convention, precedent shanghai gate 5 / `mw_gate.sh`).

**Commands and outcomes** (full capture: `denver/gates/PHASE-0-SCAFFOLD.log`):

| step | command | outcome |
|---|---|---|
| research first (R1) | `denver/docs/research/PHASE-0-SCAFFOLD.md` | written before any change; decisions D1–D5 with precedents |
| platform (D1) | `git merge shanghai --no-ff` (tip `d4a6c7d9`) | clean merge; denver's PLAN-pm/pn kept; submodule pointers to shanghai's |
| front crate (D2) | `akuma-jvm-kernel/` (Cargo.toml + lib.rs + build.rs) | 1:1 feature map, zero capabilities, `forbid(unsafe_code)` |
| kernel (D3) | `denver/` (Cargo.toml, build.rs, src/main.rs 18 lines, src/banner.txt) | the README's three-file kernel; package name `denver` |
| wiring | root `Cargo.toml` members += `akuma-jvm-kernel`, `denver` | + driver staging fix (D5) |
| build | `. /etc/akuma-dev.env && cargo build -p denver --target x86_64-unknown-none --release -j4` | **PASS** cold 22.91s, no fixes |
| default-surface check (R6) | `cargo build -p akuma-amd64 / -p taoxichuan --target x86_64-unknown-none --release -j4` | **PASS** 37.97s / 15.84s |
| boot probe (H1) | `herd vm-run /root/denver/w/vm.spec` | **FAIL** `[vm-run] no /dev/kvm0: the kernel was not booted with kvmctl` |
| boot probe (H2) | `/root/fc/firecracker` (present) | needs the same missing KVM device |
| boot probe (H3) | qemu anywhere (`/usr/local`, `/opt` incl. gentoo root, `/root`) | absent |

**Verdict.** Build half of the gate proven; boot half blocked by the host boot configuration.
Stall-protocol remedy not applicable (the diagnostic already exists: kvm.md line 192, arming at
`amd64/src/main.rs`; the missing thing is a boot flag, not a capability pass). Escalated:
`denver/ESCALATE.md` — QUESTION: reboot with `kvmctl kvmmem=4096`, then
`ESCALATE_ANSWERED=1 sh denver/autoresearch.sh 1 PHASE-0-SCAFFOLD`. Probe rig fully prepared and
reboot-survivable under `/root/denver/w/` (kernel binary, fsck-clean image = byte-copy of
shanghai's proven busybox/sshd/uname/tokioprobe image, herd spec, port 2247). After the reboot
the session only boots, probes, and records.

INTERVENTION: host reboot required — running Akuma kernel lacks `kvmctl` (no /dev/kvm0, no
QEMU/Firecracker alternative); the agent runs on the box and cannot reboot it. Unblocked by:
reboot into a kvmctl-armed kernel (see denver/ESCALATE.md). Escalations and interventions are
the same currency (work log, 2026-11-13); this is the session's first and only one.

**Wall clock:** ~40 min (17:25–18:05 UTC). **Token spend:** not instrumented in-session; the
driver log (`DENVER_LOG`, default `/tmp/denver-autoresearch.log`) is the ledger.

---

## Gate PHASE-0-SCAFFOLD — session 2 (the redo pass), 2026-11-13 18:22–18:50 UTC — GREEN

**Goal:** unchanged (session 1's row above). The escalation was answered: the human rebooted
the ryzen into the kvmctl-armed kernel (GRUB entry 29, `kvmctl kvmmem=4096` — the exact remedy
session 1 named), ran `sh denver/vm-probe.sh` by hand over ssh (capture appended to the gate
log, ending `RESULT PASS`), and committed it with the rig script as WIP `b0f1e325`. The driver
archived the answered escalation (`denver/docs/archive/escalations/2026-11-13-PHASE-0-SCAFFOLD.md`)
and launched this session.

**Commands and outcomes** (capture: `denver/gates/PHASE-0-SCAFFOLD.log`, session-2 section):

| step | command | outcome |
|---|---|---|
| research addendum (R1) | `denver/docs/research/PHASE-0-SCAFFOLD.md`, decisions D6–D8 | written before the probe run |
| rig state check (D7) | `exec 3<>/dev/kvm0` — *open*, not `ls` (the node is hidden from `ls /dev` by design) | opens; host confirmed kvmctl-armed |
| probe, first-hand | `sh denver/vm-probe.sh 2>&1 \| tee -a denver/gates/PHASE-0-SCAFFOLD.log` | **PASS** — stale slot freed, guest booted, ssh answered, guest stopped |

Probe tail as captured this session (first-hand, appended below the hand-run section):

```
== uname
Linux denver 0.1.0-8dc2dbcc-denver 8dc2dbcc-release x86_64 GNU/Linux
== tokioprobe
[ssh] connecting to 127.0.0.1:2247...
RUNTIME PASS multi-thread workers=4
UNAME PASS Linux denver 0.1.0-8dc2dbcc-denver 8dc2dbcc-release x86_64 GNU/Linux (0.00s)
HTTPS PASS https://akuma.sh/ status=200 HTTP/1.1 bytes=663 (1.14s)
  title: Akuma
RESULT PASS
RESULT PASS
```

**Verdict.** Gate GREEN on the session's own capture: `uname` says denver; tokio runs
(multi-thread, 4 workers); HTTPS egress through the host relay. The redo needed no new
intervention (D8): the reboot was session 1's, already recorded; the hand-run pass was the
human's answer to it, and this session re-proved the rig first-hand rather than borrowing the
capture. One machinery fix, logged in the work log (finding 5): the escalation-archive `mv`
now never overwrites an earlier archive — this session's phase-review escalation would have
replaced the reboot escalation's archive (same date, same gate slug).

## Phase 0 summary — scaffold (runbook transition rule)

| gate | verdict | interventions |
|---|---|---|
| PHASE-0-SCAFFOLD (sessions 1–2) | GREEN | 1 (the kvmctl host reboot) |

**What phase 0 proved.** A new platform kernel is composition, not porting: the front crate
`akuma-jvm-kernel` (~50 LOC, `forbid(unsafe_code)`, 1:1 feature map onto
`akuma-tokio-kernel`) and the three-file kernel `denver/` (~50 LOC + banner) over the proven
`akuma-tokio-kernel*` stack merged from shanghai — build green cold (22.9 s, session 1), boot
green on the metal (this session). Zero kernel code of our own; feature parity with the
pass-through by construction (it *is* the pass-through). The machine facts stand as recorded:
standalone toolchain, no aarch64 std, x86_64 + host surface is the checkable R6 default here.

**Decisions taken.** Session 1: D1 merge-not-port, D2 pass-through front crate, D3 three-file
kernel, D4 rig outside the tree, D5 driver carries root manifests. Session 2: D6 accept
operator-supplied machine state, then re-prove first-hand; D7 hidden device nodes are checked
by opening, never by listing; D8 no new intervention taken on the redo. Precedents cited in
the research artifact (R9).

**Interventions.** One for the phase: the kvmctl host reboot (session 1, recorded there with
what was stuck and what unblocked it; answered by the human). The redo pass itself needed
none — no new intervention line was added this session.

**Transition rule applied.** The phase's gate is green and records complete, but the phase
needed a human intervention — per the runbook ("autonomy is earned per phase, not granted
once") the loop stops at this summary for human review: `denver/ESCALATE.md` asks for the
phase-0 review before phase 1 (lessons 1, 2, 4: build `denver/docs/REQUIREMENTS.md` from
running the workload on this kernel, write the RED probes, measure the
hypothesis-to-verdict cycle) opens. `NEXT GATE` stays `PHASE-0-SCAFFOLD` with the recorded
reason (gate green) in `LOOP_LOG.md`.

**Wall clock:** ~30 min (18:22–18:50 UTC). **Token spend:** not instrumented in-session; the
driver log (`/tmp/denver-autoresearch.log`) is the ledger.

---

## Gate PHASE-1-METHOD — session 3, 2026-11-13 19:34–20:15 UTC — GREEN

**Goal:** open phase 1 (lessons 1, 2, 4 — the method): REQUIREMENTS.md from a first-hand
workload run; the RED probe suite written before any closing code; the hypothesis-to-verdict
cycle measured. Review of phase 0 answered before this session (archive:
`denver/docs/archive/escalations/2026-11-13-PHASE-0-SCAFFOLD.md`).

**Commands and outcomes** (capture: `denver/gates/PHASE-1-METHOD.log`):

| step | command | outcome |
|---|---|---|
| research first (R1) | `denver/docs/research/PHASE-1-METHOD.md` (D9–D12) | attempt doc recovered from git `d7878378` (not on branch); metal `hs_err` identified as gap #1 |
| workload, metal leg | `denver/probes/jvmprobe java /usr/bin/java` | **RED** SIGSEGV pc=0xffffffffff600800 (vsyscall, RIP inside it), SIGABRT after hs_err — repro of `hs_err_pid6587.log` (2026-11-09) |
| workload, guest leg | image: proven root + JRE via `sh denver/probe-image.sh`; `/bin/jvmprobe java /opt/jdk/bin/java` | **RED** `Error occurred during initialization of VM reason="Failed to mark memory page as executable…"` — the attempt's blocker 1, first-hand on x86_64 |
| RED suite, both legs | `jvmprobe getcpu / auxv / cycles / rwx` | guest: `GETCPU RED ENOSYS`, `AUXV RED AT_SYSINFO_EHDR missing`, `RWX RED EACCES` ×2, `CYCLES PASS 4367/4189 cyc/call`; metal: same ENOSYS/auxv-REDs, but `RWX PASS rw_to_rx=42` (kernel 0.0.9 allows RWX — policy moved) and `CYCLES PASS 558/680` |
| platform sanity | `/bin/tokioprobe` in the jdk image | **PASS** (tokio intact on the 640 MiB image: RUNTIME/UNAME/HTTPS + `RESULT PASS`) |
| measured loop | `HYPOTHESIS H1` → verdict → `CYCLE PASS H1 seconds=0` (clamped; host clock slewed -1 s mid-run, epochs logged) | whole method 9 s wall clock |
| deliverables | `denver/docs/REQUIREMENTS.md`; lessons L1/L2/L4 | gap table ranked by the JVM's init order (G1 W^X → G2 getcpu → G3 vDSO → G4 procfs → G5 time), suspects separated |

**Verdict.** Gate GREEN on the session's own capture: every probe arm produced a clean named
verdict on both kernels (RED the expected outcome); workload-failure evidence captured on both
kernels with the metal `hs_err` in the log; H1 cycle measured under an hour with room to spare.
No kernel code written — phase 1 is the method phase (R6 surface untouched).

**Interventions:** none this session. Machine facts recorded (not fought): host clock slewed
backward 1 s mid-run (measurement clamps + logs epochs); Akuma 0.0.9 metal answers mmap-RWX
allowed where the guest's tokio-kernel stack answers EACCES by design (`akuma-syscalls-mem::wx`);
the kernel has no xattr syscalls (`mke2fs -d` aborts without `-E no_copy_xattrs`); no vDSO
anywhere (`AT_SYSINFO_EHDR` absent from auxv on both kernels; `/proc/self/auxv` ENOENT).

**Transition rule applied.** Phase 1's gates are green with records complete and the phase
needed **zero** human interventions — per the runbook the agent opens phase 2 itself: NEXT GATE
`PHASE-2-WX` (G1, first blocker in the JVM's own init order; the gate will grep `RWX PASS` from
the guest plus the java arm getting past the `Failed to mark memory page…` line).

**Wall clock:** ~41 min (19:34–20:15 UTC). **Token spend:** not instrumented in-session; the
driver log (`/tmp/denver-autoresearch.log`) is the ledger.

---

## Phase 1 summary — method (runbook transition rule)

| gate | verdict | interventions |
|---|---|---|
| PHASE-1-METHOD (lessons 1, 2, 4 — one gate, D9) | GREEN | 0 |

**What phase 1 proved.** The method exists and runs in minutes, measured: the workload ran on
two kernels with one binary (D11), the RED suite was written before any closing kernel code
(lesson 2, `jvmprobe` named arms), and the hypothesis-to-verdict cycle is in the gate log
(`CYCLE PASS H1 seconds=0`, whole method 9 s wall clock — the kernel-change cycle is explicitly
phase 2's measurement, not claimed here). The curriculum has its first real requirements table:
`denver/docs/REQUIREMENTS.md`, ranked by the JVM's own init order, with the attempt's blocker 1
reproduced first-hand on x86_64 (`Failed to mark memory page as executable`) and a NEW gap the
aarch64 attempt never saw (metal vsyscall SIGSEGV = missing vDSO, G3). Zero kernel code, zero
interventions, machine facts recorded as constraints.

**Decisions taken.** D9 phase 1 = one gate; D10 probes = one C binary with named arms
(jitprobe shape, built on the host's alpine musl gcc); D11 workload A/B on both kernels;
D12 gate marker stays `RESULT PASS`, judged on method evidence. Precedents cited in
`denver/docs/research/PHASE-1-METHOD.md` (R9), including the worked example recovered from git
history and the `/src/akuma-merge` main checkout.

**Transition.** Zero interventions → the agent opens phase 2 itself: `NEXT GATE: PHASE-2-WX`
(G1: the W^X relaxation behind an off-by-default feature in `akuma-syscalls-mem::wx`, mapped
1:1 through `akuma-jvm-kernel` → `denver`; gate greps `RWX PASS` from the guest and the java
arm past the blocker-1 line).

---

## Gate PHASE-2-WX — session 4, 2026-11-13 20:09–21:10 UTC — GREEN (opens phase 2)

**Goal:** close G1 (`denver/docs/REQUIREMENTS.md`): the W^X policy must let HotSpot's
executable-page marking through, behind an off-by-default feature relayed 1:1 through the
front crate (phase-1 transition line). Pass: `RWX PASS rw_to_rx=42` from the guest + the java
arm past the blocker-1 line, captured first-hand in `denver/gates/PHASE-2-WX.log`.

**Commands and outcomes** (capture: `denver/gates/PHASE-2-WX.log`; gate run first, two
diagnostic boots appended in the same file):

| step | command | outcome |
|---|---|---|
| research first (R1) | `denver/docs/research/PHASE-2-WX.md` (D13–D16) | precedents: `TAOXICHUAN_WX_POLICY.md`, git `0dcc33e7`, BUG_FIX_LIST Fix 18 (metal grants W|X), attempt policy note |
| feature, pure half | `wx-relaxed` in `akuma-syscalls-mem` (checks answer `Allow`) | host tests: 46 strict / 43 relaxed, both green (`cargo test -p akuma-syscalls-mem --target x86_64-unknown-linux-musl [--features wx-relaxed]`) |
| feature, machine half | `WX_ENFORCE` boots `!cfg!(feature)` (the `wx=0` hatch at compile time) | relay `denver`→`akuma-jvm-kernel`→`akuma-tokio-kernel`→`akuma-tokio-kernel-amd64`→`akuma-syscalls-mem`; ~45 kernel LOC |
| R6 check | `cargo build -p denver / -p taoxichuan --target x86_64-unknown-none --release` (no features) | **PASS** 22.8 s / 22.9 s — default builds unchanged |
| gate kernel | `cargo build -p denver --features wx-relaxed --target x86_64-unknown-none --release` → `/root/denver/w/denver-wx` + `vm-wx.spec` | **PASS** 24.2 s; phase-0/1 rig untouched (D15) |
| gate probe | `sh denver/probe-wx.sh 2>&1 \| tee denver/gates/PHASE-2-WX.log` | **RESULT PASS first capture**; `uname` = `0.1.0-8a658723-denver` (the relaxed build's hash, in the capture) |
| platform sanity | `/bin/tokioprobe` | **PASS** — RUNTIME/UNAME/HTTPS green under the relaxed policy |
| G1 verdict arm | `/bin/jvmprobe rwx` | `RWX INFO mmap-RWX-allowed` + **`RWX PASS rw_to_rx=42`** (phase 1's RED on this same image: EACCES ×2) |
| G1 movement arm | `/bin/jvmprobe java /opt/jdk/bin/java -version` | **past blocker-1** (`Failed to mark memory page…` never appears): now `JVM RED signal=11 first=""` — raw SIGSEGV, 0 bytes, no `hs_err`, no core; ×3 boots reproducible |
| diagnostics (R8) | two appended boots: census + `hs_err` search + `-Xlog` try | `[Fault] slot 5 killed (SIGSEGV)` (kernel names the death, not the address); `getcpu(168)` never called — death precedes `os::init_2`; ENOSYS window: `setitimer(38)`, `membarrier(324)`, `sysinfo(99)`, `prlimit64(302)`, `getrlimit(97)`, uid/gid family; `/root` empty (no report file); `-Xlog:all=info` **wedged the guest** (ssh keepalive loss — the attempt's tool unusable as-is on this rig); bare `ls` not on guest PATH (`/bin/busybox`) |

**Verdict.** Gate GREEN on the session's first capture: the W^X relaxation is real
(mmap-RWX succeeds; RW→write→RX→execute returns 42), tokio is unregressed, and the workload
moved past the blocker-1 line — exactly the phase-1 pass condition. The movement is honestly
named: the workload now dies at an *unnamed* SIGSEGV before ever calling `getcpu`, so G2 is
proven **not** the next init blocker; REQUIREMENTS.md re-ranked openly (G6
`PHASE-2-FAULTNAME` inserted, reason recorded there and in LOOP_LOG D-3).

**Interventions:** none. Machine facts recorded (not fought): `-Xlog:all=info` wedges the
guest; guest ssh shell has no bare `ls` (`/bin/busybox ls`); HotSpot's report path produces
nothing for this fault (the kernel-side line is the only namer); `[E2-EOF]` spam during the
libjvm mapping (past-EOF page fill, non-fatal — the JVM mapped fine in phase 1 with the same
image); `membarrier(324)` ENOSYS on the amd64 machine half although the pure decision exists
(`akuma-syscalls-mem::membarrier`) — composition hole filed for the next sessions (R2).

**Kernel-change cycle measured** (L4's phase-2 deliverable): feature build 24 s; rebuild →
stage → boot → verdict ≈ 4 min; WIP commit → RESULT PASS capture ≈ 11 min (the probe script
carries no epoch stamps — noted in L2 as the probe improvement the next gate's research
owns).

**Wall clock:** ~61 min (20:09–21:10 UTC). **Token spend:** not instrumented in-session; the
driver log (`/tmp/denver-autoresearch.log`) is the ledger.

---

## Gate PHASE-2-FAULTNAME — session 5, 2026-11-13 22:48–23:04 UTC — GREEN (closes G6; guest-proves G3)

**Goal:** name the workload's post-G1 death (G6): the capture must carry the user-SIGSEGV
delivery path's rip+cr2 line, first-hand (`denver/gates/PHASE-2-FAULTNAME.log`).

**What the research found first (R1, `denver/docs/research/PHASE-2-FAULTNAME.md` Finding 0):**
the session-4 record was wrong in one material claim. The kernel *did* name the address: the
WX gate's own console census (`/root/denver/w/vmrun-diag.log`) carries
`[Fault] ring3 PageFault rip=0xffffffffff600800 rsp=0x00006000014fe768
cr2=0xffffffffff600800 code=0x14 -- task killed` one line above the quoted
`slot 5 killed (SIGSEGV)` line, and the line predates every denver gate (`git log -L`: it is
in `aafbb973`, the kernel file's first commit). The real gap: no gate capture carried the
console (the probe scripts read ssh-side output only), and the census quote dropped the line
that mattered. "Unnamed SIGSEGV" was true only in the module sense. The G6 re-rank stays
valid regardless — the gate's work became capture + interpretation: **zero kernel/crate LOC**
(D17), the proven WX rig unchanged (D18: uname-hash identity check), and
`denver/probe-faultname.sh` (D19): probe-wx shape + per-step epoch stamps (L2 debt 2) +
console-window attribution (byte offsets into `$W/vmrun.log`) + ABI-level module naming
(the legacy vsyscall page — the metal `hs_err` pc, cross-machine corroboration).

**Gate evidence** (two runs, both `RESULT PASS`; run 2 is the record's evidence):

```
== the naming: grep the console window from byte 18176 (+2 s flush)
[Fault] ring3 PageFault rip=0xffffffffff600800 rsp=0x00006000014fe768 cr2=0xffffffffff600800 code=0x14 -- task killed
[Fault] slot 5 killed (SIGSEGV)
FAULT PASS rip=0xffffffffff600800 cr2=0xffffffffff600800 code=0x14 module=legacy-vsyscall-page
RESULT PASS
```

`code=0x14` = user-mode instruction fetch on a not-present page: HotSpot jumps to the legacy
vsyscall page because no vDSO exists — G3's mechanism, now guest-proven (the metal `hs_err`
named the same pc in phase 1). Death line byte-identical across the two runs (deterministic).
Precedents cited in the research artifact (R9): the attempt doc's "fault report that names
the module … or the fault handler printing the VMA" (git `d7878378`); `akuma-exceptions`'
`[SIGSEGV-HEAP] far= elr=` (the metal's own fault-naming discipline); `trace-err`'s
`mm_dump_for` considered and rejected (a slab-history ring, not the map — prints nothing for
a never-mapped page; the honest module-namer is `/proc/<pid>/maps`, G4's gate).

**Decision it forced (D20, re-ranked openly in REQUIREMENTS):** G3 (PHASE-2-VDSO) is first —
the vDSO gap is the proven first blocker; G2 (`getcpu`) second (a death that precedes
`os::init_2` cannot be unblocked by closing it); G4 (maps) third; G5 deferred.

INTERVENTION: the probe's module-interpretation branch compared a prefix-less hex capture
(`sed` captured after `rip=0x`) against the `0x`-prefixed vsyscall constant — run 1 was
already RESULT PASS yet printed `module=unmapped` and a false SURPRISE line. Fixed in one
line, re-run; `tee` overwrote run 1's capture, recorded here rather than hidden. Lesson
echo: a probe whose interpretation can lie is a broken probe (L2's "unbuffered stdio or the
verdicts lie", parse-time sibling).

Machine facts recorded (not fought): the guest console already streams to `$W/vmrun.log` —
capture was a read-with-attribution problem, not new plumbing; boot→ssh round trip ≈ 8 s on
this box (the loop's floor so far); `-Xlog` wedge and busybox-PATH facts from session 4
carried unchanged.

**Kernel-change cycle measured** (L4, named): none this session — the gate is capture-only;
research→GREEN ≈ 35 min, each gate run ≈ 2 min including boot, stamps in the capture (L2
debt 2 paid: per-step `elapsed=` lines, no reconstruction).

**Wall clock:** ~16 min (22:48–23:04 UTC). **Token spend:** the driver log
(`/tmp/denver-autoresearch.log`) is the ledger.

INTERVENTION (operator, after PHASE-2-FAULTNAME, 2026-11-13 ~23:30 UTC): the driver stopped with
FATAL "bookkeeping incomplete" because session 5 wrote its LOOP_LOG gate record under a
"## Gate ..." heading, and the driver counts only "### " headings. Operator changed that one
heading line in denver/docs/LOOP_LOG.md to the "### PHASE-2-FAULTNAME - ..." form and relaunched
the driver; no content and no answer or suggestion was given to the agent. Earlier operator
fix the same day: the driver now puts /usr/local/rust/bin on PATH (cargo missing had read as a
red cargo test; commit 8a64dec6).

### PHASE-2-VDSO — GREEN (closes G3; 2026-11-13 23:37–2026-11-14 00:5x UTC, session 6)

Gate capture: `denver/gates/PHASE-2-VDSO.log` (run 3 = the record: rig `0.1.0-e0924225-denver`,
boot line names both pages, tokioprobe `RESULT PASS`, `RWX PASS rw_to_rx=42`,
`AUXV PASS AT_SYSINFO_EHDR=18446744073699061760`, java arm no-signal/no-vsyscall-fetch at the
new `Too small maximum heap` blocker, `RESULT PASS`, wall clock 7 s). Research artifact:
`denver/docs/research/PHASE-2-VDSO.md` (Findings 0+1, D21–D26).

Run ledger: run 1 RED (tokioprobe SEGV — Finding 1's design verdict: the first build disclosed
a non-ELF page as `AT_SYSINFO_EHDR` and musl's static-PIE startup parses the value unchecked);
two un-judged bisect boots (map-only PASS → map innocent; pair-only SEGV, console-named
`cr2=0xffffffffff600020` → pair guilty); run 2 RESULT PASS with a false SURPRISE line (the
intervention below); run 3 clean PASS (the record). The RED-1/RED-2 verdicts are data, not
failure: they are what forced Finding 1 and the valid-ELF disclosure.

INTERVENTION: the probe's expected-value constant for the auxv arm (`AUXV PASS
AT_SYSINFO_EHDR=<decimal>`) was computed by hand and was wrong — run 2 was already RESULT PASS
yet printed a false SURPRISE line against a correct kernel. Fixed with a python-checked
constant (`0xffffffffff5ff000` = 18446744073699061760) and re-run; `tee` overwrote run 2's
capture, recorded here rather than hidden. Same defect class as PHASE-2-FAULTNAME's run 1 (a
probe whose interpretation can lie is a broken probe), new limb: the *constant*, not the parse.
Lesson echo filed on L2 Record.

Machine facts recorded (not fought): boot→ssh round trip ≈ 3–5 s this session (faster than the
≈8 s FAULTNAME floor — the box was idle); the kernel-change cycle measured end-to-end (L4):
build ≈ 20–23 s, full gate run ≈ 7 s, research→GREEN ≈ 80 min including the RED and bisects.
R6 surface re-verified post-Finding-1: default `denver` 23.0 s, default `taoxichuan` 22.6 s,
gate kernel 20.3 s; clippy clean on the delta (one pre-existing `akuma-exec`
`stable_features` warning, untouched by this pass). The two bisect boots used a
`feature = "never"` cfg hold-out and a temporary `#[allow(dead_code)]`; both reverted before
the GREEN build (the committed tree carries neither).

Kernel delta (≈70 LOC, all behind `vsyscall`, off by default): `crates/akuma-ring3-amd64`
(gated `AT_SYSINFO_EHDR = 33` + pin test), `crates/akuma-tokio-kernel-amd64` (the `vsyscall`
module: legacy stub page + valid exports-free vDSO ELF, `map_vsyscall()` at boot, gated auxv
pair), relay manifests ×3. Zero changes to `akuma-syscalls-*`, no middleware, no hook tables
(R7).

## Operator merge verification — 2026-11-14 — NOT A GATE (no agent session; driver stopped)

INTERVENTION (operator, 2026-11-14): `litter/main` was merged into `denver` (3531cf69,
pushed) and the three gate kernels were rebuilt from the merge and re-staged. Captures:
`/root/denver/merge-verify/` (build-default.log, build-wx.log, build-vdso.log,
gate-wx.log, gate-vdso.log, gate-faultname.log). What the verification covered, and what
it deliberately did not:

- **Verified:** the WX gate on the rebuilt kernel — tokioprobe RESULT PASS, `RWX PASS
  rw_to_rx=42`, uname `0.1.0-3531cf69-denver` (the staged binary is the merge's build).
- **Verified:** the VDSO gate — `RESULT PASS`, `AUXV PASS
  AT_SYSINFO_EHDR=18446744073699061760` (= `0xffffffffff5ff000`); the gate's own verdict
  is MOVEMENT PASS.
- **FAIL by design, not a regression:** the FAULTNAME gate — `RESULT FAIL: rig changed`.
  It pins the historical wx binary hash `0.1.0-8a658723-denver`, which a rebuild cannot
  carry. The probe's identity check is corrected to accept any denver build
  (`0.1.0-*-denver`); the `wx-relaxed` proof stays in the unchanged RWX arm.
- **Not verified:** `sysinfo(2)` on the merged tree. G7's clean `Too small maximum heap`
  exit is gone from the java arm, but no sysinfo probe ran — presumed served over by
  main's memory/sysinfo changes, verdict pending a probe (G7 demoted to verification).
- **New evidence (one behaviour change):** the java arm now dies with a user-mode NULL
  write — `[Fault] ring3 PageFault rip=0x000000003005fc41 rsp=0x00006fffffffba60
  cr2=0x0 code=0x6` — past the heap blocker. The attempt's blocker-3 NULL store (suspect
  since phase 1) is the observed first blocker on the merged tree, filed as G9;
  REQUIREMENTS re-ranked, next gate PHASE-2-PROCFS (name the rip before fixing it).

Session 7 (OPERATOR-IMPROVEMENTS, LOOP_LOG) landed the loop-efficiency changes on top:
`denver/bookkeep.sh`, driver edits, `denver/docs/EFFICIENCY.md`, STATE.md digest,
COST_LEDGER.md.

## PHASE-2-PROCFS — 2026-11-14 ~06:00–06:50 UTC, session 8 (goose, glm-5.3-flash) — GREEN (closes G4)

Second session on this gate (session 7 wrote the research artifact, the kernel-side
D27–D29 work in commits through 5b0587fa, the staged rig, and the D31 probe; its first
probe execution died on a probe defect — see INTERVENTION 1). This session fixed the
probe, executed it, diagnosed the one failing arm, fixed the kernel, and captured
`RESULT PASS` in `denver/gates/PHASE-2-PROCFS.log` (guest `0.1.0-f0dcacb6-denver`).

- **Result:** G4 flipped. `MAPS PASS lines=10 exec_lines=4 bytes=418` — `/proc/self/maps`
  serves real page-table rows (static-PIE image at `0x10000000`, stack at
  `0x6ffffffff000`); `AUXV PASS procfs-auxv bytes=256 pairs=15 pagesz=4096
  AT_SYSINFO_EHDR matches stack=18446744073699061760` — `/proc/self/auxv` serves the
  whole 15-pair table, agreeing with the stack vector; phase 1's `errno=2` evidence
  line is gone. Ride-alongs green: tokioprobe `RESULT PASS`, `RWX PASS rw_to_rx=42`,
  stack-auxv disclosure unchanged.
- **The kernel fix (D32, research artifact):** `maps` was absent because the moved walk
  (D27) resolves the *metal* owner — a non-shared process — and the tokio kernel's
  `disown_space` makes every registered `Process` a `new_shared` **borrowed view** of a
  root the kernel itself owns (`OWNED_ROOT` per slot; a dead image's root is parked, not
  freed). Owner resolution answered `None` for every pid ⇒ file absent (errno=2) —
  while `auxv`, served off plain `Process` fields through the same hook table, worked,
  which pinned the divergence to the address-space model, not the registration.
  `auxv`/`maps` divergence was the bisect; no extra boot needed (L4: cheapest possible).
  Fix: `pid_map_rows_of_kernel_owned_roots` (leader's regions, pid's own view, the same
  l0 re-check; `with_process`'s exclusive slot hold excludes the concurrent swap) in
  `akuma-exec::process::maps_rows`, registered by `akuma-runtime-tokio` under
  `procfs-diag` — model knowledge stays in the kernel stack that owns it; no model
  flag, no new hook table (R7). The metal keeps `pid_map_rows` verbatim (its builds
  re-verified green). Host tests: `akuma-procfs` 45 pass; `akuma-exec`/`akuma-mmu` are
  bare-metal-build-only on this box (recorded in L4).
- **The payoff arm's limit (recorded, not hidden):** the NAME arm
  (`jvmprobe name /opt/jdk/bin/java 3005fc41 -version`) starves the guest's sshd —
  java startup plus the 2 ms `/proc/<pid>/maps` poll drops the connection within
  seconds and sshd never recovers (nine reconnect stalls over ~2.5 min; reproduced
  standalone after the gate run). Its diagnostic window *did* work: both runs' console
  shows the workload dying at the exact expected rip —
  `[Fault] ring3 PageFault rip=0x000000003005fc41 ... cr2=0x0 code=0x6` + `slot killed
  (SIGSEGV)` — while the poll ran. What the gate proves: the files serve, the workload
  dies at the expected rip, the rip constant survives a third machine. What it does
  **not** yet give G9: the maps snapshot attributing `0x3005fc41` to a module — the
  attribution report cannot cross the wedged sshd. G9's session should first capture it
  with a slower poll (jvmprobe change) or a console-side path.
- **Verdict honesty (D31 vs implementation):** the implemented probe FAILs the NAME arm
  only on `NAME MISSING`/the W^X blocker line; with ssh starved, neither `NAME PASS`
  nor `NAME SURPRISE` crossed the wire and the verdict arms did not fire —
  `RESULT PASS` rests on conditions 1–6 plus the console fault recurrence, not on an
  attribution verdict. Recorded here because the log line alone over-states the arm.

INTERVENTION: run 1 of the D31 probe (session 7, 05:00:32 UTC) FAILed a healthy rig:
the probe defined `VDSoHASH` but compared `$MERGEDHASH`; the unset variable expanded to
the empty pattern, and `*""*` matches any uname — a false "the vdso rig binary booted"
against the correct procfs rig. Fixed by naming + a fail-fast guard (empty identity
constant aborts before boot; the failure mode can only be loud). Fourth limb of L2's
"verdicts lie" rule: python-check the *value*, compile-check the *name*.

INTERVENTION: host toolchain facts, recorded for the next sessions — `cargo` at
`/usr/local/rust/bin` (nightly 1.101, std for `x86_64-unknown-linux-musl` +
`x86_64-unknown-none` + rust-src) is shadowed by the system Alpine rustc 1.96.1 unless
`PATH=/usr/local/rust/bin:$PATH` (the driver does this at line ~102); host tests need
`--target x86_64-unknown-linux-musl` (`akuma-procfs` host-tests green; `akuma-exec`,
`akuma-mmu` compile only bare-metal). Building the gate kernel overwrites
`target/.../denver` with the default build if run after it — stage in that order
(default first, gate last), verified by size (gate ≈ +29 KB).

INTERVENTION: the NAME arm's ssh starvation (detail above) — probe-side cadence, not a
kernel defect; two reproductions; data for G9's method choice, not a gate condition.

## PHASE-2-NULLSTORE — 2026-11-14, sessions 9–10 (run to PASS, cut off before records) + session 11 ~08:50–09:00 UTC — GREEN (G9 named to first order)

**Session 9–10 work (recovered from git + the gate log, reconstructed by session 11):**
research artifact (R1, D33–D37), console-side attribution at the ring3 kill path
(`fault_attribution`: map rows around rip + GPR file + stack qwords, behind `procfs-diag`;
`trace-err` relayed per D34; metal untouched, default builds byte-identical), probe
`denver/probe-nullstore.sh` (D36: seven conditions) with jvmprobe `mmaparms`/`sysinfo`
java-free arms (D35). Probe run: `RESULT PASS` in `denver/gates/PHASE-2-NULLSTORE.log`
(guest `0.1.0-800e8c80-denver` = HEAD, wall 43 s). What the gate proves: the NULL store is
**named and explained to first order** — musl fstatat's copy-out (`rep stos`) through a
buf register the KERNEL zeroed: the raw `stat` inside fstatat succeeded with a real buf
(ring nr=4 a2=0x6fffffffba80 ret 0), then r8/r9/r10 came back 0. Session 11 ran the
staged-but-never-dispatched `regpreserve` canary arm java-free on the same build: lazy
faults and plain syscalls keep every canary; the kernel-copy-out-into-an-unfaulted-page
shape clobbers all six callee regs, restoring the syscall's internal marshaled arguments
(rdi=AT_FDCWD, rdx=the buf) — **the syscall-internal fault-retry path rebuilds the user
GPR file** (D38 in the research artifact). Verdict per D37's honest branch: GREEN = named;
the fix (trap-frame preservation, bisected first) is PHASE-2-NULLFIX's opening job. Also
recorded-not-judged in the same log: mmap W+X encodes never-writable (Linux honors W+X) —
fix candidate for the suspects; `SYSINFO INFO errno=38` — G7's feed (main's merged changes
did not serve sysinfo; PHASE-2-MEMINFO stays behind the fix).

INTERVENTION: session 10 was cut off after `RESULT PASS` but before its records (LOOP_LOG
entry, RESULT section, REQUIREMENTS update, the gate-log commit) — exactly the failure
mode the records-before-turn-~120 rule exists for. Session 11 reconstructed from
STATE.md + the gate log + git diffs (the log's guest stamp is HEAD; its NAME lines match
the working-tree probe's format → the evidence is current, no re-run needed). Cost: one
session's context wasted (194k final ctx, 76 min — COST_LEDGER); the digest mechanism
itself worked as designed.

INTERVENTION: the `regpreserve` arm was compiled and staged into denver-jdk.img (07:57,
binary+image same-second mtimes) but probe-nullstore.sh has no dispatch for it — the
passing run carried dead evidence. Session 11 booted the gate build once and dispatched
the arm over ssh (java-free → sshd healthy), producing the fix gate's measured datum.
Rule for probes: an arm that isn't dispatched by the script must be runnable standalone,
and every staged arm gets a dispatch line before the run is called complete.

INTERVENTION: the ride-along's guest ended on its own after the arm's ssh timeout; the
first `echo stop > /dev/kvm0` EINVAL'd because the slot was already free — re-issued,
zero vm processes remained (pgrep). Slot discipline held: one boot, one arm, nothing
orphaned.

INTERVENTION: session 10's kernel-side refinements to D33 (`fault_attribution` stack
window 64→256 B; `ring_dump_slot` newest-first capped) were left UNCOMMITTED at the
cutoff — the gate kernel (`/root/denver/w/denver-nullstore`, mtime 07:42) was built from
the working tree, so the gate log's output format corresponds to uncommitted source.
Session 11 re-verified both targets green (`cargo build -p denver --target
x86_64-unknown-none --release` with and without
`--features wx-relaxed,vsyscall,procfs-diag,trace-err`; 27.9 s / 26.9 s, lints clean)
and committed source + evidence in one commit. Note for the next staging session: the
working tree's `target/` now holds the DEFAULT binary (session 11's last build) — stage
gate-last as recorded.

INTERVENTION: kernel builds failed until `/usr/local/rust/bin` was prepended to PATH:
the host's default `rustc` on PATH is Alpine's stable (1.96.1, no
`x86_64-unknown-none` target — "can't find crate for `core`"), while the nightly
cargo 1.101.0 at /usr/local/rust/bin (rustc 1.101.0-nightly, target installed) is
LAST in PATH, so `rustc --version` and `cargo build` resolve different compilers.
The rust-toolchain.toml pin (nightly-2026-09-19) is inert without rustup. First
observed in this session; the autoresearch driver's own build steps presumably run
with a PATH that has it first.

INTERVENTION: the regpreserve arm's shape (c) rdi/rsi expectations were
unsatisfiable-by-ABI — the arm's own asm loads rdi/rsi with the syscall arguments,
so a preserved register must return its ENTRY value (path/buf), not the canaries
those movabs put there moments earlier. The first PASS attempt failed on exactly
those two lines while r8/r9/r10/rdx (the proof that matters) came back kept.
jvmprobe.c now checks rdi/rsi against the entry values (D42), was rebuilt
(`gcc -static -O2`) and re-injected into denver-jdk.img with `debugfs -w`
(the first attempt without `-w` opened the image read-only and silently no-oped).

INTERVENTION: the probe's `head -30` evidence display cut the kill-path
attribution out of the gate log — the fault line and the `[ring]` tkill dump fit,
but `ring3 map/regs/stack`, `[mm]` and `[fault-code]` did not. Recovered from the
rig's `vmrun.log` (the real console capture) and appended post-verdict. Rule of
thumb going forward: display bounds are not evidence bounds — grep the source
file, and size window displays to the ring's cap (40) + attribution (~15), not 30.

INTERVENTION: host tests for the touched pure crates E0463'd ("can't find crate
for core") on bare `cargo test -p` — the pinned nightly has no host-std core on
this box. Resolved with `--target x86_64-unknown-linux-musl` (L4's PROCFS shape,
re-paid): 46 strict / 46 relaxed green for akuma-syscalls-mem.

INTERVENTION: attempt 1 verdict was an honest FAIL (java died at the old rip on
the sweep-only kernel); its gate log is preserved as
PHASE-2-HEAPRO.attempt1-FAIL.log and its death's full ring was read from the
rig's vmrun.log — the head-40 evidence display cut the attribution again
(the NULLFIX display-bounds fact, recurring; the verdict now cites vmrun.log).

INTERVENTION: the rwx probe arm could never PASS — it judged the mapping from
/proc/self/maps, which does not show mmap-slab mappings, so saw_w stayed -1
(the recorded "W+X dropped W" divergence was half probe gap, D42 again).
Rewritten write-then-call; two image re-injections carried the new jvmprobe
(attempts 2 and 3).

## PHASE-2-HEAPRO — 2026-11-14 ~09:47–10:55 UTC, session 13 (goose, glm-5.3-flash) — GREEN (the death moved; the W+X silent drop named and closed)

Three gate runs, one research artifact, two kernel changes, both behind off-by-default
features. `RESULT PASS` in `denver/gates/PHASE-2-HEAPRO.log` (guest
`0.1.0-c921d625-denver`, wall 37 s): all riding arms green (tokioprobe, RWX, MAPS+AUXV,
MMARMS incl. the new `MMARMS PASS rwx call=42`, `SYSINFO INFO errno=38` recorded for G7,
regpreserve 20/20, LAZYREL PASS both arms), and the java arm MOVED — the old death rip
`60000067cd7a` never appeared and no java fault reached the window at all.

- **Result:** the NULLFIX java death (user write to a present RO page, code=0x7,
  pte unmarked refs=2) is closed by `wx::exec_write_bit` (D49): under `wx-relaxed` a
  W-bearing executable mmap honors its write bit (the old slab mmap arm silently encoded
  every exec mapping `write=false` — java's RWX heap came back RO-X). The strict policy
  refused the call outright, so no silent drop remains in any build; strict `.text`
  carries only 47 relocation-level byte shifts, semantics pinned by 46+46 host tests
  (`--target x86_64-unknown-linux-musl`). G8's recorded divergence suspect is CLOSED.
  The gate's other arm, `lazy-release-sweep` (D44/D45), closes a real record-survival
  hole — released ranges stay dead (lazyrel PASS anon + file) — and rides on all future
  gate kernels.
- **Records:** research artifact `denver/docs/research/PHASE-2-HEAPRO.md` (D43–D51),
  attempt logs preserved (`PHASE-2-HEAPRO.attempt1-FAIL.log`,
  `PHASE-2-HEAPRO.attempt2-FAIL.log`), REQUIREMENTS re-ranked (G7/PHASE-2-MEMINFO next),
  LOOP_LOG entry recorded, L4 Do 5/6 + Record updated.
- **Next gate's data:** the java arm moved WITHOUT a capturable new death — no fault in
  the 10 s window, `/tmp/java-out.log` unrecoverable (sshd starved, RC=127). The new
  first fatal is uncaptured; PHASE-2-MEMINFO opens with a capture problem (longer
  window, output recovery), and sysinfo(2) is still served nowhere.
- **Recorded suspects, unranked (D51):** the slab placer hands `mmap(NULL)` ranges
  overlapping live fixed mappings (java's ring: three-plus collisions — data-corruption
  class); the `mmdbg` bisect hatch gates the slab's release bodies (default 7 is honest;
  any lower value makes munmap/MAP_FIXED silent no-ops that answer success).

## PHASE-2-MEMINFO — 2026-11-14 ~10:52–11:15 UTC, session 14 (goose, glm-5.3-flash) — GREEN (G7 closed: the sizing feed went errno=38 → the guest's real 3 GiB)

| step | command / artifact | verdict |
|---|---|---|
| research artifact | `denver/docs/research/PHASE-2-MEMINFO.md` (D52–D54; R9: metal kernel's `to_glue` translation, glue's table, taoxichuan census, BUG_FIX_LIST lessons) | written before code (R1) |
| code | one translated arm in `crates/akuma-tokio-kernel-amd64/src/lib.rs` (`Syscall::Sysinfo` → glue's `handle_syscall`, the metal kernel's shape / the `RT_SIGSUSPEND` precedent), `sysinfo` feature relayed 1:1 in 4 Cargo.tomls | ~28 LOC incl. comments; no new pure logic (glue + `akuma-syscalls-linux::Sysinfo` reused as-is) |
| R6 check | default build `.text` sha256 before/after the change | **identical** `527770305e43d34a…` (the whole-binary hash moves when comments shift line numbers — metadata carries them; section-level compare is the standard, D49's lesson) |
| host tests | `cargo test -p akuma-syscalls-linux --target x86_64-unknown-linux-musl sysinfo` | **PASS** 4/4 incl. `defaulted_sysinfo_has_no_uninitialised_bytes` (the padding-leak canary) |
| gate kernel | `cargo build -p denver --features wx-relaxed,vsyscall,procfs-diag,trace-err,fold-regpreserve,lazy-release-sweep,sysinfo --target x86_64-unknown-none --release -j4` → `/root/denver/w/denver-meminfo` + `vm-meminfo.spec` (heapro spec's disk and port, new kernel path) | **PASS** 26.6 s; NO image re-injection (the jdk image already carried the sysinfo/lazyrel arms) |
| attempt 1 | `sh denver/probe-meminfo.sh` → `denver/gates/PHASE-2-MEMINFO.attempt1-FAIL.log` | honest FAIL — the probe's own totalram sanity check sed'd the RAW multi-line `$sys` (ssh banner lines inside), so the test saw a multi-line number; the gate's named line `SYSINFO PASS totalram=3220164608` had ALREADY landed in that same run |
| gate | `sh denver/probe-meminfo.sh` → `denver/gates/PHASE-2-MEMINFO.log` | **RESULT PASS**, wall 103 s, guest `0.1.0-24398b99-denver` |

`SYSINFO PASS uptime=31956 totalram=3220164608 freeram=2675982336 mem_unit=1` — G7's feed
SERVED (was `SYSINFO INFO errno=38`, measured twice). The real 3 GiB from `akuma-pmm`'s
frame counters, not a constant (the BUG_FIX_LIST hardcoded-256MB lesson); uptime ≈ 8.9 h
answers the clock-hook known-unknown (the hook IS registered). Riding arms all green:
tokioprobe, RWX `rw_to_rx=42`, MAPS lines=10, AUXV procfs-auxv, MMARMS incl.
`rwx call=42`, regpreserve 20/20 with zero REGRED, LAZYREL both arms.

Java arm MOVED (old rip `60000067cd7a` absent) and the capture instrumentation advanced
the datum: window 30 s + two `/tmp/java-out.log` recoveries — **no fault at all, no exit
report in ≥90 s**: java survives and runs long post-sysinfo, its fate still unreadable
(sshd starved both recoveries, RC=127). The window's only fault was the lazyrel child's
DESIGNED death (rip `0x10002bb0` cr2 `600000000000` code=0x4 — the HEAPRO artifact's own
proof arm, arriving by console lag), correctly attributed, not alarmed. java's fate is
the phase-3 blocker datum; every java-carrying gate now opens with it.

**Interventions (2):**
- INTERVENTION: `git add -A` swept an unrelated embedded gitlink
  (`userspace/nca/native-cli-ai`) into the WIP commit; untracked in a follow-up
  `git rm --cached` commit (never rewrite history; the WIP was already pushed).
- INTERVENTION: attempt-1 FAIL was the probe's own sanity check misreading a multi-line
  capture — extraction must grep the judged line first (`SYSINFO PASS`), never sed the
  raw capture; the kernel change under test was already proven by that same failed run.

## Gate PHASE-2-GETCPU — 2026-11-14 ~13:50–15:05 UTC, session 15 (goose, glm-5.3-flash) — GREEN (closes G2)

| record | what | outcome |
|---|---|---|
| research artifact | `denver/docs/research/PHASE-2-GETCPU.md` (D55–D58; D57 = the superseded diagnosis trail, kept; D58 = the number correction) | updated before code moved (R1); D55's "tables agree" premise corrected |
| code | one hand-written ROW-FREE const `akuma_syscalls_abi::x86_64::GETCPU = 309` + the arm re-keyed (`akuma_syscalls_abi::x86_64::GETCPU`, was `nr::GETCPU`=168) | ~14 LOC incl. comments; glue/pure-crate/relay/probe untouched |
| R6 check | default build `.text` sha256, stash→build→pop→build (the D49 standard; the 11:27 cross-commit snapshot was invalid — attempt-1's abi-row tree) | **identical** `94da2dbf0f19c44814e2b4c9…` |
| gate kernel | the D56 feature set build → `/root/denver/w/denver-getcpu`, staged (marker build staged for the diagnosis, clean build for the gate) | boot marker `[gcpu-build]` seen; arm markers never fired for the guest's real call |
| attempts 2–3 | preserved FAIL logs (`PHASE-2-GETCPU.attempt2-FAIL.log`, `.attempt3-FAIL.log`) | honest FAILs — the arm keyed on 168; the guest performed 309 (`[nosys] nr=309` at the gate, misread twice as sshd noise) |
| gate | `sh denver/probe-getcpu.sh` → `denver/gates/PHASE-2-GETCPU.log` | **RESULT PASS**, wall 83 s, guest d4acb339+arm |

`GETCPU PASS cpu=1 node=0` — G2's feed SERVED (was `GETCPU RED ENOSYS` since PHASE-1): the
caller's core from `ring3::cpu_index()`, node 0, either output pointer optional, `EFAULT`
on bad user ranges. Riding arms all green: tokioprobe, RWX `rw_to_rx=42`, MAPS lines=10,
AUXV procfs-auxv, MMARMS incl. `rwx call=42`, regpreserve x20 zero REGRED, LAZYREL both
arms. The java arm MOVED with no fault at all in the 30 s window, output unrecoverable
over ssh again — the phase-3 capture problem (window / output recovery) is the next
blocker, per the HEAPRO and MEMINFO records.

**The lesson this gate paid for** (recorded in L2): the number a probe's libc sends is
the number to serve — `SYS_getcpu` on x86_64 musl is **309**, not the asm-generic 168
the research premise assumed; and a console `[nosys] nr=…` line that appears exactly at
the gate's moment is a datum, not noise.

**Interventions (3):**
- INTERVENTION: the session's first R6 comparison failed against `/tmp/denver-default-pre-getcpu`
  — a cross-commit snapshot (it still carried attempt-1's abi `Getcpu` row, which shifts
  every build's decode path); the honest comparator is stash→build→pop→build on the same
  commit, and it passed byte-identical.
- INTERVENTION: a feature-gated marker kernel (boot print + arm-enter/ret prints) was
  built and staged to prove the arm was never entered; reverted from the tree after the
  diagnosis (the clean d4acb339 binary was preserved beside it for the gate).
- INTERVENTION: the gate's own `[nosys] nr=309` + `[err] s4 nr=309 -38` console lines were
  attributed to sshd noise twice, costing the diagnosis several turns — per-line
  attribution (which nr, which slot, which moment) before "noise" is now the rule.
