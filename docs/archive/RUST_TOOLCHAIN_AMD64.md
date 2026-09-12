# The rust toolchain on amd64: what runs, what fails, and why

**Date:** 2026-09-12
**Rig:** the bare-metal HP box (`docs/runbooks/amd64-bare-metal-loop.md`), kernel
`fa6a9f42-release-smp-shared`, toolchain installed with `apk add rust cargo`
(Alpine `1.96.1-r0`, musl host target) onto the persistent root.
**Status:** **`rustc <file>` compiles, links and runs in the guest** — plainly,
with no `-C linker` flags, once `PATH` carries `/usr/bin` (session 3,
2026-09-12). Four kernel defects were between here and session 2: the argv cap,
`ftruncate`, `socketpair` and the two AF_UNIX lifecycle hooks under it. The open
item to pull next is that the environment a spawned child receives is **empty**
— no `PATH` at all — which is why that caveat about `PATH` exists. Sessions 1 and 2 are preserved as written;
their open item 1 is closed. Follow-up to `docs/archive/RUST_TOOLCHAIN_ISSUES.md`
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

> Item 1 is **closed** (session 3, same day) and item 3's misattribution is
> still live. Everything else here stands as written.

1. ~~**`collect2` → `ld`: "no input files".**~~ **CLOSED 2026-09-12** — it was
   the first guess on the list: **argv length.** `loader::MAX_ARGV` was 16 and
   `execve` truncated at it silently, so `ld` received a command line with its
   inputs (and, for `rust-lld`, its `-o`) removed. See "Session 3" below. The
   original text: `gcc hello.c -o hello` compiles and assembles, then its own
   link step fails — while the *same* `ld` invocation run by hand with the same
   inputs links fine. `collect2` builds the ld command line itself and
   something in that hand-off (argv length? the spawn's argv copy? a
   `/tmp/ccXXXX.o` path lookup?) drops the input files. This is the one thing
   between `gcc hello.c -o hello` working.
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

## Session 3 (2026-09-12, later the same day): it links

**`rustc` compiled, linked and ran a Rust program inside Akuma/amd64 under
Firecracker** — a 4.8 MB static `x86_64-unknown-linux-musl` binary, 5 seconds
end to end, printing `hello from akuma amd64`. Two kernel defects were between
that and session 2, and one of them is open item 1 above.

```
# busybox env LD_LIBRARY_PATH=/usr/local/rust/lib /usr/local/rust/bin/rustc \
    -C linker-flavor=ld \
    -C linker=/usr/local/rust/lib/rustlib/x86_64-unknown-linux-musl/bin/gcc-ld/ld.lld \
    -C link-self-contained=yes -o /tmp/hello /tmp/hello.rs
# /tmp/hello
hello from akuma amd64
```

### Defect 1 — `argv` was capped at 16, silently. **This is open item 1.**

`loader::MAX_ARGV` was 16, sized in a comment for `sh -c "<cmd>"`, and
`sys_execve` **truncated** at it rather than refusing. A toolchain's linker
invocation is forty-odd arguments, so `ld` ran with a command line that was
well-formed, shorter, and missing its `-o` and its inputs. Each linker then
reported it in its own vocabulary and blamed itself:

| linker | what it said | what was actually wrong |
|---|---|---|
| `collect2` → `ld` (session 2, item 1) | `no input files` | the input files were argv entries 17+ |
| `rust-lld` | `cannot open output file a.out` | `-o /tmp/hello` was argv entries 39–40 |

Session 2 guessed at "argv length? the spawn's argv copy? a `/tmp/ccXXXX.o` path
lookup?" and could not choose between them. The probe that settles it is one
line in the guest and needs no toolchain at all:

```
# busybox echo a b c d e f g h i j k l m n o p q r s t u v w x y z
a b c d e f g h i j k l m n
```

Fourteen letters — sixteen argv entries counting `busybox` and `echo`.

**Fixed:** `MAX_ARGV` 16 → 256, `MAX_ENVP` 32 → 64, and `user_strv` now returns
`None` when the caller's array is longer than the cap so `execve` and `sys_spawn`
answer **`E2BIG`**, which is what Linux answers and is impossible to
misattribute. Truncating was the whole defect; the cap was only its size.

The caps cost kernel stack, and paying for 256 entries the old way would have
been ~5 KiB against a 32 KiB `sched::STACK_SIZE`. So `build_stack` no longer
keeps `[u64; MAX_ARGV]` / `[u64; MAX_ENVP]` beside its word block: the string
pointers are written straight into the block (they are what the block holds
anyway) and the string-placement cursor is walked twice instead — once to find
the bottom of the blob, once to fill in the pointers. Net growth ~2 KiB.

