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
