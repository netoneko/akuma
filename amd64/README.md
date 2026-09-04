# Akuma/amd64

x86_64 bring-up target. Boots to long mode, brings up the kernel heap and the
physical frame allocator, maps/unmaps 4 KiB pages, services page faults with
**demand paging**, takes **LAPIC timer interrupts**, and round-robin schedules
tasks across separate stacks, and enters **ring 3** with a working
`syscall`/`sysret` path, where userspace calls **real Linux x86_64 syscalls**
(`write`, `getpid`, `exit_group`, `sched_yield`) from **isolated per-process
address spaces**, and **preemptively multitasks** between them. The kernel runs
in the **upper half**, so user programs are mapped at `0x40_0000` where a static
Linux binary is linked — and since Stage L an **ELF loader** puts one there: a
program `rustc` compiled and linked, whose `PT_LOAD` segments the kernel parses
and places, with a System V initial stack (argc/argv/envp/auxv). Since Stage M
it also drives a **virtio-blk disk** and reads its own machine description —
the PVH memory map, the virtio-MMIO command line, and ACPI's MADT — and since
Stage N it mounts **ext2** on that disk and runs a program it opened by path.
Reads only, no file syscalls for ring 3 yet, and no device interrupts.

Verified on QEMU (PVH) **and on real hardware under Firecracker v1.16.1**.

**Status: C** (active risk, expect surprises). This is a spike, not a port.

```bash
amd64/run.sh                     # build + boot under QEMU microvm, with an ext2 root
MEMORY=1024 amd64/run.sh
DISK=my.img amd64/run.sh         # attach an existing image
DISK=none amd64/run.sh           # no drive at all
amd64/mkdisk.sh out.img 8        # build the ext2 root image on its own
```

The root image is **rebuilt on every run**, on purpose: it carries the guest ELF
that was just compiled, and a stale image would silently run the previous
build's program while every check still passed. `mkdisk.sh` uses `mkfs.ext2` and
`debugfs` — no Docker, no mount, no root, so it works unprivileged on macOS.

`-M microvm`, not QEMU's default `pc`, and that is load-bearing: `pc` and `q35`
put virtio on **PCI**, while Firecracker's default transport is MMIO. A local run
against them would exercise a different transport than the one this kernel
drives, and the reason QEMU is a useful stand-in — the same entry path, the same
device model — would be gone.

This is a **choice, not a constraint**. Firecracker v1.16.1 takes `--enable-pci`
and builds a real PCIe segment (ECAM at `0xeec00000`, measured — see
`docs/reference/firecracker-amd64/README.md`). MMIO is what this kernel already
drives on both architectures, with a driver that needed no changes; PCI would
mean config-space enumeration and BAR programming for no capability this target
needs yet.

```
Akuma/amd64 — long mode reached
  hvm_start_info @ 0x0000000000001580
  memmap: 7 entries ... usable RAM: 511 MiB
  heap: 0x24f000 + 16 MiB ... ok
  pmm:  126353 free frames (493 MiB)
  ...
  -- userspace output follows --
    [ring3 A] round
    [ring3 B] round
  ring3: same VA, different frames   [OK]
  ...
  -- userspace output follows (from an ELF image) --
    [elf] loaded from a real ELF image
  elf: program ran and reported every check   [OK]

Akuma/amd64 self-test: 59 passed, 0 failed
Akuma/amd64 — all self-tests passed
```

The heap and frame allocator are **unmodified `akuma-alloc` and `akuma-pmm`** —
the same crates the aarch64 kernel uses, with no arch code added. Note the
ordering in `mem.rs`: the heap must come up *before* the PMM, because the PMM
allocates its own bitmap with `alloc::vec!`.

## Boot protocol: PVH

The kernel declares the PVH ELF note (`.note.Xen`, name `"Xen\0"`, type 18 =
`XEN_ELFNOTE_PHYS32_ENTRY`), whose descriptor holds the 32-bit entry address.

That single note decides everything. Firecracker's `configure_system_for_boot`
matches on `entry_point.protocol`, and an ELF declaring the note gets
`BootProtocol::PvhBoot` instead of `BootProtocol::LinuxBoot` — there is no
Firecracker-side switch to set. QEMU implements the same protocol, which is the
reason PVH was chosen over the 64-bit Linux boot protocol: **a local QEMU run and
a Firecracker run take the identical entry path.** The 64-bit path would have
handed us a vCPU already in long mode with paging on (less code in `boot.s`) at
the cost of having no way to reproduce that state locally.