Guarded by six boot self-tests (`elf: a full-size argv builds a stack` and its
five checks): a stack is built with a full `MAX_ARGV` argv and read back out of
the address space — `argc`, `argv[0]`, **`argv[MAX_ARGV-1]`** and the NULL after
it. The last pointer is the one a cap drops, and the cursor rewrite is the code
those tests exist for.

### Defect 2 — `ftruncate` had a handler, a number, and no row

With argv fixed, `rust-lld` saw its `-o` and failed one step later:
`cannot open output file /tmp/hello: Function not implemented`. It opens the
output, sizes it with `ftruncate`, and maps the result; the open succeeded and
the `ftruncate` was `ENOSYS`.

`sys_ftruncate` was in `akuma-syscalls-glue`, `nr::FTRUNCATE` was in
`akuma-syscalls-linux`, glue dispatched it — and `akuma-syscalls-abi`'s
`syscall_table!` had no row, so `Syscall::from_x86_64(77)` was `None`. Exactly
the `chmod`/`chdir` shape from session 2, for the third time.

**Fixed:** one row (`Ftruncate => FTRUNCATE = 77, nr::FTRUNCATE`) and one arm.

### The change that stops this recurring: name the missing number

Three sessions have now lost time to "the implementation exists, the number does
not", and every instance reached userspace as `Function not implemented` from a
program that then blamed itself. `syscall_dispatch`'s unknown-number arm now
says so on the console:

```
[syscall] no row for x86_64 nr=77 — returning ENOSYS (add it to akuma-syscalls-abi's table)
```

One line per distinct number, bounded to 32 of them, so a program probing in a
loop costs one line and a runaway cannot flood the console. It found `ftruncate`
in a single boot, and it is what turned the `git clone` failure below from a
symptom into a named gap.

### Numbers observed missing, and what each costs

From one `rustc` compile-and-link plus one `git clone` in the guest:

| x86_64 | call | consequence observed |
|---|---|---|
| 77 | `ftruncate` | **fixed** — nothing could link |
| 290 | `eventfd2` | `git clone` over https fails at `curl_multi_init` — below |
| 95 | `umask` | none seen; musl start-up probe |
| 40 | `sendfile` | none seen |
| 324 | `membarrier` | none seen (`akuma-syscalls-mem` decodes it; no row) |
| 204 | `sched_getaffinity` | none seen; rustc falls back to 1 thread |
| 157 | `prctl` | none seen |
| 260 | `fchownat` | none seen |

Only `ftruncate` was load-bearing for linking; the rest are tolerated by their
callers and are listed so the next `ENOSYS` can be checked against them rather
than rediscovered. Note what the list is also good for: a reproduction that adds
**no** new number has ruled out this whole class, which is how the `cc` failure
below was separated from it in one run. **Do not add rows speculatively** — `akuma-syscalls-abi`'s
rule 1 is that a row is a claim that both architectures' numbers were checked.

### `git clone https://…` → `curl_multi_init failed`

```
$ git clone https://github.com/netoneko/akuma-playground.git
Cloning into 'akuma-playground'...
fatal: curl_multi_init failed
fatal: remote helper 'https' aborted session
```

**Cause, named by the diagnostic above: `eventfd2` (x86_64 290) is `ENOSYS`.**
`curl_multi_init` builds the multi handle's wakeup channel with
`eventfd(0, EFD_CLOEXEC|EFD_NONBLOCK)`; when that fails it returns `NULL`, and
git's https remote helper reports the `NULL` without ever naming the syscall.
`sched_getaffinity` (204) and `prctl` (157) are also missing and are also in
libcurl's start-up path, but neither aborts it.

This is **not** the same shape as `ftruncate`, and the difference is the work:

* `akuma-syscalls-glue` *has* `eventfd` (`crates/akuma-syscalls-glue/src/eventfd.rs`,
  `eventfd_create`/`eventfd_read`/`eventfd_write`), but behind the
  `sc-eventfd` feature, and `amd64/Cargo.toml` takes glue with
  `default-features = false, features = ["smoltcp"]` — so it is **compiled
  out**, not merely undispatched.
* An eventfd is an **fd**, and amd64 keeps its own descriptor table
  (`amd64/src/fd.rs`, `FIRST_FILE_FD = 3`). Turning the feature on gives an
  `eventfd_create` whose id nothing on this target can `read`, `write`, `close`
  or poll. The row is the last step, not the first.

