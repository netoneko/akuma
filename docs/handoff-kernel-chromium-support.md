# Handoff prompt: headless Chromium (kami) on amd64 — round 3

Paste everything below the line into a fresh session started in this repo, on
the `kami` branch.

---

You're working in the Akuma kernel repo (`~/github.com/netoneko/akuma`), on the
`kami` branch. **Round 2 reached its goal on 2026-10-08:** Alpine's `chromium`
(142, musl) renders a page headless on the amd64 kernel under Firecracker on
the trashcan, and the screenshot reads "JavaScript ran: 6 x 7 = 42". Three
kernel gaps closed it (Fixes 17–19 in the record below). **This round's goal:**
`kami` itself — the screencast-to-`/dev/fb0` program in `userspace/kami` — on
the trashcan's **metal**, then the open list. Keep closing gaps one at a time,
prove each one, and say exactly where you stopped.

## Rules (from `CLAUDE.md` and the user, they override everything below)

- Justify every allocation; console output only through
  `safe_print!`/`tprint!` (never open-code a `StackWriter`).
- No fork/multi-agent fan-out; never launch background agents without asking.
- **You may commit checkpoints and push them to `litter`** (the private
  `akuma-litter` remote) at `cats/claude/kami`. Never push to `origin`, never
  rewrite history. Commit verified work as you go, not at the end.
- Leave the user's untracked `proposals/` and
  `docs/handoff-kernel-rio-support.md` alone.
- `docs/archive/BUG_FIX_LIST.md` is updated when the branch closes, not per
  fix; it is current through Fix 19.
- Every behaviour change gets a probe that is run on **Linux first** (that run
  is the expectation), then on Akuma. Never claim what you didn't run. Twice
  in round 2 the Linux run corrected the *probe's* expectation (`capget` with
  a NULL `data` answers 0, not `EINVAL`), which then corrected the kernel.
- Write it all into the docs as you go (below).

## Read first

1. `docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md`: Fixes 1–19, the
   rendering result, and the **"Still open"** list this prompt is built from.
2. `docs/runbooks/trace-failing-syscalls-amd64.md`: the tracing that found
   Fixes 11–19. Note what `[sc!]` **cannot** show: `EAGAIN` (a `clone` at the
   thread ceiling), and anything that fails identically on Linux. When the
   last failing call before a crash is Linux-identical, go to `strace_pid=`.
3. `userspace/kami/README.md` (status, `build.sh`, the two "Future" sections:
   reading mode / `.md` output, and the rio pane).
4. `docs/runbooks/amd64-bare-metal-loop.md` ("Two machines at once",
   `hpbox.stage()` / `hpbox.reboot_to("akuma")`, and the `git bundle` note:
   `hpbox.deploy()` cannot reach a commit that exists only on `litter`).

## The rig (trashcan Ubuntu side, `192.168.1.120`, ssh port 22)

Drive it with `HPBOX_IP=192.168.1.120 python3 scripts/utils/hpbox.py ub '<cmd>'`.
`hpbox ub` gives up after **300 s** and its CLI exits 0 whatever the remote
command returned, so run long jobs detached (`nohup … &`) and poll a marker.
Everything is in `userspace/kami/probe/akuma/` and installed at
`/root/cdp-probe/akuma/`.

| step | where | command |
|---|---|---|
| build the static probes, copy probes and scripts to the box | laptop | `sh userspace/kami/probe/akuma/push.sh` |
| carry `litter`-only commits over | laptop | `git bundle create k.bundle HEAD ^<box head>`, `cat` it over ssh to `/tmp/k.bundle`, on the box `git fetch /tmp/k.bundle HEAD:refs/heads/laptop-head`; then `deploy()` lands |
| sync the tree and build the kernel | laptop | `hpbox.deploy()` (check its rc!) then `hpbox.build()` |
| all probes | box | `MEM=4096 sh run-fc.sh probes.sh bpprobe trapprobe spawnprobe singletonprobe snapprobe taskprobe credprobe capprobe jitprobe thrprobe chromeprobe` — `run-fc.sh` filters its stdout; the verdict lines are in `kami-fc.log` |
| one Chromium run, whole stderr and exit status | box | `KARGS=strace_err MEM=4096 sh run-fc.sh chrome-once.sh` → `out/shot.png`, `out/dmesg.txt` |
| the older two-mode smoke run | box | `sh run-fc.sh` (`kami-smoke.sh`) |
| standard-image boot (expect 731 passed, 0 failed) | laptop | `hpbox.firecracker(vcpus=2, timeout_s=150)` |
| Linux control for a probe | box | `docker run --rm -v $PWD:/p akuma-cdp-probe /p/<probe>` |
| Linux `strace -f` of Chromium | box | `/root/cdp-probe/linux/smoke.strace` (a full trace of one page load) |

