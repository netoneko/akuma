# Self-host: compile the Akuma kernel inside Akuma

Runbook for compiling the Akuma kernel *inside* Akuma (the self-hosting
milestone). **This is NOT the devbox** — self-hosting uses the default-smoltcp
build + a nightly toolchain on a separate large disk.

> The devbox (`build-devbox.md`) is the rump-only dogfooding image with apk
> stable toolchain. Self-hosting has actually compiled the kernel (147/147
> units) and the self-built kernel boots.

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

### The two remaining blockers — both OPEN, neither is the KTG class

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
  git clone --depth 1 https://github.com/netoneko/akuma.git /mnt/disk/root/akuma &&
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
cd /root/akuma
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
`busybox find /root/akuma/target -type f -size 0` and delete the whole
fingerprint directory so cargo rebuilds it. Note that `stderr` files and
`.cargo-*lock` files are **legitimately** empty — only a zero-length *binary*
is the defect.

### 5.5 Reboot through the poisoning

One userspace crash can leave the VM in a state where every newly started
process dies instantly and ssh fails within seconds. Cargo's cache makes this
survivable: kill QEMU, `e2fsck -fy` the image, boot again, resume. Progress
across a full run was 25 → 97 rlibs over 7 boots. Reboots cost ~10 s.

The disk itself stays clean through all of this — `e2fsck` reported no errors
after a run that ended with QEMU aborting — so the corruption is purely
in-memory. Do not go looking for filesystem damage.

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

The fix is one arm of the sync-exception handler in `src/exceptions.rs`: deliver
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
  '/bin/busybox base64 /root/akuma/target/aarch64-unknown-none/release-smp-shared/akuma' \
  | base64 -d > selfhost_akuma.elf

# 2. flatten it — run this from the repo so rust-toolchain.toml selects the
#    toolchain that actually has llvm-tools, else rust-objcopy is "not found"
rust-objcopy -O binary selfhost_akuma.elf selfhost_akuma.bin

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
self-host, because the guest's `/root/akuma` checkout drifts from the host tree
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
  in-VM figure will be worse than the host's 2.0×. If a self-host build starts
  failing at the link step after this date, drop `lto` from `[profile.release]`
  before suspecting anything else — that is a one-line bisect. `extreme-size`
  keeps `lto = true` (fat) and is unaffected: it is size-gated and never
  self-hosted. Full A/B and what is still unmeasured:
  [`../archive/LTO_RELEASE_PROFILE.md`](../archive/LTO_RELEASE_PROFILE.md).
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
| `error: could not compile … (signal: 11)`, or all rustc processes frozen with `pthread_join` waiters | freshly-cloned thread SIGSEGVs at a fixed PC | **OPEN** — [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md) |
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
  starting point, not the harness.
