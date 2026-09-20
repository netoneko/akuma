# amd64: a ring-3 `#UD` stopped being a whole-machine kill, `fchdir` was two gaps deep, and `execve` learned `#!`

**Date:** 2026-09-20
**Scope:** `amd64/src/idt.rs` (vector 6), `amd64/src/usermode.rs` (`sys_execve`/`sys_fchdir`
dispatch), `crates/akuma-syscalls-abi`.
**Status:** `#UD` containment, `fchdir`, and `execve` shebang support all fixed
and verified live. The workload that found the first two — the Alpine `apk`
package of `llama-server` — never got past the same `#UD`, contained or not;
a **vendored, compile-time-dispatched build** of the same program does serve
real HTTP traffic on this target. See §4.

## 1. How this was found

The trashcan's bare-metal Akuma wedged solid running `llama-server` (freshly
`apk add`-ed, never exercised on this kernel) and needed a physical power
cycle to recover. Per this repo's rule of proving anything new on a
recoverable rig before bare metal, the same reproduction was run under
Firecracker (4 vCPU, `docs/reference/firecracker-amd64/`) on the trashcan's
Ubuntu side instead — and it wedged there too, identically: after
`llama-server --help` the guest stopped answering ssh permanently, with the
last console line being

```
[EXCEPTION] #UD invalid opcode
  rip=0x0000000124d3f1fd rsp=0x00007fffffff6040
  cs=0x0000000000000023 rflags=0x0000000000010246
  cr2=0x0000000125da2f88 cr3=0x00000000209bb000 freed=0
```

That answers the "is it bare metal" question on its own: the crash is not a
driver or hardware-specific problem, it is a ring-3 `#UD` — an illegal
instruction, almost certainly OpenBLAS's runtime CPU-feature dispatch picking
a SIMD kernel the vCPU does not actually expose — and **every** ring-3 `#UD` on
this kernel took the whole guest down with it, on real hardware or under a VMM.

## 2. Root cause: vector 6 never got the `#PF`/`#GP`/`#DB` treatment

`AKUMA_AMD64_FAULT_SIGNALS.md` (2026-09-11) gave `#PF`, `#GP` and the LAPIC
tick a path from a ring-3 fault to a delivered `SIGSEGV` (or a clean kill if no
handler is installed), and `idt.rs`'s own history added `#DB` the same way
later (a stray `TF` bit was killing the machine — see that vector's doc
comment in `idt.rs`). Vector 6 (`#UD`) was never moved onto that pattern: it
was still the generated `extern "x86-interrupt" fn invalid_opcode` entry, which
can only call `fatal()` — there is no way to recover the return address or read
the general-purpose register file from that calling convention, so a ring-3
"illegal instruction" and a ring-0 kernel bug got exactly the same answer:
halt the machine.

## 3. Fix

Same shape `#DB` already uses (no error code pushed by hardware, unlike
`#GP`/`#PF`): a hand-assembled `global_asm!` stub (`invalid_opcode_entry`) that
saves the full `TrapRegs`, and an `extern "C" fn invalid_opcode_dispatch` that:

- **ring 3:** calls `signal::deliver_fault_signal(f, regs, SIGILL, SI_KERNEL, 0)`;
  delivered → return. Not delivered (no handler installed) → `user_fault(...)`,
  the same "kill this process, not the kernel" path `#GP`/`#DB` use.
- **ring 0:** unchanged — falls through to `fatal()`. A kernel `#UD` is not a
  stray flag bit like `#DB`'s story; the instruction bytes really are bad, so
  there is no "disarm and continue" here.

`amd64/src/idt.rs`'s `init()` now points IDT vector 6 at `invalid_opcode_entry`
instead of the old `x86-interrupt` fn, which is deleted.

### `fchdir`: the table row was necessary but not sufficient

`apk add`'s busybox/llama.cpp post-install hooks were separately dying with
`fchdir: Function not implemented` (`ENOSYS`) — unrelated to the `#UD`, found
investigating the same `apk` session. The glue implementation
(`akuma_syscalls_glue::fs::sys_fchdir`) already existed and was already wired
into glue's dispatch table; the gap was entirely upstream of it, and it was
**two** gaps, not one:

1. `crates/akuma-syscalls-abi`'s `syscall_table!` had no `Fchdir` row, so
   `Syscall::from_x86_64(81)` returned `None` and the call never decoded — the
   same shape [`Self::Chdir`]'s own row comment describes from 2026-09-12.
2. Adding the row alone was **not enough**. `amd64/src/usermode.rs`'s
   `syscall_dispatch` is not a generic forward to glue for every decodable
   `Syscall` — it is a big `match` with one arm per syscall, and anything
   without its own arm falls into the catch-all `_ => errno::ENOSYS`. `Chdir`
   has an arm; `Fchdir` did not. A raw-syscall test binary confirmed the two
   layers separately: after the table-only fix, `fchdir()` still returned
   `-38`; after adding `Syscall::Fchdir => to_glue(call, [a1, 0, 0, 0, 0, 0])`
   next to `Chdir`'s arm, it returned `0`.

### `execve` shebang support

