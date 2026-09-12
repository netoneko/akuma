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

**After the fixes below (same day), under Firecracker with the 512 MiB root
image, `apk add gcc musl-dev binutils` (190.6 MiB, 16 packages) installed and:**

- `gcc -c hello.c` compiles: `cc1` (a **42 MB** binary) spawns through the
  fixed `posix_spawn` path and the compiler proper runs.
- `as` assembles, and a hand-driven `ld -static …` links a 145 KB static
  binary from gcc's output, which **executes**:
  `hello from akuma gcc` — the first C program compiled by the guest's own gcc
  to run on Akuma/amd64.
- A *dynamic* binary hand-linked with the correct
  `-dynamic-linker /lib/ld-musl-x86_64.so.1` also runs — the kernel's
  interpreter load works when the interpreter path exists.

## Symptom table

| symptom | mechanism | status |
|---|---|---|
| `git clone` → `chmod on .git/config.lock failed: Function not implemented` | **dispatch, not VFS.** `chmod`(x86_64 90), `chdir`(80), `fchmod`(91) and `fchmodat`(268) had no row in `akuma-syscalls-abi`'s `syscall_table!`, so `Syscall::from_x86_64` answered `None` and `syscall_dispatch` returned `ENOSYS` (`amd64/src/usermode.rs`, the `let Some(call) = Syscall::from_x86_64(nr) else` arm). The implementations were not missing: `akuma-syscalls-glue` has `sys_chdir`/`sys_fchdir`/`sys_fchmod`/`sys_fchmodat` (`crates/akuma-syscalls-glue/src/fs.rs`) and the AArch64 kernel dispatches them by `nr`. Glue's `chdir`/`getcwd` read and write `Process::cwd`, `resolve_path_at` resolves `AT_FDCWD` against it, and `Process::inherit_from` copies the parent's cwd on `fork` — the per-process cwd plumbing was wired; only the numbers were absent | **fixed + verified 2026-09-12** — three rows added (80↔49, 91↔52, 268↔53), `Chdir`/`Fchmod`/`Fchmodat` routed through `to_glue`, `getcwd` folded to glue, `chmod`(90) shimmed to `fchmodat(AT_FDCWD, …)`. Boot suite 654/0 with six new probes; `cd /proc && pwd`, fork-inherited `pwd` and a `chmod`/`stat` mode round-trip confirmed over ssh in the Firecracker guest |
| `top` → `can't change directory to '/proc': Function not implemented` | same gap — busybox `top` `chdir`s into `/proc` to scan | **fixed** — same change; `chdir("/proc")` now succeeds (self-test probe runs exactly that) |
| `rustc`/`gcc`/`clang` → `posix_spawnp: Function not implemented` / `posix_spawn failed: Function not implemented` / `os error 38` | **`clone`, not `execve`.** This doc first blamed `sys_execve`'s non-slotted-task guard — wrong. musl's `posix_spawn` issues `clone(CLONE_VM|CLONE_VFORK|SIGCHLD)`, no `CLONE_THREAD`, and `sys_clone_thread` refused any `CLONE_VM` without `CLONE_THREAD` with `ENOSYS` (`amd64/src/thread.rs`). The spawn never happened; the drivers reported it at exec | **fixed + verified 2026-09-12** — the `Syscall::Clone` arm routes `CLONE_VM` without `CLONE_THREAD` to `sys_spawn_clone`: `fork_process(child_pid, child_stack)` — a **CoW copy, not a shared address space** (this target does not implement `CLONE_VFORK`'s parent suspension, so sharing would race; musl's spawn child only execs or exits, so the copy buys correctness for one fork per spawn), entering at `rsp = child_stack`, return `0` — exactly musl `__clone`'s entry convention (`[rsp]=fn, [rsp+8]=arg`). `cc1`, `as` and `ld` all spawn now. Two carried notes: spawn children start cwd `/` (they do not inherit the spawner's), and CoW fork is SMP=1-only on this target, so keep Firecracker at 1 vCPU |
| shell: `cc` → `Out of memory`; serial: `[execve] load failed: Read past end of image`; **nested symlinks fail too** | **resolved — symlinks.** `cc` is a *symlink* (`/usr/bin/cc -> gcc`). `sys_execve` read the image with raw `fs::read_file`, which never runs `resolve_symlinks` (only `sys_openat`'s path does), so the loader was handed the *link text* — 3 bytes of `"gcc"` — as the image | **fixed** — `fs::read_image` (new, `amd64/src/fs.rs`): symlink-resolves first, then chunks through `read_at`, which also lifts `read_inode_data`'s 16 MiB kernel-side allocation cap — a cap `cc1` (42 MB) died on as `ENOENT`. The chunk buffer lives on the **heap** (`try_reserve_exact`); the first draft used a 64 KiB stack array against a 32 KiB kernel stack (`sched::STACK_SIZE`) and the boot died as a ring-0 `#PF` inside `ClockBlockCache::get` several frames after the overflowing write — the lesson is now in `read_image`'s doc comment |
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

All three were closed or corrected later the same day — see the symptom
table's **fixed** rows and "Open after session 2" below, which replace this
list's status.

## Verify

- `chmod +x /bin/x && stat -c %a /bin/x` round-trips mode bits through ext2 —
  **verified** (boot probe + ssh `chmod 755` / `ls -l`).
- `cd /proc && pwd` in `busybox sh` reports `/proc` — **verified** (boot probe +
  ssh).
- `git clone` of any small repo completes.
- `gcc hello.c -o hello` links end to end — **partially verified**: `cc1` and
  `as` spawn and run; the last step (`collect2` → `ld`) is open, below.
- `rustc hello.rs` produces a linked `hello` — **untested** since the spawn
  fix; rustc's own spawn path is the one that now works for gcc.

## Open after session 2 (2026-09-12, ordered)

1. **`collect2` → `ld`: "no input files".** `gcc hello.c -o hello` compiles and
   assembles, then its own link step fails — while the *same* `ld` invocation
   run by hand with the same inputs links fine. `collect2` builds the ld
   command line itself and something in that hand-off (argv length? the
   spawn's argv copy? a `/tmp/ccXXXX.o` path lookup?) drops the input files.
   This is the one thing between `gcc hello.c -o hello` working.
2. **gcc spawns bare `cc1`** — its exec-prefix lookup fails and falls back to
   PATH search, so gcc only works with
   `PATH=/usr/libexec/gcc/x86_64-alpine-linux-musl/15.2.0:$PATH`. Candidate:
   gcc `stat`s its prefix directories before choosing; find which stat
   misbehaves. (The `Cannot read interpreter` below was once suspected here;
   disproved — see 3.)
3. **"Out of memory" is a misattribution.** `sys_execve` maps *every*
   `Image::from_elf_argv_envp` failure string to `ENOMEM` (`usermode.rs`, the
   `[execve] load failed:` arm). Two confirmed instances:
   - the symlink case — loader got 3 bytes of link text, musl said
     `Out of memory`;
   - the wrong-interpreter case — a binary naming a nonexistent
     `PT_INTERP` (`file` says `interpreter /lib/ld64.so.1`; nothing by that
     name exists) dies as `Cannot read interpreter` → printed `Out of memory`.
     A hand-linked binary with the correct
     `-dynamic-linker /lib/ld-musl-x86_64.so.1` runs — so dynamic loading
     itself works; the missing file was real, the errno was the lie. The
     loader should return structured errors and exec should map
     not-found→`ENOENT`, format→`ENOEXEC`, memory→`ENOMEM`.
4. **Spawn children start cwd `/`** — `sys_spawn`-created processes do not
   inherit the spawner's cwd (`register_spawn_process` hard-codes `"/"`); only
   `fork` children do. A `chdir`ed shell that spawns (rather than forks+execs)
   lands in `/`.
5. **CoW fork is SMP=1-only** — keep Firecracker at 1 vCPU until `akuma-cpu`'s
   pinned marker divergence lifts.
6. **`forkprobe` not yet run on amd64** — the in-guest fork/clone probe;
   expected to enumerate what `clone` flag combinations are still missing
   beyond the two served (`CLONE_VM|CLONE_THREAD`, `CLONE_VM` without
   `CLONE_THREAD`).
7. **Box network setup is not persistent** — tap0/dnsmasq/NAT were built
   through `hpbox.ubuntu` (the repo's `net-setup.sh` cannot reach the Ubuntu
   personality: your `~/.ssh/config` maps bare `192.168.1.123` to Akuma's
   port 2222, and the script does not pass `-F /dev/null -p 22`). Guest needs
   `nameserver 10.0.2.2` in `/etc/apk/resolv.conf`-adjacent config and
   `http:` repos (no IPv6: apk's AAAA resolution dies with `EAFNOSUPPORT`;
   host dnsmasq runs `--filter-AAAA`). All of this is lost on an Ubuntu
   reboot.

The virtio-blk "stuck on tag" stall seen during image reads is a cost
question, not a correctness one, and it rides behind all of these. None are on
box D's xHCI critical path — they are kernel work that the first in-guest
build (`proposals/NEXT_AGENT_AMD64_SELFHOST_FIRST_BUILD.md`) will hit
immediately after the disk survives.

## Background

- `docs/archive/RUST_TOOLCHAIN_ISSUES.md` — the AArch64 toolchain
  investigation (`n` works, `cargo --version` SIGILL — since fixed).
- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the trunk plan; box D is where
  this session lands, and its "first in-guest build" proposal is what these
  gaps block.
- `docs/archive/AKUMA_AMD64_STREAMLINING.md` § 4b — the last time a busybox
  binary's `/proc` expectations drove an amd64 fix (`ps`).
