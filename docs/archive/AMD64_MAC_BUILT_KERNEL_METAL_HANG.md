# A Mac-built amd64 kernel hangs on the ryzen metal; the same source built on ryzen boots

**Date:** 2026-10-18. **Status: UNEXPLAINED — worked around by building on the box.** First
time this has been seen; it was found while verifying the `akuma-dma` / `akuma-pagetable` /
`BorrowCell` unsafe-reduction commits on metal, and it **is not caused by them** (the parent
commit, which contains none of that work, fails the same way when built the same way).

## Symptom

On ryzen (Lenovo IdeaPad, Ryzen 7 8845HS, GRUB → multiboot2), a `no-tests` kernel built **on the
Mac** loads, and then:

- the screen shows only GRUB's blinking `_` — **no kernel output at all**, with `fbverbose`,
  with `strace`, and with `nofb` (the framebuffer console does not draw until the late
  `[fb] … wc on clear` line, which is after pmm, NVMe, net, LAPIC and the scheduler start, so the
  blank screen only says "before that line");
- the machine does **not** return to Pop by itself (`autoreboot` never reaches userspace, no
  `boot-N.early`, the `count` file does not advance) and the hardware watchdog does not fire
  (it is armed at `wdt:` and petted from the BSP timer tick, so a hang with ticks alive is not
  reset);
- a power-cycle is needed each time.

## What was tried, in order (all on ryzen, one-shot GRUB entries)

| kernel | built | entry | result |
|---|---|---|---|
| `4b9edb35` — p3's installed `/boot/akuma-amd64`, uname `55bbb8c0` | **on the box** (`kbuild`) | 0 | **boots**, returns to Pop (`boot-152`) |
| HEAD `590d1ef5`, md5 `015a9084` (stripped) | Mac | 0, 23 (+`strace`), 25 (`nofb`) | **hangs**, blank |
| `ec8eab29`, md5 `33f5bd7e` | Mac (worktree) | 24 | **hangs** |
| `d2f7ce52`, md5 `ebf1f594` — **parent of all the unsafe work** | Mac (worktree) | 24 | **hangs** |
| HEAD `590d1ef5`, md5 `9046fa04` (unstripped) | **ryzen/Pop**, `cargo build … --features no-tests` | 24 | **boots** to `herd`/`sshd`/`autoreboot` (`boot-153.early`) |
| the same, `smp=8` + wifi + sshd | ryzen/Pop | 26 | **boots**, 8 cores, reachable over wifi |

(`79aaf04d`, md5 `c5375319`, was built and shipped but never booted: the parent had already
failed.)

The same Mac-built binaries are **fine** everywhere else:

- ryzen's KVM rehearsal (`overlays/ryzen/qemu.sh`, `-cpu host`, OVMF → the real GRUB → multiboot2,
  6 GiB and 14 GiB) runs them to `herd` / `sshd`;
- Firecracker on a dm-snapshot of p3 (`fc-p3.sh`): `herd` + all services, and a full
  `kbuild -c -j 4` **EXIT=0, 1m01s** with no panic, fault or canary line;
- QEMU microvm SMP=4 (Mac): boot suite 824 passed, 0 failed.

## What the ryzen-built HEAD did on the metal (entry 26, `smp=8`, wifi, sshd)

This is the verification the unsafe-reduction work was owed (xHCI/HDA `akuma-dma` + `MmioWindow`,
`akuma-pagetable` / `akuma-pte-amd64` under `paging.rs` and `akuma-mmu`, the `sched.rs` machine
table on atomics + `BorrowCell`). Kernel `9046fa04` (uname `590d1ef`), 8 cores, root on p3, reached
over wifi from the Mac:

