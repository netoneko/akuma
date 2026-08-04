# Self-host retest on `devbox-smoltcp` after the execve-leak fix (Aug 2 2026)

Follow-up to `AKUMA_SELF_HOSTING.md` §7j (the original `--release -p akuma`,
rump-devbox-era self-host success from June 19 2026). This session re-ran the
self-host experiment against current HEAD (`5ea6024`, "possibly fixes for
thread stuff") using the **`release-smp-shared` profile + `devbox-smoltcp`
feature** (`userspace-sshd` + `smp-shared`) instead of the old plain `devbox`
(rump) build, specifically to check whether `5ea6024`'s execve-heap-leak fix
(`docs/archive/EXECVE_STACK_LEAK_OOM_HANG.md`) holds up under real cargo-scale
exec volume (thousands of rustc/cc/ld spawns). Driven by
`scripts/selfhost_driver.py` (see its docstring for the apk-cargo /
nightly-rustc split and SSH-channel-death rules), instance 1, `MEMORY=8192
SMP=1 SNAPSHOT=0 DISK=disk_selfhost.img`.

**Every item below was a genuine, reproducible blocker found during setup —
none were guessed at.** They're listed in the order they were hit.

## 1. `disk_selfhost.img` was silently empty

The disk the task started from (dated Aug 2, 8 GB, allegedly "already
prepared: apk + musl-dev + nightly Rust toolchain") booted fine but the
kernel's own `fs::exists("/bin/herd")` check (`src/main.rs:1422`) failed, and
`[FS] Root directory contents:` showed only `lost+found` and `proc` — i.e. a
freshly-`mkfs`'d ext2 filesystem with **zero populated content**. Root cause:
`scripts/populate_disk.sh`'s Docker mount step needs a running Docker daemon;
whatever prepared this disk had it down, so `create_disk.sh`'s `mkfs` ran but
the subsequent `docker run` copy never executed (or errored out before
copying). The disk file was full-size and non-sparse (a zero-filled 8 GB), so
`ls -la` gave no hint anything was wrong.

**Fix:** started Docker Desktop, re-ran
`DISK=disk_selfhost.img scripts/populate_disk.sh --with-apk --with-musl-dev --with-rust-toolchain`
in full (not `--bin-only`).

**Lesson:** a disk-prep script that depends on Docker should check
`docker info` and fail loudly, not silently leave a valid-but-empty
filesystem. Verify a "prepared" self-host disk by booting it once and
checking `[FS] Root directory contents:` shows more than `lost+found`, not by
trusting `ls -la`'s size/mtime.

## 2. Base `populate_disk.sh` (no `--overlay`) ships the wrong `/etc/herd/enabled/sshd.conf` for `devbox-smoltcp`

After (1), `herd` and `/bin/sshd` started, but every SSH connection to the
guest's port 22 got RST during kex (`kex_exchange_identification: read:
Connection reset by peer`) — confirmed via a raw socket that the guest closed
the connection with zero bytes sent, not a crypto/host-key problem.

Root cause: the plain `bootstrap/etc/herd/enabled/sshd.conf` (what a bare
`populate_disk.sh` copy ships) is the **Phase-2 multikernel core-pinned**
config — `args = --port 23 --shell /bin/sh`, `core = 1` — a leftover from
`docs/MULTIKERNEL.md` §10 / acceptance/12, not the devbox-smoltcp config.
Confirmed from the kernel log: `[syscall] bind(fd=3, port=23, ...)` — sshd was
listening on the *telnet* port, not 22. `devbox.img` (the working devbox)
never has this problem because `overlays/devbox/bootstrap.sh` **wipes
`/etc` and repopulates it solely from `overlays/devbox/rootfs`** (see that
script's own comment: "Nothing from `bootstrap/etc/` is inherited
unreviewed"). `disk_selfhost.img`'s prep never applies that overlay.

**Fix:**
```bash
DISK=disk_selfhost.img scripts/populate_disk.sh --overlay overlays/devbox/rootfs
```
(after stopping the VM — a docker loop-mount concurrent with a running QEMU
instance risks the corruption `populate_disk.sh` itself warns about). After
this, boot log shows `[syscall] bind(fd=3, port=22, ip=0.0.0.0)` and SSH
works immediately.

## 3. `populate_disk.sh --with-rust-toolchain` never installs apk `cargo`

Once SSH worked, `/usr/bin/cargo` (the apk *stable* cargo the whole
toolchain-split recipe in §7j depends on to orchestrate, since nightly cargo
crashes at startup) didn't exist. `--with-rust-toolchain`'s `RUST_CMD` only
apk-installs the **C toolchain** (`clang lld gcc binutils make musl-dev`) —
it never runs `apk add cargo`. This is a real gap in the script, not
operator error; nothing about the flag name suggests it would skip cargo
itself.

**Fix (host-side, disk stopped):**
```bash
docker run --rm --privileged -v "$(pwd)/disk_selfhost.img:/disk.img" alpine:latest sh -c '
  mkdir -p /mnt/disk && mount -o loop /disk.img /mnt/disk &&
  apk --root /mnt/disk --no-scripts add cargo &&
  sync && umount /mnt/disk'
```
Confirmed nightly rustc itself still crashes unchanged from the original
diagnosis in §7j/`AKUMA_SELF_HOSTING.md` (`/usr/local/bin/cargo --version` →
`[Exception] Unknown from EL0: EC=0x0, ISS=0x0`, exit via SIGHUP) — the
execve-leak / thread fixes in `5ea6024` did **not** touch this; the apk-cargo
+ nightly-rustc split documented in §7j is still required.

## 4. `/bin/git` is `scratch` (Akuma's own minimal git), not real git — `selfhost_driver.py` needs `/usr/bin/git`

`scripts/selfhost_driver.py` hardcodes `/usr/bin/git clone --depth 1 ...`.
On this disk, `/bin/git` is a symlink to `scratch` (Akuma's own lightweight
git-subset client — see §"scratch" note in `AKUMA_SELF_HOSTING.md`'s
prerequisites) and `/usr/bin/git` didn't exist at all, so the driver's clone
step failed with `sh: /usr/bin/git: not found`. Separately, `scratch` is
confirmed **not** to understand shallow clones at all (`--depth` is a no-op
or worse for it) — so even pointing the driver at `/bin/git` would have been
wrong.

**Fix:** apk-install real git onto the disk the same way as cargo:
```bash
docker run --rm --privileged -v "$(pwd)/disk_selfhost.img:/disk.img" alpine:latest sh -c '
  mkdir -p /mnt/disk && mount -o loop /disk.img /mnt/disk &&
  apk --root /mnt/disk --no-scripts add git &&
  sync && umount /mnt/disk'
```
This lands real git 2.54.0 at `/usr/bin/git` — exactly where the driver
already expected it. (A host-side sanity check of the freshly-mounted binary
inside the Alpine container itself will fail with "Error loading shared
library libpcre2-8.so.0" — that's the *container's* linker looking in its own
`/usr/lib`, not `/mnt/disk/usr/lib`; harmless, verify by booting instead.)

## 5. `scripts/selfhost_driver.py`'s build step is missing `-p akuma`

With clone/manifest working, the build step
(`cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests
--manifest-path /root/akuma/Cargo.toml -j1`) failed immediately:
```
error: none of the selected packages contains this feature: devbox-smoltcp
selected packages: akuma, akuma-editor, akuma-exec, akuma-isolation, akuma-vfs, ...
```
Root cause: invoked over SSH via `busybox sh -c` with no `cd`, `--manifest-path`
alone doesn't establish a "current package" for cargo, so it fell back to
resolving features across every workspace member — and `devbox-smoltcp` only
exists on the root `akuma` package. Every previously-documented working
self-host invocation in `AKUMA_SELF_HOSTING.md` (§7e, §7j, §7k) includes
`-p akuma` explicitly; the driver script was missing it. This is unrelated to
the "`cd` doesn't propagate through exec in Akuma" issue documented
elsewhere in that file (§2) — `busybox` builtins run in-process so `cd`
inside a single `busybox sh -c "cd X && cp ..."` works fine; this was purely
a cargo package-selection default.

**Fix:** added `-p akuma` to `scripts/selfhost_driver.py`'s build step
(uncommitted — script-only fix, left for review).

## 6. Public GitHub HEAD lags local HEAD — no `devbox-smoltcp` feature yet

`git clone --depth 1 https://github.com/netoneko/akuma.git` (real git, now
working per §4) succeeds and correctly gets a shallow clone — but
`github.com/netoneko/akuma`'s current HEAD (`b408f92`, "smaller devbox") pre-dates
the `devbox-smoltcp` feature; at that commit `Cargo.toml` only has the older
rump-based `devbox` feature. `-p akuma --features devbox-smoltcp` correctly
failed with `error: the package 'akuma' does not contain this feature:
devbox-smoltcp` — not a driver bug this time, a real source mismatch, since
`5ea6024` (which the leak fix and `devbox-smoltcp` both live on) hasn't been
pushed to the public remote.

**Fix — served the local working tree's HEAD directly to the guest instead
of pushing to the public remote:**
```bash
# host side
touch .git/git-daemon-export-ok
git daemon --base-path=/Users/netoneko/github.com/netoneko --export-all \
  --reuseaddr --verbose --port=9418 /Users/netoneko/github.com/netoneko/akuma &

# guest side (10.0.2.2 is QEMU SLIRP's host gateway — reachable from the
# guest without any hostfwd, same pattern as the `10.0.2.2:8000` host
# HTTP-server tricks used throughout docs/archive/, e.g.
# FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md, BKL_VFS_CARVE_OUT.md)
git clone --depth 1 git://10.0.2.2:9418/akuma /root/akuma
```
This clones exactly the committed tree at local HEAD (`5ea6024`) — a `git
daemon` serves committed objects only, so uncommitted working-tree changes
and untracked scratch files are never exposed, which matters since "use the
local HEAD for the commit" was the explicit ask, not "publish the working
tree." Verified in-VM: `git log --oneline -1` → `5ea6024 possibly fixes for
thread stuff`, and `devbox-smoltcp = ["userspace-sshd", "smp-shared"]` present
in the cloned `Cargo.toml`.

## Build progress once all six were fixed

With (1)-(6) resolved, the real build
(`/usr/bin/cargo build -p akuma --profile release-smp-shared --features
devbox-smoltcp,no-tests --manifest-path /root/akuma/Cargo.toml -j1`, apk
cargo + nightly rustc via `RUSTC=/usr/local/bin/rustc`) ran cleanly: full
dependency download, then real compilation past `proc-macro2` / `quote` /
`syn` / `der_derive` (the historical wall from §7e/§7h, still clear) and
into the crypto/SSH stack (`curve25519-dalek`, `sha2`, `ff`, ...) — see the
build log referenced from `AKUMA_SELF_HOSTING.md` for the final PASS/FAIL,
artifact md5, and wall-clock time once it finishes.

## Noise encountered during the build that turned out NOT to be bugs

Two log patterns looked alarming enough mid-build to warrant a stop-and-check
before deciding to keep going; recorded here so a future run doesn't
re-diagnose the same false alarms from scratch.

- **`[EINVAL]` burst during crate extraction (~65k occurrences).** Syscall
  78 = `readlinkat`. `src/syscall/fs.rs:2552`'s `sys_readlinkat` correctly
  returns `EINVAL` (per POSIX) when the resolved path exists but is not a
  symlink — this is standard `readlink(2)` behavior, not a bug. Cargo
  extracting ~100+ downloaded crates, each with many files, and probing
  "is this a symlink?" per file/dir during extraction, legitimately produces
  tens of thousands of these. Confirmed non-blocking: the burst stopped on
  its own and `Compiling` lines resumed immediately after.

- **`tkill(tid=X, sig=10)` (SIGUSR1) in packs of exactly 100.** Every single
  pack observed (55/55 so far) is **exactly** 100 calls, one pack per newly
  spawned thread/process (rustc, cc, ld, collect2, ...), confirmed via
  `src/syscall/signal.rs:335` — the print only fires on an actual userspace
  `SYS_tkill` call, so this is genuinely userspace looping, not a kernel
  artifact. The exact-100, no-exceptions signature is the fingerprint of a
  bounded retry/spin loop with a hardcoded cap, most likely in the Rust
  runtime's or musl's thread/process-startup path, self-signaling while
  waiting on some condition Akuma never satisfies the way the loop expects,
  then falling through to a working fallback once the retry budget is
  exhausted. **Not traced to an exact rustc/musl source location** — flagged
  here as a known, harmless, reproducible pattern rather than a solved
  mystery. Doesn't block progress (build continues normally after every
  pack).

  Initially suspected as a possible source of measurable latency, but
  measurement showed otherwise — see next item, which is the real one.

- **`[munmap]` print volume dwarfs everything else, and is the more
  plausible latency cost.** `src/syscall/mem.rs:855` prints one line per
  `munmap()` **syscall** (not per page — the page count is a `{}` in the
  message). Across the build so far, `[munmap]` lines are **160,836 of
  458,821 total log lines (35%)**, vs ~5,500 `tkill` lines — roughly 29x more
  volume. In one measured process-exit window, munmap outnumbered tkill
  260:100. Averaged over process exits so far, that's **~2,900 individual
  `munmap()` syscalls per process teardown** — each paying kernel-side region
  lookup + `MmBklGuard` lock + TLB bookkeeping + a `tprint!` call. That many
  small unmaps per exit suggests either a large number of genuinely small,
  disjoint mmap regions per rustc/cc process, or a missed coalescing
  opportunity somewhere in the mmap region tracking (`MmapRegion` list in
  `src/syscall/mem.rs`) — worth a dedicated profiling pass if self-host build
  time becomes a target to optimize, but out of scope for this pass/fail
  experiment.

## Open issue: thread-spawn SIGABRT under real `-j4` parallelism (2026-08-03 follow-up, UNRESOLVED)

Follow-up session, HEAD `48abffc` ("damn it is taking forever" — includes the
`LAZY_REGION_TABLE` alloc-under-lock fix, see
`docs/archive/LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG.md`), `SMP=4`,
`release-smp-shared` + `devbox-smoltcp`, fresh `disk_selfhost.img` re-clone
from the local git daemon (§6 method).

**The `LAZY_REGION_TABLE` fix holds under `-j4`:** the previous SMP=1 instance
had been silently wedged for 90+ minutes (100% host CPU, zero log growth)
reproducing that exact hang signature (`mmap`/`mprotect` burst, lazy-region
count climbing past 100). Killed, rebuilt at HEAD, rebooted with `SMP=4`. The
new kernel stayed responsive throughout a `-j4` build attempt — log kept
growing, `PSTATS`/`THR-DUMP` snapshots showed live syscall traffic, and the
guest remained reachable over SSH.

**But `-j4` surfaces a different, new failure**, never seen at `-j1`: the
`quote v1.0.42` build script aborted mid-compile —

```
fatal runtime error: current thread handle already set during thread spawn, aborting
error: could not compile `quote` (build script)
Caused by:
  process didn't exit successfully: `/usr/local/bin/rustc ... build_script_build ...` (signal: 6, SIGABRT: process abort signal)
warning: build failed, waiting for other jobs to finish...
```

Notably, the original §"Build progress once all six were fixed" section above
documents `-j1` compiling straight through `proc-macro2` / `quote` / `syn` /
`der_derive` with no issue — so this is a real `-j4`-only divergence, not a
retest of an old bug.

**What the message means:** `fatal runtime error: current thread handle
already set during thread spawn` is Rust std's own internal consistency
check in `std::thread::Builder::spawn`, tripping because the freshly-cloned
OS thread's TLS/thread-registration slot looks, from libstd's point of view,
already occupied. This points at Akuma's `clone()`/TLS-assignment path
handing out a colliding slot under real concurrent thread creation — `-j4`
runs multiple rustc processes plus rustc's own internal worker threads
simultaneously, which `-j1` never exercises. Prime suspect, not yet
confirmed: interaction with the thread-slot recycling logic from the recent
"stale thread slot kill" commits (`0479cd8`, `dc4684a`) — `[Cleanup] Thread N
recycled after …us cooldown` lines are present in the kernel log around the
same period.

**Cargo's failure behavior, for context:** without `--keep-going` (not
passed here), a failed unit stops cargo from *scheduling new* jobs but lets
already-in-flight ones drain first (the "waiting for other jobs to finish"
message) — it does not retry the failed unit and does not recover into a
passing build. Everything depending on `quote` (`syn`, `serde_derive`, and
therefore most of the crypto/SSH stack) never gets scheduled once it fails.

**Status: open, not yet root-caused.** The spinning PC/failing clone call
site hasn't been captured live (no `GDB=1` on this boot). Next step in
progress: repeated `-j4` runs to collect more SIGABRT samples and look for a
pattern — which crate/build-script it hits, whether it clusters with
`[Cleanup] Thread N recycled` events, and the PID/TID range involved — before
attempting a fix.

## Open issue #2: a genuine lost-wakeup hang, distinct from both bugs above (2026-08-03, UNRESOLVED)

While draining the in-flight jobs after the `quote` SIGABRT above, the
`typenum v1.19.0` `rustc` invocation (pid 137, one `-j4` worker cargo had
already launched before the failure) never finished — and **it isn't slow,
it's stuck**. Its own `PSTATS` self-reporting line was sampled repeatedly
(every ~30s) and is byte-for-byte identical every time, from the very first
sample onward:

```
[PSTATS] PID 137 (/usr/local/bin/rustc)   20.85s: 17269 syscalls ...
[PSTATS] PID 137 (/usr/local/bin/rustc)  680.98s: 17269 syscalls ...
[PSTATS] PID 137 (/usr/local/bin/rustc) 1431.16s: 17269 syscalls ...
```

Same exact syscall count (`17269`), same `pgfault`, same per-syscall
breakdown, across **1410+ seconds** (23+ minutes) of elapsed guest uptime.
Real, ongoing compilation work — allocator growth, output writes — cannot
run that long with *zero* new syscalls; this rules out "just slow under
emulation" and confirms a hard stall. The host QEMU process stayed at ~2.7%
CPU throughout (not pegged), so this is **not** the `LAZY_REGION_TABLE`
rule-2 spin signature (100% CPU) — it's the opposite: something is parked,
waiting, and never getting woken.

The concurrent `[THR-DUMP]` snapshots corroborate this. `tgid=137`'s two
worker threads are both sitting inside an in-flight `futex` syscall that
never returns:

```
tid=31 st=? pid=144 tgid=137 sc=-1 tsc=98 a0=0x3cda5fc4 a1=0x80 elr=0x30060cc4
tid=32 st=? pid=145 tgid=137 sc=-1 tsc=98 a0=0x3ceb9118 a1=0x89 elr=0x30060cc4
```

`tsc=98` is `__NR_futex` on AArch64; `sc=-1` means the syscall hasn't
returned; both threads sit at the identical PC (`elr=0x30060cc4`) across
every sample. This looks like a genuine missed/lost `futex_wake` targeting
`rustc`'s internal parallel-codegen worker-thread pool — the same
synchronization primitive (`FUTEX_WAIT_PRIVATE`) as the already-fixed
[[project_futex_wake_tgid_pthread_join]]-class bug
(`futex_wake` only waking the `tgid=0` queue, missing
`FUTEX_WAIT_PRIVATE`/`pthread_join` waiters, fixed 2026-05-30) — possibly a
related gap in the same wake-target-selection logic, not yet confirmed to be
the same code path.

**Status: open, not yet root-caused, and NOT the same bug as either the
`LAZY_REGION_TABLE` spin (fixed) or the `-j4` thread-spawn `SIGABRT` above.**

### Futex audit: five confirmed Linux divergences, one of them a real lost-wakeup generator (2026-08-03)

A direct audit of `sys_futex` against Linux semantics, driven by a new probe
(`userspace/forktest/c_stress/futexops.c`). Method matters here: the **same
stripped static binary** was run on Akuma and on real Linux aarch64
(`docker run --rm --platform linux/arm64 … alpine /futexops`). It scores
**5 FAIL on Akuma, 5 PASS on Linux**, so every finding below is a measured
divergence, not a claim about what Linux "ought" to do. Full table and
citations now live in the current-state reference,
[`docs/reference/subsystems/syscalls/sync.md`](../reference/subsystems/syscalls/sync.md)
§"Known divergences from Linux".

**Two corrections to the reading of the `[THR-DUMP]` evidence above, before
the findings.** Both were my own initial misreads, caught while checking them:

1. `a1=0x89` is `FUTEX_WAIT_BITSET|PRIVATE`, and I first took that to mean
   tid=32 held a *timeout* that never fired — which would have been a damning
   anomaly on its own. It isn't: **Rust std always emits `FUTEX_WAIT_BITSET`,
   passing a NULL timespec when the wait is untimed.** So `0x89` says nothing
   about whether a timeout was set, and both stuck threads are plausibly plain
   untimed waits. No failed-timeout anomaly to explain.
2. The identical `elr=0x30060cc4` across a `0x80` waiter and a `0x89` waiter
   does **not** mean both came from the same library. On a static musl link
   both Rust std's raw `syscall()` and musl's internal futex calls funnel
   through the same `svc` trampoline, so one PC is expected.

**What was ruled OUT** (checked in code, all sound — recording so the next
pass doesn't re-walk them):

- *The wake never targeting the right bucket.* `read_current_pid` resolves
  tid → pid → **tgid** (`process/children.rs:286-291`), so private-futex keys
  really are tgid-scoped. (One caveat, below.)
- *The enqueue → park race.* `ThreadWaker::wake` sets the sticky
  `WOKEN_STATES[tid]` **unconditionally**, before and independently of the
  `WAITING` check (`threading/mod.rs:2810-2837`), and `schedule_blocking`
  consumes it on entry (`:2917`). A wake landing in the window between
  `futex_check_and_enqueue` and the park is not lost.
- *Timed waits never being re-readied.* The scheduler's wake-pass runs on
  **every** scheduler entry, including preempt-disabled timer entries, and
  readies any `WAITING` thread whose deadline passed
  (`threading/mod.rs:2059-2071`).

**The finding that matters — one-sided requeue bookkeeping.**
`futex_requeue_table` moves a waiter's tid from `key1`'s queue into `key2`'s
(`sync.rs:144-168`), but the waiting thread's loop only ever checks/removes
itself from the key it *originally* waited on (`key`, a local from
`sync.rs:266`; membership check at `:341-352`). After a requeue those disagree,
so any loop exit other than "drained by a wake on `key2`" strands the tid in
`key2`'s queue **permanently**. Measured directly: after the requeued waiter
timed out *and had been `pthread_join`ed*, `FUTEX_WAKE(key2, 1)` still reported
**1 woken** — the kernel counted a dead tid as the wake's recipient. Linux
reports 0. The probe also caught a second symptom of the same defect: the
requeued waiter's timeout returns **0 (success), not `ETIMEDOUT`**, because it
finds itself absent from its original key and concludes it was genuinely woken.

Why this one is the plausible candidate for the `typenum` stall, stated with
its limits: it is the only one of the five on a path ordinary musl userspace
takes constantly (`pthread_cond_broadcast` *is* `FUTEX_REQUEUE`;
`pthread_cond_timedwait` supplies the timeout that does the stranding), it
**accumulates** over a process's lifetime, and its signature is precisely
"thread parked forever on a wake the kernel already counted as delivered."
rustc links LLVM, whose C++ condvars go through musl's pthread_cond.

**But it is NOT proven to be what hit pid 137, and I did not prove it.** The
stall was not reproduced under this hypothesis; no `[futex-dbg]` trace was
captured from a stalled build; and rustc's *own* Rust-level condvars use raw
futex, not pthread_cond, so the requeue path is reached only via its C/C++
dependencies. Treat it as the best-supported lead, not the root cause. The
decisive next experiment is cheap: rebuild with `FUTEX_DBG_ENABLED`, reproduce
the `-j4` stall, and check whether a `REQUEUE` precedes the stranded address —
or instrument `futex_do_wake` to count wakes delivered to tids that are not in
`WAITING`, which would make stale-entry consumption self-reporting.

**On "all three bugs are connected":** partly, and less than it looks.
The slot-reclaim bug fixed above and these futex gaps are independent defects
in different subsystems — the fix for one does not touch the other. What *is*
shared is the aggravating condition: `-j4` drives thread churn, which drives
both slot pressure and condvar broadcast/timeout traffic, so both get more
reachable at once. The genuinely suspicious overlap is narrower: divergence #3
(an unreadable timespec silently becoming an *infinite* wait, `sync.rs:283`)
is reachable **under memory pressure**, because `validate_user_ptr`
demand-pages through `ensure_user_pages_mapped`. That turns a transient
allocation failure into a permanently parked thread — the same "transient
condition, permanent symptom" shape as the slot starvation, and worth ruling
in or out on any repro that also shows `[OOM]`/`[HEAP-GROW]` lines.

**One unprobed sharp edge, flagged not fixed:** `futex_key_tgid` →
`read_current_pid()` ends in `.unwrap_or(pid)` (`process/children.rs:290`). A
failed process-table lookup silently degrades the futex key from tgid to the
thread's own pid — waiter and waker would key different buckets, which is
exactly the stranding class the sync.md Stability note already warns about. Not
observed; called out because the fallback is silent rather than an error.
Three separate failure signatures are now in play from this one `-j4`
self-host attempt:
1. `LAZY_REGION_TABLE` alloc-under-lock — **fixed**, 100%-CPU spin signature.
2. Thread-spawn `SIGABRT` (`quote`'s build script) — open, `-j4`-only.
3. This lost-wakeup stall (`typenum`) — open, low-CPU parked-forever
   signature, plausibly futex-wake-related.

No live PC has been captured for either open issue (no `GDB=1` on this
boot). The stuck run was killed and a fresh `-j4` build relaunched to keep
collecting samples of all three; a future repro should boot with `GDB=1` to
catch a live PC the next time either signature reappears.

### Theory-building pass (opus subagent, 2026-08-03)

A dedicated research pass was run to (a) independently verify which phase
the `-j4` SIGABRT actually happens in — the user distrusted the original
quick read — and (b) enumerate ranked root-cause hypotheses connecting the
SIGABRT, the `tkill`-in-packs-of-100 pattern (see "Noise encountered"
above), and (loosely, since it launched before this was found) the
lost-wakeup stall. It worked from a worktree pinned to an older commit and
extracted `48abffc` from the object DB to read current source, so all
citations below are **as of `48abffc`**.

**Which phase failed, re-verified independently: compilation, not
execution — confidence ~97%, argued two ways.** (1) Cargo's `Caused by:`
line reports the exact argv of the process it spawned; `/usr/local/bin/rustc
--crate-name build_script_build … --emit=dep-info,link` is literally the
compile invocation, and no cargo path executes a *built* script through
`rustc`'s argv (execution failures name the built binary at
`.../build-script-build` with no args, under a different message,
`failed to run custom build command for`). (2) Independently: the abort
text `fatal runtime error: current thread handle already set during thread
spawn` is Rust std's `rtabort!` inside `ThreadInit::init`
(`library/std/src/thread/lifecycle.rs`), which only runs **on a freshly
spawned OS thread**, before its closure. `quote`'s `build.rs` never spawns a
thread; `rustc` always does (`rustc_interface::util::run_in_thread_with_globals`
puts the whole session on a worker thread, and codegen spawns more) — so of
the two candidate processes, only `rustc` can reach that abort at all. The
residual ~3% uncertainty: under `-j4`, stderr from up to 4 concurrent rustc
processes interleaves, so strictly the excerpt doesn't *prove* the abort
line and the `signal: 6` status share a process — considered unlikely
(nothing else in the toolchain raises SIGABRT) but not airtight. Fully
settling it would take `cargo build -v --message-format=json` or checking
whether `target/.../build/quote-*/build-script-build` exists on disk.

**A structural read of the abort, independent of Akuma specifics.**
`set_current()` fails iff `#[thread_local] CURRENT` (which lives in
`.tbss`) is non-null at thread start. musl's `pthread_create` on aarch64
never zeroes `.tbss` itself — it relies on the fresh `mmap(MAP_ANONYMOUS)`
returning zeroed pages, `__copy_tls` only memcpy's `.tdata`. So the abort
reduces to: **a newly spawned thread read a non-zero byte from a location
the kernel was contractually required to deliver as zero** (or the
translation pointed at the wrong page). Everything below ranks *how*.

