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
Stage N it mounts **ext2** on that disk and runs a program it opened by path;
Stage O gave ring 3 `open`/`read`/`close`/`lseek`/`fstat`/`mmap` and a serial
shell (`paws`); Stage Q added a **netpoll daemon** so DHCP completes and `httpd`
serves a request over virtio-net; Stage R added `getrandom`, `fcntl` and
**`sys_spawn`**, so **`sshd` serves an authenticated session** — key exchange,
ed25519 pubkey auth, and a shell started over stdin/stdout pipes; and Stage S
runs a **stock static musl `busybox`** — a binary the tree did not compile —
via `arch_prctl` (TLS base), SSE enabled in `boot.s`, `uname` and `writev`
(`busybox uname -a` prints `x86_64`); and Stage T (in progress) added path
`stat`/`lstat`/`newfstatat`, **`execve`**, **`fork`** (eager full-copy, no CoW)
+ `wait4`, per-task `%fs` base, `open`, `access`, terminal `ioctl` and `poll` —
so an **interactive `busybox sh` over SSH runs external commands**
(`uname -a`, `echo`, …), and Stage T's step 5 (2026-09-04) wired `getdents64`,
so **`ls` and `find` both work** — reusing the same `akuma_syscalls_linux::dirent`
wire encoder and the `KernelFile::dir_cache` snapshot-on-first-call the AArch64
kernel's `sys_getdents64` uses, not a hand-rolled record layout. Checking
outbound `wget` (this target's curl — busybox's static build carries no `curl`
applet) surfaced a real UDP bug, also fixed 2026-09-04: `sys_sendto`/
`sys_recvfrom` never took a destination/source address (three-argument
functions where Linux's are six), so a UDP socket with no `connect()`ed peer —
exactly musl's stub DNS resolver's shape, which addresses every nameserver by
hand on each `sendto` — had nowhere to send a query; and `sys_poll` hard-coded
every socket fd as "not ready", so even a query that *did* land got a reply
`poll` could never see. Both are now real (`akuma_net::socket::socket_send_udp`/
`socket_recv_udp`/`socket_udp_recv_ready`, used unmodified — the "wiring, not
implementation" pattern this whole target follows). `mkdisk.sh` also writes
`/etc/resolv.conf` (`nameserver 10.0.2.3`, the fixed address both QEMU usermode
net and Firecracker's `net-setup.sh` dnsmasq answer DNS on), which a guest
resolver needs to have anywhere to ask. Net result: `wget http://info.cern.ch/`
from inside the guest does a real DNS lookup, TCP connect and HTTP GET and
returns the page — verified over SSH on QEMU `microvm`. And since 2026-09-04
**writes exist**: `fd::sys_write_file` buffers into the descriptor's own
`Vec<u8>`, `fs::write_file` (`akuma-ext2`'s existing, unmodified write path)
persists it once, at `close(2)` — enough for a real **tcc** (ported to
`x86_64-unknown-none` the same day: `TCC_TARGET_X86_64`, an x86_64
`setjmp`/`longjmp`, its own `amd64_shim` standing in for `libakuma`) to
compile a C source file and write out a genuine ELF64 executable, which the
loader then **runs** it. A real **musl static libc** followed the same day —
`mkdisk.sh` unpacks the `libc.a`/crt objects/public headers already sitting
inside the Alpine `musl-dev` apk it was already fetching for tcc's own build
headers, the same package `apk add musl-dev` installs on the AArch64 image —
so the AArch64 acceptance tests' own command now runs unmodified:
`tcc -static -B /usr/lib/tcc -o /tmp/hello_c /tmp/hello.c` compiles the
AArch64 suite's own `hello.c` (`#include <stdio.h>` + `printf`), and running
the result prints `Hello, Akuma!` and exits 0 — verified over SSH. No device
interrupts, and no pipelines (`a | b`) over the interactive SSH
shell — `sys_pipe2` (step 4) is still `ENOSYS`, seen directly as `can't create pipe: Bad
file descriptor` from `wget ... | head`.

Verified on QEMU (PVH) **and on real hardware under Firecracker v1.16.1** —
`curl http://10.0.2.15:8080/` and `ssh root@10.0.2.15 'echo hi'` both from the
Firecracker host.

**Status: C** (active risk, expect surprises). This is a spike, not a port.

```bash
amd64/run.sh                     # build + boot under QEMU microvm, with an ext2 root
MEMORY=1024 amd64/run.sh
DISK=my.img amd64/run.sh         # attach an existing image
DISK=none amd64/run.sh           # no drive at all
amd64/mkdisk.sh out.img 32       # build the ext2 root image on its own
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
  memmap: 7 entries ... usable RAM: 2047 MiB
  heap: 0x2f6000 + 64 MiB ... ok
  pmm:  init(... size=1023 MiB)   # capped at PHYSMAP_LIMIT — boot.s maps 1 GiB
  pmm:  245002 free frames (957 MiB)
  ...
  -- userspace output follows --
    [ring3 A] round
    [ring3 B] round
  ring3: same VA, different frames   [OK]
  ...
  -- userspace output follows (from an ELF image) --
    [elf] loaded from a real ELF image
  elf: program ran and reported every check   [OK]
  ...
  [SmolNet] DHCP configured ... IP: 10.0.2.15/24
  net: the netpoll daemon is being scheduled   [OK]

Akuma/amd64 self-test: 161 passed, 0 failed
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

## Before writing anything here, check `crates/`

This target exists to *use* the tree's crates, not to re-derive them, and the
failure mode is quiet: hand-rolled code that works is indistinguishable from
code that had to be written. It has already happened twice — an `mmap` argument
decode that duplicated `akuma-syscalls-mem`, and a console reader that would have
re-invented `akuma-terminal`'s canonical mode (and hit the CR-vs-NL bug on the
first Enter keypress). Both were caught in review rather than by a test.

**Two commands, before adding a module:**

```bash
cargo build -p <crate> --target x86_64-unknown-none --release   # does it build?
cargo tree -p akuma --target aarch64-unknown-none -e normal     # what does the real kernel use?
```

**Building is necessary, not sufficient.** `akuma-mmu` compiles for
`x86_64-unknown-none` — it is bit arithmetic, and nothing in it is an invalid
x86 instruction — but its own header says *"AArch64, 4 KB granule, 4-level page
tables"*, and it manipulates AArch64 descriptors throughout. A crate that builds
can still be wrong; read what it says it is.

The kernel pulls in ~36 crates this target does not. That list is the roadmap,
and each entry has one of three reasons:

| crate | why amd64 does not use it |
|---|---|
| `akuma-mmu`, `akuma-elf` | **Blocked.** `akuma-mmu` is AArch64 page-table format; `akuma-elf`'s mapping half is written against it. The fix is a parse/place split for the loader, and proposal item 1 for the tables |
| `akuma-mmap` | **Blocked on item 1.** `MmapRegion.flags` is a raw AArch64 PTE `u64`; the two encodings share no field. This costs `munmap`'s clip-and-split, lazy regions, and a region list to replace `loader::MAX_PROC_FRAMES` |
| `akuma-user-access` | **Cannot build.** Its copy loop is AArch64 `global_asm!` (`cbz`). `fd.rs` has its own bounded copy helpers, which is duplication with a real reason |
| `akuma-syscalls-glue` (incl. its `pipe`) | **Cannot build** (`cbz` through `akuma-user-access`). `amd64/src/{fd,usermode}.rs` hold the file/spawn/pipe syscall bodies; the pure pipe buffer was extracted to `akuma-pipe` rather than re-derived |
| `akuma-uart`, `akuma-gic`, `akuma-psci`, `akuma-exceptions`, `akuma-el0-entry`, `akuma-entry`, `akuma-timer`, `akuma-fdt`, `akuma-firecracker` | **Different hardware.** PL011 vs a 16550 on I/O ports, GICv3 vs LAPIC, PSCI vs nothing, an FDT vs a PVH block. Genuinely arch- or machine-specific |
| `akuma-exec`, `akuma-exec-core`, `akuma-threading`, `akuma-slot-table`, `akuma-syscalls`, `akuma-kernel-core`, `akuma-kernel-glue`, `akuma-vfs-glue`, `akuma-bkl` | **Not reached yet.** The process/exec/SMP layer. `akuma-exec-core` supplied `FileDescriptor`/`KernelFile`; its `Process` and the fork/exec of `akuma-exec` are what Stage T (§3.26) needs |
| `akuma-rump`, `akuma-fpcache`, `akuma-kacho`, `akuma-isolation`, `akuma-syscalls-{poll,sync,time,ipc,log}`, `akuma-boot`, `akuma-config` | **Not needed yet.** All build for `x86_64-unknown-none`; none has a consumer on this target |

What amd64 *does* use, and what each replaced:

| | crate | note |
|---|---|---|
| heap, frames | `akuma-alloc`, `akuma-pmm` | unmodified, no arch code added |
| disk | `akuma-virtio` | unmodified; the machine facts come from `akuma-primitives::addr` |
| filesystem | `akuma-ext2`, `akuma-vfs` | unmodified; the shim is a struct and two calls |
| networking | `akuma-net`, `akuma-net-nic` | unmodified; `src/net.rs` supplies the `NetRuntime` hooks (Stage P/Q) |
| machine description | `akuma-ryzen-amd64` | written for this target, host-tested against both VMMs |
| descriptor type | `akuma-exec-core` | `FileDescriptor`/`KernelFile`; only the fd *table* is local |
| pipe buffer | `akuma-pipe` | new leaf, `#![forbid(unsafe_code)]`, host-tested (Stage R) |
| `mmap` decode | `akuma-syscalls-mem` | after a correction — it was hand-rolled first |
| console line discipline | `akuma-terminal` | canonical mode, echo, `map_cr_to_nl` |
| syscall identity | `akuma-syscalls-abi`, `akuma-syscalls-linux` | `write` is 1 here and 64 on aarch64 |
| self-tests | `akuma-selftest` | the pass/fail tally |

## Networking

`akuma-net` and `akuma-net-nic` run unmodified; `src/net.rs` supplies the twelve
`NetRuntime` hooks and `src/sock.rs` wires the socket syscalls. The NIC is found
by the same command-line discovery the disk uses — another slot in the same
array.

```bash
amd64/run.sh                       # NIC on virtio-mmio-bus.1, port 8080 forwarded
INIT=/bin/httpd amd64/run.sh       # run the server instead of the shell
curl http://localhost:8080/
```

`init=` on the kernel command line picks what gets the console after the
self-tests. `paws` wants a terminal, `httpd` wants none, so the choice cannot be
baked in.

Since Stage Q (`docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.23) a **netpoll
daemon** — a `sched::spawn_daemon` task running the `netpoll_drain_step` shape —
drives `smoltcp_net::poll()` between socket calls, so DHCP completes
(`IP: 10.0.2.15/24`) and `httpd` serves a request end to end. Verified on QEMU
`microvm` and on Firecracker (Ryzen, `FC_NET=1`):

```bash
INIT=/bin/httpd amd64/run.sh                  # QEMU
curl http://localhost:8080/

FC_HOST=... amd64/net-setup.sh                 # once: tap + dnsmasq + NAT
FC_HOST=... FC_NET=1 INIT=/bin/httpd amd64/run-firecracker.sh
curl http://10.0.2.15:8080/                    # from the FC host
```

The daemon replaced a `settle()` loop that spun on a clock (`lapic::ticks()`)
that had not started when `net::init` ran — it hung the boot before the
self-tests. Fixing that also surfaced a latent DMA bug: `akuma-net-nic`'s frame
arenas are `.bss` statics, whose address is in the kernel-image window, and
`virt_to_phys` only knew the physmap window — so every TX descriptor pointed at
~550 GiB and the device rejected it as bogus. `virt_to_phys` now translates both
windows.

`RDRAND` is CPUID-checked, with a non-cryptographic fallback that warns loudly:
QEMU's default `microvm` CPU does not expose it, and the first boot with
networking took a `#UD` because of that. `run.sh` passes `-cpu max`.

## Guest programs

Two different ways a program gets onto this target, for two different reasons:

- **`userspace/amd64/`** — one directory each, compiled straight by `rustc`
  from `amd64/build.rs` and embedded with `include_bytes!`. This predates
  Stage N (ext2), and stayed the shape for the ELF loader's own self-tests
  (`hello`, `fdprobe`): a loader test should not depend on the filesystem it
  is not testing. See `userspace/amd64/README.md` for how to add one.
- **Real disk-resident programs** — `paws`, `httpd`, `sshd`, and (since
  2026-09-04) **`tcc`** — are ordinary `cargo build -p <name> --target
  x86_64-unknown-none --release` crates under `userspace/`, staged onto the
  ext2 image by `amd64/mkdisk.sh` and opened by path like any other file.
  `tcc` is not a workspace member (`userspace/tcc/Cargo.toml` is its own
  `[workspace]` root — see `userspace/Cargo.toml`'s comment on the
  submodule-backed crates), so `mkdisk.sh` reaches it with `--manifest-path`
  rather than `-p tcc`; everything else about how it lands on the image is
  the same as `paws`/`httpd`/`sshd`.

A guest program reports what it checked through its **exit status**, which the
kernel's self-test compares against a value computed in `src/usermode.rs`. A
program that printed its verdict would have "passed" by running at all.

## What is deliberately missing

- **`fork` is an eager full copy, no CoW.** `sys_fork` (Stage T; `fork`/`vfork`/
  `clone(SIGCHLD)`) copies every mapped user page into fresh frames and runs the
  child as its own task — enough for an interactive `busybox sh` to run external
  commands and command sequences (`a; b`). What is missing: **CoW** (so `fork`
  costs one frame per page and can hit `ENOMEM` near `MAX_PROC_FRAMES`), and
  `pipe2`/`dup2` for **pipelines** (`a | b`). `sshd`'s `fork-sessions` mode is
  still out (it needs `fork` before auth, a different shape). Rest of **Stage
  T**: `pipe2` (`docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.26 — `getdents64`,
  step 5, shipped 2026-09-04).
- **No pty line discipline for a spawned shell.** `SPAWN_FLAG_PTY` is accepted
  and ignored; an interactive `sshd` shell gets raw bytes over its stdin pipe
  and does its own editing.
- **Scheduler task slots do not recycle.** `waitpid` frees a reaped child's
  process slot, frames and pipes, but not its `sched` task slot — a `Finished`
  slot stays `Finished` and its two 32 KiB stacks are never reclaimed. Each
  `execve` reuses the spawning task's slot, but each **`fork`** takes a fresh
  one. `MAX_TASKS` was raised to 96 (~6 MiB of lazily-leaked stacks against the
  64 MiB heap) so an interactive shell serves dozens of external commands before
  the ceiling bites with *"can't fork"* / *"failed to spawn"*. Actually
  recycling the slots is the real fix and is a stage of its own.
- **Writes exist (2026-09-04) but are whole-file, and there is still no mount
  table.** `open(O_CREAT|O_WRONLY)` buffers into the fd's own buffer;
  `close(2)` is the one point that buffer reaches `akuma-ext2`'s `write_file`,
  which replaces a file's entire contents in one call — there is no
  incremental/partial write to the disk, and no `unlink`/`rename`/`mkdir`
  syscall on the kernel side yet (`fd.rs`'s module header has the design, and
  `userspace/tcc/src/amd64_shim.rs`'s header has why tcc does not need those
  three anyway). `sshd` still does not use this — it still regenerates its
  host key every boot, which it tolerates — nothing has wired persistence
  *into* an existing program yet; tcc's own `-o` output is the first thing
  that writes.
- **No device interrupts.** The block driver polls the used ring; the NIC would
  too. On the AArch64 side the NIC IRQ exists only to end the netpoll loop's
  `wfi` early, so it is an optimisation on top of that loop, not a prerequisite.
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
