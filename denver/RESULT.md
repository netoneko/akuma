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