So the fix is: enable `sc-eventfd`, give `amd64/src/fd.rs` an `Eventfd` variant
routed to glue's three entry points, then add the `Eventfd2 => 290, nr::EVENTFD2`
row. `epoll` is the same shape behind `sc-epoll` and is what libcurl wants next.

Until then, in-guest `git` works over `git://`/`ssh://` but not `https://`, and
`apk` (which uses its own HTTP client, not libcurl) is unaffected.

### Open: `rustc hello.rs` with no `-C linker` → `could not exec the linker \`cc\`: Function not implemented (os error 38)`

Plain `rustc hello.rs` still fails, and the errno is wrong in a way that hides
what is actually going on. What is **measured**, in order:

1. **A spawned child gets no environment at all.** Over ssh, `busybox env`
   prints exactly:

   ```
   SHLVL=1
   PWD=/
   ```

   Both are the shell's own additions, so what sshd handed it was *empty*: no
   `PATH`, no `HOME`, no `TERM`. (The shell's `echo $PATH` shows
   `/sbin:/usr/sbin:/bin:/usr/bin`, which is busybox ash's built-in default for
   an unset `PATH` — a default it does not export.) This is the standing item
   in [`AMD64_SSH_TERM_SIZE_NOT_PASSED.md`](AMD64_SSH_TERM_SIZE_NOT_PASSED.md)
   break 8 seen from the other end, and it is almost certainly upstream of
   everything below: a toolchain driver decides what to exec from `PATH`.
   The `VAR=val cmd` prefix form *does* work within the shell (`PATH=/zzz
   busybox env` fails looking for `busybox` in `/zzz`), so this is about what
   crosses `sys_spawn`, not about the shell.
2. **rustc replaces the child's `PATH` with its own two directories** —
   `…/rustlib/x86_64-unknown-linux-musl/bin` and that path's `self-contained`
   subdirectory, **which does not exist** in a `--profile minimal` toolchain.
   Neither contains `cc`. So on this image `cc` is genuinely unfindable by name
   from rustc's linker invocation, and would be on Linux too.
3. **An absolute linker path spawns fine.** `-C linker=/usr/bin/cc` gets all the
   way to running gcc, which then fails for reasons 4 and 5. So neither the
   spawn path nor argv is at fault.
4. `collect2` then reports `cannot find 'ld'` — same PATH, one layer down. The
   user reproduced this by hand with the exact command line rustc printed.
5. With gcc's exec-prefix `PATH` supplied by hand, a `cc -static` of a C file
   reaches `collect2: fatal error: cannot get program status: Interrupted
   system call` — **a spurious `EINTR` out of `waitpid`**, which is its own
   kernel bug and is the same family as the spurious-`EINTR`-after-a-signal
   defect C3 found on both kernels ([`AKUMA_AMD64_C3_CLOCK.md`](AKUMA_AMD64_C3_CLOCK.md)).

**Ruled out, by measurement rather than by argument:**

* *Not* the argv cap. It is 256 now, this command line is ~40 entries, and the
  `= note:` rustc prints is the line it *built*, which arrives complete.
* *Not* a missing syscall row. The new `[syscall] no row for …` diagnostic
  printed **nothing new** across a reproduction — the set before and after is
  identical.
* *Not* `execve` mis-reporting a missing file. Absolute missing paths answer
  `ENOENT` (`/tmp/definitely-not-here`, `/usr/bin/zzz-not-here`, a seven-deep
  `/tmp/a/b/c/d/e/f/g/zzz`), a missing program under a **nonexistent** directory
  answers `ENOENT`, and musl's own `posix_spawnp` of an absent program answers
  `ENOENT` (observed from gcc: `cannot execute 'cc1': posix_spawnp: No such file
  or directory`). A busybox `PATH` search that steps over a missing directory
  first still finds the program in a later one.
* *Not* a failed `execve` poisoning the next one in the same task — the
  `PATH=/tmp/nope:/usr/bin` control runs `cc` fine.

**Root cause: `socketpair` (x86_64 53) is `ENOSYS`** — found with the kernel's
own tracer, `strace` on the command line, in a 1502-line boot:

```
[sc>] cpu=0 task=2 nr=53 a1=0x0000000000000001
[syscall] no row for x86_64 nr=53 — returning ENOSYS (add it to akuma-syscalls-abi's table)
[sc]  cpu=0 task=2 nr=53 -> 0xffffffffffffffda        (= -38)
cc, rustc's PATH -> Function not implemented (os error 38)
```

