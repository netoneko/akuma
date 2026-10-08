# Handoff prompt: headless Chromium (kami) on amd64 — round 2

Paste everything below the line into a fresh session started in this repo, on
the `kami` branch.

---

You're working in the Akuma kernel repo (`~/github.com/netoneko/akuma`), on the
`kami` branch, at or after `b3dfd5b3`. **Goal:** Alpine's `chromium` (142, musl)
renders a page headless on the amd64 kernel. Get it working under Firecracker
on the trashcan first, then on the metal. Each step so far was a kernel gap
that Chromium hit in order. Keep closing them one at a time, prove each one,
and say exactly where you stopped.

## Rules (from `CLAUDE.md` and the user, they override everything below)

- Justify every allocation; console output only through
  `safe_print!`/`tprint!` (never open-code a `StackWriter`).
- No fork/multi-agent fan-out; never launch background agents without asking.
- **You may commit checkpoints and push them to `litter`** (the private
  `akuma-litter` remote) at `cats/claude/kami`. Never push to `origin`, never
  rewrite history. Commit verified work as you go, not at the end.
- Leave the user's untracked `proposals/` and
  `docs/handoff-kernel-rio-support.md` alone.
- Every behaviour change gets a probe that is run on **Linux first** (that run
  is the expectation), then on Akuma. Never claim what you didn't run.
- Write it all into the docs as you go (below).

## Read first

1. `docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md`: Fixes 1–16 and the
   **"Still open"** list this prompt is built from.
2. `docs/runbooks/trace-failing-syscalls-amd64.md`: the tracing that found
   Fixes 11–16. Use it before forming theories.
3. `userspace/kami/README.md` (status, and the two "Future" sections:
   reading mode / `.md` output, and the rio pane).
4. `docs/runbooks/amd64-bare-metal-loop.md` ("Two machines at once", and the
   `git bundle` note: `hpbox.deploy()` cannot reach a commit that exists only on
   `litter`).

## The rig (trashcan Ubuntu side, `192.168.1.120`, ssh port 22)

Drive it with `HPBOX_IP=192.168.1.120 python3 scripts/utils/hpbox.py ub '<cmd>'`.
`hpbox ub` gives up after **300 s** and its CLI exits 0 whatever the remote
command returned, so run long jobs detached (`nohup … &`) and poll a marker.
Everything is in `userspace/kami/probe/akuma/` and installed at
`/root/cdp-probe/akuma/`.

| step | where | command |
|---|---|---|
| build the static probes, copy probes and scripts to the box | laptop | `sh userspace/kami/probe/akuma/push.sh` |
| carry `litter`-only commits over | laptop | `git bundle create k.bundle HEAD ^<box head>`, `cat` it over ssh, on the box `git fetch k.bundle HEAD:refs/heads/laptop-head` |
| sync the tree and build the kernel | laptop | `hpbox.deploy()` (check its rc!) then `hpbox.build()` |
| all probes | box | `MEM=4096 sh run-fc.sh probes.sh bpprobe trapprobe spawnprobe singletonprobe snapprobe taskprobe credprobe chromeprobe` |
| one Chromium run, whole stderr and exit status | box | `KARGS=strace_err MEM=4096 sh run-fc.sh chrome-once.sh` |
| the older two-mode smoke run | box | `sh run-fc.sh` (`kami-smoke.sh`) |
| standard-image boot (expect 727 passed, 0 failed) | laptop | `hpbox.firecracker(vcpus=2, timeout_s=150)` |
| Linux control for a probe | box | `docker run --rm -v $PWD:/p akuma-cdp-probe /p/<probe>` |
| Linux `strace -f` of Chromium | box | `docker run … akuma-cdp-probe sh -c "strace -f -s 200 -o /out/x.strace /usr/lib/chromium/chromium --headless …"` (a full trace is at `/root/cdp-probe/linux/smoke.strace`) |

`KARGS` adds kernel command-line flags:
- `strace_err`: one `[sc!]` line per failing syscall (pid, x86_64 nr, decoded
  paths, decimal errno), plus a `[sig!]` line for every fault signal delivered
  to a program's own handler;
- `strace_nr=89,267`: also successful calls of those numbers (`readlink`
  shows its target);
- `strace_pid=<n>`: the full trace for one thread group. Use with `VCPUS=1`.

Pids depend on the script: in `chrome-once.sh` the browser is **pid 15**.

The Chromium image's boot prints `self-test: 615 passed, 41 FAILED`. That's
expected: the fixtures are missing from that image. Compare the count across
runs, not against zero.

