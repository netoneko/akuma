# The rust toolchain on amd64: what runs, what fails, and why

**Date:** 2026-09-12
**Rig:** the bare-metal HP box (`docs/runbooks/amd64-bare-metal-loop.md`), kernel
`fa6a9f42-release-smp-shared`, toolchain installed with `apk add rust cargo`
(Alpine `1.96.1-r0`, musl host target) onto the persistent root.
**Status:** observations from a first self-host probe session. The toolchain
*installs* and `rustc --version` works; compiling anything that links does not
get past the kernel gaps below. Follow-up to `docs/archive/RUST_TOOLCHAIN_ISSUES.md`
(the AArch64 investigation) and part of box **D** of
`docs/archive/AKUMA_SELF_HOSTING_AMD64.md`.

## What works

- `rustc --version` → `rustc 1.96.1 (31fca3adb 2026-06-26) (Alpine Linux Rust
  1.96.1-r0)`. The driver starts, parses arguments and prints — identity
  syscalls (`uname -a` reports the real commit + profile string), `date` (C3
  SNTP wall clock), and the loader all behave.
- `apk add rust cargo` completes, so the toolchain-on-akuma path is `apk`, not a
  prepared image, exactly as box D predicted.

## Symptom table

| symptom | mechanism | status |
|---|---|---|
| `git clone` → `chmod on .git/config.lock failed: Function not implemented` | **dispatch, not VFS.** `chmod`(x86_64 90), `chdir`(80), `fchmod`(91) and `fchmodat`(268) had no row in `akuma-syscalls-abi`'s `syscall_table!`, so `Syscall::from_x86_64` answered `None` and `syscall_dispatch` returned `ENOSYS` (`amd64/src/usermode.rs`, the `let Some(call) = Syscall::from_x86_64(nr) else` arm). The implementations were not missing: `akuma-syscalls-glue` has `sys_chdir`/`sys_fchdir`/`sys_fchmod`/`sys_fchmodat` (`crates/akuma-syscalls-glue/src/fs.rs`) and the AArch64 kernel dispatches them by `nr`. Glue's `chdir`/`getcwd` read and write `Process::cwd`, `resolve_path_at` resolves `AT_FDCWD` against it, and `Process::inherit_from` copies the parent's cwd on `fork` — the per-process cwd plumbing was wired; only the numbers were absent | **fixed 2026-09-12** — the three rows added (80↔49, 91↔52, 268↔53; each pair differs, so `tables_disagree_where_linux_does` holds), `Chdir`/`Fchmod`/`Fchmodat` routed through `to_glue`, `getcwd` folded to glue (the old arm hard-coded `/`), and x86-only `chmod`(90) shimmed to `fchmodat(AT_FDCWD, …)` per the abi crate's rule 2. Not yet verified on the metal |
| `top` → `can't change directory to '/proc': Function not implemented` | same gap — busybox `top` `chdir`s into `/proc` to scan | open — verify the fix on the metal |
| `rustc hello.rs` (with `CC=gcc`) → `error: could not exec the linker 'cc' … os error 38` | the exec-caller-shape refusal — same mechanism as the next row, seen raw (`os error 38` = `ENOSYS`) because `gcc` is a real file, so the exec reached the kernel and was refused on *who* was calling, not *what* was being exec'd | see next row |
| `gcc hello.c -o hello-gcc` → `fatal error: cannot execute 'cc1': posix_spawnp: Function not implemented` — and clang the same: `clang-22 … posix_spawn failed: Function not implemented` | the exec-caller-shape refusal, now from **three** independent drivers (gcc, clang, rustc): musl's `posix_spawn` — what every one of them uses to run its subprograms — forks via `clone(CLONE_VM|CLONE_VFORK)` and execs from that child, and this target's `sys_execve` has exactly one `ENOSYS`, the not-a-slotted-user-task guard (`amd64/src/usermode.rs`, `current_proc_slot() >= PROC_SLOTS`). Each driver names the spawn call in its own error, which is what pins the mechanism: **nothing that uses musl's `posix_spawn` can exec on this kernel today.** The drivers reach the spawn; the child it creates cannot exec | open — exec from a `CLONE_VM` spawn child must work or musl-spawned processes cannot exec anything |
| shell: `cc` → `Out of memory`; serial: `[execve] load failed: Read past end of image`; **nested symlinks fail too** | **resolved — symlinks.** `cc` is a *symlink* (`/usr/bin/cc -> gcc`, 2 188 112 bytes real). `sys_execve` reads the image with `fs::read_file`, and ext2's path walk `lookup_path_internal` (`crates/akuma-ext2/src/ext2.rs`) never tests `S_IFLNK` on any component — so a path ending in a symlink resolves to the **link inode**, and a fast symlink's `read_inode_data` returns the *target text* as the file contents: 3 bytes of `"gcc"`. The ELF loader then reads its header off a 3-byte image → `ElfSource::Bytes::read_at` → `Read past end of image`; `sys_execve` maps every image rejection to `ENOMEM`, which musl prints as `Out of memory`. This is why the *shell's* exec of `cc` dies differently from rustc's (previous row): the shell forks a slotted child that reaches the loader, and the loader eats the link text. A **nested** chain fails identically at every level — the walk returns each next link's text, never the file. Every other consumer that resolves paths through glue's ladder (`resolve_path_at`) has the same hole for link components *inside* the path; only `readlinkat` answers links correctly today | open — the path walk must follow symlinks (final and mid-path, bounded against loops), which is shared ext2 work, not amd64-specific |
| shell: bare `gcc` and `gcc --version` → work (`gcc (Alpine 15.2.0)`) | the control group: `/usr/bin/gcc` is a **real 2.18 MB ELF**, and the shell's own `fork`+`exec` of it loads and runs — so the loader handles a binary this size, dynamic linking and all, and neither "eager mapping is too slow" nor "driver binaries are too big" is a live cause. The failures split exactly along the two mechanisms above: direct exec works, symlinked exec dies on the link, spawned exec dies on the shape | — |
| `cc1` → `not found` from the shell | not a kernel issue: `cc1` lives under `/usr/libexec/gcc/…`, which is not in `PATH`. gcc reaches it by absolute path through `posix_spawnp` — row one | environmental |
| **ELF loads are slow — virtio-blk `[BLK] stuck on tag 9`-shaped stalls** during image reads | hypothesis from the same session: `read_file` pulls the whole image block-by-block through the virtio-blk rings, and a request that wedges on a tag stalls the load for the tag timeout. If the `[execve] load failed:` line is preceded by a long silent gap, time the load of `/usr/bin/gcc` (2.18 MB, known-good) against `gcc --version`'s wall clock — if seconds go by before the banner, the block path, not the ELF parser, is the cost. Candidate second consumer for whatever the xHCI timeout-recovery work builds | open — needs a timing probe |
| `rustc` also linked `-lgcc_s`, `-lc` — needs `apk add gcc musl-dev` | not a kernel issue: Alpine's rust package carries no linker. `CC=gcc` in the probe was a workaround for `cc` being absent from the environment rustc built (`PATH="/usr/lib/rustlib/x86_64-alpine-linux-musl/bin"` — no `/usr/bin`), which is itself worth knowing: rustc's spawned-linker `PATH` is inherited, so a stripped `PATH` silently changes which `cc` is found | environmental, not kernel |