Rust `std`'s `Command::spawn` will not use `posix_spawnp` when the program has
no slash **and** the command overrides `PATH` — `posix_spawnp` searches the
*caller's* `PATH`, not the child's, so it would look in the wrong directories.
It forks instead, and the fork path opens a **`socketpair`** to carry the
child's exec errno back to the parent. That is the call that fails, before any
exec happens, and `spawn` returns its errno verbatim.

Which is why the failure looks like it is about `cc` and is not:

| shape | path taken | result |
|---|---|---|
| `cc`, rustc's `PATH` | fork + socketpair | **os error 38** |
| `cc`, inherited `PATH` | `posix_spawnp` | works |
| an absent name, rustc's `PATH` | fork + socketpair | **os error 38** |
| `/usr/bin/cc`, rustc's `PATH` | `posix_spawn` (has a slash) | works |

**FIXED 2026-09-12, and it was not one line.** It starts as the `ftruncate`
shape — `nr::SOCKETPAIR = 199` existed, `akuma-syscalls-glue::net::sys_socketpair`
existed (AF_UNIX, pipe-backed), glue dispatched it, and there was no row in
`akuma-syscalls-abi`. But unlike `ftruncate` it *creates* objects, and each
layer under it had to be reached in turn. Three fixes, each found by the one
after it failing:

1. **The row and the arm.** `Socketpair => SOCKETPAIR = 53, nr::SOCKETPAIR`,
   and `Syscall::Socketpair => to_glue(…)`. The descriptors need no work of
   their own: they land in `Process::fds`, which is the table `crate::fd`
   allocates into, and `read`/`write`/`close` on this target are already glue's.
2. **The two `ExecRuntime` hooks.** `unix_sock_close` and
   `unix_sock_clone_ref` were `not_wired!` panics reading "AF_UNIX is not built
   for this target" — true of the *syscalls* and never of the code, since glue's
   `unixsock` module is ungated and has always been compiled in. With step 1 in
   place the first `fork` of a process holding a pair took the machine down with
   exactly that message. That panic is well-designed: it named the hook, the
   file and the reason, and it is the difference between five minutes and a
   session. Both now point at `akuma_syscalls_glue::unixsock`.
3. **AF_UNIX before the native stack, in `recv`/`send`.** Rust's spawn channel
   is a `SOCK_SEQPACKET` pair, so it `recv`s rather than `read`s — and
   `crate::sock::sys_recvfrom` knows only `FileDescriptor::Socket`, an index
   into the smoltcp table. It answered `ENOTSOCK` for a descriptor
   `socketpair(2)` had returned three syscalls earlier, which Rust reported as
   `the CLOEXEC pipe failed: Not a socket`. The `Sendto`/`Recvfrom` arms now
   test the family first and hop to glue (whose own dispatchers have had that
   shape all along); `crate::sock` stays the smoltcp implementation rather than
   growing a second family.

Guarded by 14 boot checks (`usermode::socketpair_smoke_test`): the row decodes,
a pair is created, bytes cross in **both** directions — the endpoints are not
symmetric in the implementation and a crossed pair passes a one-way test — both
ends close through the newly-wired hooks, a closed endpoint is `EBADF`, teardown
leaks no frames, and `AF_INET` is `EAFNOSUPPORT` rather than `ENOSYS`. 696/0.

### What it bought

```
# rustc -o /tmp/hello2 /tmp/hello.rs        (no -C linker, PATH carrying /usr/bin)
# /tmp/hello2
ssh late.sh from Akuma akuma 0.0.7 83a3752f-release-smp-shared x86_64
```

**Plain `rustc <file>` now links**, through `cc` → `collect2` → `ld`, in ~4 s.
The program is `userspace/amd64/selfhost/hello.rs`, which makes a raw
`uname(2)` and unpacks `struct utsname` by hand — `std` has no `uname` and
pulling in the `libc` crate would need a registry and a network, and doing it
raw tests something a wrapper would hide: that a Rust `std` program, with
musl's start-up and TLS behind it, can issue an arbitrary syscall here and
unpack a `repr(C)` struct the kernel filled.

**One known divergence remains on this path.** A *failed* exec reports the
child's exit status rather than its errno: `Command::spawn` of an absent
program answers `Ok(status 1)` where Linux answers `Err(ENOENT)`. Rust's child
writes the errno into the pair and `_exit(1)`s, and the parent reads EOF
instead of those bytes — so the bytes are being lost when the write end closes.
That is the "EOF-on-success" approximation `sys_socketpair`'s own doc comment
warns about, seen from the other side, and it is a **pipe teardown** question
(does a pipe with buffered data and a closed write end still deliver it?)
rather than a socketpair one. It costs a misleading error message, not a
failure to link.