| check | result |
|---|---|
| boot to `herd`, `klogd`, `powersaved`, `sshd`, `wifi auto`, box init | up in ~100 s, `nproc` = 8 |
| 8 parallel loops × 150 `busybox true` (1,200 spawns, fork+exec+wait4 across cores) | `SPAWN-DONE`, no panic / canary / fault |
| incremental `kbuild -j 4 --features no-tests` | 27.8 s, EXIT=0 |
| **clean** `kbuild -c -j 8 --features no-tests` (52 crates compiled on Akuma, on the metal) | **56.6 s, EXIT=0** |
| `dmesg` after all of it | no `panic`, `CANARY`, `[Fault]`, `SIGSEGV`, `[TLB] stuck`, `SWITCH NO-BKL`, `FREED-CR3`; 8 `[BKL] stuck … tag=11/501` (the munmap/mmap_regions class seen on the baseline kernel under Firecracker too) and the usual `yield_now with IRQs masked` warnings |

Entry 24 (`nosmp`, `autoreboot`) also reached userspace (`boot-153.early`: NVMe, ext2, LAPIC,
touchpad, framebuffer, `herd`, `sshd`, `autoreboot`); the run was cut by a manual reboot before
its 140 s `.dmesg` save, so there is no `boot-153.dmesg`. Not exercised: audio (this machine's
HDA function is AMD, not Intel — "leaving audio off" — so the `hda.rs` conversion has only run
under QEMU's `intel-hda`), and xHCI bring-up (`usb` was not on the command line; only the
quiesce path ran, and it is untouched).

## What was ruled out (with the check that rules it out)

- **The unsafe-reduction commits.** `d2f7ce52` has none of them and fails when Mac-built; HEAD
  with all of them boots when ryzen-built.
- **Transfer corruption.** Every kernel was md5-checked after the copy to ryzen and again after
  the copy onto p3. (There *was* a real transfer bug — see "Side findings" — but that file was
  caught before it was installed.)
- **`rust-strip --strip-debug`.** The stripped and unstripped Mac HEAD have byte-identical
  contents in every `PT_LOAD` segment and the entry point; only `e_shoff` in the ELF header
  differs.
- **ELF layout / the multiboot2 header / `boot.s`.** Program headers of the Mac and ryzen HEAD
  builds are near-identical (same offsets and addresses; `.text` 208 bytes apart, `.bss` 4112
  bytes apart). `.multiboot2` and `.boot` (the first code to run) differ in **three bytes**: the
  `movabs` immediate that jumps to `kmain`.
- **Which GRUB entry / how the kernel is found.** The failing and the working kernels were both
  tried through `${akk}` (entry 0) and through `(${akp3})/boot/akuma-amd64.verify` (entries 23–26).

## What does differ

Same source, different compiler. `rust-toolchain.toml` is the **floating** `channel = "nightly"`,
so two machines resolve it differently:

| host | rustc |
|---|---|
| Mac (where the failing kernels were built) | `1.100.0-nightly (e457a7b0d 2026-08-27)` |
| ryzen/Pop (`nightly-x86_64-unknown-linux-gnu`) | `1.100.0-nightly (420ed2a0c 2026-09-18)` |
| `kbuild` on Akuma (`nightly-x86_64-unknown-linux-musl`) | not recorded |

Comparing the Mac and ryzen builds of **HEAD** by symbol: 4,672 vs 4,631 symbols, **238 shared
symbols differ in size**, e.g. `akuma_syscalls_glue::sync::sys_futex` 4,937 vs 7,248 bytes,
`akuma_amd64::futex::purge_task` 2,066 vs 129, `akuma_threading::spawn_user_thread_initializing`
2,214 vs 366 — LLVM inlining decisions, not source differences. So the two binaries are the same
program laid out and inlined differently.

Also different: the **build host** (aarch64 macOS vs x86_64 Linux) for every `build.rs` and
proc-macro — `amd64/build.rs`, `akuma-fbcon`'s font tables, `tcc`. Not compared yet.

## What this means, and what it does not

