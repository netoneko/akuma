# PHASE-0-SCAFFOLD — research (R1), written before any change

**Gate.** `denver/gates/PHASE-0-SCAFFOLD.log` carries the probe output; the gate is green when
`RESULT PASS` appears there (driver convention, `denver/autoresearch.sh`). Phase-0 done-when:
`denver/` + `akuma-jvm-kernel/` crates exist; **Denver boots tokio, `uname` says denver, feature
parity with the pass-through** (`denver/docs/archive/KERNEL_HOWTO_BUILDING_CUSTOM_PLATFORM_KERNEL.md`
phase table; lesson 3).

**Probe.** `tokioprobe` — the exact workload lesson 2 names and Shanghai's gate 5 ran on this
machine: it builds a multi-thread tokio runtime (epoll), runs `uname -a` through `tokio::process`
(pipe2, clone/execve, wait4, SIGCHLD), fetches a page over HTTPS (DNS, non-blocking connect,
TLS), and prints `RUNTIME PASS`, `UNAME PASS …`, `HTTPS PASS …`, `RESULT PASS`
(`userspace/tokioprobe/src/main.rs`, shanghai branch). `UNAME PASS Linux denver …` proves the
uname half of the gate; the probe running at all proves the "boots tokio" half; the kernel
itself proves the scaffold.

## 1. What the gate needs that the denver branch does not have

The frozen plan is explicit about the platform: lesson 3 builds "`akuma-jvm-kernel`: front crate +
feature map … features propagate the way `console` does today", and the plan's *Read in this
order* table cites `crates/akuma-tokio-kernel/README.md` and `shanghai/README.md`. **Neither file
exists on branch `denver`.** The whole `akuma-tokio-kernel*` stack lives on the sibling branch
`shanghai` (tip `d4a6c7d9`, 240 commits ahead of the merge-base `cf687e25`), where it was
extracted from Taoxichuan's `src/` by proposal B15 (`proposals/B15_TOKIO_KERNEL_KIT.md`,
"B15. Moved from Taoxichuan's src/") and **proven end to end on this very machine**: shanghai
`RESULT.md` records six gates PASS on the ryzen booted into Akuma bare metal — build, image,
boot, `busybox uname -a` over ssh, tokioprobe at 1 and 4 cores (`RESULT PASS` twice), and goose
answering PONG inside the guest. The host I am on *is* that box (`uname -a`: `Akuma akuma-ryzen
0.0.9 … x86_64`), and its `/root/shanghai/w/` still holds the artifacts: `shanghai.img` (busybox,
sshd, `/bin/uname`, `/bin/tokioprobe`, authorized key), `tokioprobe.x86_64`, `key`, `vm.spec`.

So the first scaffold decision is how the platform stack arrives on `denver`.

## 2. Crates, files, functions (named, with paths)

**Front crate** `crates/akuma-tokio-kernel/src/lib.rs` (44 lines, `#![no_std]`,
`#![forbid(unsafe_code)]`):
- `entry!(main)` macro — expands to `kmain` (PVH) and `kmain_mb2` (multiboot2); `Kernel::new(start_info, CARGO_PKG_NAME, CARGO_PKG_VERSION)` — **the package name is `uname -n`** (so a crate named `denver` makes uname say denver).
- `pub use akuma_tokio_kernel_amd64::{Kernel, boot_multiboot2}` under `cfg(all(target_arch = "x86_64", target_os = "none"))`.
- `build.rs` — relays the machine half's linker script: `DEP_AKUMA_TOKIO_KERNEL_AMD64_LINKER_SCRIPT` → `cargo:linker_script=<path>`, because `DEP_<links>_<key>` reaches only a *direct* dependent.
- Features (its `Cargo.toml`): `console` → `akuma-tokio-kernel-amd64/console`; `disk`, `net`, `smp`, `shell`, `no-tests`, `trace-err` likewise; `default = ["console", "disk", "net", "smp"]`.

