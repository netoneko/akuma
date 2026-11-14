# Lesson 3 — building a kernel platform crate (living)

**Motivating archive doc:** `denver/docs/archive/KERNEL_HOWTO_BUILDING_CUSTOM_PLATFORM_KERNEL.md`
lesson 3; the brief it follows is `crates/akuma-tokio-kernel/README.md`; the extraction it
consumes is `proposals/B15_TOKIO_KERNEL_KIT.md`. This page is the living version — *Do* is
rewritten as-happened after each attempt, *Record* appended per gate (the shanghai-01 rule:
quote every place the plan was wrong).

## Goal

One sentence: a new platform kernel is a front crate with a feature map and a three-file kernel
that boots tokio, and Denver builds both (`akuma-jvm-kernel`, `denver`) without writing kernel
code.

## You have

- Branch `denver` with the autoresearch machinery and this lesson; the plan cites
  `crates/akuma-tokio-kernel/README.md` and `shanghai/README.md` — **which were not on the
  branch** (plan written against the shanghai tree; driver checks out `denver`). The whole
  proven stack lives on branch `shanghai` (six gates green on this box).
- A bare-metal amd64 box whose Akuma kernel may or may not have been booted with `kvmctl`;
  when it is, the `/dev/kvm0` node exists but is **hidden from `ls /dev` by design** (kvm.md
  § *VMs as services*): check it by opening it, or by `vm-run`'s own error text — never by
  listing. Standalone `/usr/local/rust` toolchain (no rustup; no aarch64 std — the aarch64
  default build is not runnable on this host).
- The rig under `/root/denver/w/` and `denver/vm-probe.sh` (frees a stale slot, boots, waits
  for an ssh round trip, probes, stops the guest). The probe is green:
  `denver/gates/PHASE-0-SCAFFOLD.log` carries `RESULT PASS` twice — the hand-run after the
  answered reboot, then session 2's first-hand rerun.

## Do (as-happened, 2026-11-13)

1. **Orient on the branch reality before writing code.** `git merge-base denver shanghai`,
   `git diff --name-status denver..shanghai -- crates/`, read `proposals/B15_TOKIO_KERNEL_KIT.md`
   and the tokio-kernel README. Checkable outcome: a research artifact that names every crate the
   gate needs and where it lives (`denver/docs/research/PHASE-0-SCAFFOLD.md`). — Plan said
   "Denver turns the attempt into a curriculum" as if the platform were present; it was on a
   sibling branch. *The plan's Read-in-this-order table is a dependency list: verify it exists on
   YOUR branch first.*
2. **Bring the proven stack over as a branch merge, not a port.** `git merge shanghai --no-ff`;
   keep denver's own planning docs; let submodule pointers move. Checkable outcome: clean merge,
   `cargo build -p akuma-amd64/-p taoxichuan` green (the pairing moved together). — The
   extraction moved code *into* shared crates, so the crates state is only coherent with its
   paired root/metal wiring; a partial copy is fork-shaped (R5).
3. **Write the front crate as a pure pass-through.** `akuma-jvm-kernel/`: features 1:1 onto
   `akuma-tokio-kernel`, `entry!`/`Kernel` re-export, the linker-script relay (`links =
   "akuma_jvm_kernel"`, `cargo:linker_script=`), `#![forbid(unsafe_code)]`. Checkable outcome:
   it compiles for the host and maps every feature the machine half offers.
4. **Write the kernel as three files plus a banner.** `denver/`: `Cargo.toml` (package name
   `denver` — `CARGO_PKG_NAME` is `uname -n`), `build.rs` reading
   `DEP_AKUMA_JVM_KERNEL_LINKER_SCRIPT`, `src/main.rs` calling `.name("Denver").banner(..)
   .console().disk().net().smp().run_init()`. Checkable outcome:
   `cargo build -p denver --target x86_64-unknown-none --release` green cold (~23s), zero
   kernel code of our own.
5. **Members line in the root `Cargo.toml`, and make the driver carry the root manifests.**
   Without the members line the gate commit cannot build its own kernel; without staging
   `Cargo.toml`/`Cargo.lock` the driver would leave exactly that out (D5). Checkable outcome:
   `sh -n denver/autoresearch.sh` and the staging list contains the manifests.