The tree has a **layout- or codegen-sensitive failure in early boot on real hardware** that the
ryzen toolchain's output happens not to hit and the Mac toolchain's does. It is a real bug in
*something* — an instruction sequence the CPU handles differently, a stack or alignment
assumption, an uninitialised read, a stale-TLB window that only some inlining exposes — and
"the compiler did it" is not an explanation, only a way to reproduce it. It does not reproduce
under KVM, TCG or Firecracker, which is what makes it expensive: each probe is a power-cycle.

It does **not** show that the Mac toolchain is wrong, and it does not show the working build is
safe — only that it did not hit it.

## How to find it (not done)

1. **Pin the toolchain.** `rust-toolchain.toml` with a dated nightly. Until then two builds of one
   commit are two different programs. Do this regardless of the hunt.
2. Build the Mac HEAD with ryzen's exact compiler (`cargo +nightly-2026-09-18 …`) and the
   ryzen HEAD with the Mac's. If the failure follows the compiler, it is codegen; if it follows
   the host, it is a `build.rs` output (fonts are the first thing to diff: `.rodata` is the same
   size to the page, but its *contents* were not compared).
3. Get a breadcrumb out before the framebuffer console exists: write a few pixels straight into
   the multiboot2 framebuffer tag's address from `kmain`'s first lines and from each early init
   step, and bisect by which pixel row appears. The console cannot say where it died because it
   is not up yet; `ecram`/`fbtrace` are no help either (no serial on this machine).
4. Bisect *inlining*: `-C llvm-args=-inline-threshold=…` on the failing toolchain moves the
   boundary without changing the source.

## Side findings (all fixed or recorded)

- **`overlays/ryzen/cycle.py` could install a truncated kernel.** `ship()` ran
  `cat > x.new && mv x.new x`; when the local ssh timeout killed the client, the remote `cat`
  saw EOF and exited 0, so `mv` renamed the *partial* file (8.8 of 10.5 MB here). It now uploads
  to `.new`, checks the md5, and renames only on a match; it also skips the transfer when the
  remote already has the right md5 (the wifi link moves a kernel in ~100 s).
- **`qemu.sh` cannot rehearse entry 0 any more.** It only sets `next_entry` when the entry is not
  0, but `grub.cfg` now has `set default=14`, so "entry 0" boots entry 14 and then fails its own
  "guest reset" check. (It still proves the kernel reaches userspace, which is how this was
  noticed.)
- **The kernel GRUB boots is p3's `/boot/akuma-amd64`, not the ESP copy.** `install.sh` writes the
  ESP copy, which is only entry 22's fallback; a test kernel has to go on p3 (mount it rw from Pop
  with Firecracker stopped) or into a dedicated entry. `grub.cfg` now has entries 23–26 booting
  `/boot/akuma-amd64.verify` for exactly this, appended so no index moved.
- **Do not mount `/dev/mapper/p3snap` on the host while a Firecracker guest is using it.**
- **`~/akuma-metal/akuma`'s `origin` URL on ryzen embeds a GitHub personal access token in
  plaintext** (it shows in `git remote -v`). Rotate it and use a credential helper.
- The Firecracker boot prints `[BKL] stuck: owner=1 waiter=N tag=501` several times in the first
  minute on the **baseline** kernel too (11 vs 8 on HEAD), so it is not new.

## Background

- The commits being verified: `79aaf04d` (`akuma-dma`, `MmioWindow`), `93c1bdc2` / `ec8eab29`
  (`akuma-pagetable`, `akuma-pte-amd64`), `590d1ef5` (`sched.rs` machine table: atomics +
  `BorrowCell`).
- Metal loop and entries: `overlays/ryzen/README.md`; Firecracker-on-p3 rig:
  `/home/netoneko/akuma-selfhost/fc-p3.sh` on ryzen.
- Related: `AMD64_TLB_STUCK_MMAP_REGIONS.md` (the other "whole box dead, only the host can see it"
  class), `AKUMA_AMD64_ON_RYZEN_LAPTOP.md`.
