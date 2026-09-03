# Akuma/amd64

x86_64 bring-up target. Boots to long mode, brings up the kernel heap and the
physical frame allocator, maps/unmaps 4 KiB pages, services page faults with
**demand paging**, takes **LAPIC timer interrupts**, and round-robin schedules
tasks across separate stacks, and enters **ring 3** with a working
`syscall`/`sysret` path. No loader, no preemption, no device interrupts.

Verified on QEMU (PVH) **and on real hardware under Firecracker v1.16.1**.

**Status: C** (active risk, expect surprises). This is a spike, not a port.

```bash
amd64/run.sh                     # build + boot under QEMU
MEMORY=1024 amd64/run.sh
```

```
Akuma/amd64 — long mode reached
  hvm_start_info @ 0x0000000000001580
  version=1 modules=0 rsdp=0x0000000000000000 cmdline=0x0000000000000560
  memmap: 7 entries
    0x0000000000000000 + 0x000000000009fc00  RAM
    ...
  usable RAM: 511 MiB
  heap: 0x000000000024e000 + 16 MiB ... ok
  pmm:  126354 free frames (493 MiB)
  test: heap vec[4096] sum=22898104320
  test: pmm alloc 8 frames, free 126354 -> 126346 -> 126354   [OK]
  test: paging map/write/verify/unmap @0x0000000040000000   [OK]
  test: W^X encoding   [OK]
  test: demand paging 4 faults serviced, frames 126380 -> 126378   [OK]
  lapic: base=0x00000000fee00000 id=0 timer vector=32 periodic
  test: timer interrupts 5 ticks in 62080 spins   [OK]
  test: scheduler 3 tasks x 4 rounds, 5 switches, ticks=17   [OK]
  test: tick-driven resched observed   [OK]
  test: ring 3 entered, 2 syscalls, arg=0x1234 status=0x2468   [OK]

Akuma/amd64 — memory subsystem up
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

## What is deliberately missing

- **No upper-half mapping.** The aarch64 `linker.ld` splits kernel VA from
  physical at `0xFFFF000040000000`; this target runs on the identity map. That is
  the next structural thing to mirror, and it is absent rather than half-done.
- **No `hvm_start_info` parsing.** The pointer is printed, not read. Its memory
  map is where the amd64 equivalent of `PlatformInfo`
  (`proposals/REDUCING_PLATFORM_DEPENDENCY.md` §2) comes from — x86_64 Firecracker
  passes no DTB, so `akuma-fdt` has nothing to say here.
- **No use of the kernel crates.** 34 of 52 build for `x86_64-unknown-none`
  (§0 of that proposal), but they take the *host stub* out of `akuma-cpu`: a no-op
  `dsb_ish`, a `wfi` that does not park. Calling them from here would be wrong in
  ways QEMU will not show you. Read §0.3 before wiring the first one up.

---

**Background:** `docs/archive/AKUMA_FIRECRACKER_AMD64.md` records how this target
came up, the `akuma-cpu` arch-gate bug it uncovered (13 → 34 crates building for
`x86_64-unknown-none`), and the two verification methods that turned out not to
work. `proposals/REDUCING_PLATFORM_DEPENDENCY.md` §0 carries the corrected claim.