6. **Boot under Akuma KVM.** `herd vm-run /root/denver/w/vm.spec` (spec: PVH kernel, ext2 disk,
   `outbound=1`, a forwarded port that is not shanghai's 2246) — via `sh denver/vm-probe.sh`,
   which frees a stale slot first (a leftover guest answers EBUSY/errno 16 and reads as "KVM
   broken"), waits for an *ssh round trip* (never a log marker), probes `uname` + tokioprobe,
   and always stops the guest. Checkable outcome: `RESULT PASS` + `UNAME PASS Linux denver` in
   the captured gate log. — **Blocked in session 1** (host not booted with `kvmctl`; no QEMU on
   the box; Firecracker needs the same device → escalated, not worked around). **Completed in
   session 2** after the answered reboot (GRUB entry 29, `kvmctl kvmmem=4096`): the human ran
   the probe by hand first; the session re-proved it first-hand. Two plan-wrong facts the redo
   surfaced, both now quoted in the Record: the device node is hidden from `ls` (a bare
   `ls /dev/kvm0` ENOENT is *not* evidence KVM is disarmed — this session misread exactly that
   and the file evidence corrected it), and the first post-reboot boot hits the stale-slot
   EBUSY before it hits KVM.

## Gate

| passes when | probe |
|---|---|
| `denver/gates/PHASE-0-SCAFFOLD.log` contains `RESULT PASS` and `UNAME PASS Linux denver` | tokioprobe over ssh in the denver guest (shanghai gate 5 command shape), boot console captured alongside |

## Record

- 2026-11-13, session 1: steps 1–5 done and committed; step 6 blocked on the host boot flag —
  escalated, not worked around (a reboot is machine policy; the session runs on the box).
  Decisions forced: D1 merge-not-port (precedent: B15 + shanghai RESULT.md), D2 pass-through
  feature map (precedent: the tokio-kernel front crate itself), D3 three-file kernel (precedent:
  the README's `osaka`), D4 rig outside the tree (`/root/denver/w/`, precedent: shanghai
  RESULT.md), D5 driver carries root manifests (precedent: work-log driver fixes). Full record:
  `denver/RESULT.md`, `denver/docs/LOOP_LOG.md`; capture: `denver/gates/PHASE-0-SCAFFOLD.log`.
- 2026-11-13, session 2 (the redo): answered escalation → kvmctl-armed host; step 6 closed with
  the session's own `RESULT PASS` / `UNAME PASS Linux denver …` in the gate log (the human's
  hand-run capture preserved above it). Where the plan was wrong, quoted: (a) session 1 wrote
  "`/dev/kvm0` exists only then" — half-right; it exists *and is hidden from `ls /dev`* when
  armed, so `ls` cannot discriminate armed from unarmed (kvm.md § *VMs as services*); check by
  opening. (b) The gate table's "boot console captured alongside" — the probe script captures
  `vm-run`'s console to a log outside the tree and the ssh output into the gate log; the proof
  the gate greps is the ssh line, and the console is corroborating (`vmrun.log`). Phase 0's
  done-when met; the phase needed one human intervention (the reboot), so the transition rule
  stopped the loop at the phase summary (`denver/ESCALATE.md`). Decisions D6–D8: research
  artifact addendum.
- 2026-11-14 (PHASE-2-PROCFS GREEN): a verbatim walk move (D27) is portable exactly as far as
  the ownership model it assumes. The metal's `pid_map_rows` resolves the non-shared *owner*
  and rejects shared views (the freed-tables hazard); the tokio kernel's `disown_space` makes
  **every** registered `Process` a `new_shared` borrowed view of a root the kernel itself owns
  (`OWNED_ROOT`, parked-not-freed at exec) — so owner resolution answered `None` for every pid
  and `maps` stayed absent (errno=2) while `auxv` — served off plain `Process` fields through
  the same hook table — worked, which is what pinned the divergence to the model, not the
  registration. Fix: a sibling walk, `pid_map_rows_of_kernel_owned_roots` (leader's regions,
  pid's own view, the same l0 re-check; `with_process`'s exclusive slot hold excludes the
  concurrent swap), chosen at the runtime's registration — the model knowledge stays in the
  kernel stack that owns it; no model flag, no new hook table. Residual recorded: per-view
  locks over one root mean a sibling's fault can flip a PTE mid-walk — the staleness class
  the kernel's own reclaim sweep already walks under.
