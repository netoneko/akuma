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
- A bare-metal amd64 box whose Akuma kernel may or may not have been booted with `kvmctl`
  (`/dev/kvm0` exists only then); standalone `/usr/local/rust` toolchain (no rustup; no aarch64
  std — the aarch64 default build is not runnable on this host).
- The probe that currently fails: nothing boots; after this session, the build is green and the
  boot probe waits only for a kvmctl-armed reboot (`denver/gates/PHASE-0-SCAFFOLD.log`).

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
   `outbound=1`, a forwarded port that is not shanghai's 2246). Checkable outcome: guest console
   in `vm.log`, then `uname -a` + tokioprobe over ssh with the captured `RESULT PASS`. —
   **Blocked 2026-11-13: the host was not booted with `kvmctl`; no QEMU on the box; Firecracker
   needs the same device. Escalated (denver/ESCALATE.md). The step itself is unchanged for the
   redo.**

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