## Where it stands (2026-10-08)

The browser runs about 10 s and gets through the singleton, crashpad, zygote
and snapshot setup. It launches children through the zygote, and then:

```
ERROR:content/common/zygote/zygote_communication_linux.cc:160] NOTREACHED hit. Did not receive ping from zygote child
ERROR:content/zygote/zygote_linux.cc:633] Zygote could not fork: process_type utility numfds 5 child_pid -1
ERROR:…/scoped_ptrace_attach.cc:27] ptrace: Function not implemented (38)
ERROR:…/exception_handler_server.cc:143] tgkill: No such process (3)
… GPU process launch failed: error_code=1002  (x6)
FATAL:content/browser/gpu/gpu_data_manager_impl_private.cc:415] GPU process isn't usable. Goodbye.
== chromium exit status 191
```

`SCM_CREDENTIALS` (Fix 16) is in and `credprobe` passes, so the ping path
itself works in isolation. The last run, with the `[sig!]` logging, logged
**no** caught fault and no `[Fault]`. So the zygote child isn't dying of a
fault signal. Suspects, in order:

1. **What the zygote child actually does before dying.** Find its pid from
   the `[sc!]` lines (the zygote is `--type=zygote`; its children are forked
   from it), then rerun with `strace_pid=<child>` and `VCPUS=1`. Compare
   against the Linux strace of the same fork (`grep` for `kZygoteChildPing`
   sizes, the `sendmsg` from the zygote child). Check its exit status, e.g. by
   tracing `exit_group`'s argument.
2. **`gettid()` of a main thread is its thread slot, not its pid** (both
   kernels, by design: `tkill`, futexes and the per-thread arrays index by
   slot). Every log prefix reads `[<pid>:<small>:`. Chromium's zygote and
   sandbox code compare `gettid()` with `getpid()` to mean "single-threaded /
   main thread". This is a big cross-kernel change, so prove it is the
   cause with a probe before touching it.
3. The fork path the zygote uses (`fork` vs `clone` flags,
   `--change-stack-guard-on-fork=enable`). `strace_nr=56,57,58,435` shows
   them.

## Also open (do them when they block, or when cheap)

- Missing x86_64 rows: 40 `sendfile`, 239 `get_mempolicy`, 297
  `rt_tgsigqueueinfo` (crashpad re-raises a crash with it; then `exit 191`),
  444 `landlock_create_ruleset`; `inotify_init`. Decide each one's honest
  answer, then row plus arm in `akuma-syscalls-abi` and `usermode.rs`. A
  decoded row without an arm now prints `no dispatch arm`, and the audit
  one-liner is in the runbook.
- `ptrace` for crashpad dumps (only matters after a crash).
- `/proc/cpuinfo`, `/proc/sys/fs/inotify/max_user_watches`,
  `/proc/<pid>/oom_score_adj`, `/sys/devices/system/cpu/{possible,present}`
  (logged as `ERROR`, not fatal so far).
- `O_CREAT` ignores the umask (`fs::UMASK` is applied for `mkdir` only).
- `init=` does not follow symlinks, and ext2's `read_at` on a symlink inode
  reads the target's bytes as block numbers.
- The 1324 GiB reservation's `munmap` costs about 290 ms (Linux: 0.5 ms).

## Verify, every kernel change

- clippy and host tests for each crate you touch (`CLAUDE.md` § Testing);
- `cargo check` of both kernels (`--release` for AArch64;
  `-p akuma-amd64 --target x86_64-unknown-none --release`);
- the probe pass above, all green, `chromeprobe` 16/16;
- the standard-image boot: `727 passed, 0 failed`;
- then the Chromium run, which shows the next error.

Success is `/tmp/shot.png` (dumped to `out/`) reading "JavaScript ran: 6 x 7 =
42". After that: run `kami` itself on the trashcan's metal (it needs
`/dev/fb0`; `userspace/kami/build.sh`; the default page needs networking).
Metal only when the user asks (`hpbox.stage()`, `hpbox.reboot_to("akuma")`).

## When you stop

- In `docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md`: add a numbered
  `## Fix N` per closed item and correct "Still open".
- Update `userspace/kami/README.md`'s status paragraph.
- Add symptom rows to `docs/README.md` and a reference-doc section for any
  new subsystem behaviour.
- For landed fixes, follow `docs/runbooks/update-bug-fix-list.md` (not yet
  done for Fixes 8–16).
- Rewrite this file for the next round.