**Machine half** `crates/akuma-tokio-kernel-amd64` (x86_64-unknown-none only; "The machine half
of akuma-tokio-kernel on amd64: a whole PVH guest kernel … Moved from Taoxichuan's src/ (B15)").
Its dependency lines are the crate inventory a composed kernel needs, in boot order:
`akuma-boot-amd64` (PVH trampoline, physmap, page tables, heap+PMM), `akuma-comconsole` (feature
`console`), `akuma-gdt`, `akuma-idt-amd64`+`akuma-trap-amd64` (`[Fault]` reporting), `akuma-ryzen-amd64`
(PVH handoff, memmap, cmdline), `akuma-alloc`+`akuma-pmm`, `akuma-primitives` (print hook, physmap
limit, virtio window), `akuma-virtio` (virtio-mmio blk/net), `akuma-ext2` (feature `disk`),
`akuma-vfs`+`akuma-vfs-glue` (shared mount table), `akuma-cpu`, `akuma-cow`, `akuma-mmap`,
`akuma-fpu-amd64`, `akuma-user-tree`, `akuma-ring3-amd64` (GDT/IDT/sysret), `akuma-elf-exec`
(PT_INTERP/auxv decisions), `akuma-syscalls-linux/abi/mem/proc`, `akuma-net`+`akuma-net-nic`,
`akuma-isolation` (default-features = false), `akuma-mmu`, `spinning_top`, `akuma-tty`,
`akuma-exec`+`akuma-threading`, `akuma-lapic-amd64`, `akuma-sched-amd64`, `akuma-user-walk`,
`akuma-user-layout`; syscall surface `akuma-runtime-tokio`.

**Crate delta shanghai→denver** (`git diff --name-status denver..shanghai -- crates/`):
- 13 **added**: `akuma-tokio-kernel`, `akuma-tokio-kernel-amd64`, `akuma-runtime-tokio`,
  `akuma-affinity`, `akuma-fpu-amd64`, `akuma-sched-amd64`, `akuma-syscalls-glue-amd64`,
  `akuma-syscalls-proc`, `akuma-syscalls-signal`, `akuma-user-layout`, `akuma-user-tree`,
  `akuma-user-walk`, `akuma-virtqueue`. No `mw-*` middleware crates exist on shanghai (R7 is
  already the recorded lesson there).
- 14 **modified** shared crates (the extraction moved code *into* them): `akuma-exceptions`,
  `akuma-exec` (gains off-by-default `elf-loader` feature — R6's own pattern),
  `akuma-isolation`, `akuma-kernel-core`, `akuma-kernel-glue`, `akuma-mmu`, `akuma-net`,
  `akuma-net-nic`, `akuma-syscalls-glue`, `akuma-syscalls-net`, `akuma-timer`, `akuma-vfs-glue`,
  `akuma-virtio` (blk/net driven in-tree via `akuma-virtqueue`), `akuma-vm-spec`; ~137 files,
  +15.7k/−1.0k lines.

**The pairing.** The moved code means old callers must move too — this is why a crates-only sync
would break the tree. Shanghai pairs the crate state with: root `Cargo.toml` (members,
default-members, `sound` feature without `virtio-drivers/alloc`, `sc-containers` +
`akuma-isolation/containers`, root deps `akuma-exec` +`elf-loader`, `akuma-isolation`
default-features = false, `akuma-syscalls-glue` +`exec,akuma-native`, new dep
`akuma-syscalls-signal`, `virtio-drivers` direct dep dropped, `smoltcp` `socket-dns` feature
dropped); metal `amd64/src/*` (sched −733, signal −259, futex, usermode — pulled into
`akuma-sched-amd64`/`akuma-syscalls-signal`); `src/process_tests.rs` (+24, one new test);
`taoxichuan/src/*` (main.rs 3899 lines → the crates); `CLAUDE.md` (4 lines). One coherent,
machine-proven state.

## 3. Reusable as-is / missing / plan

**Reusable as-is:** all of the above — R2 (compose, don't fork) and R5 (a partial port is
fork-shaped: any fix I write against a half-synced closure diverges from the proven whole).
**Missing on denver:** the `akuma-jvm-kernel` front crate (zero capabilities at scaffold), the
`denver/` kernel directory, the root `members` line, and the driver's staging of root manifests.

**Decision D1 — acquire the platform by `git merge shanghai`** (not cherry-pick, not re-port).
Precedents: the plan cites files that exist only on that branch; B15 is the extraction proposal
the crates implement; shanghai `RESULT.md` is the proof they work here; R2/R5 above. A merge
keeps provenance in history and takes the *pairing* (crates + root manifest + metal kernel +
taoxichuan) as one unit — exactly the property a partial copy cannot give. Denver's own files
(`denver/autoresearch.sh`, `denver/docs/**`, the recipe) are untouched by the merge (shanghai
never had them). Two denver-side keepers after the merge: `PLAN-pm.md`/`PLAN-pn.md` (shanghai
deleted them as housekeeping; denver's planning docs stay). Known merge friction: the two
submodule pointers shanghai advanced (`userspace/rumpkernel/src-netbsd`, `userspace/tcc/tinycc`,
both currently dirty in the working tree) — resolve to shanghai's pointers if git balks, and
`Cargo.toml`/`Cargo.lock` merge cleanly (denver never edited them post-branch).

**Decision D2 — `akuma-jvm-kernel` is a 1:1 feature-map pass-through, zero capabilities.**
Precedent: `akuma-tokio-kernel` itself is exactly this shape one level down (front features map
onto machine-half features, `console = ["akuma-tokio-kernel-amd64/console"]`); plan lesson 3
("features propagate the way `console` does today"); R6. So:

```toml
[features]
default = ["console", "disk", "net", "smp"]
console = ["akuma-tokio-kernel/console"]   # disk, net, smp likewise
shell = ["akuma-tokio-kernel/shell"]
no-tests = ["akuma-tokio-kernel/no-tests"]
trace-err = ["akuma-tokio-kernel/trace-err"]
```

`lib.rs` re-exports `{Kernel, entry}` (+ `boot_multiboot2`) under the same target cfg; `build.rs`
relays `DEP_AKUMA_TOKIO_KERNEL_LINKER_SCRIPT` → `cargo:linker_script=` under
`links = "akuma_jvm_kernel"` — the same relay pattern, one hop up. `#![forbid(unsafe_code)]`,
host-empty (machine code is target-gated), like the front crate it mirrors.

**Decision D3 — the denver kernel is the README's three-file kernel.** Precedent:
`akuma-tokio-kernel/README.md` ("A new kernel is a directory at the repository root with three
files", example `osaka/`) and `shanghai/src/main.rs` (18 lines):

```rust
#![no_std]
#![no_main]
akuma_jvm_kernel::entry!(main);
fn main(kernel: akuma_jvm_kernel::Kernel) -> ! {
    kernel.name("Denver").banner(include_str!("banner.txt")).console().disk().net().smp().run_init()
}
```

Package name `denver` ⇒ `uname -n` says denver. `Cargo.toml` depends on `akuma-jvm-kernel`
(default features + `no-tests`, exactly as shanghai's kernel did); `build.rs` reads
`DEP_AKUMA_JVM_KERNEL_LINKER_SCRIPT`. Banner within the shanghai rules (≤ 19 lines, ≤ 75 columns,
plain ASCII). Not in `default-members` (x86_64-only, same reason as taoxichuan/akuma-amd64);
root `members` gains `"akuma-jvm-kernel", "denver"`.

**Decision D4 — probe rig copied from shanghai gate 5, artifacts outside the tree.**
`/root/denver/w/{denver,denver.img,vm.spec,vm.log,vm.pid}` (precedent: shanghai `RESULT.md`
— "All images, keys, wrappers and logs live outside the tree under `/root/shanghai/`"). Image:
byte-copy of `/root/shanghai/w/shanghai.img` (it already carries busybox, sshd,
`/bin/uname`, `/bin/tokioprobe`, the authorized key — nothing in it is shanghai-specific).
Boot: `herd vm-run <spec>` — `type=kvm`, `kernel=/root/denver/w/denver`,
`cmdline=init=/bin/sshd initargs=--port,22 smp=4 epoch=1793645322`, `ram_mib=3072`, `vcpus=4`,
`disk=/root/denver/w/denver.img`, `outbound=1`, `forward=2247:22` (port 2247, not shanghai's
2246; only the VM I start, stopped only via its pid file). Probe: `/bin/ssh -i key -p 2247
root@127.0.0.1 'busybox uname -a && /bin/tokioprobe'` (shanghai gate 4/5 command shape; the
Akuma ssh client takes no `-o` options — recorded deviation). Expected tail of the capture:
`UNAME PASS Linux denver …`, `HTTPS PASS …`, `RESULT PASS`.

**Decision D5 — driver staging must include the root manifests** (`Cargo.toml`, `Cargo.lock`):
without the `members` line the gate commit cannot build its own kernel (`cargo build -p denver`
fails for a non-member), which would make the record a broken tree — the one unforgivable exit.
Machinery fixes are logged in `denver/docs/archive/AUTORESEARCH_WORK_LOG.md` (precedent: the
endpoint-pinning and lock-handling entries); the staging loop also learns to skip root-manifest-only
paths when deriving `cargo test -p` packages, so the record commit does not pay a full root-kernel
host-test for a one-line members change.

## 4. New-LOC estimate (R5 budget)

- `akuma-jvm-kernel/`: Cargo.toml ~25 lines, lib.rs ~15, build.rs ~8 — **~50 LOC, zero unsafe**.
- `denver/`: Cargo.toml ~15, build.rs ~6, main.rs ~10, banner.txt ~19 — **~50 LOC**.
- Root `Cargo.toml`: +1 members token (plus the merged proven state, which is import, not new code).
- Driver: +1 staged path, +1 skip clause — **~2 LOC**.
Everything else arrives by merge from a machine-proven branch. If implementation exceeds ~2×
these estimates, stop and re-research (R5).

## 5. Risks and what would falsify the plan

- Merge conflicts on `Cargo.lock` (regenerate by build if messy), submodule pointers (resolve to
  shanghai's), `CLAUDE.md` (4-line delta, take shanghai's).
- The aarch64 default build must stay green (R6: "the repo's default build never changes") —
  verified post-merge with a targeted `cargo check` of the root package before the probe.
- tokioprobe as *not-pid-1* under sshd is the proven shape; `init=/bin/tokioprobe` directly is a
  fallback hypothesis only (stall protocol applies after three).
- HTTPS probe needs `outbound=1` + the image's `nameserver 10.0.2.3` (shanghai gate 5 conditions,
  unchanged).

---

## Post-reboot addendum (session 2, the redo pass — written before this session's probe run, R1)

Session 1's verdict stands unchanged; the plan survived contact with the answered escalation.
What the redo pass had to decide, with precedents:

**D6 — the escalation's answer is accepted as machine state, then re-proven first-hand.** The
human rebooted into the kvmctl-armed kernel (GRUB entry 29, `kvmctl kvmmem=4096` — the exact
remedy session 1 named), ran `sh denver/vm-probe.sh` by hand, appended the capture (it ends
`RESULT PASS`; `UNAME PASS Linux denver …`), and committed it with the script as WIP
`b0f1e325`. Precedent for accepting operator-supplied machine state: shanghai `RESULT.md`
(rig prep outside the tree, human owns machine policy) and the escalation archive itself
(`denver/docs/archive/escalations/2026-11-13-PHASE-0-SCAFFOLD.md` — "the next session only
boots, probes, and records"). The session's own evidence is still first-hand: the probe is
rerun by this session against the same rig and captured again (see `denver/gates/`).

**D7 — `/dev/kvm0` must be checked by opening it, never by listing `/dev`.** The kvm
reference says it outright (docs/reference/subsystems/kvm.md, § *VMs as services*): the device
"is opened by the kernel's own `openat` arm like `/dev/power`, so `ls /dev` does not list it;
`open` is `ENOENT` unless the kernel was booted with `kvmctl`." Session 1 correctly diagnosed
the un-armed host from `vm-run`'s own error text, but this session initially re-read a bare
`ls /dev/kvm0` → ENOENT as "still not armed" and was corrected by the file evidence
(`vmrun.log` showing live guest cpuid/DNS relay lines). On an Akuma host, `ls` of a
kernel-arm-style device node proves nothing; the check is the open, or the tool's own error
(`[vm-run] no /dev/kvm0: …`). Precedent cited in session 1's capture; now also lived.

**D8 — no new intervention is taken on the redo.** The reboot intervention is already
recorded (session 1, `RESULT.md`); recording its answer is prose, not a new `INTERVENTION:`
line — the runbook hands autonomy back per phase and the redo needed none (the probe ran
green from the session's own hands; see the session-2 capture).
