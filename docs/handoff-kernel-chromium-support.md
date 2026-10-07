# Handoff prompt: kernel support for headless Chromium (kami) on amd64

Paste everything below the line into a fresh session started in this repo, on
the `kami` branch.

---

You are working in the Akuma kernel repo (`~/github.com/netoneko/akuma`), on
the `kami` branch. The goal of this session: **Alpine's `chromium` (142, musl)
renders a page on the amd64 kernel**. First under Firecracker on the trashcan,
then on the metal. Each blocker below is a kernel gap that Chromium hits in
order. Close them one at a time, prove each one, and say exactly where you
stopped.

## Read first, in this order

1. `CLAUDE.md`, whose rules override anything below. In particular:
   - "Kernel conventions": justify every allocation; console output only
     through `safe_print!`/`tprint!`.
   - "Working with Claude Code in this repo": **the user drives all commits**.
     Commit only when asked, never push to `origin`, no fork/multi-agent
     fan-out, never launch background agents without asking.
   - `litter` (the private `akuma-litter` remote) is where the user lets you
     push, to `cats/claude/<topic>`.
2. `docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md`: what was fixed, the
   **"Still open"** list this prompt is built from, and § "The whole-file
   heap" for blocker 1.
3. `userspace/kami/README.md`: what kami is, the Ubuntu measurements, the
   `strace` summary of what Chromium asks of the kernel (processes, fd
   passing, shared memory, huge reservations), and the future rio-pane plan.
4. `docs/runbooks/amd64-bare-metal-loop.md`. Especially "Two machines at once"
   (fast lane before metal), and the 2026-10-07 note under "Getting the source
   onto the box": `hpbox.deploy()` cannot reach a commit that exists only on
   `litter`, so carry commits over with `git bundle`.
5. `docs/reference/subsystems/syscalls/net.md` § "SCM_RIGHTS" and
   `docs/reference/subsystems/amd64-shared-write-mmap.md`: the fd-passing and
   shared-memory paths Chromium's IPC runs on. Both landed 2026-10-08.

## The rig (in the repo; already set up on the trashcan)

All of it lives in `userspace/kami/probe/akuma/`; read its scripts' headers.
It runs on the **trashcan's Ubuntu side**, at `192.168.1.120`. Drive it with
`HPBOX_IP=192.168.1.120 python3 scripts/utils/hpbox.py ub '<cmd>'`, ssh port
22; port 2222 is the Akuma personality. It is already installed at
`/root/cdp-probe/akuma/`, with `kami-root.img` built.

| step | where | command |
|---|---|---|
| build and copy the static probes and the scripts to the box | laptop | `sh userspace/kami/probe/akuma/push.sh` |
| (re)build the Chromium image from `../Dockerfile` | box | `cd /root/cdp-probe/akuma && sh mkimg.sh` |
| sync and build the kernel | laptop | `hpbox.deploy()` then `hpbox.build()`; see the `git bundle` note in the runbook |
| one Chromium run under Firecracker | box | `cd /root/cdp-probe/akuma && sh run-fc.sh` |

- **What `run-fc.sh` does:**
  1. boots a **copy** of `kami-root.img` (`kami-run.img`), with
     `kami-smoke.sh` and `exeprobe` written into its root;
  2. runs `init=/bin/busybox initargs=sh,/kami-smoke.sh`. **Not**
     `init=/bin/sh`: `init=` does not follow symlinks, one of the open items;
  3. prints the interesting log lines;
  4. dumps any `/tmp/shot*.png` into `out/`. The full log is `kami-fc.log`.

  Defaults: a 10240 MiB guest, so it gets the 1 GiB kernel heap until
  blocker 1 is fixed; 2 vCPUs; a 400 s bound. Override with
  `MEM=`/`VCPUS=`/`TIMEOUT=`/`KERNEL=`. To run a different script or probe,
  pass it: `sh run-fc.sh myprobe.sh shmvar chromeprobe`. Extra files land in
  the image root.
- **Expected noise.** The boot prints `self-test: 615 passed, 41 FAILED` on
  this image. The kernel's fs/fd self-tests look for fixtures that only the
  standard `amd64-root.img` has. Compare that count across runs, not against
  zero.
- **Success** is `== got out/shot.png` with the page text "JavaScript ran:
  6 x 7 = 42". `kami-smoke.sh` runs Chromium with and without `--no-zygote`.
- **The state at handoff** (2026-10-08, the code of `b636f410`; its `uname` reads `78b5edb0`, the commit it was patched onto): the run reproduces
  exactly the four blockers below, in order. `exeprobe` prints `/exeprobe`
  on both sides of the re-exec.
- **Linux control.** Run every probe on the trashcan's Ubuntu first:
  directly (`push.sh` leaves `chromeprobe`, `shmvar` and `exeprobe` in
  `/root/cdp-probe/akuma/`), or in the `akuma-cdp-probe` container. That run
  is the expected output. For the call sequence a blocker sits in,
  `userspace/kami/probe/cdp.py strace URL` (in the container) gives a full
  `strace -f` of Chromium on Linux, and `probe/analyze.py` summarises it.
- **Probes** are in `userspace/forktest/c_stress/`:
  - `chromeprobe.c`: fd passing, shared files, reservations. 16/16 on Linux
    and Akuma; keep it that way.
  - `shmvar.c`: five shared-mapping shapes.
  - `exeprobe.c`: `/proc/self/exe` across a re-exec.

  Add new probes beside them and to `push.sh`'s list. They are built
  `x86_64-linux-musl-gcc -static`.

