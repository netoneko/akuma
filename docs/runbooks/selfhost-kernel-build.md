# Self-host: compile the Akuma kernel inside Akuma

Runbook for compiling the Akuma kernel *inside* Akuma (the self-hosting
milestone). **This is NOT the devbox** — self-hosting uses the default-smoltcp
build + a nightly toolchain on a separate large disk.

> The devbox (`build-devbox.md`) is the rump-only dogfooding image with apk
> stable toolchain. Self-hosting has actually compiled the kernel (147/147
> units) and the self-built kernel boots.

## Run a build trial (current procedure, 2026-08-16)

A clean `-j4` `--release` build is now **expected green** (10/10 clean builds
on 2026-08-15, devbox-smoltcp image; recorded then as ~7–12 min each — see the
re-measurement below, which supersedes that figure). Every stochastic crash class
from the 08-05/08-07 era below has been root-caused and fixed — the chronology
lives in the archive docs linked from **Common failures**, not here. The
consequence for procedure: **stop retrying through failures. A clean-build
failure today is a regression finding — capture it.**

1. Boot (§3, or `overlays/devbox/run-smoltcp.sh` for the devbox image). Add
   `GDB=1` if you can afford it — a wedge on a VM booted without the gdbstub
   is uninspectable, and the one still-open failure (Defect A, the all-core
   wedge) has no other diagnostic path.
2. In-guest, **cwd = the manifest dir**, and **start from `cargo clean` —
   this is the trial, not hygiene.** A green *incremental* build proves
   nothing: the fingerprint cache resumes past exactly the paths the fixed
   classes exercised (proc-macro recompiles, every `.rlib` re-link, the full
   toolchain re-faulted through the file-page cache). No script does the clean
   for you — not `run_selfhost_kernelbuild.py`, `loop_selfhost_kernelbuild.py`,
   nor `j4_selfhost_campaign.py`.

   ```sh
   cd /src/github.com/netoneko/akuma && cargo clean && \
     nohup sh -c '{ cargo build --release -p akuma -j4 --offline; echo EXIT=$?; } \
       > /tmp/build.log 2>&1' &     # detached — §5.2, sshd kills long channels
   ```

3. While it runs, watch the **host-side boot log**, not just the guest:
   any of `[PMM-RESURRECT]`, `[FILL-SHORT] got=Ok(0)`, `[PMM-UAF]`,
   `[PMM-POISON]`, `[WILD-DA]` is a failed trial even if the build exits 0.

### Verify

```sh
# in-guest: EXIT=0 and a real ELF at the target-triple path
grep EXIT= /tmp/build.log                          # EXIT=0
ls -la target/aarch64-unknown-none/release/akuma   # ~4.3 MB, not 16 KB

# host-side boot log: every tripwire silent / zero
grep -ac 'PMM-RESURRECT\|PMM-UAF\|PMM-POISON\|WILD-DA' <boot.log>   # 0
grep -a 'FILL-SHORT' <boot.log>                    # no got=Ok(0) lines
grep -a 'defer_leak' <boot.log>                    # defer_leak=0
```

**On failure:** keep both logs, match the symptom against **Common failures**
below, and A/B against the parent commit before blaming your change — do not
resume-retry past it. The §5 machinery (detached runs, wedge detection, 0-byte
artifacts, reboot-through-poisoning) is still the right *mechanics* for driving
a long campaign; its framing of crashes as weather is historical.

**Between campaigns:** `e2fsck` the image (§5.5). Dozens of clean-build cycles
with hard kills accumulate real filesystem damage whose symptom — a 15-minute
boot behind a watchdog storm — impersonates a kernel regression.

### How long a trial takes — **~45 s on the devbox profile** (re-measured 2026-08-30)

> **Re-measured 2026-08-30, devbox-smoltcp profile, in-guest clean builds.**
> Config: the devbox image's own tree at
> `/src/github.com/netoneko/akuma` (`52ba7d4c`), SMP=4, MEMORY=4096, HVF, and
> the exact `build_devbox_smoltcp.sh` line:
> `cargo clean && cargo build --release --features devbox-smoltcp,no-tests -j4 --offline`.
> Three consecutive trials: **44.51 / 44.63 / 44.75 s** (±0.12 s), each clean
> removing 543 files / 79 MiB and rebuilding it — real work, not a cache resume;
> ELF 2,690,480 B. A same-day run on the identical configuration reported 41.66 s,
> ~6 % under — inside host-load jitter. Against the 2026-08-16 baseline below the
> build portion is **~2.2× faster**, on a different configuration (this box:
> devbox image, 4 GB; the old record: `-p akuma` default features, 8 GB). Two
> adjacent facts that keep the number honest: the **default-feature** build (no
> `no-tests`) is materially slower, ~61 s — the boot suite is ~20 k lines of
> `src/`, so `no-tests` is most of the win; and a tree with today's fault-path
> changes builds in the same time, the delta being comments and two PTE re-reads.

**A whole trial is ~2m10s of wall clock**, boot + `cargo clean` + build
inclusive, and it is *stable*: five consecutive trials measured 131, 132, 131
and 132 s (the probe run before them, 127 s). The build is real work, not a
fingerprint-cache resume — it recompiles from `scopeguard` up through
`akuma-exec`, leaving 255 artifacts in `deps/` and a 3.8 MB ELF. Check that
before trusting a fast trial: a *genuinely* no-op build is also quick, and the
two are told apart by the artifact count, not the clock.

Timing is a property of the **configuration**, so record it alongside the
number. This one is:

```bash
DISK=devbox.img MEMORY=8192 SMP=4 cargo run --release --features devbox-smoltcp,no-tests
# in-guest, per trial:
cargo clean && cargo build --release -p akuma -j4 --offline
```

— the image's own `/src/github.com/netoneko/akuma`, 8 GB, four cores, `-j4`, `--offline` against a
pre-primed registry (below). The **~7–12 min** figure recorded on 2026-08-15 is
left in place at the top of this section rather than rewritten; it predates this
configuration. A trial that takes ten minutes again is therefore a finding about
the machine or the image, not a return to normal.

**Consequence for procedure: a five-trials-per-arm batch costs ~11 minutes, not
an hour, so stop treating trial count as the expensive knob.** Ten per arm is
~22 min unattended and buys real power against a stochastic class — a 1-in-5
flake is unremarkable in five samples and cannot be told from a regression.

### Give every trial its own sentinel — `/tmp` survives the reboot

The detached-run pattern (§5.2) ends the build with `echo __EX__$?` into a log
the driver polls. Across a **batch** that pattern has a trap: the devbox is not
booted in snapshot mode, so `/tmp` persists from one trial into the next, and
with `>>` the previous trial's sentinel is still sitting at the top of the file
when the next build starts appending under it.

A driver that greps for a bare `__EX__` then scores the *previous* trial's exit
code, at the first poll, and tears the VM down mid-build. Measured 2026-08-16:
one trial in ten came back `GREEN` in **33 s** — less than a boot takes — with
`Compiling byteorder` as its last log line. Two things give it away, and both
are worth asserting rather than eyeballing:

- **wall clock far below the ~2 min norm**, and
- **a stunted boot log** — 94 KB against the 554 KB its neighbours produced.

Two fixes, and use both:

1. **Per-trial log path and per-trial sentinel** (`__EX_<label>_<n>__$?` into
   `/tmp/build_<label>_<n>.log`). A stale file then cannot satisfy the check for
   a different trial, which a plain `rm -f` does not guarantee — the `rm` is one
   more ssh that can quietly fail.