One Firecracker at a time on the box: `run-fc.sh` kills every `firecracker`
it finds, and `hpbox.firecracker()` boots the same binary.

`KARGS` adds kernel command-line flags:
- `strace_err`: one `[sc!]` line per failing syscall (pid, x86_64 nr, decoded
  paths, decimal errno), plus a `[sig!]` line for every fault signal delivered
  to a program's own handler;
- `strace_nr=89,267`: also successful calls of those numbers;
- `strace_pid=<n>`: the full trace (`[sc>]`/`[sc]` lines, keyed by task) for
  one thread group. Pids are deterministic per image and script at
  `VCPUS=2`: in `chrome-once.sh` the browser is **pid 15**, the first GPU
  process was 73 and the first renderer 72 on 2026-10-08.

The Chromium image's boot prints `self-test: 619 passed, 41 FAILED`. That's
expected: the fixtures are missing from that image. Compare the count across
runs, not against zero.

## Where it stands (2026-10-08, end of round 2)

Round 2 closed, in order: the zygote child's `capset` (`ENOSYS`, no x86_64
row — Fix 17), V8's RWX code range (`mprotect(PROT_WRITE|PROT_EXEC)` refused
— Fix 18), and the 64-entry system-wide thread table (`clone` → `EAGAIN` for
the GPU process — Fix 19). `chrome-once.sh` then exits 0 in ~28 s of guest
time with the screenshot above; a session peaks at ~103 live user threads of
a 512-slot budget.

## This round

1. **`kami` on the trashcan's metal.** `userspace/kami/build.sh`; it needs
   `/dev/fb0`, and the default page needs networking. Stage with
   `hpbox.stage()`, boot with `hpbox.reboot_to("akuma")`, and read
   `docs/runbooks/amd64-bare-metal-loop.md` first — metal only when the user
   asks. Expect the metal to differ from Firecracker in the NIC (Realtek
   `rtl8169`), the clock, and memory size; the Chromium path itself is the
   same binary.
2. **Then the open list** (do them when they block, or when cheap):
   - `fallocate` (x86_64 285) has no row: 82 `ENOSYS` per run, glue has the
     arm. Row plus arm, pinned in the dispatch self-test like `capset`.
   - The 256-row process table (`akuma-exec`), which every `pthread_create`
     on this target takes a row in and which **panics** rather than refuses
     when full. A heavier page than the test page may reach it.
   - Missing x86_64 rows: 40 `sendfile`, 86 `link`, 239 `get_mempolicy`,
     253/294 `inotify_init`/`inotify_init1`, 297 `rt_tgsigqueueinfo`
     (crashpad's re-raise), 444 `landlock_create_ruleset`, 101 `ptrace`.
     Decide each one's honest answer, then row plus arm; a decoded row
     without an arm prints `no dispatch arm`. The `no row for` print keeps a
     32-number table and stops naming numbers once it is full — Fix 17 hid
     behind that; `[sc!]` lines with a bare `nr=` and `-> -38` are the rest.
   - `gettid()` of a main thread is its thread slot, not its pid (both
     kernels, by design). Chromium rendered without it mattering; it stays
     listed because sandboxed modes compare the two.
   - Missing `/proc` and `/sys` files (`/proc/cpuinfo`,
     `/proc/sys/fs/inotify/max_user_watches`, `/proc/<pid>/oom_score_adj`,
     `/sys/devices/system/cpu/{possible,present}`), `O_CREAT` ignoring the
     umask, `init=` not following symlinks, the slow `munmap` of the
     1324 GiB reservation (~290 ms vs 0.5 ms), and the amd64 `cargo clippy`
     debt (pre-existing `-D warnings` failures in `hda.rs`, `kbd.rs`,
     `fd.rs`, `usermode.rs`, none from this work).

## Verify, every kernel change

- clippy and host tests for each crate you touch (`CLAUDE.md` § Testing);
  the AArch64 `cargo clippy --release -- -D warnings` covers glue;
- `cargo check` of both kernels (`--release` for AArch64;
  `-p akuma-amd64 --target x86_64-unknown-none --release`);
- the probe pass above, all green, `chromeprobe` 16/16, `capprobe` 7/7,
  `jitprobe` 9/9, `thrprobe` 96/96;
- the standard-image boot: `731 passed, 0 failed`;
- then the Chromium run: exit status 0 and `out/shot.png` reading
  "JavaScript ran: 6 x 7 = 42". A regression shows up there first.

## When you stop

- In `docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md`: add a numbered
  `## Fix N` per closed item and correct "Still open".
- Update `userspace/kami/README.md`'s status paragraph.
- Add symptom rows to `docs/README.md` and a reference-doc section for any
  new subsystem behaviour.
- Rewrite this file for the next round.