### The probe: `userspace/forktest/c_stress/execenv.c`

Written for this, and it is what separated the three candidate causes. It
re-execs itself as its own child so that "what a child receives" is written
down in one place. Measured in the guest:

| # | case | result |
|---|---|---|
| 0 | what the probe itself received | `envc=2`, **`PATH` unset** — the empty-environment gap |
| 1 | `execve` carries `envp` | **works**, all 3 entries |
| 2 | `posix_spawn` carries `envp` | **works**, both entries |
| 3 | `posix_spawnp` searches the *passed* `PATH` | `ENOENT` — **correct**, POSIX says it uses the caller's |
| 4 | `posix_spawnp("cc")` with rustc's `PATH` | **finds and runs cc** — so musl's spawn is not the fault |
| 5 | `fork` + `execvp` of an absent name | `ENOENT` — correct |
| 6/7 | `waitpid` across `alarm(1)`, with and without `SA_RESTART` | **`alarms=0`** — the signal never fired at all |
| 8 | a spawned child inherits the spawner's cwd | **works** (`/tmp`) — open item 4 below is **stale** |
| 9 | fork + install `envp` over `environ` + `execvp` (Rust `std`'s shape in C) | `ENOENT` — correct |

Two things fall out of that table beyond the root cause:

* **Open item 4 is fixed.** Spawn children do inherit cwd; the note below
  predates it.
* **`alarm(2)` does not deliver `SIGALRM`** (cases 6 and 7, both arms, handler
  installed, three-second child). No `EINTR` either way, so the two arms cannot
  be told apart — and `collect2`'s `cannot get program status: Interrupted
  system call` is the *other* half of the same subject: an `EINTR` arriving
  where none should, while a real signal arrives nowhere. Worth one probe of
  its own.

### Staging the toolchain

**Workarounds today**, in order of preference:

* `-C linker-flavor=ld -C linker=<sysroot>/lib/rustlib/x86_64-unknown-linux-musl/bin/gcc-ld/ld.lld -C link-self-contained=yes`
  — the toolchain's own linker, no `cc` and no `PATH` involved. This is the
  combination that compiled, linked and ran a program.
* `-C linker=/usr/bin/cc` plus a `PATH` carrying `/usr/bin` and gcc's exec
  prefix, which then meets items 4 and 5.

### Staging the toolchain

Session 2 used `apk add rust cargo` (Alpine 1.96.1) onto the persistent root.
Session 3 staged **nightly** into the Firecracker root image instead, because
the kernel's own manifest needs a nightly cargo to parse
(`cargo-features = ["panic-immediate-abort"]`) and Alpine ships stable —
the same conclusion the AArch64 side reached
(`docs/archive/AKUMA_SELF_HOSTING.md`). Procedure:
[`docs/runbooks/stage-rust-toolchain-amd64.md`](../runbooks/stage-rust-toolchain-amd64.md).

Three things worth carrying:

* The toolchain must be the **musl host** build
  (`rustup toolchain install nightly-x86_64-unknown-linux-musl --profile minimal
  --force-non-host`); `--force-non-host` is needed because the box's own rustup
  host is gnu. 777 MiB, 161 files, no symlinks.
* `rustc` is dynamically linked against `/lib/ld-musl-x86_64.so.1` (already on
  the image, from `apk add musl`) and finds `librustc_driver-*.so` through
  `DT_RUNPATH` `$ORIGIN/../lib`. **musl expands `$ORIGIN` by reading
  `/proc/self/exe`**, and this target has no procfs, so every invocation needs
  `LD_LIBRARY_PATH=/usr/local/rust/lib`. That is a real gap, not a nuisance: any
  binary that relies on `$ORIGIN` will behave the same way.
* There is no `cc` on this image, so linking goes through the toolchain's own
  `rust-lld` with `-C linker-flavor=ld -C link-self-contained=yes`. Session 2's
  `apk add gcc musl-dev binutils` route also works and is what `collect2` needs.

## Background

- `docs/archive/RUST_TOOLCHAIN_ISSUES.md` — the AArch64 toolchain
  investigation (`n` works, `cargo --version` SIGILL — since fixed).
- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the trunk plan; box D is where
  this session lands, and its "first in-guest build" proposal is what these
  gaps block.
- `docs/archive/AKUMA_AMD64_STREAMLINING.md` § 4b — the last time a busybox
  binary's `/proc` expectations drove an amd64 fix (`ps`).
