# Handoff prompt: headless Chromium (kami) on amd64 — round 4

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

## Where it stands (2026-10-08, end of round 3)

Round 3 closed `fallocate` (Fix 20, x86_64 285; gate `fallocprobe.c`; standard
image `733 passed, 0 failed`; `chrome-once.sh` exit 0, `shot.png` still reads
"JavaScript ran: 6 x 7 = 42", 0 `nr=285` failures against 82 before), then put
`kami` on the trashcan's **metal** for the first time.

**On the metal (kernel `f00fca8f` + Fix 20, `--features no-tests`, installed as
`/boot/akuma-amd64`; the previous kernel is `/boot/akuma-amd64.prev`):**

- Chromium (Alpine's, merged onto the Akuma partition with
  `rsync --ignore-existing`, so a later `hpbox.restage_disk()` deletes it) and
  `/bin/kami` start. `kami` opens `/dev/fb0` (3840x2160, stride 16384,
  `rgb@16/8/0`) and Chromium starts in ~4.7 s. `chromium --dump-dom
  http://example.com/` returns the real DOM, so the metal's network works for
  Chromium.
- **The framebuffer path is proven.** `fbpattern.c` (colour bars + ramp, the
  mapping read back) shows exactly that on the TV, colours correct. `kami`'s
  blit is the same row copy `akuma-cli-wgpu` uses.
- **What is not working: the CDP screencast delivers an empty frame**, on
  Linux a painted one. Fully recorded (table, what it is not) in the archive doc
  § "Open: the CDP screencast delivers an empty frame". `Page.captureScreenshot`
  is correct on Akuma, so `kami` now polls it when the first screencast frame is
  empty. **Untested on the metal at the time of writing** (see the sshd item).
- Chromium logs `Corruption detected in shared-memory segment`
  (`persistent_memory_allocator.cc:886`) 71 times in one run. **Not known to
  predate Fix 20**: the Firecracker rig was unreachable (the box was booted
  into Akuma) so there was no comparison. Suspect the open divergence below.
- **sshd on the metal: parked, see the `docs/README.md` row.** herd does start
  sshd at boot (pid 7 in the boot snapshot) and it is gone by ~60 s, not revived.
  The user's decision: boot straight into `init=/bin/sshd` (GRUB entry on the
  Ubuntu side; Akuma cannot edit it) and look again later. GRUB currently has
  `initargs=daemon` on the default entry (made no difference); the backup of the
  previous entry is `/root/45_akuma.bak-20261008-061615` on Ubuntu.

**Open divergence found by `fallocprobe`:** a `MAP_SHARED` write is not visible
to `pread` until `munmap` (an `ftruncate`-sized file behaves the same).
`FALLOC_FL_KEEP_SIZE` is `EOPNOTSUPP`.

**Next, in order:**

1. (Fix 20 is cleared for the empty frame: same result without it. Still open for
   the shared-memory corruption count.) A/B Fix 20 against the corruption: from Akuma,
   `cp /boot/akuma-amd64.prev /boot/akuma-amd64 && sync && /bin/busybox reboot -f`
   (it has no `fallocate` row, so Chromium sizes by `ftruncate`), run `kami`
   with `KAMI_TRACE=1`, count `Corruption detected` in `/tmp/kami.log`, then put
   the new kernel back. Mind that `sshd` may not come up; there is no remote way
   back to Ubuntu.
2. Why no second frame / no paint: run `KAMI_TRACE=1 kami ...` (names every CDP
   event) and `KAMI_DUMP=/tmp/f.png` (keeps the first frame; pull it with
   `base64`, the ssh channel turns LF into CRLF). Likely suspects: the shared
   memory coherence above, or compositor frames that never start without a GPU.
3. Then the open list below.

**kami debug knobs added this round:** `KAMI_DUMP=<path>` (first PNG),
`KAMI_TRACE=1` (event names on stderr). A top-left status overlay was asked for
and is not written yet (needs a built-in font; `akuma-fbcon`'s would drag in
`ab_glyph` and a submodule).

## This round (carried over)

1. **`kami` on the metal** — see above; goal not met yet (white, not the page).
2. **The open list**:
   - The 256-row process table (`akuma-exec`), which every `pthread_create`
     takes a row in and which **panics** rather than refuses when full.
   - Missing x86_64 rows: 40 `sendfile`, 86 `link`, 239 `get_mempolicy`,
     253/294 `inotify_init`/`inotify_init1`, 297 `rt_tgsigqueueinfo`
     (crashpad's re-raise), 444 `landlock_create_ruleset`, 101 `ptrace`.
     Decide each one's honest answer, then row plus arm; the `no row for`
     print keeps a 32-number table and stops naming numbers once full, so a
     bare `nr=N -> -38` is a missing row.
   - `gettid()` of a main thread is its thread slot, not its pid.
   - Missing `/proc` and `/sys` files (`/proc/cpuinfo`,
     `/proc/sys/fs/inotify/max_user_watches`, `/proc/<pid>/oom_score_adj`,
     `/sys/devices/system/cpu/{possible,present}`), `O_CREAT` ignoring the
     umask, `init=` not following symlinks, the slow `munmap` of the 1324 GiB
     reservation, and the amd64 `cargo clippy` debt in `hda.rs`, `kbd.rs`,
     `fd.rs`, `usermode.rs`.

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
