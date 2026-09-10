# amd64 C1 step 4b, batch 3b: the `stat` family

**Date:** 2026-09-10
**Status:** landed.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's `4b` row.
**Predecessor:** `AKUMA_AMD64_4B_FOLD_BATCH3A.md` (`read`/`pread64`/`write`/
`lseek`/`getdents64`), whose §7 is the plan this batch executes: *"Folding the
`stat` family needs a converter in `akuma-syscalls-abi` … with `offset_of!`
assertions, and a split of glue's `sys_fstat`/`sys_newfstatat` at the user-copy
seam."*

Five arms moved: `fstat`, `newfstatat`, `statfs`, `fstatfs`, and `statx` (which
was **not dispatched at all** before — every call was `ENOSYS`).
`amd64/src/fd.rs`: **+120 / −280**, 3 608 → 3 448 lines.

---

## 1. `struct stat` is the third architecture vocabulary

After the syscall numbers (C1 step 1) and `open(2)`'s flag word (4b
prerequisites), `struct stat` is the third place x86_64 and asm-generic
disagree on a wire layout:

| field | x86_64 | asm-generic (aarch64) |
|---|---:|---:|
| `st_nlink` | 8 bytes at 16 | 4 bytes at 20 |
| `st_mode` | 24 | 16 |
| `st_rdev` | 40 | 32 |
| `sizeof` | **144** | **128** |

`akuma_syscalls_linux::Stat` is the asm-generic layout — its own header says so
— and every shared crate that fills a `stat` fills that one. amd64 carried a
hand-rolled `encode_stat` that wrote literal x86_64 offsets into a `[u8; 144]`,
under a comment calling it "proposal item 5 territory".

**That item is now `akuma_syscalls_abi::stat`** (`+135` lines): a `#[repr(C)]
X8664` struct with every one of `encode_stat`'s literal offsets pinned by an
`offset_of!` assertion, and `to_x86_64(&Stat) -> X8664` — a field-by-field
re-lay, not a cast (the two structs disagree about `st_nlink`'s *width*, so
there is no reinterpret that does this). It lives in the abi crate for the same
reason `open_flags` does: that crate is `#![forbid(unsafe_code)]` and its whole
job is "which representation, on which architecture". One host test pins the
conversion and the offset of `st_size` (the field a 64-bit `ls` reads for the
size).

`statx` needs none of this — `struct statx` is 256 bytes with identical offsets
on both architectures, so its fold is a number and nothing else.

## 2. The seam: `sys_fstat` / `sys_newfstatat` split in two

The same split batch 2d drew for `sys_openat`/`openat_path`:

- `akuma_syscalls_glue::fs::fstat_fill(fd) -> Result<Stat, u64>` — the whole
  arm except the user write. `pub`.
- `newfstatat_fill(dirfd, path: &str, flags) -> Result<Stat, u64>` — likewise.
- glue's own `sys_fstat`/`sys_newfstatat` are thin wrappers over those (validate
  the pointer, fill, `write_user_val`) — **byte-identical behaviour for the
  AArch64 dispatch**.
- amd64's arms call `*_fill`, run the result through
  `akuma_syscalls_abi::stat::to_x86_64`, and `write_val` the 144-byte struct.

`statfs`/`fstatfs` needed no split: `struct statfs` is asm-generic on both
architectures (120 bytes, three offsets asserted in `akuma-syscalls-linux`), so
amd64's arms are now a straight forward to glue's `sys_statfs`/`sys_fstatfs`,
and `statfs_into` + `fs_magic` leave `fd.rs` entirely. Glue's `fs_magic` is
richer than the one that left — it knows `proc`, `tmpfs`, `overlay` — which
`df` prints as the mount type.

## 3. Two bugs in the *shared* crate, found by folding onto it

Following the pattern of every batch in this series — the real content is what
the fold turns up.

### `fstat` on a socket answered `EBADF` on **both** kernels

`fstat_fill`'s `match` had no `Socket` / `UnixSocket` / `RumpSocket` arm and
fell to `_ => EBADF` — so `fstat(socket_fd)` told the caller its descriptor was
closed, on a descriptor that is open and perfectly usable. Linux answers
`S_IFSOCK | 0777`. amd64's local arm had always given that; the fold would have
imported the gap. **Fixed in glue**, so both kernels get it, with a boot-suite
check on each (`sock: fstat on a socket succeeds` / `… reports S_IFSOCK`).

### `newfstatat` did not implement `AT_EMPTY_PATH`

Glue's `sys_newfstatat` copied the path, and an empty one with `AT_EMPTY_PATH`
set (the `fstat`-spelled-as-`fstatat` form) resolved `""` against `dirfd` —
which happened to work for a `File` dirfd and returned `ENOENT` for a pipe or
socket one. It now redirects to `fstat_fill(dirfd)`, Linux's actual semantics,
for both kernels. A small improvement on AArch64; the amd64 arm always had it.

## 4. `statx` is new on this target

