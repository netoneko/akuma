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
