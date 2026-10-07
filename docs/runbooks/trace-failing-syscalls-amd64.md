# Find which syscall failed, in which process, on which path (amd64)

For a big userspace program on amd64 that fails with an errno and no
context ("No such file or directory (2)", "Failed to create ..."), where you
need the **one** failing call out of a hundred thousand. Built for Chromium
(2026-10-08), where it found all three remaining startup blockers in two runs.

## 1. Boot with `strace_err`

It's a kernel command-line flag, read at `run_init`:

```sh
# Firecracker rig (userspace/kami/probe/akuma/run-fc.sh):
KARGS=strace_err sh run-fc.sh
# local QEMU:
KCMDLINE=strace_err sh amd64/run.sh
# metal (GRUB line):
multiboot2 /boot/akuma/akuma-amd64 init=/bin/herd strace_err
```

Every syscall that returns `-4095..=-1` prints **one** line, written with a
single `tprint!` so SMP cannot shred it:

```
[T7.04] [sc!] pid=28 task=11 nr=262 a1=0xb a2=0x104e9855 a3=0x7fffffffe060 -> -2 "self/task/"
```

- `nr` is the **x86_64** number, and the errno after `->` is **decimal**.
- Path arguments are decoded for the path-taking calls (`trace_paths` in
  `amd64/src/usermode.rs`: `open`, `stat`, `execve`, `mkdir`, `unlink`,
  `symlink`, `readlink`, every `*at`, `rename*`, `statx`, …). For an `*at`
  call the dirfd is `a1`.
- `EAGAIN`, `EINTR` and `ETIMEDOUT` are left out; a polling program returns
  them all day.

## 2. Summarise

```sh
grep -a '\[sc!\]' kami-fc.log | sed -E 's/^\[T[0-9.]+\] //; s/ a1=0x[0-9a-f]+ a2=0x[0-9a-f]+ a3=0x[0-9a-f]+//' \
  | awk '{$2=""; $3=""; print}' | sort | uniq -c | sort -rn | head -50
```

Read the top of the list for **repetition** and the tail for **one-offs**:

- **One call failing many times with the same argument** points at its input,
  not at the call. Chromium's `mkdir("/tmp/.org.chromium.Chromium.scoped_dir.EAAIAA")`
  failed `EEXIST` 100 times: musl's `mkdtemp` seeds from `CLOCK_REALTIME`, and
  the clock was frozen at 0.
- **An odd path** usually names the bug outright: `execve("chrome_crashpad_handler")`
  is relative where Linux has an absolute path.
- **An errno Linux never returns for that call** (e.g. `brk -> -38`) is a
  dispatch gap. See step 4.

## 3. When the cause is a call that *succeeded* with the wrong answer

The errors-only trace can't see a call that succeeded with a wrong answer.
Add `strace_nr=<n,n,...>` to also print every call of those numbers, failed
or not. `readlink`/`readlinkat` (89, 267) print the target they returned:

```sh
KARGS="strace_err strace_nr=89,267" sh run-fc.sh
grep -a 'nr=89 ' kami-fc.log | grep '/proc/self/exe'
#   ... -> 26 "/proc/self/exe" = "/usr/lib/chromium/chromium"
#   ... -> 8  "/proc/self/exe" = "chromium"      <- same pid, later: the bug
```

That pair is how `prctl(PR_SET_NAME)` was found rewriting `/proc/<pid>/exe`.

## 4. Check the dispatch itself

A row in `akuma-syscalls-abi` whose `Syscall` variant has no arm in
`usermode.rs`'s match falls into the default. Since 2026-10-08 that default
prints, so look for:

```
[syscall] x86_64 nr=12 decodes to Brk but has no dispatch arm — returning ENOSYS
[syscall] no row for x86_64 nr=40 — returning ENOSYS (add it to akuma-syscalls-abi's table)
```

The first means a missing arm (fix in `amd64/src/usermode.rs`), the second a
missing row (fix in `crates/akuma-syscalls-abi`). To audit every row against
the arms at once, from the repo root:

```sh
sed -n '/syscall_table! *{/,/^}/p' crates/akuma-syscalls-abi/src/lib.rs \
  | grep -oE '^\s*[A-Z][A-Za-z0-9]+\s*=>' | awk '{print $1}' \
  | while read v; do grep -q "Syscall::$v\b" amd64/src/usermode.rs || echo "NO ARM: $v"; done
```

## 5. Correlate with the kernel's own log

`dmesg` in the guest reads the same ring as the serial log, so the `[sc!]`
lines, `[Fault]`, `no row` and OOM lines all land in it. The Chromium smoke
saves it to `/tmp/dmesg.txt` and `run-fc.sh` dumps it to `out/dmesg.txt`.

## The heavier tools, and when they are worth it

- **`strace`** (command-line flag) prints every syscall with an entry line and
  a result line. That's useful for a short program and far too much for
  Chromium (~100 k calls). The entry line now decodes the same paths as
  `[sc!]`.
- **`--features syscall-debug-info`** (compile-time, `amd64/Cargo.toml`)
  forwards the AArch64 kernel's debug mode: glue's per-arm `[syscall]` lines.
  Its `[FORK-DBG]`/`[TRAMP]` lifecycle traces print **nothing** on amd64,
  whose fork and exec are its own. In the Chromium run it added 244 lines and
  found nothing `strace_err` didn't. Leave it off unless you are debugging
  inside a glue arm.

## Verify

- `KARGS=strace_err` boots, and a deliberately failing call shows up: `ls
  /nonexistent` in the guest prints an `[sc!]` line naming `"/nonexistent"`
  with `-> -2`.
- `KARGS="strace_err strace_nr=89"` on a run of `userspace/forktest/c_stress/exeprobe`
  prints `= "/exeprobe"` for each of its `readlink("/proc/self/exe")` calls.

## Background

- `docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md`: the run this was built
  for, and each fix it led to.
- `docs/reference/subsystems/config-flags.md` § Tracing: the compile-time knobs.