Every `statx(2)` returned `ENOSYS` before this batch — the number had no row in
`akuma_syscalls_abi`'s table. It has one now (`Statx => 332 / nr::STATX`), amd64
dispatches it straight to glue with no preamble, and a modern coreutils
`stat(1)` and Rust's `std::fs::metadata` both try it before falling back to
`newfstatat`.

## 5. The probe grew two lines

`userspace/forktest/c_stress/lazybuf.c` — the "syscall writes into an untouched
`mmap` page" probe from batch 3a — gained `probe_newfstatat` and `probe_statx`.
Its `probe_fstat` line, which batch 3a left as a *future* check ("the day
`fstat` folds, this line starts asking the question"), now asks the same
prefault question of glue's `fstat_fill` that `probe_read` asks of `sys_read`.

| where | lazybuf |
|---|---|
| Akuma/amd64, QEMU | **8 PASS** (was 6) |
| Akuma/amd64, bare metal | **8 PASS** |

## 6. Verification

| gate | before 3b | after |
|---|---|---|
| QEMU/TCG `SMP=1` / `SMP=4` | 590/0 / 600/0 | **596/0 / 606/0** |
| Firecracker/KVM `SMP=1` / `SMP=4` | 576/0 / 586/0 | **580/0 / 590/0** |
| bare metal (HP 500-502nj) | 590/0 | **596/0** |
| `amd64_ring3_check --smp 1 -n 30` | OK | **OK**, heap +29 kB (in tolerance) |
| `lazybuf` (QEMU / metal) | 6/6 | **8/8 · 8/8** |
| `openflags` (QEMU / metal) | 20/20 | **20/20 · 20/20** |
| `apk update` + `apk add file` (QEMU + metal) | OK | **OK**, `file-5.47` runs |
| host tests | 1371 | **1372** (+1, the abi `stat` conversion) |
| clippy, both kernels | clean | **clean** |

**+6 checks on QEMU/metal, +4 on Firecracker** (the two `sock:` checks need a
NIC). The six: `fstat` mode type bit read at the x86_64 offset; `statx`
succeeds / reports size / reports a regular file; `sock: fstat` succeeds /
reports `S_IFSOCK`. Every one was falsified against the code it tests — a
negative-control run that broke `to_x86_64` (writing `st_mode` at the
asm-generic offset) and deleted the `Socket` arm turned all six red, and took
`execve`, `fork`, `redirect` and `/proc/self/exe` down with them, because a
`struct stat` with the mode in the wrong place breaks every `S_ISREG`/`S_ISDIR`
test busybox makes.

### AArch64: no regression, and the loop still does not close

`park_indefinitely` aside (batch 3a), this batch's AArch64 surface is: the
`fstat_fill`/`newfstatat_fill` split (behaviour-identical wrappers), the new
`Socket` arm in `fstat_fill` (was `EBADF`), and `AT_EMPTY_PATH` in
`sys_newfstatat`. All three are improvements or no-ops.

Verified by booting the **committed HEAD** kernel and this batch's kernel
side-by-side under Lima/KVM: **identical failure sets**, line for line —

- `test_spawn_ext_passes_env` panics the suite (`src/process_tests.rs:3411`),
  the pre-existing failure `AKUMA_SELF_HOSTING_AMD64.md` Open issue 4 records as
  branch-transient;
- `test_mmap_file_oom_survives` FAILs with the identical PMM numbers
  (`before=33247 after=23680`);
- four `EC=0x25` exceptions, which `stp_xzr_ec15_handler_fires` explains as this
  QEMU's syndrome for `stp`-to-`PROT_NONE`.

So the AArch64 userspace verification loop is still blocked upstream of anything
this batch touches. Host tests (`cargo test`, 1372) and clippy are the coverage
that does run, and both are green.

## 7. What is left in `fd.rs`

The arms still implemented here, and why each has not folded:

| arm | blocker |
|---|---|
| `fcntl` | glue has `sys_fcntl`; the `F_GETFL`/`F_SETFL` flag word is x86_64-encoded and wants an `open_flags` round-trip in the preamble — small, next batch |
| `dup` / `dup3` / `pipe2` | glue has all three; mechanical forwards, next batch |
| `poll` / `select` | glue's `akuma-syscalls-poll` family; the amd64 arms are `ppoll`/`pselect6` shims already and the readiness model differs — needs care |
| `ioctl` | half x86-only (the `TIOC*` terminal set is shared, the framebuffer/input ones are not) |
| `access` | glue has `sys_faccessat`; trivial, next batch |
| `utimensat` | glue has `sys_utimensat`; the `UTIME_NOW`/`UTIME_OMIT` sentinels are shared — next batch |
| `poll_input_event` (313) | x86-only, USB keyboard — stays forever |

## Background

- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH3A.md` §7 — the plan this batch runs.
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH2D.md` — the `sys_openat`/`openat_path`
  split this copies, and the inode pins `fstat_fill` reads.
- `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` §5 — "proposal item 5", the
  `struct stat` layout divergence, now closed.
- `crates/akuma-syscalls-linux/src/stat.rs` — the asm-generic `Stat`/`Statx`/
  `Statfs` layouts and their assertions.
