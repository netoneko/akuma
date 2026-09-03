# Akuma/amd64

x86_64 bring-up target. Boots to long mode with a working console and stops there
— no userspace, no interrupts, no scheduler, no MMU management beyond the identity
map `boot.s` builds.

**Status: C** (active risk, expect surprises). This is a spike, not a port.

```bash
amd64/run.sh                     # build + boot under QEMU
MEMORY=1024 amd64/run.sh
```

```
Akuma/amd64 — long mode reached
  hvm_start_info @ 0x0000000000001580
```

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

Not reproducible on an Apple Silicon host: Firecracker needs KVM, and the guest
ISA here is not the host's. It needs an x86_64 Linux box with `/dev/kvm`
(`c5.metal`, `m5.metal`, or any bare-metal/nested-virt x86 host).

The kernel is a plain ELF64 — `linux-loader` requires ELFCLASS64, so unlike the
aarch64 target there is **no objcopy step and no flat binary**. Point
`boot_source.kernel_image_path` straight at
`target/x86_64-unknown-none/release/akuma-amd64`.

```json
{
  "boot-source": {
    "kernel_image_path": "akuma-amd64",
    "boot_args": ""
  },
  "machine-config": { "vcpu_count": 1, "mem_size_mib": 512 }
}
```

No `drives` and no `network-interfaces`: nothing in this kernel reads a disk or a
NIC yet. Console output arrives on Firecracker's serial, which it writes to its
own stdout.

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