**Solved outright: the `tkill`-packs-of-100 pattern is `jobserver-rs`'s
helper-thread teardown loop, not a Rust-runtime retry loop as originally
guessed.** It's `Helper::join` in
[`jobserver-rs/src/unix.rs`](https://github.com/rust-lang/jobserver-rs/blob/main/src/unix.rs)
(~lines 439-459): a hardcoded `for _ in 0..100` that `pthread_kill`s
(→ `tkill`) its own helper thread with `SIGUSR1` once per iteration, trying
to interrupt a blocking `read()` on the jobserver pipe so the helper notices
`consumer_done` and exits. One helper spawns per rustc that reaches codegen
(`rustc_codegen_ssa::back::write`), torn down right before rustc execs the
linker — matching the archive's "one pack per newly spawned process"
observation, just attributed to the wrong trigger. **Why it always runs the
full 100 on Akuma, and why this is a real (if separate) bug:** on Linux,
`SIGUSR1` interrupts the blocking `read()` with `EINTR` after one `tkill`.
On Akuma, `sys_tkill` only ever calls `pend_signal_for_thread`
(`src/syscall/signal.rs:331-397`) which sets a bit + wakes the target —
delivery happens "at the next syscall return." But every blocking read loop
in the tree (e.g. `src/syscall/fs.rs:540-558`, and by the same pattern
`fs.rs:413,551,607,634,657,967,1064,1090`, `poll.rs:777`, `time.rs:89`,
`term.rs:420`, `proc.rs:953,1007,1069,1089,1124`,
`crates/akuma-net/src/socket.rs:434`) only checks
`is_current_interrupted()` — a *separate* flag set solely by Ctrl-C
(`crates/akuma-exec/src/process/children.rs:256-269`) — and never consults
`PENDING_SIGNALS[tid]`. The pending SIGUSR1 is woken into but never
delivered, so `EINTR` never happens, so `consumer_done` never gets set, so
all 100 `tkill`s fire (~10ms apart per jobserver's own timeout — ~1s per
pack) before the loop just gives up and **leaks the helper thread**.

**And that leak is the link the user suspected between the two symptoms —
mechanistically, not causally-identical.** The leaked helper sits blocked in
the same kind of loop, which also never checks
`take_thread_kill_request()`. At process exit, `kill_thread_group`'s
grace-wait spins up to ~2s waiting for it, so `unregister_process` runs
against thread-group state that's stale by up to 2s against a ~10ms
slot-cooldown — the exact hazard `docs/archive/STALE_THREAD_SLOT_KILL.md`
documents. So: every rustc that reaches codegen leaks ≥1 unkillable blocked
thread, quadrupled at `-j4`, which manufactures more thread-lifecycle
contention for whatever the SIGABRT's real bug is to land in. Two distinct
bugs, one feeding the other's reproducibility.

**A second, independently-confirmed gap (found while auditing the "stale
thread slot kill" commits `0479cd8`/`dc4684a`):** both guards added there
(`slot_still_owned_by` in `process/signal.rs:96-197` and the
`THREAD_PID_MAP` check in `table::unregister_process`,
`process/table.rs:166+`) treat "no `THREAD_PID_MAP` entry" as "slot
unowned, safe to kill." But a slot claimed via `clone_thread` sits
`INITIALIZING` with **no map entry yet** for ~33 lines of setup
(`process/mod.rs:2782` claim → `:2814-2816` map insert). A `kill_thread_group`
landing in that window kills a thread that's still being born —
`entry_point_trampoline` self-terminates it (`process/mod.rs:2873`) before
it runs a single user instruction, even though `clone_thread` already
returned success up through musl to `pthread_create`. This produces a
**silent thread that never starts** (a hang/missing-output symptom), not
this abort — flagged as a real, still-open gap worth closing regardless.

**Ranked hypotheses for the abort itself** (full reasoning and citations in
the agent's original report; condensed here):
1. **(~35%, top-ranked) Stale PTEs after a thread's stack+TLS `munmap`,
   VA recycled to the next `pthread_create` before teardown is complete.**
   `sys_munmap`'s lazy arm (`src/syscall/mem.rs:907-931`) sizes the unmap
   from the **recorded** region extent
   (`LazyRegionMap::munmap_one_overlap`, `children.rs:1056-1077`); if that
   extent understates what was actually faulted in — the same bug *class*
   as the already-fixed [[project_cow_fork_mmap_region_extent]] — pages stay
   mapped while the VA is freed back to `free_regions` and handed to the
   next thread's TLS, which then inherits the previous thread's live
   `CURRENT` pointer. This is the only hypothesis that naturally produces a
   *valid-looking* stale pointer rather than garbage, matching
   `set_current`'s "any non-null value is an error" check. The agent
   separately ruled out unzeroed-page and stale-TLB explanations: page
   allocation is unconditionally zeroed (`src/pmm.rs:686-792`), and
   inner-shareable TLB shootdown is correctly gated under `kernel_smp_shared`
   (`crates/akuma-exec/src/mmu/mod.rs:1382-1413`) — pushing this toward
   "wrong page still mapped," not "dirty page" or "stale translation on a
   peer core."
2. **(~20%) A lazy-fault race**: parent thread's `__copy_tls` write and the
   child's first touch both fault the same shared VA concurrently; one PTE
   install wins, the loser's write is orphaned. `get_or_create_table_atomic`
   (`mmu/mod.rs:1424+`) guards *table*-level double-allocation, but the
   agent didn't confirm the same exists at leaf-PTE granularity for
   concurrent anonymous faults on a CLONE_VM address space.
3. **Two coincidental-but-linked bugs** — the tkill storm and the abort are
   not the same root cause, but the storm's thread-leak manufactures the
   contention that makes the abort's race reachable (see above).
4. **(~5% this abort, but a real bug) The `THREAD_PID_MAP` guard hole**
   (above) — ranked low for *this* symptom since it produces a hang, not a
   SIGABRT.
5. **(~5%) TPIDR_EL0 aliasing** — child inherits parent's TLS base. Checked
   and found clean: `clone_thread` sets it explicitly
   (`process/mod.rs:2774`), all three EL0 vectors save/restore it
   (`src/exceptions.rs:168/237, 310/401, 471/560`), plus a post-syscall
   resync (`:2815-2820`). Kept only because it would produce exactly this
   message and absence wasn't provable without a live run.
6. **(~3%) The unrelated `EC=0x0`/`ISS=0x0` nightly-`cargo`-startup crash**
   (`docs/archive/RUST_TOOLCHAIN_ISSUES.md` §1) — kept only to note it's a
   structurally different failure mode (kernel-level EL0 trap kill vs. a
   userspace `rtabort!`), not to conflate it.
7. **(~10% as a contributor) `THREAD_RESTORE_SIGMASK_PENDING` inherited
   across slot recycling** — cleared by one of the two slot-claim paths but
   not the recycler (`threading/mod.rs:1213-1235` vs. the claim paths) —
   could independently keep SIGUSR1 blocked on a recycled thread, a second
   possible reason the jobserver's 100-loop never breaks even after the
   EINTR gap above is fixed.

**Recommended next steps, in the agent's stated order:** (1) settle the
compile-vs-execute question in seconds via
`ls target/release-smp-shared/build/quote-*/build-script-build`; (2)
reproduce under `TRACE_MUNMAP=true` (`src/config.rs:715`) and check whether
the aborting thread's TLS VA was previously `munmap`'d, directly testing
hypothesis 1; (3) fix the EINTR gap (blocking loops should consult
`PENDING_SIGNALS[tid]`, not just `is_current_interrupted()`) — independently
confirmable and the one finding here that's fully proven, not hypothesized;
(4) close the `INITIALIZING`-slot guard hole; (5) re-run `-j4` and see what
survives.

**Superseded/extended by a live finding after this pass landed:** while
staging a Rust/C comparison harness (`userspace/selfhost_repro/futextest.rs`
+ new `userspace/forktest/c_stress/futextest.c`, see below), a *third*,
independently-confirmed, code-level bug was found that the agent's pass
didn't have — `clone_thread`'s actual slot-claim path
(`ThreadPool::spawn_user_closure_initializing`, `threading/mod.rs:843-909`)
has no retry-with-reclaim at all (unlike the sibling
`spawn_user_thread_fn_internal`, `threading/mod.rs:3312-3333`, which already
carries the fix for this exact class of starvation). See the next section.

### Confirmed root cause (one of several): `clone_thread` never reclaims cooled-down slots (2026-08-03)

**Reproduced from pure C with no Rust runtime involved at all**, using a new
companion to `futextest.rs`: `userspace/forktest/c_stress/futextest.c`
(cross-compiled on the host for `aarch64-linux-musl` via
`aarch64-linux-musl-gcc`, sidestepping the in-VM-compile chicken-and-egg
problem noted in `AKUMA_SELF_HOSTING.md` §7g — "building `futextest.rs`
in-VM currently hangs rustc itself"). A **tight, correctly-`join()`ed**
200x `pthread_create`/`pthread_join` loop (phase 2, no concurrency, no
fan-out) fails deterministically at **iteration 58**:

```
[2] pthread_create FAILED at iter 58: rc=11 (Resource temporarily unavailable)
```

`rc=11` is `EAGAIN` from musl's `pthread_create` itself — a clean kernel
refusal, not memory corruption. Root cause, confirmed by reading the code:
`clone_thread` claims its slot via `ThreadPool::spawn_user_closure_initializing`
(`crates/akuma-exec/src/threading/mod.rs:843-909`), which does **one linear
scan** for a `FREE` slot in `THREAD_STATES` and returns
`Err("No free user thread slots")` immediately on a miss — it never calls
`reclaim_terminated_slots()`. Compare `spawn_user_thread_fn_internal`
(`mod.rs:3312-3333`), which on the identical miss explicitly reclaims
cooled-down `TERMINATED` slots and retries once, with a comment citing the
exact prior incident this protects against
(`docs/archive/BKL_VFS_CARVE_OUT.md` §11.4: "slots sat `TERMINATED`... spawns
eventually failed with 'No free user thread slots' while gigabytes of RAM
were free"). **That fix was never applied to the `clone_thread` path** —
i.e. to real `pthread_create`, the exact call musl/Rust's `std::thread::spawn`
makes on this target.

With `MAX_THREADS = 64` / `RESERVED_THREADS = 8` (`src/config.rs:78,87`)
and a ~10ms-minimum per-slot cooldown (`[Cleanup] Thread N recycled after
…us cooldown`, observed 21-107ms in this session's logs), a tight creation
loop trivially outruns collection and hits the wall long before 64 threads
are ever concurrently alive — consistent with hitting it at iteration 58
with several slots already consumed by background system threads
(herd/httpd/sshd/the SSH session itself).

**Not yet confirmed whether this is the same bug as the SIGABRT or the
lost-wakeup hang** — `EAGAIN`-from-`pthread_create` is a different
observable than either (a clean `Result::Err`, not a corrupted TLS pointer
or a stuck futex), so it's plausibly an *additional*, independently-real bug
in the same subsystem rather than the root cause of the other two. It is
directly actionable regardless: give `spawn_user_closure_initializing` the
same reclaim-and-retry fallback `spawn_user_thread_fn_internal` already
has. Next: apply that fix, re-run `futextest_c`/`futextest_rs` phase 2 to
confirm it clears, then see whether it also changes the `-j4` self-host
build's failure rate/signature.

### Fixed and A/B-verified (2026-08-03)

Applied, but **one level up from where the previous section pointed**. The
retry could not go inside `ThreadPool::spawn_user_closure_initializing`
itself: that method runs with the `POOL` lock held (its only caller takes it
at `mod.rs:837`), and `reclaim_terminated_slots` →
`cleanup_terminated_internal` takes `POOL` itself (`mod.rs:1202`, and again
at `mod.rs:1245` on the size profile). Calling it from inside the method
would have self-deadlocked on the non-reentrant spinlock. The fallback
therefore went into the wrapper `threading::spawn_user_thread_initializing`
(`crates/akuma-exec/src/threading/mod.rs:826-864`), which is *outside* the
lock and is also the single funnel all three exhaustion-prone callers pass
through — `fork_process` (`process/mod.rs:2494`), `vfork_process`
(`process/mod.rs:2653`) and `clone_thread` (`process/mod.rs:2782`) — so one
site covers fork, vfork and `pthread_create` alike. The shape matches
`spawn_user_thread_fn_internal` (`mod.rs:3322-3333`) and
`spawn_system_thread_fn` (`mod.rs:3172-3183`) exactly: miss →
`reclaim_terminated_slots()` → retry once → still-miss → fail. Duplicated
rather than factored into a shared helper, deliberately: the three call
sites claim from different ranges and under different lock disciplines.

A/B on one VM (`INSTANCE=1 DISK=disk_selfhost.img MEMORY=8192 SMP=4
SNAPSHOT=0`, guest fetches the stripped static binaries over
`curl http://10.0.2.2:<port>/…` — note this rootfs has **`curl`, not
`wget`**, and no `scp`/SFTP):

| | `FUTEXTEST_PHASE=2 /tmp/futextest_c` |
|---|---|
| before (HEAD as of §"Confirmed root cause") | `[2] pthread_create FAILED at iter 68: rc=11 (Resource temporarily unavailable)`, exit 1 |
| after | `[2] ok` → `=== FUTEXTEST_C DONE — all phases passed ===`, exit 0 |

The pre-fix failure iteration is **not** fixed at 58 — this session's
baseline run on the same binary failed at **68**. It moves with how many
slots background system threads (herd/httpd/sshd/the SSH session) hold and
how recently they churned, which is what a collection-rate bug should look
like; the *deterministic* part is that a 200-iteration tight loop always
hits the wall well before 200.

Full suites after the fix, both binaries, all 7 phases `ok` and exit 0:
`/tmp/futextest_c` (pure C, musl `pthread`) and `/tmp/futextest_rs` (Rust
`std::thread`, cross-compiled `rustc --target aarch64-unknown-linux-musl -O
-C linker=aarch64-linux-musl-gcc userspace/selfhost_repro/futextest.rs`).
Phases 3-7 (fan-out, mutex+condvar, barrier, wake-before-wait, park/unpark)
were already passing before the fix and still pass — no regression in the
futex paths from reclaiming on the spawn path.

**Relationship to the two still-open bugs: still unconfirmed, and this fix
does not close them.** Neither the `-j4`-only `SIGABRT` ("current thread
handle already set during thread spawn") nor the lost-wakeup hang was
re-tested here. A weak argument for a link: a spawn that returns `EAGAIN`
and one that lands on an under-recycled slot are both symptoms of slots
being reused/refused under churn, and this path is now the one that
*reclaims* before spawning. A stronger argument against: `EAGAIN` is a clean
`Result::Err` that never constructs a thread at all, whereas both open bugs
involve a thread that *did* start and then observed inconsistent state. Treat
them as independent until a `-j4` run says otherwise.

## Reproducing this session end-to-end

```bash
# host: kernel already at HEAD, rebuild if needed
scripts/build_devbox_smoltcp.sh

# host: disk prep (only needed once; skip if disk_selfhost.img already has
# herd/sshd/cargo/git — verify by booting once and checking `busybox ls`)
DISK=disk_selfhost.img scripts/populate_disk.sh --with-apk --with-musl-dev --with-rust-toolchain
DISK=disk_selfhost.img scripts/populate_disk.sh --overlay overlays/devbox/rootfs
docker run --rm --privileged -v "$(pwd)/disk_selfhost.img:/disk.img" alpine:latest sh -c \
  'mkdir -p /mnt/disk && mount -o loop /disk.img /mnt/disk && apk --root /mnt/disk --no-scripts add cargo git && sync && umount /mnt/disk'

# host: boot instance 1, snapshot off
INSTANCE=1 DISK=disk_selfhost.img MEMORY=8192 SMP=1 SNAPSHOT=0 \
  cargo run --profile release-smp-shared --features devbox-smoltcp,no-tests > selfhost_vm.log 2>&1 &

# host: serve local HEAD instead of relying on GitHub being up to date
touch .git/git-daemon-export-ok
git daemon --base-path=/Users/netoneko/github.com/netoneko --export-all --reuseaddr --port=9418 \
  /Users/netoneko/github.com/netoneko/akuma &

# guest (once booted): clone from the host, not GitHub
ssh -p 2322 root@localhost 'git clone --depth 1 git://10.0.2.2:9418/akuma /root/akuma'

# guest: the driver from here (manifest strip + build), with -p akuma fixed
python3 scripts/selfhost_driver.py 2322
```

## Background

Builds on `AKUMA_SELF_HOSTING.md` (the original self-host bring-up, June
2026) — read that first for the toolchain-split rationale, the `cd`-doesn't-
propagate-through-exec quirk, and the icache-coherency root cause behind the
earlier "x8 race" crashes. See also `docs/archive/EXECVE_STACK_LEAK_OOM_HANG.md`
for the heap-leak bug this session was retesting against.

---

# Retest after the futex-divergence fixes (2026-08-04)

Rerun of the whole experiment at HEAD `b4e641b` ("more fixes", the five-op futex
audit closure documented in `FUTEX_REQUEUE_LOST_WAKEUP.md`), same recipe as
"Reproducing this session end-to-end" above but `SMP=4` + `cargo -j4`, guest
cloning local HEAD from the host `git daemon` (§6 method). Disk was already
prepared; §§1-4 did not recur.

**Result: FAIL. Both open issues reproduce unchanged. The futex fixes are
real and correct, and they are not the cause of either.**

## The futex fixes verified good — and exonerated

`userspace/forktest/c_stress/futexops` (unchanged binary, only its comments
moved in `b4e641b`) run in-guest:

```
=== FUTEXOPS DONE — 0 divergence(s) from Linux ===
```

5 FAIL → 5 PASS against the same probe that produced the original audit table.
`futextest` (7 phases, both the C and Rust builds) also passes end to end on an
idle VM. So the requeue/bitset/EFAULT/WAKE_OP work is confirmed landed — and
confirmed *not* to be what stalls the self-host build.

## The `-j4` build still dies the same way

Failed at +121s, with the **same** abort as 2026-08-03 but on different crates:

```
fatal runtime error: current thread handle already set during thread spawn, aborting
error: could not compile `proc-macro2` (build script)
error: could not compile `const-oid` (lib)
```

Previously this hit `quote`. Two different crates this run, so it is a **race,
not a crate-specific defect** — worth recording because the original entry could
be read as implicating `quote`'s build script specifically.

Two rustc processes then hard-stalled while cargo drained (`typenum` again,
byte-identical `[PSTATS]` syscall counts across 118s→208s, host QEMU at 3.2%),
so the build never returned. Same signature as "Open issue #2".

## The useful outcome: a minimal repro with no rustc, no cargo, no clone

The stall reproduces from **8 concurrent copies of a ~100-line Rust program**:

```bash
# host: cross-compile the existing probe
rustc --target aarch64-unknown-linux-musl -O -C linker=aarch64-linux-musl-gcc \
      -C target-feature=+crt-static userspace/selfhost_repro/futextest.rs -o futextest_rs
# guest: 8 at once, phase 2 only (the 200x spawn/join loop)
for i in $(seq 0 7); do
  busybox nohup busybox sh -c 'busybox env FUTEXTEST_PHASE=2 /tmp/futextest_rs' &
done
```

One instance alone passes all 7 phases. **Eight at once wedged 7 of 8.** Every
wedged process had only its main thread left, parked in `FUTEX_WAIT_PRIVATE`:

```
tid=13 st=? pid=1469 tgid=1469 sc=98 tsc=98 a0=0x21824ad8 a1=0x80
tid=23 st=? pid=1891 tgid=1891 sc=98 tsc=98 a0=0x21824ad8 a1=0x80
```

Note pid 1469 and pid 1891 — two unrelated processes — parked on the **identical
VA**. Akuma has no ASLR, so every copy of a binary lays out identically; that is
expected, and harmless *provided* the futex key includes the thread group.

**Caveat that matters for anyone using this repro: it is timing-sensitive.**
Later 8× runs passed 8/8 on kernels that differed only by added logging. Any
claim of "fixed" needs many trials, not one green run.

## Two wrong-key hypotheses, both disproven by measurement

The natural reading of the above is a futex key-namespace collision, via one of
the two silent fallbacks on `crates/akuma-exec/src/process/children.rs:290`:

1. `futex_key_tgid` (`src/syscall/sync.rs`) → `read_current_pid().unwrap_or(0)`,
   which drops a PRIVATE futex into the shared `(0, uaddr)` bucket keyed by VA
   alone — where, with no ASLR, N copies of one binary share one queue.
2. `read_current_pid` → `with_process(pid, |p| p.tgid).unwrap_or(pid)`, which
   degrades tgid → the thread's **own** pid. `get_process_ptr_inner`
   (`process/table.rs:341`) only matches slots in state `ACTIVE`, so this is
   reachable in the window between `unregister_process`'s ACTIVE→RETIRED CAS and
   the thread's last instruction. It would bite non-leader threads only — which
   matches the archive's stalled `typenum` threads (`pid=144 tgid=137`) exactly.

Both were instrumented (a rate-limited `[futex] WARNING` at the first site, an
`[identity] WARNING` + `TGID_RESOLVE_MISSES` counter at the second) and both
fired **zero times** — across boot, an 8× run, and a 16× run that included
stalls, EAGAIN panics and ~1600 thread creations.

**So neither fallback triggers on these workloads, and the earlier reasoning in
this session that blamed them was wrong.** Recorded because the argument is
seductive and someone will re-derive it: `VFORK_FASTPATH_ENABLED` is `true`, so
`THREAD_PID_MAP` is consulted first and *hits* for every registered user thread,
returning before `unwrap_or(0)` is reachable at all. Reaching it needs a map miss
**and** boot TTBR0 or a zero pid page — which an EL0 futex syscall does not have.
The `tgid=0` waits visible in a `FUTEX_DBG_ENABLED` trace are ordinary
**non-private** ops (`op & 128 == 0`), which key to 0 by design. Do not read a
`tgid=0` WAIT line as evidence of degradation; the `WAKE` trace line now prints
its `tgid` so the two sides of a key can actually be compared.

## What *was* found: per-slot state inherited across thread-slot recycling

Auditing every `[_; MAX_THREADS]` array against what each slot transition clears
turned up three **different** scrub lists that had drifted apart:

| path | cleared |
|---|---|
| `claim_free_slot` (`threading/mod.rs:708`) | signal mask, restore-mask pending, trap frame, BKL tag |
| direct claim in `ThreadPool::spawn_user_closure_initializing` | **trap frame only** |
| `cleanup_terminated_internal` | pending signals, pending kill, preemption counters, trap frame, sigaltstack, context |

The middle row is the path **every real `pthread_create` takes** (`clone_thread`
→ `spawn_user_thread_initializing` → this claim). So a cloned thread inherited
the previous slot occupant's `THREAD_SIGNAL_MASK` and
`THREAD_RESTORE_SIGMASK_PENDING`, plus nine other registers no list cleared:
`WOKEN_STATES` (the sticky wake flag), `USER_COPY_FAULT_HANDLER` (a fixup address
into a syscall frame that no longer exists), `WAKE_TIMES`, `LAST_SIGCHLD`,
`THREAD_CURRENT_SYSCALL`, `LAST_CORE`, `TOTAL_CPU_TIMES`, `PREEMPTION_DISABLED_AT`.

An inherited blocked signal mask is a plausible mechanism for the
jobserver-`SIGUSR1` storm already documented under "Noise encountered" above: a
signal the new thread never blocked is silently never delivered, the helper's
blocking `read()` is never interrupted, and all 100 `tkill`s fire. **Not proven
to be that bug's cause** — recorded as a mechanism that now cannot happen.

The stale diagnostic registers deserve their own warning: `THREAD_CURRENT_SYSCALL`
and `LAST_CORE` feed `[THR-DUMP]`/`[PSTATS]`, so before this fix a freshly
claimed slot could *display* the previous occupant's syscall number — i.e. the
evidence used to diagnose hangs in this very document could be stale.

**Fix:** one `#[inline] fn scrub_thread_slot(i)` in `threading/mod.rs`, called
from both claim paths and once more before a slot returns to `FREE`, so the lists
cannot drift again. Deliberately excluded: `THREAD_STATES` (the caller's CAS owns
it), `IS_IDLE_THREAD` (a permanent property of idle slots), `ON_CPU`,
`THREAD_CONTEXTS`. 163 host unit tests pass; clippy clean.

**This is not claimed to fix the stall.** The 8× repro passed 8/8 after it, but
also passed 8/8 before it on a differently-instrumented build.

## Thread-slot capacity, measured

New probe `userspace/forktest/c_stress/threadmax.c` separates the two questions
that both surface as `EAGAIN` from `pthread_create`:

```
[A] simultaneous live threads reached: 51 (+1 main)
[A] stopped by: rc=11 (Resource temporarily unavailable)
[B] 400x sequential spawn/join: ok
=== THREADMAX DONE — ceiling=51, churn ok ===
```

So one process can hold **51** threads at once against a kernel ceiling of 56
(`MAX_THREADS=64` − `RESERVED_THREADS=8`); the missing 5 are herd, httpd, sshd
and the SSH session. Churn is healthy — the 2026-08-03 reclaim-and-retry fix
holds at 400/400.

**But the usable ceiling under load is far below 56, and not because of
concurrency.** New `[threads]` census logging on the exhaustion path:

```
[threads] slots exhausted (live=13 terminated=43 ceiling=56) — reclaimed 39 and retrying
[threads] slots exhausted (live=22 terminated=34 ceiling=56) — reclaimed 31 and retrying
[threads] slots exhausted (live=30 terminated=26 ceiling=56) — reclaimed 20 and retrying
```

Only 13 threads were *running* while **43 of 56 slots were corpses**. A dead
thread's slot stays `TERMINATED` for at least `THREAD_CLEANUP_COOLDOWN_US`
(10 ms, `src/config.rs:603`) and only becomes `FREE` when something runs a
reclaim pass. Effective capacity is therefore

```
usable ≈ ceiling − (thread deaths/sec × 0.01s)
```

16 concurrent copies of the repro hold at most 32 live threads — comfortably
under 56 — yet produce enough deaths per 10 ms window to fill the pool with
corpses, and three of them died with

```
failed to spawn thread: Os { code: 11, kind: WouldBlock }
```

That is genuine exhaustion, so **16× was a bad repro configuration** — its
failures confounded slot starvation with the stall under investigation. (Both
constraints below were lifted later the same day; 16× is now clean.)

## Follow-ups the same session

### `FUTEX_WAITERS` never dropped a dead thread's tid

`futex_remove_tid_anywhere` is called only from the waiter's **own** timeout/EINTR
path (`sync.rs:491,532`). A thread that dies while parked — `exit_group` killing
siblings, a consumed `PENDING_KILL`, a fault-kill — never runs it, so its tid
stayed queued forever. Once the slot recycled, that entry named a *live,
unrelated* thread: `futex_do_wake` pops it, wakes the new occupant, and counts it
toward `max_wake`, so a `FUTEX_WAKE(uaddr, 1)` is consumed while the real waiter
stays parked. Same "stale entry absorbs a wake" defect the requeue fix closed for
requeued waiters, left open for dead ones — and invisible to `futexops`, which
never kills a parked thread.

Fixed with `futex_purge_tid`, registered via a new
`threading::set_slot_purge_callback` hook (the kernel tables are unreachable from
`akuma-exec`) and called at **both** ends:

- `mark_thread_terminated` — immediately, because the slot stays TERMINATED for
  ≥10 ms and often far longer, and for that whole window a dead tid is still a
  live wake target. Safe to drop this early precisely because a queue entry is of
  no further use to a terminated thread, unlike its trap frame/stack/sigaltstack.
- the recycler — catches anything that reached TERMINATED via a leaf that skipped
  the first call.

`DRAINING[tid]` (`process/reclaim.rs`) had the same shape — set on entry to
`drain_retired`, cleared on the way out, and its own docs admit the terminal
site "runs on an already-terminated thread" and can be preempted mid-sweep. A
recycled occupant then took the "already draining" early return forever. Cleared
in `scrub_thread_slot`.

**Why not scrub at every termination leaf instead?** Because the leaves are the
unreliable place — the three abandoned-stack leaves (§4 of
`../reference/subsystems/thread-lifecycle.md`) end a kernel stack without
unwinding, so a fault-killed or peer-killed thread never runs code placed there;
a terminating thread is still *using* its trap frame and stack (the recycler's
`CURRENT_TRAP_FRAME` clear must precede `free_stack_for_slot`); and a peer killer
running on another core would race a victim that may still be executing. The rule
that came out of it: **scrub slot registers at the ownership boundary, purge
external registrations at death.**

### The thread ceiling was a stale duplicated constant, not memory

`compute_thread_limit` (`src/main.rs:389`) already scales the pool from RAM —
¼ of user pages ÷ `USER_THREAD_STACK_SIZE` — then clamps to `MAX_THREADS`. On an
8 GB devbox it produced a far larger figure and clamped, so **RAM was never the
constraint**.

Raising `config::MAX_THREADS` alone did nothing, and the failure was silent and
instructive: boot logged `Thread limit: 256 slots` while `[threads]` census lines
kept printing `ceiling=56`. `akuma-exec` carried its *own*
`threading/types.rs:11` `MAX_THREADS = 64` — the real static-array size and the
value `set_thread_limit` clamps against — with only a "must match" comment
binding them. `config::MAX_THREADS` is now a `pub use` re-export of the crate
constant, so they cannot diverge again, and the profile split lives at the
definition: 256 normally, 64 under `kernel_profile_size` (4 MB floor).

Measured effect on `MEMORY=8192 SMP=4`:

| | before | after |
|---|---|---|
| `threadmax` phase A ceiling | 52 | **244** |
| 16× `futextest_rs` phase 2 | 3 × `EAGAIN` panic, stalls | **16/16 exit 0** |
| `.bss` (release-smp-shared) | `0x8ff40` | `0x98580` (**+33 KB**) |
| `size` profile | 64 slots | unchanged |

Three host tests hard-coded the old 64 (`constants_sanity` and two
`calculate_stack_requirements` cases); they now derive from `MAX_THREADS`.

### Rebuild after all of the above: still fails, identically

Fresh clone, `-j4`, kernel carrying the slot scrub, the futex purge hook and
`MAX_THREADS=256`. **Same abort, same +121s, fourth distinct crate**
(`unicode-ident`; previously `quote`, `proc-macro2`, `const-oid`) — conclusively
crate-agnostic. It reached 9 `Compiling` lines instead of 7, which is race
jitter, not progress.

`uname` did confirm the build-identity work is live:
`Akuma akuma 0.0.7 8b82119-release-smp-shared aarch64 Linux`.

**Thread exhaustion is ruled out for this run.** The only two exhaustion events
in an 84 k-line log are at lines 1546-1547, and the build's cargo does not appear
until line 7962 — they belong to a `threadmax` probe run earlier in the same boot.
Peak during the build was ~40 live threads against a 248 ceiling. The two failure
modes are also distinguishable by their symptom: slot exhaustion surfaces as Rust's
`failed to spawn thread: Os { code: 11 }` **panic (exit 101)**, which is exactly
what the 16× `futextest_rs` runs produced; this is an **abort (signal 6)**.

**After the abort, cargo hangs forever.** Every rustc child exits, and cargo sits
in "waiting for other jobs to finish" indefinitely with both of its own threads
parked in `FUTEX_WAIT_BITSET|PRIVATE` and nothing left to wait for. `/tmp/build.rc`
is never written. So one `-j4` run now reproduces **both** open issues in ~2 minutes
— previously the stall needed a multi-hour build.

### Where the abort comes from in Rust, exactly

`rtabort!("current thread handle already set during thread spawn")` is
`library/std/src/thread/lifecycle.rs:162`, in `ThreadInit::init`, which runs **on
the freshly spawned OS thread before its closure**. It fires when `set_current`
(`library/std/src/thread/current.rs:121`) returns `Err`, and there are *two* ways
for that — the second is easy to miss:

```rust
if CURRENT.get() != NONE { return Err(thread); }   // TLS pointer non-null
match id::get() {
    Some(id) if id == thread.id() => {}
    None => id::set(thread.id()),
    _ => return Err(thread),                        // stale thread-id in TLS
}
```

Both read thread-local storage. musl's `__copy_tls` on aarch64 memcpy's only
`.tdata` and relies on the fresh `mmap(MAP_ANONYMOUS)` behind `.tbss` arriving
zeroed. So the abort reduces to a kernel-contract question: **did a new thread
read non-zero from storage that had to arrive as zeros, or did its TPIDR_EL0 aim
at another thread's block?**

### `tlsdirty.c` — the contract probe, and four falsified hypotheses

New probe `userspace/forktest/c_stress/tlsdirty.c` tests that contract with no
Rust runtime: every spawned thread asserts its own `.tbss` reads zero, scribbles a
sentinel, and exits, so a VA recycled without re-zeroing is caught by name; a
fan-out phase publishes `&tls_current` per live thread to detect TLS aliasing.
Phases: 2000× sequential churn, 24-way fan-out, and churn-under-fan-out.

**On a clean boot it passes: `dirty_hits=0 alias_hits=0`, exit 0.** So on an idle
system Akuma delivers zeroed TLS and never aliases it.

Four hypotheses were tested and **falsified** — recorded so they are not re-derived:

| hypothesis | test | result |
|---|---|---|
| Dirty/aliased TLS on a fresh thread | `tlsdirty`, clean boot | zero hits |
| Thread-slot exhaustion causes the abort | log line numbers + symptom | exhaustion → exit 101 panic, not signal 6 |
| A neighbouring Rust abort misdelivers its signal/stderr | `boom` (deliberate `std::process::abort` from a thread) × 400 run alongside `tlsdirty` | `tlsdirty` exit 0, zero `BOOM-MARKER` leakage |
| Deleted-file pages leak into fresh files | write 200 KB marker file → delete → create small file, ×6 | correct sizes, zero marker hits |

### RESOLVED: the "foreign abort text" was a filesystem bug, and it faked a SIGABRT

Run on a VM that had already run a *failing* build, the pure-C `tlsdirty` and
(earlier, separately) the pure-C `futextest` both appeared to exit **134
(SIGABRT)** with fragments of the *rustc* abort text in their own redirected
output files. Neither C binary contains that string (`strings | grep -c` = 0).
That looked like signal/stderr misdelivery. **It was not. Nothing was
misdelivered, and the C programs never aborted.**

Isolated with a `GDB=1` boot that reproduced the `-j4` jam **without any abort
occurring in that boot at all** (0 `fatal runtime error` in the build log) — so
no live process could possibly have produced the string. `tlsdirty` was then run,
and while it was **still running** (`ps` showed it alive, its `.rc` file never
written) its output file already read:

```
fatal runtime error: current thread handle already set during thread spawn, aborting
 each time
```

`" each time"` is the tail of *tlsdirty's own* line "…checking TLS is zero each
time". The abort line is 85 bytes, that tail is 11 — exactly the 96 bytes the
file reports. So the first 85 bytes are foreign and only the tail is the real
writer's.

**The harness assumption was wrong first: `/tmp` is on the persistent ext2 disk,
not tmpfs.** With `SNAPSHOT=0` the image survives reboots, and `/tmp` still held
`build.out` from an earlier boot containing that exact line (verified: it is line
10 of that file). Every conclusion drawn from a `.out`/`.rc` file in this
document was therefore drawn from storage that is **not** cleared between runs.
That alone explains the misreadings.

**What is left over and NOT explained** — stated as an open anomaly, not a root
cause, because it has not been isolated: `jam.out` was a *freshly created* inode
(6935, made after an `rm -f`) whose first 85 bytes were content no process in
that boot wrote, with the writer's own bytes appearing only from offset 85. It is
not live block sharing — `build.out` (inode 22) is byte-intact at 1166 bytes with
no `tlsdirty` text in it. Simple create/delete/create and truncate-on-open cycles
do **not** reproduce it. Whether that is stale block content, a size/offset
bookkeeping bug, or an artifact of the long-running writer is **unproven**; do
not cite it as a known filesystem defect without reproducing it deliberately.

**The consequence that matters for every earlier conclusion in this document:
`rc=134` was almost certainly stale `.rc` content too.** The wrapper writes
`echo $? > /tmp/<tag>.rc`, so those files are subject to the same defect, and a
`134` written by a genuinely-aborting run can resurface in a later run's `.rc`.
In the isolated case above the process was demonstrably alive with an empty
`.rc`. **So the entire "pure C programs get SIGABRT under load" thread was a
measurement artifact of this filesystem bug, not a kernel signal defect.**

Four other hypotheses had already been falsified before this one landed (dirty
TLS, slot exhaustion, live abort misdelivery, naive block reuse) — see the table
above. The lesson for future passes: **on this rootfs, `/tmp` is disk-backed and
persistent; never trust an output or `.rc` file as evidence without checking its
size against what the writer actually emitted**, and prefer a fresh disk or a
unique filename per run.

**Still open, and not touched by this:** the rustc `-j4` abort itself is real —
cargo reports `signal: 6` for a specific rustc it spawned, which is its own wait
status and not file content. And the jam is real and now reproducible *without*
the abort (see above), which proves the two are independent defects.

### Signal-mask inheritance was missing on fork/vfork, and racy on clone (2026-08-04, FIXED)

Found by auditing every thread/process creation leaf for mask correctness after the
crash evidence kept landing in the post-fork, pre-exec window.

**Why it hid.** There are *two* masks. `Process.signal_mask` **is** inherited on fork
(`process/mod.rs:1927/2622/2755`), so the code reads as correct at a glance — but signal
delivery deliberately does not use that field. `exceptions.rs:1246` says so outright:
*"per-thread mask (NOT proc.signal_mask, which is shared across CLONE_THREAD siblings)"*.
The authoritative per-thread mask (`THREAD_SIGNAL_MASK`, what `sys_rt_sigprocmask`
reads and writes) was **never seeded on fork**, and `claim_free_slot`/`scrub_thread_slot`
zero it on slot reuse — so a forked child started with **everything unblocked**.

That is a straight Linux/POSIX divergence (fork inherits the mask), and it is
load-bearing for `Command::spawn`: the runtime blocks every signal in the parent
immediately before forking precisely so the child cannot take one in the pre-exec
window, where its handler state and sigaltstack are not yet valid — the window that
produces `[signal] sig N needs sigaltstack but slot M has none — re-pending`. Akuma
reopened exactly the window the caller paid a syscall to close.

**Second defect, same audit: the one seeding that did exist was racy.** `clone_thread`
called `mark_thread_ready(tid)` (`process/mod.rs:2836`) *before returning*, and
`sys_clone` seeded the mask afterwards (`syscall/proc.rs:445`). On SMP the child could
already be executing with a zeroed mask before the seed landed.

**Fix — seed at every creation leaf, before the child is runnable:**

| leaf | before | after |
|---|---|---|
| `fork_process` | per-thread mask never seeded | seeded from parent before `mark_thread_ready` |
| `vfork_process` | never seeded | same |
| `clone_thread` | seeded *after* readiness | seeded before readiness; the syscall-layer seed is now an idempotent repeat |
| `execve` | untouched | verified correct — POSIX preserves the mask across exec |
| slot claim / recycle | zeroed | correct as a baseline, since every creator now seeds |

163 host tests pass, clippy clean.

**It did not fix the `-j4` build.** The build still jams, and a rustc still dies at
0.04 s. Recorded as a genuine ABI bug fixed on its own merits, not as the self-host cure.

### The crash has a fixed address (2026-08-04)

Across two independent boots the fault is **byte-identical**:

```
FAR=0x0  ELR=0x300204e8  ISS=0x47  x0=0x0  x3=0x8  x30=0x30020498
[Fault] Process N (/usr/local/bin/rustc) SIGSEGV after 0.02-0.05s
[Fault] SIGSEGV in clone_thread, calling exit_group
```

Same faulting PC, same link register, same NULL dereference. `last_sc` is the
"no syscall" sentinel, so the thread faults **before issuing a single syscall** — i.e.
at thread startup — with `TPIDR_EL0` already valid. Every victim is 0.02-0.05 s old.

This is the same family as the `-j4` SIGABRT (`current thread handle already set during
thread spawn`): both say *a newly created thread's startup state is not what it should
be*. One reads a NULL where a pointer belongs; the other reads a non-zero where zero
belongs.

A later run produced **three** faults in one build and refined the picture — there are
*two* recurring PCs, and at one of them the faulting address varies:

```
FAR=0x7  ELR=0x3801c58c   pid 60, SIGSEGV after 0.03s
FAR=0x5  ELR=0x3801c58c   pid 61, SIGSEGV after 0.02s
FAR=0x0  ELR=0x300204e8   pid 62, SIGSEGV after 0.03s
```

**That rules out a corrupted pointer.** Garbage pointers are large; `0`, `5`, `7` are
small integers — the shape of an **errno, an fd, or a count being used where a pointer
belongs**. So the failing code is reading a value that is *legitimately* a small integer
in some other context, i.e. an unchecked error return or a union/tag confusion, rather
than memory that was scribbled on.

Worth noting what this exonerates: Akuma's errno convention is correct
(`const EINVAL: u64 = (-22i64) as u64`, `syscall/mod.rs:409`), so this is not the classic
"positive errno read as success" ABI bug.

**Next step, and it is cheap:** symbolize `0x300204e8` and `0x3801c58c` in the guest's
libstd/musl. Two addresses, both recurring across boots, both in threads younger than
0.05 s. That names the exact code and turns a kernel-side guessing game into a specific
question. No session has ever had a live PC for this failure; now there are two, and they
are reproducible in under two minutes.

**Every victim is a brand-new thread** (`SIGSEGV in clone_thread`, `last_sc` = the
"no syscall yet" sentinel), which is the same statement the `-j4` SIGABRT makes:
*a newly created thread's startup state is not what it should be*. One victim was even
named `coordinator` — rustc's parallel-codegen coordinator thread.

### ASID free/flush ordering (2026-08-04, FIXED — but not the cure either)

Chased because every victim is 20-50 ms old, i.e. 2-5× `PROCESS_RECLAIM_COOLDOWN_US`
(10 ms) — the window on which `Process::drop` runs and recycles resources, **including
the ASID** (`docs/reference/subsystems/memory.md`).

`Drop for UserAddressSpace` (`mmu/mod.rs`) did this:

```rust
with_irqs_disabled(|| ASID_ALLOCATOR.lock().free(self.asid));
flush_tlb_asid(self.asid);                      // ← after the free
```

`AsidAllocator::alloc` (`mmu/asid.rs:20`) only flips a bit — it performs **no** TLB
maintenance — so that `flush_tlb_asid` is the sole invalidation for the dying address
space. Freeing first opens an SMP window: a peer core can `alloc()` the same ASID,
install it in TTBR0 and start executing while the dead space's translations are still
live, so the new owner reads the **dead process's memory** through them. That produces
plausible-looking junk rather than obvious garbage — precisely the small integers
(`0`, `5`, `7`) seen in `FAR`. Fixed by flushing before freeing; `tlbi aside1is` is
inner-shareable so peers are covered.

**Also fixed: ASID exhaustion was silent.** `alloc()` returning `None` propagated through
`?`, so address-space creation simply began failing with nothing in the log. With
`MAX_ASID = 256` and a build spawning thousands of processes, one missed
`UserAddressSpace::drop` leaks an ASID permanently. Now rate-limit-logged as
`[asid] EXHAUSTED`. Measured on a full `-j4` run: **zero occurrences** — so ASIDs are
not leaking today, and the diagnostic stands as a tripwire.

**Neither change fixed the build.** A run immediately after still failed with two
`SIGSEGV`s at the same two PCs. Both are recorded as correctness fixes on their own
merits, not as the self-host cure — the same standing as the signal-mask work above.

### The jam: a child dies and the parent never finds out (2026-08-04) — SUPERSEDED

*The reasoning in this section was wrong; kept because the measurements are still
useful and the retraction is instructive. See the correction at the end.*

The `GDB=1` boot that jammed with **zero aborts** gives the clearest picture yet.

Exactly one fault occurred in the whole boot:

```
[T47.12] [Fault] Data abort from EL0 at FAR=0x0, ELR=0x300204e8, ISS=0x47
[Fault]  x0=0x0 x1=0x16667f908 x2=0x0 x3=0x8
[Fault] Process 79 (/usr/local/bin/rustc) SIGSEGV after 0.02s
[Fault] SIGSEGV in clone_thread, calling exit_group
```

A rustc NULL-dereferenced **inside `clone_thread`**, 0.02 s into its life. Note
this is a *different* failure from the `-j4` SIGABRT — a kernel-delivered SIGSEGV,
not a userspace `rtabort!` — and it is the same signature seen once before in the
`futexdbg` boot.

**Cargo never learned.** `build.out` records 8 `Compiling` lines, **0 errors** — a
reaped `signal: 11` child always produces `could not compile … (signal: 11)`. The
build then stopped dead: no rustc alive, no new output for 20+ minutes, and the
artifact tree shows exactly where it stalled — `libcfg_if-*.rmeta` and
`libproc_macro2-*.rmeta` exist with **no matching `.rlib`**, i.e. pipelined rustc
runs that emitted metadata and then vanished mid-compile.

`[THR-DUMP]` for cargo's thread group (tgid=11) shows why it stays alive — and
**no thread is in `wait4`**:

```
tid=11 pid=26 tgid=11  sc=-1 tsc=63  a0=0x5           read() on the jobserver pipe
tid=15 pid=11 tgid=11  sc=98 tsc=98  a1=0x89          leader, FUTEX_WAIT_BITSET
tid=12 pid=73 tgid=11  sc=-1 tsc=98  a0=0x160034244
tid=39 pid=66 tgid=11  sc=-1 tsc=98  a0=0x160034244   same address as tid=12
tid=21 pid=70 tgid=11  sc=-1 tsc=98  a0=0x300c2030
tid=22 pid=80 tgid=11  sc=-1 tsc=98  a0=0x300c2030    same address as tid=21
```

So cargo is not blocked reaping — it is blocked on its own job/token accounting:
a slot is held by a job it believes is still running, its jobserver token is
never returned, and the helper's `read(fd=5)` waits forever for a token that will
never be written.

**The suspicious site, flagged not proven** (`src/exceptions.rs:3543-3550`):

```rust
let is_clone_thread = proc.address_space.is_shared();
if is_clone_thread {
    sys_exit_group_pub(-11);                       // never returns
}
notify_child_channel_exited_pub(proc.pid, -11);    // unreachable on that path
vfork_complete(proc.pid);                          // unreachable on that path
```

`sys_exit_group` does perform its own `notify_child_channel_exited` for both
`pid` and `tgid` (with a load-bearing ordering comment), so the notify is
*attempted* and this is not self-evidently the bug.

**CORRECTION — this section's conclusion is retracted.** Three independent results
killed it:

1. **`segvchild.c`** (new probe) tests the exact claim: a parent forks a child that
   dies via each path, and watchdogs its `wait4`. Cases A (normal exit), B (main-thread
   SIGSEGV) and C (**SIGSEGV inside `clone_thread`** — the suspect path) all **reap
   correctly**, and still do under 8-way concurrency × 6 rounds (48 runs, zero hangs).
2. A later `-j4` run **did** have cargo reap and report a dying rustc:
   `error: could not compile 'byteorder' … (signal: 11, SIGSEGV)`, exiting cleanly.
3. The "leader in `exit`" reading was a **field misread**. In `[THR-DUMP]`, `sc` is the
   *Process*-level `current_syscall` and `tsc` is the per-thread one
   (`threading/mod.rs:3726-3750`). The thread showed `tsc=98` with `a1=0x80`
   (`FUTEX_WAIT|PRIVATE`) and an address in `a0` — for `exit(93)` `x0` would be a small
   exit code. **All three rustc threads were parked in futex; none was exiting.**

So the reap path is sound and nothing was orphaned by an `exit`/`exit_group` asymmetry.
What the dumps do show is the original "Open issue #2" shape: rustc's whole thread group
parked in `FUTEX_WAIT`, one of them at `0x3cda5fc4` — the *same* address as the very
first `typenum` stall recorded in this document.

**Pipes are also exonerated**, via the new `pipe_dump()`:

```
[PIPE-DUMP] 3 live
  pipe=10 bytes=1 readers=3 writers=3 pollers=0     token available, nobody waiting
  pipe=30 bytes=0 readers=1 writers=1 pollers=1
  pipe=31 bytes=0 readers=1 writers=1 pollers=1
```

No pipe holds buffered data with a parked reader — the jobserver pipe even has a spare
token and zero pollers. `pipewake.c` (new probe: blocked-read handoff, cross-process
echo, write-before-read) passes single and 8-way. The kernel is not sitting on an
undelivered pipe wakeup.

### `threadmax` phase B: cooldown wall, not a regression

Phase B failed at iteration 0 in one run, having passed 400/400 earlier. Cause
was the probe, not the kernel: it started churning immediately after phase A
joined 52 threads, so every slot was inside its 10 ms cooldown. The probe now
settles 300 ms first and, on a refusal, retries after 100 ms and **reports which
it was** — a retry that succeeds means a transient cooldown wall, a retry that
fails means real collector starvation. Rerun: `[B] ok`, 400/400, twice, without
ever entering the retry path.

## Status of the three signatures after this session

1. `LAZY_REGION_TABLE` alloc-under-lock — still fixed.
2. Thread-spawn `SIGABRT` — **open**, unchanged, now known to be crate-agnostic.
3. Lost-wakeup stall — **open**, unchanged, but now reproducible in ~2 minutes
   from a small Rust binary instead of a multi-hour cargo build.

Instrumentation left in place for the next pass (all default-on except the
verbose tracing): `[futex] WARNING` on shared-namespace degradation, `[identity]
WARNING` on tgid-resolution miss, `tgid` on the `FUTEX_DBG_ENABLED` `WAKE` line,
`[threads]` high-water and exhaustion census. `FUTEX_DBG_ENABLED` itself is back
to `false` — note that turning it on perturbs timing enough to sometimes hide
the stall.
