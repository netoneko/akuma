# amd64: staging agent binaries on the trashcan, and the three kernel defects that fell out

**Date:** 2026-10-02 (box on `Akuma/amd64`)
**Status:** every kernel fix below is **installed and verified on the metal** (kernel `no-tests`, stamp
`719d3406` + the epoll/eventfd change; see § 7's table for the md5s). `nca` runs end to end against Kimi,
`goose` now works, a multi-threaded tokio stream no longer stalls. Still open: `opencode`, the TUI items in
`userspace/nca/docs/ISSUES.md`, and the list in § 8.

The ask began as "set up Rust and cargo in the local (TV) console, stage `opencode`, then try `goose` with Kimi", and
turned into a build of `nca` on the box. Each step stopped at a kernel behaviour that was wrong, not at a tool.
This records what was measured, what was only inferred, and what to do next.

## 1. The TV console had no Rust environment

herd gives its `console = true` service only `PATH`, `HOME` and `TERM`; sshd sets nothing at all
(`scripts/box/akuma-dev.env` says so). `/etc/profile` sources `/etc/akuma-dev.env`, but only login shells read
`/etc/profile`, and the console shell is not one. The console shell *does* source `$ENV` = `/etc/console.rc`.

**Fix:** append `. /etc/akuma-dev.env` to `/etc/console.rc` (done live, backup `/etc/console.rc.pre-env`; and in the
`mkdisk.sh` template, guarded with `[ -f … ] &&`). A first draft of the template change escaped the `&&` as `\&\&`
inside a `printf` format string and would have written those backslashes into the file; caught by reading the
generated line back. Verified on the box: `rustc`/`cargo` resolve from `sh -c '. /etc/console.rc; …'`. **Not verified
on the TV itself** — the already-running shell does not re-read it.

## 2. Getting agents onto the box

Facts established, all against this kernel:

- `busybox wget` cannot do HTTPS; `hget` caps bodies at 8 MiB (`docs/archive/AKUMA_AMD64_*`: it buffers). `apk add curl
  ca-certificates` works and `curl` then fetches GitHub release assets directly (63 MB, ~7 min at ~150 KB/s).
- `hpbox.akuma()` has a **60 s** subprocess timeout. A long `ssh` call raises `TimeoutExpired`; the remote command
  keeps running. Start long work with `nohup … &` and poll.
- **Two `curl`s writing the same path corrupted the download**: a leftover curl from a timed-out call plus a second one
  produced a file of the right size starting with zeros (`gzip: invalid magic`, hash mismatch). The retry, with no
  other writer, matched GitHub's `digest` exactly. Always compare `sha256sum` with `gh api …/releases/latest --jq
  '.assets[].digest'`; size alone passed.
- `tar xzf` of a 150–200 MB member takes longer than 60 s. Background it too.
- `apk add` reports `4 errors` on this image after a successful install (post-install triggers); the packages work.

### opencode
The official `anomalyco/opencode` (formerly `sst/opencode`) is ~27 MB of TypeScript at v1.18.34. The Linux "musl"
release is a 196 MB **dynamic** (`/lib/ld-musl-x86_64.so.1`) executable, not a static Rust binary. The org's Rust
repos (`opencode-pty`, `rift`, `hex`, `terminal-control`) are helpers, not the agent. Staged anyway at
`/usr/local/bin/opencode` on request. **`opencode --version` never prints and never exits** (a process, a second
process and three `HeapHelper` threads, no output after 2.5 minutes). Cause not investigated.

### goose
`aaif-goose/goose` v1.52.0 is Rust; `goose-x86_64-unknown-linux-musl.tar.gz` (51.6 MB, digest verified) holds one
149,764,512-byte `goose`. Staged at `/usr/local/bin/goose`, configured for Kimi Code through its OpenAI provider
(`https://api.kimi.com` + `coding/v1/chat/completions`, model `kimi-for-coding`), with `/usr/local/bin/goose-kimi` as a
wrapper that reads the key from the box's existing `/root/kot/kimi.token` so the secret is never copied into goose's
config. (`~/.akuma/kimi/token` does not exist on the box; `kot` keeps it at `/root/kot/kimi.token`.)

Observed, all on the box and later confirmed by the user:
- `goose --version` prints `1.52.0` **late** and then keeps running, ~77 s of CPU without exiting.
- `goose run --no-session -t …` fails before any network I/O: `Could not create session: error returned from database:
  (code: 15) locking protocol`. The database is `~/.local/share/goose/sessions/sessions.db` in WAL mode (`-wal`/`-shm`
  created). SQLite code 15 is `SQLITE_PROTOCOL`.
- The `goose tui` shows nothing (user report; not investigated).

**Resolved (§ 9):** `F_GETLK` answered "locked" for every query, which is exactly what SQLite's WAL
shared-memory setup interprets as `SQLITE_BUSY` → `SQLITE_PROTOCOL`. Fixed; goose now starts, answers and
exits 0. (`goose --version` not exiting and the blank TUI were seen before the fix and not re-checked.)

## 3. nca on the box: `build.rs` hardcoded aarch64

`userspace/nca/build.rs` hardcoded the target triple, the `aarch64-linux-musl-gcc` linker/CC/CXX/AR env, the NEON
`target-feature`, the `strip` tool and the output path. It now derives all of it from the arch it is built for:

- `NCA_ARCH` wins, else `CARGO_CFG_TARGET_ARCH`, else `aarch64`. **From this tree that is always `aarch64`**: the
  repo-root `.cargo/config.toml` sets `[build] target = "aarch64-unknown-none"`, and cargo applies it from
  `userspace/nca` too, so a native build on the box needs `NCA_ARCH=x86_64`. (The first on-box attempt built for
  aarch64 and failed with `can't find crate for 'core'`.)
- Cross tools (`<arch>-linux-musl-gcc/g++/ar/strip`) are used only if found on `PATH`. On a native box they are left
  unset so the host's own `$CARGO_HOME/config.toml` linker applies — the box links with the toolchain's `ld.lld`, which
  a hardcoded `gcc` would override.
- The aarch64 output is unchanged (`bootstrap/bin/nca`); amd64 goes to `bootstrap/bin/nca-x86_64` so it cannot
  clobber the AArch64 image's copy.

Getting past the build script on the box needed two more things: `apk add linux-headers` (`aws-lc-sys` probes
`linux/random.h`), and the kernel fix in § 4. After that the build reached the final `nca` crate and then ran for more
than 45 minutes — `lto=fat`, `codegen-units=1`, one thread. That build was **abandoned**. The binary that is on the box
was cross-built on the Mac with the recipe in `reference_amd64_staging_meow_nca` (static-pie, 13,212,648 bytes, md5
`bf8c829507414f603793c2979e86efc5`) and copied to `/usr/local/bin/nca`. It starts (`--version` is rejected by clap, which
proves the process runs). After the kernel in § 5–6 was installed it was exercised against Kimi — see "nca against
Kimi" below.

### nca against Kimi (metal, after the new kernel)

`nca doctor` works with `HOME=/root` from an empty directory. Kimi Code goes in through nca's **Custom** provider, all by
environment (the key is read from the box's `/root/kot/kimi.token`, never printed):
`NCA_DEFAULT_PROVIDER=custom CUSTOM_PROVIDER_BASE_URL=https://api.kimi.com/coding CUSTOM_PROVIDER_MODEL=kimi-for-coding
CUSTOM_PROVIDER_COMPATIBILITY=openai CUSTOM_PROVIDER_API_KEY=…` (nca appends `/v1/chat/completions`).

1. First `nca run --prompt "Reply with exactly the single word: pong"` reached Kimi (DNS, TLS, request all fine) and was
   refused: `invalid temperature: only 1 is allowed for this model`. A setting, not a kernel fault. Fixed with
   `/root/.local/share/ncacli/config.toml` containing `[provider.custom]` / `temperature = 1.0`.
2. The retry exited 0 with session status `Completed`: the assistant turn is `pong` (3,817 tokens in, 69 out, ~13 s to
   the first streamed token). `nca run` printed only `[session] <id>`; the reply is in
   `~/.local/share/ncacli/workspaces/<ws>/sessions/<id>.json` and shows in `nca sessions`.
3. nca logs `IPC disabled: socket bind failed: … Address family not supported by protocol (os error 97)` — **AF_UNIX
   is not available on the amd64 kernel** (`EAFNOSUPPORT`). Harmless to a one-shot run; it disables nca's IPC
   (`attach`/`spawn` control). Not investigated further.

## 4. `ar: x.a: invalid operation` — `fcntl(F_GETFL)` never returned the access mode

**Symptom.** GNU `ar cq` failed for every mode on the amd64 kernel after writing the 8-byte `!<arch>\n` header. That
killed the `ring` and `aws-lc-sys` build scripts, hence every native C-dependency crate on the box.

**Method.** Reproduced in isolation on the box (4 mode variants, all failing; not the rlimit — identical at limits 64
to 65536). `/proc/<pid>/syscalls` keeps a post-exit ring but only logs calls that go through `akuma-syscalls-glue`, with
asm-generic numbers and no `openat`/`read`/`write`; it showed only `prlimit64`, closes, a `close` returning `-9`, and an
`unlinkat` of the output. For a full trace I built a local rig: the default amd64 kernel under QEMU `microvm`,
`STRACE=1`, an ext2 image from `amd64/mkdisk.sh` with the box's `ar` and its four shared libs written in with
`debugfs`. The trace shows `ar` creating `/tmp/stXXXX` with `open`, immediately `fcntl`, then failing.

**Cause.** That `fcntl` is BFD's `bfd_fdopen`, which calls `F_GETFL` on the `mkstemp` fd it is about to write and
refuses a read-only one. `sys_fcntl` returned `if is_nonblock { O_NONBLOCK } else { 0 }` — `0` is `O_RDONLY`. Every
file in the system claimed to be read-only.

**Fix.** `akuma_syscalls_linux::flags::fcntl::getfl_status(open_flags, nonblock)` → access mode | `O_APPEND` |
`O_NONBLOCK`, used for `FileDescriptor::File`; other fd kinds keep their old answer (a pipe's write end still reads as
`O_RDONLY`). It lives in the zero-dependency ABI crate with a host test; the access-mode and `O_APPEND` bits are the
same on x86_64 and asm-generic. **Verified in QEMU** (`ar-rc=0`, 2,616-byte archive) **and on the box** after
install (`ar-rc=0`, `ar t` lists both members) — see § 7.

**A rig trap that cost one run.** `mkdisk.sh` hardlinks every applet name to one busybox inode. `debugfs rm bin/ar`
freed the shared inode and `write` reused it, so `/bin/busybox` *became* GNU `ar` and the VM printed `ar`'s usage text
with `/bin/busybox` as the program name. Install extras under new names (`/bin/gnuar`).

## 5. Accounting: error or starvation?

The user saw `ps` TIME that barely moved, `top` at 0–1% for the build, and asked which. Measured on the box and then
in QEMU, in this order:

1. **`/proc/<pid>/stat` `utime` is right.** A CPU-bound `awk` in QEMU advanced ~103 jiffies/s at SMP=2 (a core at
   100 Hz). The long-lived `cargo` processes were genuinely blocked (`[PSTATS]`: ~1.8 M ms in a kernel wait), so ~0%
   for them is correct. During the final `nca` crate, rustc processes turned over quickly (one per crate), which is
   what "crate names change all the time" in `ps` was.
2. **`times`/`getrusage`/`wait4`'s `rusage` were hard zeros.** A 14 s CPU loop under `busybox time` printed
   `user=0.00 sys=0.00`. All three wrote literal zeros.
3. **`/proc/stat` billed idle as busy.** On an idle 2-core VM over 6 s: user +1208 ticks, idle +2. amd64's
   `sched::idle_loop` did `sti; hlt; cli` and the scheduler bills a thread for its whole residency;
   aarch64's `idle_halt` credits the halt back, amd64 never did.
4. **`/proc/stat` per-CPU rows and several other fields are unreliable**: per-core deltas went negative on the box
   (`LAST_CORE` attribution: a migrating thread's total jumps between rows), `loadavg` is a hard `0.00`, and
   `/proc/<pid>/status` `State` reads `R` for almost every process, including ones that must be waiting.

**Fixes (in tree, verified in QEMU).**
- `akuma_syscalls_linux::{Rusage, Tms}` wire types with size/offset assertions and tests.
- `akuma_exec::process::group_cpu_time_us(tgid)` sums the thread group's `get_thread_cpu_time`.
- `publish_child_exit` records the child's group CPU time into `ProcessChannel::cpu_time_us` *before* marking the
  channel exited, because the child's thread slot can be recycled before the parent reaps it. `wait4` reports it at all
  four reap sites.
- `times` → `tms_utime` in 100 Hz ticks; `getrusage(RUSAGE_SELF/THREAD)` real; `RUSAGE_CHILDREN` **still zero**;
  an invalid `who` is now `EINVAL`. Everything is `utime` — there is no user/kernel split.
- `threading::credit_halted_time(tid, entered_us)` called around the `hlt` in `idle_loop`.

QEMU result: `time awk` real 11.25 s, user 11.23 s (was 0.00). Idle window: idle +2 → +243 of a possible +1200.

**The remainder is real, measured with a temporary per-thread print (removed):** over an idle 8 s window tid 0 was
billed 100% and tid 1 ~44%. tid 0 is the boot thread and *is init's kernel thread* (`run_init`), which spins while init
waits; tid 1 is the netpoll daemon, which polls by design. So "idle never reaches 100%" is partly design, not accounting.

**Starvation: no evidence for it.** With accounting working on the metal (§ 7) the baseline could finally be taken:
the same 20M-iteration `awk` loop took **14.12 s on an idle box** (`user=14.11`), against 13.85–14.19 s while the nca
build was running. A CPU-bound process was getting its CPU under load, so the build looking idle in `ps`/`top` was the
accounting, not the scheduler. (The 14 s itself is slow in absolute terms; the loop had no Linux baseline.) Threads
that `hlt` on their own (`futex.rs` timed waits) are still billed for the halt.

## 6. The kernel's version label was a day old

`uname -v` said `6d9017b1` (commit of Oct 1 20:24) for a kernel built on `e40e50a0`. The user spotted it. The binary on
the box was byte-identical to what was built (md5 matched) — the *label* was stale, not the kernel.

**Cause.** `crates/akuma-syscalls-glue/build.rs` registered `rerun-if-changed` for `.git/HEAD` and for the branch ref
only if that ref was a loose file *at that run*. The cached run for the `--features no-tests` unit recorded
`['../../.git/HEAD', '../../Cargo.toml']` — no branch ref (it was probably in `packed-refs` then). `.git/HEAD` had not
been touched since Oct 1 13:23, so cargo never reran the script and every later commit was invisible, while a plain
`--release` unit with a different fingerprint did track the ref (and said `1fa42be0`).

**Fix.** Also track `.git/logs/HEAD` (appended by every commit, checkout and reset) and `.git/packed-refs`. The rebuilt
kernel reports `71bec22c`, the then-HEAD. The "constant for a given source tree" property the file's comment insists on
is preserved. Not tested across a real commit (nothing here may commit).

## 7. What is verified where

| claim | QEMU | box (metal) |
|---|---|---|
| `F_GETFL` fix makes `ar` work | yes | **yes** (kernel installed 2026-10-02, `ar-rc=0`) |
| `times`/`getrusage`/`wait4 rusage` report CPU time | yes (11.25 s / 11.23 s) | **yes** (`busybox time awk`: user=14.11 real=14.12; was 0.00) |
| idle halt credited | partial (idle +2 → +243) | **yes, partly** — idle window of 8 s: idle +1374 of 3200 ticks (43%), was ~0%; the rest is the polling threads |
| version stamp follows HEAD | yes (`71bec22c`) | **yes** (`uname -v` = `71bec22c-release-smp-shared`, `git rev-parse --short HEAD` = `71bec22c`) |
| console shell has rust/cargo env | n/a | partly (sourced file; TV not looked at) |
| static amd64 `nca` runs | n/a | **yes**, one-shot against Kimi answered `pong` |
| goose with Kimi | n/a | **fails** (§ 2) |

The box runs `/boot/akuma-amd64` md5 `152e478f687e428649f235cdf5342339` (`no-tests`, 6,742,016 bytes, stamp
`71bec22c`); the previous kernel is `/boot/akuma-amd64.prev`. `.good` was not promoted. One slip during the deploy: the
first install-and-reboot in a single call printed nothing, did not install, and rebooted onto the old kernel; the
install was repeated on its own (rc=0, md5 verified) and then the reboot. Do those as separate steps and check the
install's output before rebooting.

## 8. Open items

1. ~~Install the staged kernel; confirm on the metal.~~ Done (§ 7).
2. ~~Goose~~ fixed (§ 9, § 13: `--version` hang, interactive session verified; § 14: real `fcntl` record locks).
3. `opencode --version` hang, uninvestigated.
4. `RUSAGE_CHILDREN` / `cutime`; killed children report 0 CPU in `wait4`.
5. `loadavg`; the `State` field; per-CPU bucketing. (Starvation baseline: done, § 5.)
6. `wc: Interrupted system call` appeared once from a pipeline's `wc -l` in the box shell — an `EINTR` where Linux
   would not return one. Not chased.
7. A pipe's write end still reports `O_RDONLY` from `F_GETFL`.
9. **The shared file offset is not shared across `fork`/`exec`.** `for …; do ./cmd; done > out` has every child
   write from offset 0 and overwrite its siblings (seen as a garbled results file; `>>` per command is the
   workaround). Not investigated.
10. `[E2-EOF] inode=… caller believed the file extended past off?` from the ext2 layer during SQLite WAL activity.
11. nca: the items under "OPEN" in `userspace/nca/docs/ISSUES.md` (run-together tool previews; the exit abort and
    keyboard scroll and `AF_UNIX` are closed — § 13).
8. Four surviving processes after `kill -9` on the box (pids 32 and 4188, the `nca` build script and `rustc`) — a
   zombie shell from the first timed-out download (pid 49) also persisted. Cause unknown.

## Files

`userspace/nca/build.rs`, `amd64/mkdisk.sh`, `crates/akuma-syscalls-linux/src/{flags,time,lib}.rs`,
`crates/akuma-syscalls-glue/src/{fs,proc}.rs`, `crates/akuma-syscalls-time/src/lib.rs`,
`crates/akuma-exec/src/process/{channel,children,table}.rs`, `crates/akuma-threading/src/lib.rs`,
`amd64/src/sched.rs`, `crates/akuma-syscalls-glue/build.rs`. Docs updated alongside:
`syscalls/fs.md`, `syscalls/time.md`, `subsystems/vfs.md`, `runbooks/amd64-console-shell.md`,
`runbooks/amd64-bare-metal-loop.md`, `userspace/nca/README.md`.

## Method notes worth keeping

- A local rig that reproduces a metal-only symptom with a *full* syscall trace beat reasoning from the lossy
  `/proc/<pid>/syscalls` ring: `STRACE=1 DISK=<img> INIT=/bin/busybox INITARGS=sh,/t.sh sh amd64/run.sh`, and extras
  written into the image with `debugfs -w -R "write …"`.
- When you only kill what you started: match QEMU by its image path (`pgrep -f "qemu-system-x86_64.*<img>"`), never by
  a pattern that also appears in your own shell's command line (that killed the shell once, exit 144), and never
  `pkill qemu` — the user's own q35 VM was running.
- Host port 2222 may be in use; `SSH_PORT`/`HTTP_PORT` override it.
- Check a "stale" answer's origin before explaining it: the version label looked like a different kernel and was a
  build-script fingerprint.

## 9. Goose: `F_GETLK` (SQLite `SQLITE_PROTOCOL`)

The lead from § 2 was right. A probe (`t.c`, static musl, run on the box) gave:

| call | result on the old kernel | Linux |
|---|---|---|
| `F_GETLK` on a byte nobody holds | `l_type=1` (`F_WRLCK`: "locked") | `l_type=2` (`F_UNLCK`) |
| second process `F_SETLK` on a byte the first holds | succeeds | `EAGAIN`/`EACCES` |
| `F_GETLK` from the second process | `l_type` unchanged, `pid=0` | `F_WRLCK`, `pid=<holder>` |
| `flock(EX)`, `mmap(MAP_SHARED)` of a file + `pread` | ok | ok |

SQLite's `unixLockSharedMemory` (WAL's `-shm`) calls `F_GETLK` on the DMS byte: `F_UNLCK` means "I am the first
connection, truncate and carry on"; `F_WRLCK` means "another process holds it" → `SQLITE_BUSY` → reported as
`SQLITE_PROTOCOL`, code 15. A static SQLite 3.53 shell on the old kernel confirmed the family: `PRAGMA
journal_mode=WAL; CREATE TABLE t…` then `no such table: t` (non-WAL worked).

**Fix:** `sys_fcntl` answers `F_GETLK` with `F_UNLCK` (writes only `l_type`). `F_SETLK`/`F_SETLKW` are unchanged
(always succeed), which is the remaining wrong half. **Verified** in the QEMU rig and on the metal: WAL journal,
`count(*)` 3 then 4 after reopening, `integrity_check` `ok`; then `goose-kimi run --no-session -t "Reply with
exactly the single word: pong"` printed `pong` and exited 0 (its old database files are in `/root/goose-old-db/`).

## 10. nca streaming: the stall was an eventfd edge, then a printer race

The photographs of the TV showed `streaming 55s … 202s`, bursts of text after 90 s silences and then `error sending
request`. In order, what was ruled in and out:

1. **Not a kernel network limit.** `curl -N` of a 1,500-token reply streamed 1,200 lines in 21 s, steady; DNS, TLS
   and the TCP path were fine. A `--verbose` nca run showed the HTTPS connection made and pooled normally.
2. **Reproduced without Kimi** with nca's own stack (`nettest-reqwest`, tokio + hyper + reqwest + rustls) against
   `scripts/net_delay_server.py`: `stream …/drip/10/1000` took 13–57 chunks in the first ~0.3 s and then stalled
   (no `RESULT`; killed at 30–45 s) 4/4 times plain and TLS. **A single-core QEMU rig could not reproduce it** (it
   needs more than one core — the same run completed there at SMP=4 too).
3. **The Mac's `netstat` named the side**: `Send-Q 3465` unacknowledged, the server stuck in `FIN_WAIT_1`, shrinking by
   bytes per probe — the box's receive window was closed because its socket buffer was full and unread.
4. **A temporary `[epolldiag]` line in the epoll scan** showed the same socket: `rev=0x5 was_last=0x5 report=0x0
   st=ESTABLISHED can_recv=true rq=16384` — readable, buffer full, edge **suppressed**. No `PRUNE` lines, so the fd
   was not being dropped from the interest list. (The diagnostic and an earlier hypothesis that the scan's
   decision-then-record raced a reader's re-arm are both addressed: the race is fixed in `scan_entry` with a host test
   of the interleaving, but it was **not** the cause of this stall — the fix alone left 4/4 still stalled.)
5. **The bisect that found it**: `NETTEST_RT=current` completed 1,001/1,001; `multi` stalled; `multi` with
   `TOKIO_WORKER_THREADS=1` still stalled; **`NETTEST_SPAWN=1` (whole probe inside one spawned task, no per-chunk wake of
   the main thread) completed 3/3.** So the stall needed a cross-thread wake. Raw `futex` ping-pong with bursts, both
   `FUTEX_WAIT` and `FUTEX_WAIT_BITSET` (what Rust's `std` uses), lost nothing (4/4 runs, `lost_wakes=0`), and neither did a
   pthread condvar — so not the futex.
6. **The cause**: tokio wakes a reactor thread that is parked in `epoll_pwait` by writing an `eventfd` that mio
   registered `EPOLLET`. Linux raises a fresh edge for every write; this kernel derives edges from readiness
   *transitions*, and mio never reads the eventfd, so it stays readable and only the first write counted. The consumer
   (main thread) release of hyper's body backpressure woke nobody, the connection task never read again, the window
   closed. `read(eventfd)` also never re-armed the edge, unlike every socket/pipe read.

**Fix:** `akuma-syscalls-glue/src/fs.rs` re-arms the `EPOLLIN` edge on every eventfd write (before the write and its
wake) and after every eventfd read. **Verified on the metal:** `nettest-reqwest stream …/drip/10/1000`, multi-thread:
3/3 complete plain, 3/3 TLS; the 1 s SSE stream's end-of-body lag (last chunk at 20.2 s, body complete at 26.2 s) is
gone (20.4 s).

**Then a second, unrelated thing looked like the same problem.** With the stall fixed, `nca run` still ended
mid-sentence with exit 0 (`340 + 51`, the next piece would be ` = 391`), and `hyper` had logged the body complete. A logging
proxy on the Mac (adding the key itself, so none crossed the LAN) showed Kimi sending the whole reply in 2.25 s: 19
reasoning chunks, `391`, `finish=stop`, `[DONE]`. The loss was in nca: `run` aborted its event task right after `run_turn`
returned, discarding whatever was still queued (see `userspace/nca/docs/ISSUES.md`). The session event log was
incomplete for the same reason, which is why counting events in it had been misleading. Fixed in nca by draining before
aborting. `error sending request for url` now prints its cause chain; its cause on the TV runs was not captured.

## 11. Reasoning ("thinking") in nca

Neither of nca's SSE parsers handled Kimi's reasoning, so a reasoning turn was silent. Added for both APIs
(`reasoning_content`; `thinking_delta`), shown as a dim `thinking` block in the TUI and on stderr in `run`. Kimi's
Anthropic-style endpoint (`/coding/v1/messages`, either auth header) works and returns `thinking` blocks; there is no
"effort" setting because nca's Custom provider sends no `thinking` parameter and only MiniMax implements
`--thinking-budget`. Details: `userspace/nca/docs/ISSUES.md`.

## 12. What changed on the box in this last stretch

| item | state |
|---|---|
| kernel | `no-tests` build with `F_GETLK`, the `scan_entry` race fix and the eventfd re-arm; diagnostics removed |
| `/usr/local/bin/nca` | cross-built on the Mac with reasoning, the error chain and the exit-drain fix (md5 `56911f31…`; previous builds kept as `nca.v2`, `nca.prev`). Verified: 4/4 `nca run` complete in 4–7 s with the answer, `Session ended (Completed)` and the token line |
| `/usr/local/bin/goose`, `goose-kimi` | working; old DB moved to `/root/goose-old-db/` |
| test servers | `scripts/net_delay_server.py` (HTTP 18080, TLS 18443) and a logging proxy (18090) were run on the Mac, not left on the box |
| test binaries on the box | `/tmp/nt/nettest-reqwest{,2}`, `/tmp/lk/{t,sqlite3,pp,fx,fx2}` (in `/tmp`) |

## 13. 2026-10-03: `exit_group` from a non-leader thread, AF_UNIX, nca PageUp/PageDown

**`goose --version` printed `1.52.0` and never exited.** Not a tokio worker, not SQLite, not the epoll thread it first
looked like. Method, because the first three theories were wrong: `/proc/<pid>/syscalls` (leader only), then a
temporary per-process `SYSCALL_TRACE` (goose + its workers, to the console ring — `dmesg`), then
`sched::dump_slot_table()` on the periodic tick. The trace showed the thread that ran `main` call `exit_group(0)` and
return, while `[exit-diag]` markers proved the **leader never reached `run_process`'s epilogue**; the slot table put the
leader (`pid=13`, a different slot from the `exit_group` caller) in an untimed `FUTEX_WAIT`. Rust's goose runs `main` on a
spawned thread (`goose-cli-main`) and joins it; `std::process::exit` from that thread is `exit_group`, which set
`GROUP_EXIT` and `leave` for *the calling thread only*. On this target a group dies only through the leader's epilogue
(`drain`, fd sweep, `SPAWN` row), and `should_leave_now()` is false for the leader by construction, so nothing ever told it.
The same bug is why `nca run` ended in exit 134 / hung and why `timeout … goose --version` never returned.

**Fix** (`amd64/src/thread.rs` `exit_group_from_thread`, `signal.rs` `deliver_pending`, `usermode.rs`
`exit_current_with_code`): a non-main `exit_group` records the code in `GROUP_EXIT_STATUS` (new `GROUP_EXIT_CODE_FLAG`
bit, so code 0 is distinguishable from "none" and a signal death `-(sig)` stays negative), then
`request_thread_kill` + `sched::wake` on the leader — resolved through `thread_for_pid`, never `Process::thread_id`. The
leader's blocking arm returns `EINTR`, `deliver_pending` reads the status and leaves with the code. Siblings are reached by
the leader's existing `drain`. **Verified on the metal:** `userspace/forktest/c_stress/groupexit_thread.c` rc=3 in all
three leader-parked modes (join futex, `read(pipe)`, `epoll_wait(-1)`) with a `pause()`d sibling; `goose --version` 2.9 s,
exit 0; `goose-kimi run` → `pong` in 5 s; interactive `goose session` renders, answers, exits 0 on ^C^C;
`nca run` exit 0 (was 134).

**AF_UNIX.** `socket(AF_UNIX, …)` answered `EAFNOSUPPORT`; only `socketpair` was routed to glue's `unixsock`. The
`socket` arm now sends `AF_UNIX` there and `bind`/`listen`/`accept`/`accept4`/`connect` follow the descriptor
(`fd::is_unix_socket`) — the same "AF_UNIX first, then the native stack" shape as `sendto`/`recvfrom`.
`userspace/forktest/c_stress/unixsock_amd64.c`: 14/14 on the metal (STREAM+DGRAM socket, path bind is `S_IFSOCK`,
listen, `getsockname`, epoll on the listener, accept4, a forked client's connect + round trip, connect-to-nothing fails,
abstract bind). nca's IPC listener binds (`/tmp/nca/session-*.sock`); `IPC disabled … os error 97` is gone.

**nca PageUp/PageDown** (`crates/tui/src/tui/app.rs`, composer key handler): one page less a line, same follow-tail rules
as the mouse wheel. Driven through a pty from the laptop (`ssh -tt`, ESC `[5~`/`[6~` after a 150-line reply): PageUp
redraws earlier lines (1–23 …), PageDown returns to the tail. Not verified on the TV's own keyboard.

Still open: `opencode --version`; nca run-together tool previews. (`F_SETLK` — § 14.)

## 14. 2026-10-03: POSIX record locks, and Python turns out to work

**`database disk image is malformed` in goose.** `F_SETLK`/`F_SETLKW` returned success unconditionally, so nothing excluded
anything. SQLite's WAL index serialises on byte-range locks in the `-shm` file, and goose runs a main process plus several
child processes against one `sessions.db`: two wrote the WAL at once, and the next open said `(code: 11) database disk image
is malformed` (seen on the TV; reproduced after goose was hard-killed mid-write). This was the "`F_SETLK` is a no-op" item
left open in § 9.

**Fix.** `crates/akuma-reclock` (new, `#![forbid(unsafe_code)]`, 11 host tests including SQLite's WAL reader/exclusive-sweep
pattern): per-file range table, conflict test, split on partial unlock, merge of adjacent same-kind locks, release by owner.
`crates/akuma-syscalls-glue/src/recordlock.rs` is the acting half (`struct flock` copy, `l_whence`, `F_SETLKW` poll-wait
every 5 ms, `EINTR`). **Owner is the fd table's identity** (the same `holder` `flock` uses): `CLONE_FILES` threads share it,
a `fork` child inherits nothing. **Release rides `flock::flock_release`**, which every fd-teardown path already calls, so
closing *any* descriptor for a file drops the process's locks on it (POSIX's rule) and exit drops the rest.

**amd64 trap:** `exec_runtime.rs` had `flock_release: |_, _, _| {}` — a deliberate no-op from when this target dispatched no
`flock`. With real locks that meant a process that died holding one held it forever (the probe's "a holder's exit released
its lock" check failed on the first deploy). Now wired to glue's function, which also makes `flock` release on exit.

**Verified on the metal:** `userspace/forktest/c_stress/fcntl_lock.c` — 13 checks, 5 fail on the old kernel, all pass on the
new (refusal across processes, shared reads, disjoint ranges, `F_GETLK` reporting the holder's pid, downgrade, `F_SETLKW`
blocking ≥250 ms until unlock, close-any-fd release, exit release). Goose then ran three back-to-back sessions on a fresh DB
with `integrity_check` ok. **Not verified:** a long multi-process goose session end to end at the time of writing.
Not done: `EDEADLK` detection, `F_OFD_*`, `SEEK_END`; keyed by path, so two paths to one inode do not contend.

**Python works on the box** (found by accident; nobody had checked). Goose, asked to research, installed `uv` and a standalone
CPython into `/root/.local` — `cpython-3.14.8-linux-x86_64-musl` under `/root/.local/share/uv/python/`. There is no system
`python3` and nothing on `PATH`; only `uv`/`uvx` are in `/root/.local/bin`. Checked with that interpreter: starts
(`platform` says `Akuma-0.0.8-x86_64`), `ssl` (OpenSSL 3.5.9), `sqlite3` (WAL mode, `integrity_check` ok — the new locks),
`threading` (4 threads, join), `subprocess.run`, and HTTPS (`urllib` to `api.github.com`, 200 in 0.1 s). **Not checked:**
`os.fork` directly, `multiprocessing`, `asyncio` under load, `pip`/wheels with C extensions. To make it a real tool rather
than a side effect, install it deliberately and put it on `PATH`.

**Seen, not investigated:** `rm -rf` of a fresh `git clone` directory printed `can't remove '.git/hooks': Directory not
empty` — a directory that still counts as non-empty after its entries were removed (VFS/ext2). Goose's own `git clone`
also needed a retry the same way.

## 15. 2026-10-03 (later): the locks were necessary, not sufficient — `MAP_SHARED` is not coherent across processes

**What happened.** After § 14's locks landed, a fresh goose DB survived three back-to-back runs, then a long interactive
research session (≈50 tool calls, one goose process plus extension children) came back `database disk image is malformed`
again (4 MB WAL, 684 KB main file; preserved at `/root/goose-corrupt3/` on the box). Nobody had killed goose this time. So the
§ 14 theory ("no locks, so several processes wrote the WAL together") was real but not the whole story.

**Reproduced without goose.** `scripts/benchmarks/sqlite_wal_stress.py` (any python3 in the guest; the box has uv's CPython,
§ 14): 6 connections in **one process** → `integrity_check` ok; **4 processes** on one WAL database → `database disk image is
malformed`, with real `fcntl` locks in place.

**Root cause, confirmed by a 40-line probe** (`userspace/forktest/c_stress/shmcoh.c`): two processes `mmap(MAP_SHARED)` one file;
one writes, the other reads — **neither sees the other's writes** (both lines `NO`). That is the documented design, not a
regression: `docs/reference/subsystems/amd64-shared-write-mmap.md` § "What this does not promise" — writable `MAP_SHARED` is
demand-paged fills plus write-back on `munmap`/`msync`, **no page cache, no cross-mapper coherence**. SQLite's WAL index (the
`-shm` file) is precisely a `MAP_SHARED` file mapping that every connection, in every process, must see live; with one frame
set per process each process runs on its own private copy of the index and they diverge. (Threads are fine: one address space,
one mapping.)

**Not yet fixed. Options, with what each costs:**
1. **A shared writable page cache** (principled). Every mapper of `(mount, inode, page)` maps one physical frame RW; the
   existing write-back (`SharedWriteBack`, flush walks present leaves) keeps the file current. `akuma-fpcache` already keys
   frames by `(inode, mount id, file offset)` and refcounts them through the CoW refcount — but only for **read-only** mappings
   (`fill_file_pages`: `sharing = !region_pte.write && cap > 0`), mapped CoW-marked. The work is a RW, non-CoW share path:
   fault, `munmap`/exit teardown refcounts, `fork` (a child currently *drops* the record), `msync`/write-back from a shared
   frame, `read(2)`/`write(2)` coherence with the mapping (not needed for SQLite), and keeping aarch64's separate
   `SharedFileMapping` in step (`mmap::plan` is shared code — check both dispatchers).
2. **Special-case `-shm`.** Back `MAP_SHARED` mappings of a path ending in `-shm` with kernel-global frames keyed by path, never
   flushed to disk. SQLite rebuilds the index from the WAL when no process holds it (the deadman-switch lock byte, which the
   § 14 locks now answer correctly). Small and low-risk; fixes SQLite only and leaves ParityDB-style users incoherent.
3. **Workaround only.** Keep goose to one process touching `sessions.db` (e.g. a single `goose serve` under herd). No kernel
   change; multi-process WAL stays broken for everything else.

**Also seen:** goose's extension children are separate processes (`goose-cli-main` ×4, ~500 futex/s each); whether they
open the session DB was not established — it is the question that decides whether option 3 would even help.
