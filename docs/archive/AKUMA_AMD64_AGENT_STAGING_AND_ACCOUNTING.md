# amd64: staging agent binaries on the trashcan, and the three kernel defects that fell out

**Date:** 2026-10-02 (box on `Akuma/amd64`)
**Status:** all kernel fixes below are **installed and verified on the metal** (kernel stamped `71bec22c`, md5
`152e478f…`). `nca` runs end to end against Kimi. `goose` and `opencode` are still broken (§ 2, § 8).

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

**Lead, unverified:** `sys_fcntl` returns `0` for `F_GETLK`/`F_SETLK`/`F_SETLKW` (`akuma-syscalls-glue/src/fs.rs`),
and for `F_GETLK` it does **not** write `l_type = F_UNLCK` back, so a caller asking "does anything hold this lock?" is
told yes. SQLite's unix VFS uses exactly those calls. `flock(2)` is a different thing and is not the gap: amd64
dispatches `Syscall::Flock => 0` (a stub), AArch64 has a real one. The WAL `-shm` file's `mmap` is a second suspect.
Not tested; `goose` has not been re-run since the `F_GETFL` fix below.

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
2. Goose: re-run after the fix; if `SQLITE_PROTOCOL` persists, make `F_GETLK` write `F_UNLCK` back (and decide whether
   `F_SETLK` should track real locks), then check the WAL `-shm` mapping. `goose --version` not exiting and the blank
   TUI are separate.
3. `opencode --version` hang, uninvestigated.
4. `RUSAGE_CHILDREN` / `cutime`; killed children report 0 CPU in `wait4`.
5. `loadavg`; the `State` field; per-CPU bucketing. (Starvation baseline: done, § 5.)
6. `wc: Interrupted system call` appeared once from a pipeline's `wc -l` in the box shell — an `EINTR` where Linux
   would not return one. Not chased.
7. A pipe's write end still reports `O_RDONLY` from `F_GETFL`.
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
