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
