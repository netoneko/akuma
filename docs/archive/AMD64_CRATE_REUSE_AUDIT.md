# Four things the amd64 port hand-rolled that the tree already had

**Date:** 2026-09-04, during Stage O of the amd64 port.
**Scope:** process, not a defect. Nothing shipped broken, and in every case the
code that replaced the duplicate is better than what it replaced.

This is written down because the failure mode is **quiet**. Hand-rolled code that
works is indistinguishable, at a glance, from code that had to be written. A
duplicate that compiles, passes its tests and boots leaves no symptom — it just
means the tree now has two implementations of something, and the second one does
not get the first one's bug fixes.

The first three were found the same way: the author of the crates read the diff
and asked "don't we already have that?" The fourth was found by looking first,
which is the outcome this document is for.

## 1. The `mmap` argument decode

**Written:** a local decode of `MAP_ANONYMOUS`, `MAP_FIXED`, `PROT_WRITE|EXEC`
and the file-backed case, ~25 lines in `amd64/src/mm.rs`.

**Existed:** `crates/akuma-syscalls-mem::mmap::plan(prot, flags, fd, pages,
eager_max_pages) -> Plan`, plus `fixed_addr_unaligned_einval` and
`fixed_overlaps_kernel_va`. A pure function of the argument bits, host-tested,
with **seven pinned divergences from Linux** recorded in the crate.

**Cost of the duplicate, had it stayed:** the amd64 target would have drifted
from the AArch64 kernel on exactly the arguments where Linux compatibility is
subtle — the seven divergences are subtle by definition, which is why they are
pinned. Two of the functions in that crate failed their first host test with an
overflow reachable from unprivileged userspace (`AKUMA_EXTRACT_MMAP.md` §10.1);
a fresh implementation would have had to rediscover that.

**Now:** imported. What stays local is frame allocation and page-table writes,
which are genuinely per-architecture.

## 2. The console reader

**Written:** a byte-at-a-time loop over the 16550 that returned each byte as it
arrived.

**Existed:** `crates/akuma-terminal` — canonical mode, line buffering,
backspace, Ctrl+D as EOF, echo, `translate_output`, `enter_raw_mode`. `no_std`,
one dependency (a spinlock), already building for `x86_64-unknown-none`.

**Cost, and this one was about to be paid:** `map_cr_to_nl`. A serial terminal
sends **CR** when Enter is pressed; every line-oriented reader waits for **NL**.
The hand-rolled loop would have echoed correctly, looked correct, and never
returned a line. That is a debugging session, and the crate exists because
somebody already had it.

**Now:** the console has two paths, and the split is real rather than a
compromise (see §4 below).

## 3. The file-descriptor type

**Written:** `struct OpenFile { data: Vec<u8>, pos: usize }`.

**Existed:** `akuma_exec_core::process::{FileDescriptor, KernelFile}` — the
tree's descriptor enum, in the **unsafe-free core** of the execution crate,
behind only `akuma-primitives`, `akuma-mmap` and `akuma-syscalls-linux`, all of
which build for this target.

**And it is strictly better than what was written:**

* it carries a `dir_cache`, a snapshot of directory entries taken on the first
  `getdents64`, so entries deleted between calls cannot drift the position;
* it addresses a file by `(mount_id, inode)` rather than by path, so a `read`
  never re-resolves a name that could now mean a different file;
* it holds an `InodePin` that keeps the inode's data alive for the descriptor's
  lifetime — load-bearing purely through `Clone`/`Drop`, which is what makes
  `dup`, `fork`, `close` and `exec`'s table clear balanced without any of them
  knowing the pin exists.

It also already had the exact mode this target needs: `KernelFile::new(path,
flags)` leaves the inode 0, which its own doc defines as *"no inode: read by
path"*.

**Now:** imported; `OpenFile` deleted.

## 3b. `sockaddr_in` parsing (the fourth, caught without prompting)

**Written:** a decode of `struct sockaddr_in` — the `AF_INET` family check, the
**big-endian** port, the octet order — plus local `AF_INET`/`SOCK_STREAM`
constants, in `amd64/src/sock.rs`.

**Existed:** `akuma_net::socket::SockAddrIn` with `to_addr()` and `from_addr()`,
in the crate that owns sockets, alongside
`socket_const::{AF_INET, SOCK_STREAM, SOCK_DGRAM}`.

**Cost:** that decode is exactly where the classic byte-order bug lives — a port
read host-endian lands on 8080 instead of 80 (`0x1F90` vs `0x901F`). The
existing implementation has been right about it for as long as the AArch64
kernel has served connections.

**Now:** imported. Only the privilege-boundary copy stayed local, for the same
reason as §2 in the table below.

This one was found by looking rather than by review, which is the point of
writing the first three down.

## 4. What is genuinely local, and why

Three duplications survive, each with a checked reason:

| local | the crate | why it cannot be used |
|---|---|---|
| the file syscall *bodies* (`sys_read`, `sys_openat`, `sys_lseek`) | `akuma-syscalls-glue` | **Does not build.** `error: invalid instruction mnemonic 'cbz'` — AArch64 asm reaching it through `akuma-user-access`. Its dependency list is 25 crates deep including `akuma-exec`, `akuma-mmu` and `akuma-bkl` |
| `copy_from_user` / `copy_to_user` | `akuma-user-access` | **Does not build.** Its copy loop is AArch64 `global_asm!` |
| the fd *array* | `akuma-exec`'s `Process` | Does not build here. The descriptor *type* is shared; only the table is local |

The raw-vs-canonical console split is not duplication either: `read(0)` goes
through `akuma-terminal`'s canonical mode, and Akuma's own `poll_input_event`
returns unechoed single bytes because its caller (`paws`) does its own line
editing. The terminal crate has `enter_raw_mode` for exactly that split; serving
both from the canonical path would make a shell wait for a whole line before
echoing the first character.

## 5. The imprecision that made this easy

A sweep was run early: "which crates build for `x86_64-unknown-none`?" It
answered 39 of 54, and that number was then treated as a list of what could be
used. **It is not.**

`akuma-mmu` compiles for `x86_64-unknown-none` — it is bit arithmetic and
contains no invalid x86 instruction. Its own header says *"AArch64, 4 KB granule,
4-level page tables"*, and it manipulates AArch64 descriptors throughout, with
191 arch-specific tokens in one file. A crate that builds can still be wrong.

**Building is necessary, not sufficient.** The audit table in `amd64/README.md`
therefore records three distinct reasons a crate is unused — blocked on a known
proposal item, cannot build, or not reached yet — rather than one boolean.

## 6. What to do before adding a module to `amd64/`

```bash
cargo build -p <crate> --target x86_64-unknown-none --release   # does it build?
cargo tree -p akuma --target aarch64-unknown-none -e normal     # what does the real kernel use?
```

The second is the one that would have caught all three of these. The kernel pulls
in **38 crates** the amd64 target does not, and that list *is* the roadmap: each
entry is either blocked, unbuildable, or simply not reached yet. Reading it
before writing a module is cheaper than reading a diff afterwards.

Then read what the crate says it is. `akuma-mmu` announces its architecture in
line 3 of its header; `KernelFile` documents the `inode == 0` mode that made it
fit; `akuma-syscalls-mem` states that its plan is a function of argument bits
with no process behind it. In every one of these three cases the crate's own
first paragraph would have settled the question.

---

**Background:** `docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.21 is the stage
these were written during; `amd64/README.md` carries the standing audit table.
`docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` §1 is the open item that blocks
`akuma-mmap` and `akuma-elf`.