## The blockers, in the order Chromium hits them

1. **`execve` copies the whole executable into the kernel heap**
   (`amd64/src/fs.rs` `read_image`, capped at 256 MB). Chromium is 250 MB and
   re-execs itself (`execve("/proc/self/exe")`) for the zygote and the
   utility processes. On a 512 MiB heap the second copy fails
   (`[ALLOC FAIL] requested=249690856`), and the exec returns `EIO` after
   about 20 s.
   **Fix:** a streaming loader. Parse the ELF and program headers from a
   small read, then copy each `PT_LOAD` segment straight from the file into
   its freshly mapped pages in bounded chunks, with no whole-file buffer.
   Keep the eager mapping for now; lazy file-backed text is a later step.
   **Prove it:**
   - a 4096 MiB guest gets past every Chromium exec;
   - the `Slab:` line of `/proc/meminfo` (the kernel heap) does not jump by
     the binary's size around an exec;
   - the amd64 boot suite and `chromeprobe` stay green.
2. **crashpad: `posix_spawn chrome_crashpad_handler: No such file or
   directory`.** The file exists. musl's `posix_spawn` is a
   `clone(CLONE_VM|CLONE_VFORK)` child that `execve`s, and reports errno back
   through a pipe. Write a probe that `posix_spawn`s
   `/usr/lib/chromium/chrome_crashpad_handler --help` and run it on Linux,
   then on Akuma. Find out whether the failure is the path Chromium computes,
   or vfork-plus-exec in a `CLONE_VM` child that is not a thread.
3. **`Failed to create socket directory`** (Chromium's ProcessSingleton,
   seen with `--no-zygote`). That is `mkdtemp` under `/tmp`, followed by a
   socket `bind` inside the new directory and symlinks next to the profile.
   Probe each step on its own.
4. **Zygote children: `FATAL: Error loading V8 startup snapshot file`.** The
   browser opens `v8_context_snapshot.bin` and hands the fd down. In zygote
   mode it reaches the child through the zygote's fork request (descriptors
   sent with `SCM_RIGHTS`, then remapped to fixed numbers). Find which step
   loses it: the passing, the `dup2` remap, or an `mmap`/`read` of the
   passed fd. Linux `strace` of the same flow shows the expected sequence.

Then whatever comes next. Repeat until the screenshot is right.

## Smaller items on the same path (do them when they block, or when cheap)

- **`int3` from ring 3 arrives as SIGSEGV.** The `#GP` has `err=0x1a`, which
  means IDT vector 3's gate is not DPL 3. Every Chromium `CHECK` failure looks
  like a segfault. Make `#BP` reachable from user mode and deliver SIGTRAP.
  It is worth doing early, because it makes every later crash legible.
- **`init=` does not follow symlinks**, and ext2's `read_at` on a symlink
  inode reads the target's bytes as block numbers
  (`read_sectors: sector 14819201400`, which is "/bin" read as a block).
  ext2 should refuse; `init=` should resolve.
- **Missing x86_64 syscall rows** (ENOSYS, non-fatal so far): 40 `sendfile`,
  141 `setpriority`, 239 `get_mempolicy`, 297 `rt_tgsigqueueinfo`, 444
  `landlock_create_ruleset`. Add them to `akuma-syscalls-abi`'s table with the
  honest answer for each. That may be `EPERM`/`ENOSYS` on purpose, but it
  should be decided, not defaulted.
- **The 1324 GiB reservation** works, but costs about 290 ms (Linux: 0.5 ms),
  nearly all of it in `munmap`. Look at it if startup time matters.
- **`SO_PASSCRED`** is accepted but no `SCM_CREDENTIALS` are generated. If
  crashpad's handler registration needs them, implement them on the receive
  side (`akuma_net_unix::scm` has the encoder shape).

## How to verify (do not claim anything you did not run)

- **Every kernel change:**
  - clippy and host tests per `CLAUDE.md` for each crate you touch;
  - `cargo check` of both kernels (`--release` for AArch64,
    `-p akuma-amd64 --target x86_64-unknown-none --release`);
  - one Firecracker boot showing `Akuma/amd64 — all self-tests passed`;
  - `chromeprobe` still 16/16.
- **Every blocker:** a minimal probe that fails before the fix and passes
  after, with the Linux output as the expectation. Then the Chromium smoke
  run, which shows the next error.
- **Metal last.** Only after the fast lane is green, and only when the user
  asks: `hpbox.stage()` then `hpbox.reboot_to("akuma")` on the trashcan. The
  Ryzen laptop has its own loop (`overlays/ryzen/README.md`; rehearse with
  `qemu.sh` before every arm). On Akuma, `apk add chromium font-noto` gives
  the browser.
- When Chromium renders under Firecracker, run `kami` itself. It needs
  `/dev/fb0`, which Firecracker does not have, so that step is for the
  trashcan's metal. Static binary: `userspace/kami/build.sh`. The default
  page is tumblr.com, so it needs networking.

## When you stop

Update `docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md`:
- move each closed item from "Still open" into a numbered `## Fix N` section;
- add each new finding to "Still open".

Update the status paragraph in `userspace/kami/README.md`. If you landed fixes,
follow `docs/runbooks/update-bug-fix-list.md`. Leave the work uncommitted
unless the user asks for commits.