2. **Make the trial prove it did the work.** A clean build must recompile the
   crate you changed and leave a full `deps/` tree, so require
   `Compiling <your-crate>` in the log **and** an artifact count in
   `target/aarch64-unknown-none/release/deps` at the expected figure. Score
   anything else `INVALID`, not `GREEN`: absence of a failure is not evidence of
   a build, and a no-op resume is fast for the same reason a stale sentinel is.

   **Measure that figure before you start the batch — do not guess it, and do
   not reuse the one below.** It is small, and it moves with the feature set:

   | build | files in `deps/` |
   |---|---|
   | in-guest `cargo build -p akuma` (default features) | **86** |
   | host `--features devbox-smoltcp,no-tests` | **95** (32 `.d` + 31 `.rmeta` + 31 `.rlib` + 1 binary) |

   Both are "one complete clean kernel build", and they differ — which is the
   whole reason to calibrate against *your* configuration rather than inherit a
   number. Do one known-good build first and count:

   ```sh
   ls target/aarch64-unknown-none/release/deps | wc -l
   ```

   A guessed threshold is not a safe over-approximation. Measured 2026-08-16: a
   `>= 200` guess, taken from a single earlier observation of 255 that was never
   one clean build of one configuration, marked **all twenty** trials of a
   10-vs-10 `INVALID` while every one of them had exited 0 and compiled
   `akuma-exec`. That batch was only salvageable because the verdict string
   carried the raw `deps=` number, so the criterion could be reapplied
   afterwards — **print the measured value in the verdict, never just
   pass/fail**, or a mis-set threshold costs you the whole run.

   Gating on the **output ELF** instead (exists, and ~3.8 MB rather than 16 KB)
   is the sturdier check where you can use it: it is the artifact you actually
   care about and it does not drift with the dependency graph.

This belongs in the same family as the Tier 3 `>>`-not-`>` rule in
[`verify-trim-fat-change.md`](verify-trim-fat-change.md) — both are cases where
the *harness* silently produced a healthy-looking answer about work that never
ran.

### Prime the cargo registry first, or `--offline` fails misleadingly

The devbox image ships `/src/github.com/netoneko/akuma` and the nightly toolchain at
`/usr/local/bin/rustc`, but an **empty** `/root/.cargo/registry`. `--offline`
then dies in *resolution* rather than compilation, naming an arbitrary
dependency:

```
error: no matching package named `arm_pl031` found
location searched: crates.io index
note: offline mode (via `--offline`) can sometimes cause surprising resolution failures
```

That reads like a broken manifest and is not. Prime once over the guest's
network and every later trial is hermetic — `cargo clean` removes `target/`,
never `CARGO_HOME`:

```sh
cargo fetch --manifest-path /src/github.com/netoneko/akuma/Cargo.toml     # ~14 MB, one time
```

Priming **before** the batch rather than letting the first build fetch is the
point: it keeps DNS/HTTP out of every trial, so a red trial means the memory
path and not the network.

## Status (2026-08-05) — the `release-smp-shared` build completes

| | `cargo build --release -j1` (§1-§4 below) | in-VM `release-smp-shared` + `devbox-smoltcp` |
|---|---|---|
| kernel *source* compiles on the host | yes — clean, clippy clean, 483 host tests pass | same source |
| in-VM build | reaches the ELF | **reaches the ELF** (2026-08-05) |
| how | — | `-j4` for the ~97 dependency crates, then **`-j1` for the final `akuma` crate** (§5) |

```sh
cargo build -p akuma --profile release-smp-shared \
    --features devbox-smoltcp,no-tests -j4      # deps
cargo build -p akuma --profile release-smp-shared \
    --features devbox-smoltcp,no-tests -j1      # final crate — see §5
```

Result, verified end to end:

```
Finished `release-smp-shared` profile [optimized] target(s) in 1m 08s
```

rc=0; artifact `target/aarch64-unknown-none/release-smp-shared/akuma`,
1,998,392 bytes, ELF64 aarch64 ET_EXEC. Extracted with `busybox base64` over
ssh, `rust-objcopy -O binary` → 1,465,088 bytes (host build of the same tree:
1,456,901), and **booted**: full boot, userspace sshd, `uname`/`uptime` over
ssh.

> The tree that was built was the guest's own checkout at **67c2c23**, 12
> commits behind the host at the time — so that particular self-hosted kernel
> predates the `mprotect` fix in [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md)
> §2b. Check `.git/HEAD` in the guest before claiming a self-host build of a
> given commit; the kernel prints the hash in `uname -a`.

> **Superseded 2026-08-15** — the crash classes this paragraph described have
> since been fixed; see "Status (2026-08-15)" above. Kept as written below for
> the record of what the 08-05 runs looked like.

The two crash classes below still fire during the dependency phase and still
cost whole crate compiles — they are **not** fixed, they are only survivable by
retrying (§5). See [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md).

What changed at the 2026-08-04 futex key-namespace fix
([`debug-futex-lost-wakeup.md`](debug-futex-lost-wakeup.md) §5):

| | before | after |
|---|---|---|
| cross-process futex wake leak | `woken=1` (deterministic FAIL) | `woken=0` (PASS, matches Linux) |
| first failure mode | hung forever, no error | fails in ~40 s with a real `signal: 11` cargo error |
| how far the build gets | wedged at the final crate / early deps | through the dep graph to `ecdsa`/`heapless`/`ghash` |
| `[FUTEX-ORPHAN]` lines | present | **zero** — the "parked ⇒ queued" invariant holds throughout |