docs/README.md's symptom matrix already had this one, unfixed: "`execve` of a
`#!` script fails with `Exec format error` … `amd64/src/usermode.rs::sys_execve`
has no shebang handling at all." Fixed the same session, since `apk fix`'s
`busybox` post-install hook needed it right after `fchdir` started working.
`sys_execve` is split the same way AArch64's `akuma-syscalls-glue::proc`
already is: an outer function that only turns user pointers into owned
`path`/`argv`/`envp`, and an inner `do_execve(slot, path, argv, envp)` that
reads the image, checks for `#!`, and — on a match — parses the interpreter
line with `akuma_exec::process::parse_shebang`, resolves it with
`akuma_vfs_glue::resolve_symlinks`, rebuilds argv with
`akuma_exec::process::shebang_hop`, and **recurses into itself** with the
interpreter's path. Same parser, same argv-construction rule as the AArch64
side (`akuma-exec`'s host-tested `shebang_tests`), so the two cannot drift on
what argv a `#!` line produces.

## 4. Verification, and what is still open

- `cargo build -p akuma-amd64 --target x86_64-unknown-none --release` and
  `cargo build --release` (AArch64) both clean; `cargo clippy` clean for
  `akuma-amd64`; `cargo test -p akuma-syscalls-abi` 18/18, including the new
  `Fchdir` spot-check in `cwd_and_mode_rows`.
- Boot suite unaffected: 731/0 on the box, same as before the fix (the count
  itself is lower than a clean-disk boot's 775 because the test disk still
  carries a partially-broken `apk` state from before `fchdir` worked — see
  below — not a regression from these two fixes).
- **`#UD` containment, live:** the exact same `llama-server` binary that
  wedged the whole guest before the fix now dies with a plain
  `Segmentation fault`, exit 139, and the guest stays reachable —
  reproduced three times total (two after the fix, on different cores/threads)
  with the kernel's own diagnostic in the log each time:
  ```
  [Fault] #UD invalid opcode (ring 3, no handler) in ring 3 on cpu 2 rip=0x0000000124d461fd ...
  ```
- **`fchdir`, live:** a minimal raw-syscall test binary (`open(".")` then
  `fchdir(fd)`) went from `fchdir(fd=3) -> -38` to `fchdir(fd=3) -> 0` across
  the two-part fix.
- **`execve` shebang, live:** a raw-syscall test binary calling
  `execve("/tmp/test.sh", …)` directly (not through a shell, which would mask
  the kernel path by interpreting the `#!` itself) on a script whose
  interpreter line was `#!/bin/busybox cat` printed the script's own
  contents — the interpreter loaded and ran as expected, where before this fix
  the same call returned `ENOEXEC`. With both this and `fchdir` live, `apk
  fix`'s busybox post-install hook runs to completion (`OK: 88.8 MiB in 16
  packages`, exit 0) instead of erroring out on either call. It does not
  fully repair *this one test guest's* `/bin/ls`, `/bin/uname` etc., because
  their symlinks were corrupted by the *original*, pre-fix `apk add` run and
  the hook's own `--install -f` (force, i.e. unlink-then-symlink) hits a
  **separate, narrow, unexplored gap** — `busybox: -f/<applet>: Function not
  implemented` for every applet, while a plain `busybox --install -s
  <cleandir>` (no `-f`, no pre-existing file to unlink) works perfectly and
  produces fully functional symlinks. Not investigated further — out of this
  session's scope, and a fresh disk that never passed through the broken
  pre-`fchdir` state would never hit it.
- **The `apk`-packaged `llama-server` still does not serve traffic** — this is
  narrower than it first looked, and the correction is below.  With both
  fixes live, the Alpine package was launched against a real model
  (`bartowski/SmolLM2-135M-Instruct-GGUF`, Q8_0, 144 MiB) with
  `--host 0.0.0.0 --port 8080 -c 512 -t 2 --no-mmap`. The process hit the
  *same* `#UD` shortly after spawning its OpenBLAS thread pool
  (`[threads] new high-water: 13` immediately precedes the fault line, exactly
  as in the pre-fix crash) — contained this time, but the process was never
  observed to open port 8080, and `curl`/`nc` against it were refused the
  whole time. A secondary, smaller oddity: the killed PID stayed listed in
  `ps` (and survived `kill -9`) after the group-kill — `psstats`/`ps`
  bookkeeping for a fault-killed multi-threaded process looks incomplete, not
  investigated further here.
- **Correction, same session:** the OpenBLAS runtime-dispatch theory was
  right, and the fix is not "find the one bad instruction" — it's **don't
  link OpenBLAS at all**. `llama.cpp`'s own GGML CPU backend picks its SIMD
  kernel at *compile time* from CMake flags (`-DGGML_BLAS=OFF` plus every
  instruction-set flag explicit rather than left at its surprising
  `GGML_NATIVE=OFF` default — see `userspace/llama.cpp/docs/AMD64_BUILD.md`
  for why the default is *not* what it sounds like). Built that way with a
  musl.cc cross toolchain and `-march=x86-64` (baseline, no runtime CPUID
  guess to get wrong), the vendored `llama-server` loaded the same model,
  answered `/health`, and served a real `/v1/chat/completions` request over
  HTTP — repeatedly, not a one-shot fluke. So: **`llama-server` on
  amd64/Firecracker works**; what specifically does not is the *Alpine
  package's* OpenBLAS-linked build, and the crash-containment fix in §2-3 is
  still worth having regardless — it is what stops any future OpenBLAS-style
  runtime-dispatch mistake from taking the whole kernel down with it.

## Background

- [`AKUMA_AMD64_FAULT_SIGNALS.md`](AKUMA_AMD64_FAULT_SIGNALS.md) — the
  `#PF`/`#GP`/tick fault-to-`SIGSEGV` design this extends to `#UD`/`SIGILL`.
- [`AKUMA_NET_BIND_NO_ADDRINUSE.md`](AKUMA_NET_BIND_NO_ADDRINUSE.md) — the
  unrelated bug found the same week on the same Firecracker rig.
- [`docs/reference/firecracker-amd64/README.md`](../reference/firecracker-amd64/README.md)
  — the environment both reproductions ran on.
- `docs/runbooks/amd64-bare-metal-loop.md` — "grep the archive first"; the
  shebang/`execve` limitation this doc surfaces but does not fix is already
  known there in a different context (the self-hosted linker).
