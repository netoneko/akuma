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