So the futex layer is doing its job. The wedged waiters that remain are musl
`pthread_join` parked on `detach_state` (`0x3d90f5e8`/`0x3d90b5e8`) — i.e.
**joining threads that died**, killed by the thread-spawn SIGSEGV. Diagnose from
[`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md), not from the futex
table.

Two traps when measuring progress here: a `Compiling`-line stall heuristic is
not a liveness signal (use `/proc/<pid>/syscalls` trace liveness,
[`debug-futex-lost-wakeup.md`](debug-futex-lost-wakeup.md) §0), and a build that
dies on a SIGSEGV'd rustc still *advances* — crates that compiled stay
compiled — so "it got further" is only meaningful against the failing crate, not
the count.

## Status (2026-08-07) — `--release` at `-j4` completes, twice

The `--release` (default-feature) kernel build now runs to completion in-VM at
`-j4`, **including the final `akuma` crate** — no `-j1` step:

```sh
cargo build --release -p akuma -j4 --offline     # disk_selfhost.img, SMP=4, MEMORY=14336
```

> **The artifact is at `target/aarch64-unknown-none/release/akuma`, not
> `target/release/akuma`.** With `CARGO_BUILD_TARGET` set (or the tree's
> `.cargo/config.toml` `[build] target`), `target/release/` holds only build-script
> output, so "the build never produced a binary" is usually just this. And **run the
> build with cwd = the manifest dir**: cargo discovers `.cargo/config.toml` and
> `rust-toolchain.toml` by walking up from the *process cwd*, never from
> `--manifest-path`. From a foreign cwd you silently lose `-Clink-arg=-Tlinker.ld`
> and get a 16 KB ELF with `entry=0x0` and no `.text` — with `Finished` and exit 0.
> The override, if you need it, is spelled `CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS`
> (see `scripts/run_selfhost_kernelbuild.py`) and wants an **absolute** linker.ld path.

Six `-j4` attempts are on record, all 2026-08-07 — one before the
`kill_thread_group` stale-tid fix, five after (each a fresh boot with an empty
`target/`, so each is an independent sample):

| run | wall | result |
|---|---|---|
| pre-fix | 11m 21s | **green** — 97 deps + final crate through LTO, 4.3 MB ELF |
| 1 | — | **hard wedge at T459** (defect A below) |
| 2 | 9m 43s | **green** — 108 crates, `EXIT=0`, 4.3 MB ELF at `target/aarch64-unknown-none/release/akuma` |
| 3 | 91 s | **`EXIT=139`** (SIGSEGV), 15 crates (defect B below) |
| 4 | 8m 36s | **green** — 108 crates, `EXIT=0` |
| 5 | 6m 34s | **green** — 108 crates, `EXIT=0` |

So **4 of 6 green, 3 of 5 post-fix** — the build completes in one go more often
than not, but is not yet dependable. Kernel for runs 1-5 = the
`kill_thread_group` stale-tid fix
([`../archive/KTG_STALE_TID_EXIT_STAMP_J4_HANG.md`](../archive/KTG_STALE_TID_EXIT_STAMP_J4_HANG.md)).

**Scope — this does not close §5.1.** Both runs built the `release` profile with
default features. §5.1's deterministic final-crate deadlock was on
`release-smp-shared` + `devbox-smoltcp`, which has **not** been re-run at `-j4`;
`-j1` remains the documented recipe for that profile's final crate.

What the second run proves that the first could not: the forged-exit hang is
gone under real load. The `[KTG-STALE-CH]` guard fired **63 times in that single
build** — 63 occasions where PHASE 2 would have stamped a group exit code
through a recycled tid onto a live process. Each fire looks like:

```
[KTG-STALE-CH] my_pid=138 sib_pid=140 tid=26 recycled to pid=Some(148) — not stamping channel
```

which is the shape of the original autopsy (`my_pid=113 sib_pid=117 tid=31
recycled to pid=140`). Pre-fix, any one of them could reap a live linker and
leak the pipe write refcount that hung `rustc` in `read()` forever. Note the
print is rate-limited to the first 64 fires, so 63 is near the cap and a longer
run under-reports — reboot between measurements rather than looping in one boot.

### The two remaining blockers — A still OPEN, B FIXED 2026-08-14; neither is the KTG class

**Defect A — all-core wedge (run 1).** Hard-wedged at T459 (~7.6 min in): all 4
vCPUs pinned at 100 %, serial console silent from mid-`[mmap]` storm onward,
guest sshd accepting the forwarded TCP connection but never sending a banner,
kernel heartbeat stopped. **Zero** `[KTG-STALE-CH]`, zero `[PROC-ORPHAN]`, no
`PANIC`, no `[WILD-DA]`, and PMM 89 % free — so neither the KTG class nor OOM.
Did not recur in runs 2-5. Boot with `GDB=1` (gdbstub on `1234+INSTANCE`, so
`:1235` at `INSTANCE=1`) and dissect with lldb if it recurs — QEMU's gdbstub
must be armed at launch, so a wedge on a VM booted without it is uninspectable.
A serial log that stops advancing is the only load-independent wedge signal:
SSH banner timeouts are normal under build load and mean nothing.

**Defect B — cargo's heap corrupts (run 3).** `cargo` (pid 17) took a null
dereference and the build died with `EXIT=139` after 15 crates:

```
[WILD-DA] pid=17 FAR=0x0 ELR=0x104e48c8 last_sc=222
```

Twice at the same PC, 20 ms apart. cargo's text loads at `seg_va=0x10000000`
(`filesz=0x1da1c6c` — match that, not the 1 MB `0x109ad0` segment another binary
maps at the same base), so the PC is file offset `0x4e48c8`:

```
4e48b4 <drop_glue<cargo::compiler::unit::UnitInner>>:
  4e48c0:  ldr x8, [x0, #288]     ; load the Rc<PackageInner> pointer
  4e48c8:  ldr x9, [x8]           ; FAULT — x8 == 0
```

It is the refcount decrement in `Rc::drop`, and the pointer field at
`UnitInner+288` read back as **zero**. Safe Rust cannot construct a null `Rc`,
so this is cargo's heap being corrupted underneath it rather than a cargo bug —
a qword that should hold a live pointer reading as zero. That points at page
management (a zeroed or wrong page handed back), the same family as the fixed
`madvise(WILLNEED)` zero-fill of file-backed lazy pages. Previously logged as a
teardown-only curiosity; it is not teardown-only, it fired at T72 mid-build and
killed the run.

> **FIXED 2026-08-14.** The guess above was the right family and nearly the right
> call: it was `madvise`, the sibling advice value. `MADV_DONTNEED` `memset` the
> **physical frame**, which after a `fork` is the frame the peer is still reading
> — the peer's whole page went to zeroes, 0 of 4096 bytes surviving. cargo forks
> per rustc invocation, so its heap is exactly the shape this destroys. Proven
> deterministically in milliseconds by `userspace/forktest/c_stress/madvshared.c`
> rather than by re-running this ~1-in-5, 15-minute repro. **The corruption is
> proven; that Defect B took this route is inference** — when you next run a build
> here, read `dontneed_shared_frame` out of the `[MADV]` PSTATS line and settle it.
> See
> [`../archive/MADV_DONTNEED_SHARED_FRAME.md`](../archive/MADV_DONTNEED_SHARED_FRAME.md),
> whose "Method lessons" are the part worth reading before chasing the next
> stochastic defect here. Defect **A**, the unexplained all-core wedge in the same
> table, is a separate open item — do not conflate them.

## Prerequisites

- Host: `ollama serve` is NOT needed for self-host (that's for meow). You need
  Docker (to pre-clone the repo into the disk image).
- Disk: a large separate image (`disk_selfhost.img`), **not** `disk.img`.

## 1. Create the self-host disk + toolchain

```bash
DISK=disk_selfhost.img bash scripts/create_disk.sh 8192
DISK=disk_selfhost.img bash scripts/populate_disk.sh \
    --with-apk --with-musl-dev --with-rust-toolchain
```

`--with-rust-toolchain` downloads the **nightly** musl-host toolchain to
`/usr/local` (unlike the devbox's apk stable — see the constraint below).

## 2. Pre-clone the repo into the disk (in-VM git is broken for this)

```bash
docker run --rm --privileged -v "$(pwd)/disk_selfhost.img:/disk.img" alpine sh -c "
  apk add git e2fsprogs &&
  mount -o loop /disk.img /mnt &&
  git clone --depth 1 https://github.com/netoneko/akuma.git /mnt/disk/src/github.com/netoneko/akuma &&
  umount /mnt"
```

For crates.io deps, vendor them on the host and copy in:
`cargo vendor selfhost_vendor` (44 MB), then mount-copy.

## 3. Boot

```bash
MEMORY=14336 DISK=disk_selfhost.img SNAPSHOT=1 INSTANCE=1 cargo run --release
```

SSH lands on **:2322** (INSTANCE=1). Boot verified at 6/8/10/12/14/16 GB.

## 4. Compile (in-VM)

```bash
export PATH=/usr/local/bin:/usr/bin:/bin:$PATH
export CARGO_HOME=/root/.cargo
cd /src/github.com/netoneko/akuma
cargo build --release -j1            # timeout ~7200s; -j1 avoids memory spike
```

`--offline` fallback if crates.io unreachable inside the VM.

## 5. Drive it to completion (`release-smp-shared`, 2026-08-05)

The build does not survive one uninterrupted run. It needs a supervisor. Four
things each cost a session's worth of time when done the obvious way.

### 5.1 The `-j4` deadlock on the final crate — root-caused 2026-08-05

`-j4` **deterministically deadlocked** on the final crate: both attempts wedged
~27 s in, with rustc's threads parked forever on musl's `__thread_list_lock`
(`0x300c2340`) and `queued_for` climbing past 900 s. `-j1` compiled the same
crate in **68 s**.

It was never a futex bug and never a musl bug. It was a **lost scheduler
wakeup**: `schedule_blocking` published `WAITING` and only re-read the sticky
`WOKEN_STATES` flag *after* asking to be switched out, so a waker that landed in
the gap between the entry check and the `WAITING` store found the target still
`RUNNING`, armed the flag, and skipped the `READY` transition. Nothing else in
the kernel ever reconsiders a `WAITING` thread on account of that flag, so the
waiter slept forever. Fixed by `publish_waiting_and_take_pending_wake`
(`crates/akuma-exec/src/threading/mod.rs`), which makes the store-and-recheck
atomic against being descheduled. Full diagnosis and the evidence that separates
it from a futex bug:
[`debug-futex-lost-wakeup.md`](debug-futex-lost-wakeup.md) §4a.

`__thread_list_lock` is where it surfaced because musl leans on the kernel there
harder than anywhere else — `pthread_create` holds that lock across `__clone`,
and every thread create and exit in the process serialises on it — so one lost
wake takes down the whole rustc rather than one thread.

**`-j1` is still the safe recipe for the final crate of *this* profile.** The fix
is verified by a deterministic host race test (`park_wake_race_tests`, which
fails on iteration 0 against the pre-fix code and passes with it), and — since
2026-08-07 — by two full in-VM `-j4` builds that compiled the final `akuma`
crate without a `-j1` step. Both of those were the **`release`** profile
(default features), not `release-smp-shared` + `devbox-smoltcp`, so they are
strong circumstantial evidence rather than a direct retest of the deadlock
documented here. See "Status (2026-08-07)" above.

Why `-j1` and not a codegen knob: cargo hands rustc a **jobserver**, and rustc
sizes its own codegen threads from it — so `-j1` shrinks rustc's *internal*
parallelism too. Unlike `-Ccodegen-units=1` or any `RUSTFLAGS` edit, it changes
**no fingerprints**, so the ~97 already-built dependency rlibs stay valid.
Reaching for `RUSTFLAGS` here invalidates every target rlib and costs hours.

Use `-j4` for the dependency phase anyway: throughput wins there even though a
SIGSEGV'd rustc costs a whole crate compile, because a retry resumes — crates
that compiled stay compiled.

### 5.1a Why `-j4` was also *slow*, separately from the deadlock (fixed 2026-08-05)

The deadlock above is one problem. `-j4` also scaled badly, and that had a
distinct, purely mechanical cause: **file-backed mmap used to give every process
a private copy of every page it touched.**

A demand fault on a `LazySource::File` region allocated a fresh PMM frame and
`read_at`-ed the file bytes into it, per process. Four concurrent `rustc`s
mapping the same toolchain — `librustc_driver.so` is 295 MB, `rust-lld` 154 MB —
therefore held *four* physical copies of the same read-only text, filled by four
separate ext2 read sweeps. Physical memory then ran short, `reclaim_clean_file_pages`
evicted clean RO file pages, and each eviction bought a fresh disk read on the
next touch. More jobs → more copies → more pressure → more eviction → more I/O.
`-j1` never enters that loop because one copy of the working set fits.

`src/file_page_cache.rs` now deduplicates those pages on `(inode, file_offset)`,
so all mappers share one frame, one fill, and one I-cache maintenance pass.
Measured on a 1 GB single-core boot, three concurrent `mmap_file` processes over
the same 8.4 MB file:

| | frames allocated | ext2 page reads |
| --- | --- | --- |
| before (per-process copies) | 3 × 2065 | 3 × 2065 |
| after (`[FPCACHE]` 2065 misses / 4130 hits) | 2065 | 2065 |

Kill switch: `config::SHARED_FILE_PAGES_ENABLED = false` restores private copies
for a clean A/B. Watch the `[FPCACHE]` line in the 30 s PSTATS block — `hits` is
exactly the number of private allocations + `read_at` sweeps avoided.

**Trap when testing invalidation: stage the test on the ext2 root, never in
`/tmp`.** `/tmp` is not ext2, so its inode resolves to 0 — and `inode != 0` is an
eligibility rule (`../reference/subsystems/memory.md` -> "Shared file pages"), so
nothing there is ever cached. A `/tmp` invalidation test passes without
exercising a single line of the code under test. Redo it on the ext2 root and
`cmp` the overlapping bytes after an overwrite. (A whole-file hash mismatch there
is a red herring — `cp` does not truncate; that is pre-existing and unrelated.)

**Scope of the measurement.** The numbers above come from the single-core
`release` profile driven by a synthetic `mmap_file` probe. `release-smp-shared`
builds clean but the real 4-way cargo phase has not been re-measured, so the
magnitude on the actual build is extrapolated from the frame/read counts, not
observed.

This is a *separate* fix from the `-j4` deadlock in §5.1 — it made `-j4` slow, it
never made it wedge.

### 5.2 Run the build detached, or ssh will throw the work away

ssh into the guest drops roughly every 5 minutes under build load, which kills
the remote `cargo` and discards everything in flight. Detach it and poll:

```sh
busybox setsid busybox sh -c '<BUILD> > /root/build.log 2>&1;
                              busybox echo $? > /root/build.rc' &
```

Poll with a command that exits **0 whenever the VM is alive**, whether or not
the rc file exists yet:

```sh
busybox sh -c "busybox cat /root/build.rc 2>/dev/null; busybox echo _ALIVE_"
```

A bare `cat` on a not-yet-existing file returns non-zero, which reads as a dead
VM and triggers a pointless reboot every time.

#### 5.2a Boot the **`devbox-smoltcp`** kernel, or you get the in-kernel shell

The host kernel that runs the self-host VM must be built with the userspace-sshd
feature set — the same one §5 builds *inside* the guest:

```bash
cargo build --profile release-smp-shared --features devbox-smoltcp,no-tests
```

`--features smp-shared` alone builds the same SMP kernel but serves port 22 from
the **in-kernel** SSH server, whose shell is a builtin dispatcher, not a login
shell. It is not a smaller shell, it is a different thing, and the failures it
produces all look like guest or kernel bugs:

- It **splits the command line on `;` before the guest sees it** — including a
  `;` inside single quotes — so `sh -c 'a; b'` arrives as `Unknown command: b`,
  and §5.2's `<BUILD> …; echo $? > rc` detach idiom silently loses its tail.
  (`&&`/`||` survive, if you are stuck on this path.)
- A detached `cargo` gets **`Hangup`** written into its own log when the session
  closes — under `setsid`, under `nohup`, and under both.
- There is **no `/dev/null`**; `> /dev/null` fails with `Not found`.

Under the `devbox-smoltcp` kernel all three simply go away.

#### 5.2b Two traps that impersonate kernel bugs

- **A disk written by a VM that died with `SNAPSHOT=0` can be left in a state
  where `cargo` aborts deterministically**, before compiling anything:
  `rustc - --crate-name ___ --print=file-names …` dies with
  `(signal: 6, SIGABRT)` on *every* invocation, on a fresh boot, at any `-j`,
  with **no `[Fault]` line in the kernel log at all**. That is not the
  thread-spawn bug and not a lost wakeup — the same kernel on a **freshly
  re-cloned image** compiled crates immediately. `e2fsck` will not flag it
  (§5.5's "the disk stays clean" is about metadata, not file contents). Re-clone
  the image before diagnosing a deterministic cargo abort. Note `rustc -` run by
  hand still succeeds on the damaged disk — only cargo spawning it fails — so a
  manual probe will *not* reproduce it.
- **A retry loop with no backoff manufactures §5.5 poisoning by itself**: 1000
  respawns in 12 minutes left every new process dying instantly, which then reads
  as a kernel bug. Sleep between attempts, and treat 3 consecutive sub-15-second
  failures as "reboot", not "retry".

Host-side: reusing an `INSTANCE` port across VMs trips ssh's
`REMOTE HOST IDENTIFICATION HAS CHANGED`, which `StrictHostKeyChecking=no` does
**not** bypass — add `-o UserKnownHostsFile=/dev/null`.

#### 5.2c `apk` cargo was the HVF workaround, and it cost you the cache

> **Obsolete as of 2026-08-06 (§6 fix):** nightly cargo now runs under HVF. Use
> `/usr/local/bin/cargo` directly. The fallback below is retained for history
> and for a cold `target/` you specifically want to seed with apk-cargo
> fingerprints.

Nightly cargo used to die under HVF (`[Exception] Unknown from EL0: EC=0x0` on
every exec — the "Common failures" row below). The documented workaround, apk's
`/usr/bin/cargo` with nightly `rustc` on `PATH`, works — but it is a **different**
cargo, so it does not accept the fingerprints the nightly cargo wrote. It
re-resolves and starts rebuilding the ~97 dependency crates from scratch, which
throws away the fast path to the final `akuma` crate (the one §5.1 is about).
It does not *delete* the old rlibs — the count goes up, not down — so a later
nightly-cargo run can still use them.

### 5.3 Detect the wedge from the kernel console, not from the guest

Both obvious liveness signals are useless for the final crate: it emits **no log
output for tens of minutes**, and busybox `ps` reports **no per-process CPU
time** (the TIME column is `0:00` for everything). Use the kernel's own dump —
parse the last `[FUTEX-DUMP]` block out of the serial log and treat any
`queued_for=(\d+)us` over ~300 s as wedged. That fires in minutes instead of
burning the 90-minute timeout.

### 5.3a The hang with **no `rustc` running** — check the process table first

Observed 2026-08-14 on devbox-smoltcp: the build stopped after
`akuma-exec (lib) generated 4 warnings`, and the guest showed

```
  PID  PPID STAT COMMAND
  334     ? RW   /usr/local/bin/cargo build --release -p akuma --manifest-path /tmp/akuma/Cargo.toml
  335     ? RW   {futures-timer} /usr/local/bin/cargo build ...
  337     ? RW   /usr/local/bin/cargo build ...
```

— **cargo alive, and not one `rustc` process.** That distinguishes the two wedge
families in one command, so run it before anything else:

| What `ps` shows | What it means |
|---|---|
| `rustc` present, burning CPU | a slow or looping *compile* — not this class; check `[FSCACHE]` and memory |
| `rustc` present, idle | the lost-wakeup deadlock, §5.1 |
| **no `rustc` at all**, cargo alive | cargo is waiting on a child that is gone — the orphan/reaping class. `wait4` never returns, or the child died without cargo being woken |

Do not diagnose this from the guest alone — read the kernel console for the
`[KTG]`/`[TERM]`/`[Cleanup]` lines around the last successful crate, per §5.3.

**Not caused by `lto = "thin"`** (added to `[profile.release]` the same day): it
reproduces with that key commented out. Same session also saw two rustc ICEs at
**1024 MB** — `decode error: Expected header tag [79, 68, 72, 84] ... found
[0, 0, 0, 0]`, zeros where a dependency's metadata should be, in two parallel
proc-macro jobs a second apart, followed by `Segmentation fault`. Unattributed;
it has the shape of a *read* serving zeros under memory pressure rather than a
corrupt file on disk, but that was never established.
[`../archive/LTO_RELEASE_PROFILE.md`](../archive/LTO_RELEASE_PROFILE.md) §5.1
carries the detail, including a corruption test that looked conclusive and was
not — `grep -c ODHT` scores **0 on the guest toolchain's own known-good
`libcore.rlib`**, so it cannot tell a corrupt artifact from a healthy one.

### 5.4 A 0-byte artifact will block one crate forever

A crash mid-link leaves a **0-byte `build-script-build`** that cargo still
considers fresh, so every later attempt dies identically:

```
could not execute process `…/build/num-traits-<hash>/build-script-build` (never executed)
Caused by: Exec format error (os error 8)
```

This is not a kernel bug and no amount of retrying clears it. Find them with
`busybox find /src/github.com/netoneko/akuma/target -type f -size 0` and delete the whole
fingerprint directory so cargo rebuilds it. Note that `stderr` files and
`.cargo-*lock` files are **legitimately** empty — only a zero-length *binary*
is the defect.

### 5.5 Reboot through the poisoning

One userspace crash can leave the VM in a state where every newly started
process dies instantly and ssh fails within seconds. Cargo's cache makes this
survivable: kill QEMU, `e2fsck -fy` the image, boot again, resume. Progress
across a full run was 25 → 97 rlibs over 7 boots. Reboots cost ~10 s.

A *single* crash leaves the disk clean — `e2fsck` reported no errors after a run
that ended with QEMU aborting — so for one bad run the corruption is purely
in-memory and there is no filesystem damage to look for.

**Corrected 2026-08-15: that does not hold across a long campaign.** After 30+
`cargo clean` + full-build cycles with repeated hard kills, `devbox.img` had
real damage: unattached inodes reconnected to `lost+found`, wrong inode ref
counts, and wrong free-block counts in two block groups *and* the superblock.
The visible symptom is not a filesystem error — it is **boot degrading to 15+
minutes behind a ~1900-line watchdog storm**, which reads like a kernel
regression and is not one. Any measurement taken on an image in that state is
uninterpretable (see `../archive/SELFHOST_ZERO_PAGE_HUNT.md` §14).

Repair it from a container with `e2fsprogs` — the host is macOS and has no
`e2fsck`. Kill QEMU first; `e2fsck` on an image a live VM holds will corrupt it.

```bash
pkill -f qemu-system-aarch64; sleep 2
docker run --rm --entrypoint bash -v "$(pwd)/devbox.img:/devbox.img" \
  <any-image-with-e2fsprogs> -c "e2fsck -fy -D /devbox.img; echo EXIT=\$?"
```

`-f` forces the check (the superblock's clean flag lies here), `-y` auto-answers,
`-D` reindexes and compacts directories — worth it, since a build tree churns
hundreds of thousands of dirents. **`EXIT=1` means "errors were fixed", not
"done": re-run until it exits 0**, which is the only state to resume measuring
from. A healthy result looks like
`53411/393216 files (2.5% non-contiguous), 702577/1572864 blocks`.

**Verify, and expect to re-stage the build tree.** After the 2026-08-15 repair
the same image booted to sshd in **11 s with 18 `[WATCHDOG]` lines** (from 15+
min and ~1900). But the working tree the campaign built in — `/tmp/akuma` — came
back an **empty directory**: its dirents were what the crashes destroyed, and
`e2fsck` could only reconnect the inodes to `/lost+found` (33 entries, 87 MB,
including a whole orphaned copy of `userspace/`). The reconnected inode numbers
sit in the same 44xxx range as the `[FILL-SHORT]` victims, which is consistent —
the churned build artifacts are exactly what goes orphaned.

The pre-cloned `/src/github.com/netoneko/akuma` (§2) survives this and is the one to re-stage from;
check it has `src/`, `crates/`, and `userspace/` before trusting it. Budget a
full cold rebuild for the next arm — `target/` does not survive. `/lost+found`
is recovered junk once you have confirmed `/src/github.com/netoneko/akuma` is intact; deleting it
reclaims the space.

### 5.6 `busybox nproc` reports the real CPU count (was: always 1)

On the `devbox-smoltcp` kernel (real shared-kernel SMP), `busybox nproc` used to
report **1** even at `SMP=2`/`SMP=4`. cargo's `num_cpus` reads
`sched_getaffinity`, so `cargo build` with no `-j` silently defaulted to
**`-j1`** — which (a) serializes the dependency phase, and (b) entirely masks
the §5.1 `-j4` path: you can neither reproduce the `-j4` final-crate deadlock
nor benefit from the multikernel parallelism unless you pass `-jN` explicitly.
(This is why the §6 verification build ran at `-j1` despite `SMP=2`.)

**Cause:** the `SCHED_GETAFFINITY` handler in `src/syscall/mod.rs` had two bugs.
(1) It wrote a fixed mask of `1` (CPU 0 only), ignoring the online CPU count.
(2) It returned `0`; musl's `sched_getaffinity` libc wrapper treats the return
value as the number of bytes written and zeroes the remainder
(`if (r < size) memset(mask+r, 0, size-r)`), so returning 0 made it wipe the
whole buffer — `busybox nproc`/cargo then saw 0 CPUs and fell back to 1. (Linux
returns the byte count placed in the mask.) **FIXED (2026-08-06):** the handler
now returns `(1 << nr_cpus) - 1` in the mask and the byte count written as the
return value, where `nr_cpus` is `smp_shared::probed_core_count()` under
`kernel_smp_shared` (BSP + secondaries, all online after
`bringup_secondaries`) and `1` on single-core and multikernel builds (a
multikernel core runs only its own kernel). `busybox nproc` and cargo's `-j`
default now reflect the actual SMP count.

## 6. FIXED: nightly `cargo` under HVF — undefined instruction delivered as SIGILL (2026-08-06)

**Root cause + fix:** `EC=0x0` at the constant `ELR=0x112ac280` was
**OpenSSL's `OPENSSL_cpuid_setup` armcaps probe executing `SM3SS1`
(`0xce63c004`, FEAT_SM3)** inside `_armv8_sm3_probe`, statically linked into
cargo via its git/curl stack. The probe is *meant* to raise `SIGILL`, which a
userspace handler catches to clear the capability bit and continue — but the
kernel's `EC=0x0` handler hard-killed the process instead of delivering
`SIGILL`. Apple Silicon lacks FEAT_SM3/SM4/SVE/SVE2 (so the probes trap under
HVF `-cpu host`); TCG `-cpu max` implements them, so no trap occurs there and
`HVF=0` avoided the crash. Full investigation + evidence + rule-outs:
[`../archive/NIGHTLY_CARGO_HVF_SIGILL.md`](../archive/NIGHTLY_CARGO_HVF_SIGILL.md).

The fix is one arm of the sync-exception handler in `akuma-exceptions`: deliver
`SIGILL` (signal 4) via the existing `try_deliver_signal` path before killing —
mirroring what the kernel already did for `SIGSEGV` and the spurious-SVC `SIGILL`
case. Verified end to end: `cargo --version` runs under HVF (RC=0), a
`cargo build` proceeds (13 `SIGILL` deliveries during startup, all at OpenSSL
probe PCs, all caught by the handler), and `rustc` still works; `HVF=0` (TCG)
shows `EC=0x0` count = 0 (the fix is inert there). Host tests + clippy clean.

**Consequence for the cache (§5.2c):** nightly cargo now *runs*, so the
apk-cargo fallback is no longer needed. The first nightly-cargo build on a
`target/` previously written by apk cargo still recompiles the ~97 deps once
(incompatible fingerprints), but that run writes **nightly-cargo** fingerprints,
so the next nightly-cargo invocation is a `Finished` cache hit — the fast path
§5.2c said the fallback threw away.

The trace below is the original (pre-fix) symptom, kept for reference:

```
[syscall] execve(path="/usr/local/bin/cargo", …)
[Exception] Unknown from EL0: EC=0x0, ISS=0x0
  Thread=11, ELR=0x112ac280, FAR=0x112ac220, SPSR=0x800
[PSTATS] PID 211 (/usr/local/bin/cargo) 0.00s: 71 syscalls …
          mmap=13 mprotect=3 openat=5 read=1 readlinkat=1 brk=6
```

Decoding the load base from the kernel's own `[IA-DP]` line
(`seg_va=0x10000000`) gives the file offset `0x12ac280`; the 4 bytes there are
`0xce63c004` = `SM3SS1`, in a table of OpenSSL probes (`_armv8_sm4_probe`,
`_armv8_sha512_probe`, `_armv8_eor3_probe`, `_armv8_sve_probe`,
`_armv8_sve2_probe`, `_armv8_cpuid_probe`, `_armv8_sm3_probe`) immediately
followed by `CRYPTO_memcmp` / `OPENSSL_cleanse`.

## 7. Swap the running kernel in place (`KERNEL_DROPOFF`)

Everything above ends with pulling the built ELF *out* of the guest and
booting it on a fresh disk/VM (§"Verify" below) — useful for a one-off check,
but it never actually lets the guest boot into what it just built. As of
2026-08-25 there's a shorter loop: raw block-device fds
([`../archive/RAW_BLOCK_DEVICE_FD.md`](../archive/RAW_BLOCK_DEVICE_FD.md))
mean a guest process can `dd` a new kernel straight onto the drop-off drive
and `reboot(2)` into it, with the host-side relaunch handled automatically.

**Prerequisite:** boot with `KERNEL_DROPOFF=1`. This mounts the host's own
`akuma.bin` — the exact file `-kernel` points QEMU at — as a second
virtio-blk drive (`/dev/vdb` on a fresh devbox-smoltcp boot, next to `/dev/vda`
the rootfs). `scripts/cargo_runner.sh` pairs it with `-action reboot=shutdown`
unconditionally, so a guest `reboot(2)` exits QEMU cleanly and the runner's own
`while` loop relaunches, re-reading `-kernel` from whatever is now on disk.

```sh
INSTANCE=1 KERNEL_DROPOFF=1 DEVBOX_DISK=devbox.img overlays/devbox/run-smoltcp.sh
```

**In guest**, after a self-host build (§4/§5) produces a fresh ELF, flatten it
and drop it onto the drive with `scripts/dropoff_kernel.sh` — checked in at
the repo root the guest's own `/src/github.com/netoneko/akuma` checkout already has, busybox-sh
compatible, and it refuses to run anywhere that isn't Akuma (`uname -s`):

```sh
scripts/dropoff_kernel.sh                      # defaults: the release ELF -> /dev/vdb
scripts/dropoff_kernel.sh <elf-path> <drive>    # override either
reboot -f
```

It does the same three steps as the raw commands (objcopy, `dd`, done) and does
**not** reboot itself, so you always get a chance to look at its output first.
Flattening is delegated to `scripts/mkbin.sh`, which tries `rust-objcopy`, then
`llvm-objcopy`, then plain **`objcopy`** — and on this image it is the last one
that fires: the rootfs gets GNU objcopy from apk `binutils`
(`populate_disk.sh --with-rust-toolchain`), whereas `rust-objcopy` needs the
rustup `llvm-tools` component the image does not carry. Verified 2026-08-26:
GNU `objcopy -O binary` on the kernel ELF is **byte-identical** to
`rust-objcopy -O binary` (md5 `cc25e983…`, 3 322 096 B), so nothing about the
resulting image depends on which one ran.

**Since 2026-08-26 you can usually skip the flatten entirely.** `.cargo/config.toml`
points the kernel target's linker at `scripts/link_kernel.sh`, which links and
then writes `<elf>.bin` in the same step — so an in-guest `cargo build --release`
already leaves `target/aarch64-unknown-none/release/akuma.bin` next to the ELF.
`dropoff_kernel.sh` still re-flattens (it is cheap and it is the one path that
cannot be stale), but if you are `dd`-ing by hand, the `.bin` is already there.
The wrapper is POSIX sh and never fails the link: on a rootfs with no objcopy at
all it prints `[mkbin] no objcopy found` and the build still succeeds with a
working ELF, just no `.bin`. Verified in-guest on the devbox rootfs (busybox
`/bin/sh`, no objcopy): wrapper exits 0, link unaffected.

**`reboot -f`, not bare `reboot`** — busybox's plain `reboot` tries to signal
an init process first and fails `EPERM` on this kernel (no init to signal);
`-f` calls `reboot(2)` directly and is what actually exits QEMU.

If the guest toolchain has none of `rust-objcopy` / `llvm-objcopy` / `objcopy`
(`mkbin.sh` will say so and exit 1 rather than guess), pull the ELF out and
flatten it on the host first (§"Verify" below has the `base64` extraction
command), then `scp`/base64 the `.bin` back in and `dd` it onto `/dev/vdb`
directly — the script is only for the in-guest objcopy step. Installing apk
`binutils` in the guest is the cheaper fix, and the self-host image already
has it.

**Host side**, confirm the relaunch and that it's actually the new build:

```sh
grep -a 'relaunching with the current' <boot.log>
ssh -o UserKnownHostsFile=/dev/null -p <ssh-port> root@localhost uname -a
```

**Gate on the `uname -a` git hash over ssh, not on a console string** —
cross-core console interleaving can tear boot markers at `SMP>1`
([`../archive/DEVBOX_ISSUES.md`](../archive/DEVBOX_ISSUES.md) Issue 3).

Two things that bite:

- **`/dev/vdb` under `KERNEL_DROPOFF=1` *is* `akuma.bin`, live, not
  snapshotted** — unlike `DISK` under `INSTANCE>0`. A bad `dd` here corrupts
  the exact file the next relaunch boots from. Back it up, or be ready to
  rebuild it (`cargo build --release`, then `rust-objcopy -O binary <elf>
  <elf>.bin` — what `cargo_runner.sh` does unconditionally on every `cargo
  run`) before trusting the next reboot.
  - **Corollary, hit 2026-08-25**: virtio-blk reports a *fixed* capacity to
    the guest, set once from `$BIN`'s byte size at the moment this QEMU
    process opened it — it cannot grow for the life of that process. An
    in-guest self-hosted rebuild that's bigger than *this boot's* kernel (even
    if still under the size-guard ceiling) will `ENOSPC` partway through the
    `dd`, and since the drive is the live file, that partial write **lands** —
    corrupting `akuma.bin` into a truncated image before you ever get to
    `reboot -f`. Recovered the same way: `rust-objcopy -O binary <elf>
    <elf>.bin` from the still-intact ELF. `cargo_runner.sh` now pads `$BIN` up
    to the size guard's own ceiling (`$SIZE_LIMIT` — 4 MB for
    release/devbox-smoltcp) whenever `KERNEL_DROPOFF=1`, precisely so a rebuild
    has room to grow without hitting this — but only up to that ceiling; a
    kernel that grows past 4 MB still needs a host-side QEMU restart to pick up
    a bigger drop-off drive.
- **This only works against the unmounted drop-off drive.** Write-open of a
  *mounted* device — i.e. trying the same trick against `/dev/vda`, the
  rootfs — is refused `EBUSY` by design, so it can't be used to self-modify
  the running root filesystem's backing image this way. Reads are
  unrestricted on every device.

## Verify

- `/usr/local/bin/rustc --version` contains `"nightly"`.
- Success = produced ELF at `target/aarch64-unknown-none/release/akuma` (or
  `…/release-smp-shared/akuma` for the §5 flow).
- Record the highest milestone reached: manifest parse → build.rs/proc-macro →
  deps → akuma crate → rust-lld link.

Do not stop at "cargo printed Finished" — that has been true while the artifact
was unusable. Close the loop by booting what the guest built:

```bash
# 1. pull the ELF out (binary-safe; the guest has busybox base64)
ssh -p <port> root@localhost \
  '/bin/busybox base64 /src/github.com/netoneko/akuma/target/aarch64-unknown-none/release-smp-shared/akuma' \
  | base64 -d > selfhost_akuma.elf

# 2. flatten it — scripts/mkbin.sh picks whichever objcopy exists
#    (rust-objcopy needs rustup's llvm-tools component; GNU objcopy from
#    binutils produces a byte-identical image, verified 2026-08-26)
scripts/mkbin.sh selfhost_akuma.elf selfhost_akuma.bin

# 3. boot it on its own disk clone and ports (never the image a live VM holds)
qemu-system-aarch64 -machine virt,gic-version=3 -accel hvf -cpu host -smp 4 \
  -m 4096 -serial mon:stdio -display none -semihosting \
  -netdev user,id=net0,hostfwd=tcp::2622-:22 \
  -global virtio-mmio.force-legacy=false \
  -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0 \
  -drive file=bootcheck.img,if=none,format=raw,id=hd0 \
  -device virtio-blk-device,drive=hd0,bus=virtio-mmio-bus.1 \
  -device virtio-rng-device,bus=virtio-mmio-bus.2 \
  -kernel selfhost_akuma.bin
```

Expect a full boot and a working `uname -a` over ssh on :2622. `uname` prints
the **git hash the guest built from** — check it against the commit you meant to
self-host, because the guest's `/src/github.com/netoneko/akuma` checkout drifts from the host tree
(it was 12 commits behind on 2026-08-05). Measured sizes for reference: ELF
1,998,392 B, `.bin` 1,465,088 B, versus 1,456,901 B for the host build of the
same tree.

## Key constraints

- **Nightly is mandatory** (panic-immediate-abort cargo-feature). Host must be
  `aarch64-unknown-linux-musl`.
- **`cargo build --release`** is the realistic target — no `build-std` needed.
- **`[profile.release]` sets `lto = "thin"` since 2026-08-14, and this build is the
  reason it is thin rather than fat.** LTO's cost lands here twice — a longer link
  and, decisively, higher peak linker memory. Measured on the host (macOS,
  cross-compiling; treat the ordering as transferable and the absolute numbers as
  not), rebuild-and-relink after touching one file:

  | `[profile.release]` | image | `.text` | rebuild+link | **peak RSS** |
  |---|---:|---:|---:|---:|
  | no `lto` (pre-2026-08-14) | 3,480,848 | 2,175,428 | 5.3 s | 738 MB |
  | **`lto = "thin"`** (current) | 3,715,496 | 2,404,116 | 10.6 s (2.0×) | **779 MB** |
  | `lto = "fat"` | 3,355,824 | 2,289,896 | 19.0 s (3.6×) | **1,090 MB** |

  Fat wins on image size and loses on the only axis this build cares about: it
  peaks over **1 GB**, so it will not link on a 1 GB guest, where thin's ~780 MB
  will. That is the whole reason for the choice. Fat also costs 3.6× the link time
  per iteration, which compounds across a full kernel build.

  Budget accordingly: **thin roughly doubles link time versus no LTO**, and the
  in-VM figure will be worse than the host's 2.0×. `extreme-size`
  keeps `lto = true` (fat) and is unaffected: it is size-gated and never
  self-hosted. Full A/B and what is still unmeasured:
  [`../archive/LTO_RELEASE_PROFILE.md`](../archive/LTO_RELEASE_PROFILE.md).

  > **Correction (2026-08-15): this paragraph used to say "if a self-host build
  > starts failing at the link step after this date, drop `lto` before suspecting
  > anything else — that is a one-line bisect." Do not do that first; it is a red
  > herring.** Re-measured on the 2026-08-15 tree, `thin` costs **+1.4% peak RSS**
  > (793,952,256 → 805,044,224) and **1.2×** relink (8.4 s → 10.2 s) — not the 2.0×
  > and not an OOM cliff. The table above was measured on an older tree; treat its
  > *ordering* as transferable and its magnitudes as stale. The build failure that
  > actually prompted this warning was
  > [`../archive/SELFHOST_ZERO_PAGE_HUNT.md`](../archive/SELFHOST_ZERO_PAGE_HUNT.md),
  > which has nothing to do with LTO, and chasing `lto` first cost real time.
- **`fs-cache` feature** is already **on** — it is in `default`, so
  `--features devbox-smoltcp,no-tests` includes it. Nothing to add. The lever is
  its *size*: `src/fs.rs` caps it at `min(RAM/8, 384 MB)` at mount. The ceiling
  was 128 MB until 2026-08-05, which was sized against `rustc --version` rather
  than a real compile and left the cache pegged full and evicting; 384 MB cut
  in-VM `rustc -O` wall time 15.8% (10.72 s → 9.03 s per compile). 256 MB buys
  nothing and 512 MB buys only 1.1 point more while pushing the heap past
  512 MB — the response is a step, not a slope. Measure with the `[FSCACHE]`
  PSTATS line (`hits/misses/slots`); `slots=N/N` means it is evicting. See
  [`../reference/subsystems/config-flags.md`](../reference/subsystems/config-flags.md).
- **`MAX_ARG_STRLEN`** 128 KB (release) — the Go forktest fix is a regression
  guard.

## Common failures

| Symptom | Cause | Fix |
|---|---|---|
| `rc=137` / SIGSEGV | OOM | Raise `MEMORY` (verified up to 16 GB) |
| `[ENOSYS] nr=NNN` | Missing syscall | Decode against asm-generic table (see [`../reference/subsystems/syscalls.md`](../reference/subsystems/syscalls.md)) |
| MAP_SHARED linker output fails | file-backed mmap writeback | FIXED (§7 of the archive doc) |
| futex/exit_group thread-group reaping | thread-group not fully reaped | FIXED |
| icache stale (`dc cvau`) | icache not flushed after code write | FIXED |
| Stale I-cache spurious SVC | spurious svc during execve | FIXED (the headline §7k.6) |
| `cargo --version` crash (EC=0x0) | **FIXED 2026-08-06** — OpenSSL `OPENSSL_cpuid_setup` executes `SM3SS1` (FEAT_SM3) which Apple Silicon lacks; the kernel's `EC=0x0` arm didn't deliver SIGILL so the probe handler couldn't recover. Now delivers SIGILL via `try_deliver_signal` (§6). The old "traps HVF CNTP" reading was a misattribution (that would be EC=0x18) | Nightly `/usr/local/bin/cargo` now runs under HVF. (Old workaround: apk cargo + nightly rustc, or `HVF=0` — no longer needed) |
| `error: could not compile … (signal: 11)`, or all rustc processes frozen with `pthread_join` waiters | freshly-cloned thread SIGSEGVs at a fixed PC | **FIXED 2026-08-06** — three sub-causes (§2e tid-clear, §2g `THREAD_STATES` races, §2h trampoline/`AS MISMATCH`), all in [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md). A recurrence is news: identify *which* class per that runbook before assuming it is the same bug |
| rustc ICE `decode error: Expected header tag [79, 68, 72, 84] but found [0, 0, 0, 0]` (`ODHT`), or any page reading back as zeros | The kernel served a zero page where file bytes belonged | **FIXED 2026-08-15**, two root causes (prefault stub hook; inode reuse under a live mapping). Latent rather than proven gone: `[FILL-SHORT] got=Ok(0)` and `defer_leak=` must stay 0 — [`../archive/ZERO_PAGE_ICE_FIX.md`](../archive/ZERO_PAGE_ICE_FIX.md) |
| `rust-lld` rejects an `.rlib` it just wrote — `invalid sh_name (0x5000feed)`, `Archive::children failed`, `invalid sh_type … expected SHT_STRTAB` | Bytes decode to `0xFEEDFACE…` quarantine poison: a mapped file page's frame was freed and poisoned under the mapper | **FIXED 2026-08-15** (file-page-cache refcount windows W1/W2; 6/10 red → 10/10 green). `[PMM-RESURRECT]` must never print — [`../archive/MAPPED_PAGE_PREMATURE_FREE_FIX.md`](../archive/MAPPED_PAGE_PREMATURE_FREE_FIX.md) |
| Final `akuma` crate sits on `Compiling akuma v0.0.7` forever at `-j4`; waiters on `0x300c2340` with `queued_for` > 900 s, **no** `[kill]` and **no** `[Fault]` lines | Lost scheduler wakeup in `schedule_blocking` — a wake that raced the `WAITING` publication was dropped. **FIXED 2026-08-05** | Fix is in; `-j1` remains the recipe until a `-j4` run confirms it (§5.1). Diagnosis: [`debug-futex-lost-wakeup.md`](debug-futex-lost-wakeup.md) §4a |
| `Exec format error (os error 8)` on the same `build-script-build` every attempt | 0-byte artifact from a crashed link, still "fresh" to cargo | Delete the fingerprint dir (§5.4) — not a kernel bug |
| Build restarts lose all progress; ssh dies ~every 5 min | remote cargo killed with the ssh session | Run detached + poll (§5.2) |
| `qemu-system-aarch64: Assertion failed: (isv), hvf.c:1883` | guest touched MMIO with an instruction HVF can't decode, after the kernel went off the rails | Symptom of the ld-musl class — [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md) |
| All 4 vCPUs at 100 %, **serial log stops advancing**, sshd accepts TCP but sends no banner, heartbeat gone | Defect A — unexplained all-core wedge. Not KTG (zero `[KTG-STALE-CH]`), not OOM (PMM 89 % free) | **OPEN** — boot with `GDB=1` and take an lldb dump of all vCPUs. Log staleness is the signal; SSH timeouts alone are not (§"Status (2026-08-07)") |
| Build dies `EXIT=139`; `[WILD-DA] pid=<cargo> FAR=0x0` at a fixed `ELR` | Defect B — a pointer in cargo's heap read back as zero (null `Rc` in `drop_glue`); heap corruption, not a cargo bug | **FIXED 2026-08-14** — `MADV_DONTNEED` was zeroing a CoW-shared frame out from under the peer: [`../archive/MADV_DONTNEED_SHARED_FRAME.md`](../archive/MADV_DONTNEED_SHARED_FRAME.md). If it recurs, first run `madvshared` (expect `ALL PASS`), then decode `ELR` against `seg_va=0x10000000 filesz=0x1da1c6c` |

## Background

- `archive/AKUMA_SELF_HOSTING.md` — the full progression §1–§7j (SELF-HOSTED).
- [`acceptance/10_selfhost_compile_akuma.md`](../../acceptance/10_selfhost_compile_akuma.md).
- `scripts/loop_selfhost_kernelbuild.py` — retry loop. Note it targets the
  **`--release -j1`** build with a hardcoded `-p 2322`, and it retries in-band
  over ssh, so it does not cover the §5 flow (detached build, wedge detection,
  reboot-through-poisoning, `-j1` only for the final crate). Treat it as the
  starting point, not the harness. **It also never runs `cargo clean`** — nor
  does any other script here — so out of the box it measures incremental
  resumes, not clean-build reliability; see "Status (2026-08-15)" for why that
  distinction is the whole trial.