Entry state, per the x86 HVM direct boot ABI: 32-bit protected mode, paging off,
interrupts off, flat segments, `%ebx` = physical address of `hvm_start_info`, and
**no stack**. `boot.s` sets `%esp`, builds a 3-level identity map of the first
1 GiB with 2 MiB pages, enables PAE + `EFER.LME` + paging, loads a 64-bit GDT and
far-jumps to `long_mode_start`, which calls `kmain(hvm_start_info)`.

The address printed at boot is the cheapest possible check that the note was
honoured: QEMU reports `0x1580`, and a multiboot fallback reported `0x9500`. If
that line is missing entirely, the note is the first thing to suspect —
`run.sh` greps for it before launching for exactly that reason.

## Console

16550 UART on I/O port `0x3F8`, polled, no interrupts. This is what Firecracker
exposes, and it is `serial.rs`'s only target. There is no VGA path.

## Running under Firecracker

**Verified 2026-09-03** on Firecracker v1.16.1, AMD Ryzen 7 8845HS (Zen 4),
Pop!_OS 22.04, native KVM.

```bash
FC_HOST=user@host amd64/run-firecracker.sh
# MEMORY=1024 VCPUS=1 FC_KEY=~/.ssh/id_ed25519 TIMEOUT=20 also honoured
```

Not reproducible on an Apple Silicon host: Firecracker needs KVM, and the guest
ISA is not the host's. It needs an x86_64 Linux box with `/dev/kvm`. The setup on
that box is one static binary and no `sudo` — no Lima, no KVM configuration:

```bash
VER=$(curl -sI https://github.com/firecracker-microvm/firecracker/releases/latest \
      | grep -i '^location:' | sed 's|.*/tag/||' | tr -d '\r\n')
curl -L -o /tmp/fc.tgz \
  "https://github.com/firecracker-microvm/firecracker/releases/download/${VER}/firecracker-${VER}-x86_64.tgz"
tar -xzf /tmp/fc.tgz -C /tmp && mkdir -p ~/bin
install -m755 "/tmp/release-${VER}-x86_64/firecracker-${VER}-x86_64" ~/bin/firecracker
```

The kernel is a plain ELF64 — `linux-loader` requires ELFCLASS64, so unlike the
aarch64 target there is **no objcopy step and no flat binary**. Point
`boot_source.kernel_image_path` straight at the linked artifact.

```json
{
  "boot-source": { "kernel_image_path": "akuma-amd64", "boot_args": "" },
  "drives": [],
  "network-interfaces": [],
  "machine-config": { "vcpu_count": 1, "mem_size_mib": 512 }
}
```

**`drives` and `network-interfaces` are mandatory here even though they are
empty.** The single-JSON path does not default them — omit either and Firecracker
exits with `missing field 'drives'`. The API path has different rules. Nothing in
this kernel reads a disk or a NIC yet, so both stay `[]`.

Console output arrives on Firecracker's serial, which it writes to its own
stdout; the staged `run.sh` tees it to `~/akuma/boot.log`. The kernel halts rather
than exiting, so Firecracker never returns on its own — run it under `timeout`.

**Use `timeout --foreground`, not plain `timeout`.** Plain `timeout` puts the
child in its own process group, so it is no longer the terminal's foreground
group; Firecracker attaches guest serial input to stdin, and reading the TTY from
a background process group raises `SIGTTIN` and stops the process right after its
banner. The guest never runs and there is no error. It only reproduces on a real
terminal — over a pipe (`ssh` with no `-t`) there is no controlling TTY and plain
`timeout` works fine.

## The machine description

There is **no device tree**. `crates/akuma-ryzen-amd64` parses the three things
this machine does provide, and is host-tested against bytes measured from both
VMMs:

| | where | what it carries |
|---|---|---|
| PVH `hvm_start_info` | address in `%ebx` at entry | E820 memory map, command-line pointer |
| the kernel command line | a string that block points at | every virtio-MMIO transport: base, size, IRQ |
| ACPI | found by **scanning** the BIOS window | local APIC, I/O APICs, CPU list (the MADT) |

