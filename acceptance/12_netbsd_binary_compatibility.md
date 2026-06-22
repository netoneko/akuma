# Acceptance: NetBSD binary compatibility (pkgsrc) via a per-process syscall table

**Status: FUTURE / design target — NOT runnable yet, NOT yet implemented.** This is
the end-state design for running **actual NetBSD/aarch64 binaries** (ultimately the
whole **pkgsrc** package set) on Akuma, unmodified, with their NetBSD syscalls
served by a rump kernel. It supersedes the userspace approaches in
`userspace/rumpkernel/docs/ARCHITECTURE_QUESTIONS.md` (LD_PRELOAD hijack /
rumprun) as the long-term answer; those remain the path to M1.

## The idea

A program makes syscalls in *some* ABI. Akuma today serves one ABI (its own,
musl-shaped). A NetBSD binary speaks the **NetBSD** ABI (its `sockaddr` has
`sin_len`, its `SOCK_*`/`O_*`/errno values and **syscall numbers** are NetBSD's).
Instead of *translating* a foreign binary's calls in userspace (the hijack shim,
with its `SOCK_NONBLOCK`/`sin_len` fix-ups), give the **kernel** a **swappable,
per-process syscall table** and let the **ELF loader pick it from the binary's ABI
tag**. The NetBSD binary then traps `SVC` exactly as it would on NetBSD; Akuma
dispatches through *that process's* NetBSD table; the handlers implement NetBSD
semantics and route to rump. **No translation, no LD_PRELOAD, no relinking.**

This is precisely how production kernels do binary compat:
- NetBSD: `struct emul` + `e_sysent` per process, selected by the ELF `EI_OSABI` /
  the `.note.netbsd.ident` note.
- FreeBSD: `struct sysentvec` (and the Linuxulator).
- Linux: `personality(2)` + per-binfmt handlers.

## How it maps onto Akuma (the three touch-points)

1. **ELF loader** (`crates/akuma-exec/src/elf/`) — read `e_ident[EI_OSABI]`
   (`ELFOSABI_NETBSD = 2`) and/or the `PT_NOTE` `.note.netbsd.ident`; classify the
   image's ABI. Today the loader ignores OSABI.
2. **Process state** (`crates/akuma-exec/src/process/mod.rs`, `struct Process`) — add
   an `abi` / `syscall_table` field set at load time (default = Akuma-native; NetBSD
   for tagged images).
3. **Syscall dispatch** (`src/syscall/mod.rs`, `handle_syscall`) — dispatch through
   `current_process().syscall_table[nr]` instead of the single fixed match. The
   Akuma-native table is today's behaviour verbatim; the NetBSD table is the new
   `emul`.

The **NetBSD syscall table** is where the work lives: each entry carries NetBSD
semantics and decides its backing —
- **network** (`socket`/`connect`/`bind`/…) → the box's **rump** TCP/IP instance
  (`rump_sys_*`), the stack this whole port builds;
- **VFS/file** → rump VFS, or Akuma's own VFS with NetBSD-shaped args;
- **process/mem/time/signal** → Akuma primitives wrapped in NetBSD ABI.

The ABI fix-ups that the userspace shim does by hand (`sin_len`, `SOCK_NONBLOCK`,
errno mapping) now live **once, in the table**, applied uniformly to every binary.

## The demo (what success looks like)

```sh
# 1. obtain an unmodified NetBSD/aarch64 binary from pkgsrc (e.g. curl, or a
#    static NetBSD base tool) — NOT recompiled for Akuma.
#    `file` shows: ELF 64-bit LSB executable, ARM aarch64, for NetBSD, ...
#    (EI_OSABI = ELFOSABI_NETBSD)

# 2. drop it on the disk, run it in a --net box (rump TCP/IP backing the NetBSD ABI)
box open --net
/usr/pkg/bin/curl -s http://<qemu-host-ip>/         # a real NetBSD binary

# 3. it works with no shim and no recompile: the ELF loader saw the NetBSD note,
#    installed the NetBSD syscall table, and curl's NetBSD syscalls hit rump.
```

**Proof (same instrumentation as acceptance/11):**
- `file` confirms the binary is a genuine `ELFOSABI_NETBSD` aarch64 ELF (not an
  Akuma/musl rebuild).
- the virtif seam counters (`[VIRTIF TX/RX]`) show the HTTP frames — carried by the
  NetBSD rump stack.
- negative control: the same binary on a non-`--net` box (no rump) cannot reach the
  network, confirming the NetBSD table is what's backing it.

The payoff: **pkgsrc**. Once the NetBSD `emul` covers enough of the syscall
surface, the prebuilt NetBSD/aarch64 package ecosystem becomes installable and
runnable on Akuma as-is.

## Prerequisites & open design questions (to resolve when this is scheduled)

- **ABI detection**: trust `EI_OSABI`, or require the `.note.netbsd.ident` note? Mixed
  base/pkgsrc binaries vary; pick a robust rule + a fallback.
- **emul coverage**: which NetBSD syscalls must the table implement for a useful
  subset (network client tools first)? Stub the long tail with `ENOSYS` and grow it.
- **rump binding**: one rump instance per `--net` box; the table dispatches that
  box's network/VFS syscalls to its instance (ties into plan §10.1 config-driven
  rump + §10.2 per-box primary stack).
- **MD/ABI details for aarch64**: NetBSD signal trampolines, TLS (`tcb`), `errno`
  placement, struct layouts (`stat`, `dirent`, `timespec`) — these must match
  NetBSD/evbarm64, independent of the 2016 source pin used for the rump libs.
- **dynamic linking**: NetBSD `ld.elf_so` + NetBSD shared libs, or restrict to
  static pkgsrc builds first.
- **relation to M1**: strictly post-M1. M1 ships on the userspace shim
  (acceptance/11); this is the "run NetBSD software, not just our glue" follow-on.

See `userspace/rumpkernel/docs/IMPLEMENTATION_PLAN.md` §10.5 and
`docs/ARCHITECTURE_QUESTIONS.md` design (C).