## The shape of it

Everything observed so far is a **kernel gap, not a toolchain failure**, and
the session's control experiment is what proves it: bare `gcc` — a real
2.18 MB, dynamically linked ELF — loads and runs through the shell's
`fork`+`exec` fine, so the loader and image mapping are not the wall. Every
failure splits into one of three mechanisms:

1. **ext2 never follows symlinks in a path walk** (`lookup_path_internal` has
   no `S_IFLNK` arm — not on the final component, not mid-path, so nested
   chains fail identically) — kills every symlinked binary, `cc` being the one
   everything reaches for. Shared ext2 work; bites every consumer of a
   resolved path, not just exec.
2. **musl's `posix_spawn` caller shape is refused by `execve`** — kills gcc,
   clang and rustc alike at their first subprogram, each naming the spawn call
   in its own error. The deep one: cargo spawns its subprocesses the same way,
   so nothing builds until this works.
3. the `chmod`/`chdir` **dispatch gap** — closed 2026-09-12 (rows in
   `akuma-syscalls-abi`, arms in `usermode.rs`).

The virtio-blk "stuck on tag" stall seen during image reads is a cost
question, not a correctness one, and it rides behind all three. None of these
are on box D's xHCI critical path — they are kernel work that the first
in-guest build (`proposals/NEXT_AGENT_AMD64_SELFHOST_FIRST_BUILD.md`) will hit
immediately after the disk survives.

## Verify

- `chmod +x /bin/x && stat -c %a /bin/x` round-trips mode bits through ext2
  (`akuma-ext2` has real `chmod` backing; today this kernel ignores the execute
  bit, so `fchmodat` on a file should change the on-disk mode and `stat` report it).
- `cd /proc && pwd` in `busybox sh` reports `/proc` (chdir + getcwd agree).
- `git clone` of any small repo completes.
- `gcc hello.c -o hello` links — `cc1` spawns and the assembler runs end to end.
- `rustc hello.rs` produces a linked `hello` once the `posix_spawn` exec shape
  is in place.

## Background

- `docs/archive/RUST_TOOLCHAIN_ISSUES.md` — the AArch64 toolchain
  investigation (`n` works, `cargo --version` SIGILL — since fixed).
- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the trunk plan; box D is where
  this session lands, and its "first in-guest build" proposal is what these
  gaps block.
- `docs/archive/AKUMA_AMD64_STREAMLINING.md` § 4b — the last time a busybox
  binary's `/proc` expectations drove an amd64 fix (`ps`).