`hvm_start_info.rsdp_paddr` exists in the ABI and is **0 on both machines**, so
the ACPI root pointer is found the BIOS-era way. And no table address may be a
constant — every one of them moves with the vCPU count. The measured evidence,
at 1/2/4/8 vCPUs, is `docs/reference/firecracker-amd64/`; regenerate it with:

```bash
FC_HOST=user@host amd64/dump-machine.sh
```

That boots **Linux** under Firecracker and reads its boot log, which lists every
ACPI table long before a root filesystem is needed — so no rootfs is involved.

## Storage

`akuma-ext2` is mounted on the virtio-blk device, **unmodified**. Its entire
interface to a disk is a two-method `BlockDevice` trait, which is exactly what
`akuma-virtio` exposes, so `src/fs.rs`'s adaptation is a struct and two
forwarding calls.

There is no mount table — one filesystem, reached through `fs::with_root`.
`akuma-vfs`'s `Filesystem` trait is used; its `MountTable` arrives when there is
a second thing to mount. Reads only: the driver and the filesystem can both
write, but a self-test that mutated the image would make it stateful across
boots.

## Guest programs

The programs the loader runs live in `userspace/amd64/`, one directory each,
compiled straight by `rustc` from `amd64/build.rs` and embedded with
`include_bytes!` — there is no disk driver, so there is nowhere to put a file the
kernel could open by path. See `userspace/amd64/README.md` for how to add one.

A guest program reports what it checked through its **exit status**, which the
kernel's self-test compares against a value computed in `src/usermode.rs`. A
program that printed its verdict would have "passed" by running at all.

## What is deliberately missing

- **No file syscalls.** The kernel reads files; ring 3 cannot. There is no
  descriptor table and no `open`/`read`/`close`. That, plus a read path on the
  16550 and a minimal `execve`, is what a serial-console shell needs — and it
  needs neither `fork` nor sockets, which is why it is a much shorter road than
  sshd.
- **No writes**, and no mount table.
- **No NIC, and no device interrupts.** The block driver polls the used ring.
  The IOAPIC's address is read from the MADT and nothing uses it; routing a GSI
  to a vector is what would let the driver stop polling. The *host* side of
  networking is ready — `amd64/net-setup.sh` builds a tap, DHCP and NAT through
  Docker with no sudo, proved with a Linux guest taking a lease — and all four
  `akuma-net*` crates build for `x86_64-unknown-none`.
- **No VGA.** Considered and dropped: dead code on Firecracker, whose console is
  the 16550 at I/O port `0x3F8`.
- **No demand paging for user segments.** The `#PF` handler services a
  not-present fault inside one armed region; an ELF's segments are allocated and
  copied eagerly. Wiring the two together needs a per-space region table.
- **No FP/SIMD save on the syscall path.** No `xsave`, no lazy FPU. Nothing in
  the current handler touches a vector register, which is a property of today's
  code rather than a guarantee — see
  `docs/archive/AMD64_SYSCALL_ABI_REGISTER_CLOBBER.md` §8.
- **No SMP**, no `CR4.SMAP`/`SMEP`, no TSS/IST, no ACPI.
- **Little use of the kernel crates.** 36 of 54 build for `x86_64-unknown-none`
  (§0 of `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md`), but they take the *host
  stub* out of `akuma-cpu`: a no-op `dsb_ish`, a `wfi` that does not park.
  Calling them from here would be wrong in ways QEMU will not show you. Read
  §0.3 before wiring the first one up. `akuma-elf` in particular is unusable —
  its mapping half is written against the AArch64 `akuma-mmu`, which is why
  `src/loader.rs` exists.

---

**Background:** `docs/archive/AKUMA_FIRECRACKER_AMD64.md` records how this target
came up stage by stage, the `akuma-cpu` arch-gate bug it uncovered (13 → 36 crates
building for `x86_64-unknown-none`), and the two verification methods that turned
out not to work. The one kernel defect the port has found —
`syscall_entry` clobbering six registers the Linux ABI preserves — is
`docs/archive/AMD64_SYSCALL_ABI_REGISTER_CLOBBER.md`.
`docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` §0 carries the corrected claim.
