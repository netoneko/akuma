# Akuma Self-Hosting — compiling the kernel inside Akuma

This documents how to build the Akuma kernel **with `rustc` running inside Akuma
itself**, the prerequisites that make it possible, and the kernel changes that
were needed to give the build enough RAM.

It is the reference companion to the runnable playbook
[`acceptance/10_selfhost_compile_akuma.md`](../acceptance/10_selfhost_compile_akuma.md).
The single-file bring-up that preceded it (`rustc hello.rs`) is in
[`docs/RUST_TOOLCHAIN.md`](RUST_TOOLCHAIN.md).

---

## 1. Prerequisites

### 1.1 A nightly, musl-host Rust toolchain

Two hard constraints decide the toolchain:

1. **Nightly is mandatory.** The root `Cargo.toml` opens with
   `cargo-features = ["panic-immediate-abort"]`. That is a *nightly cargo*
   feature, so **any** cargo invocation on stable fails at manifest-parse time —
   Alpine's apk `rust` (stable 1.91) cannot even read the workspace. The build
   must use a nightly `cargo`/`rustc`.

2. **The host must be `aarch64-unknown-linux-musl`.** Akuma's userspace is musl;
   a glibc `rustc` binary would not run (no glibc loader). Nightly *does* ship a
   musl-host toolchain — verified present on `static.rust-lang.org`:
   `rustc`, `cargo`, and `rust-std` for `aarch64-unknown-linux-musl`.

### 1.2 What a `--release` kernel build actually needs

| Component | Needed for `cargo build --release`? | Why |
|---|---|---|
| nightly `rustc` + `cargo` (musl host) | **yes** | nightly cargo-feature + the codegen/link |
| `rust-std` for `aarch64-unknown-linux-musl` | **yes** | compiling/running `build.rs` and proc-macros (host artifacts) |
| `rust-std` for `aarch64-unknown-none` | **yes** | the kernel's target; `profile.release` = `panic="abort"` links against the **precompiled** target std — **no `build-std`** |
| `rust-src` | no (release) / **yes** for `size`/`extreme` | those profiles are `panic="immediate-abort"` → need `-Z build-std` |
| C toolchain (`clang`, `ld.lld`, `gcc`, `cc`, `ar`, `make`) | recommended | `cc`-rs build scripts; the kernel link itself uses bundled `rust-lld` |

So the realistic self-host target is **`cargo build --release`**: it does *not*
need `build-std`, only the precompiled `aarch64-unknown-none` std.

### 1.3 Dependencies (offline)

Building the workspace pulls ~real crates from crates.io plus one git fork
(`embedded-tls`). **cargo-over-TLS inside Akuma works** — proven June 2026 by an
in-VM `cargo build` of the `userspace/hello` crate, which fetched all its deps
(`format_no_std`, `talc`, `lock_api`, `scopeguard`) from `index.crates.io`/
`static.crates.io` over HTTPS into `/root/.cargo/registry` (§7d). TLS/HTTPS is
already used elsewhere in Akuma (apk-over-TLS, in-kernel SSH), so this was expected.
Vendoring (`cargo vendor`, point `.cargo/config.toml` at `vendored-sources`) is
therefore optional — a way to make the build fully offline/deterministic, not a
requirement.

### 1.4 RAM

`rustc` needed ≥2 GB just for `hello.rs`. A self-host build runs hundreds of
`rustc` invocations and Akuma's `fork` CoW is heavy (see
`docs/RUST_TOOLCHAIN.md` §5b), so give the VM as much as it can take. The kernel
now boots and runs at **`MEMORY` up to 16 GB** (§3); use `-j1` to bound peak
footprint while probing.

### 1.5 Disk

An ext2 image with the toolchain (~1 GB) + the source + room for `target/`.
8 GB is comfortable.

---

## 2. Preparing the disk (host side)

A new `--with-rust-toolchain` flag on `scripts/populate_disk.sh` installs the
nightly musl-host toolchain into the image via Docker (no waiting inside Akuma),
and a `DISK` env override on both disk scripts lets you build a separate image
without touching the primary `disk.img`.

```bash
# 1. 8 GB image, separate from the primary disk.img
DISK=disk_selfhost.img bash scripts/create_disk.sh 8192

# 2. bootstrap + busybox + musl-dev + the nightly Rust toolchain
DISK=disk_selfhost.img bash scripts/populate_disk.sh \
    --with-apk --with-musl-dev --with-rust-toolchain

# 3. put the akuma source in the image (git clone, or the offline-vendored
#    staging described in acceptance/10) at /root/akuma
```

`--with-rust-toolchain` downloads (from `static.rust-lang.org`) and installs under
`/usr/local`: `rustc`, `cargo`, host + `aarch64-unknown-none` `rust-std`, and
`rust-src`; it also apk-installs the C toolchain. Implementation notes that bit
during bring-up: busybox `tar -J` corrupts these large `.xz` streams (decompress
with the real `xz` binary and pipe to `tar`), and the rust `install.sh` needs
`bash` (Alpine ships only busybox `sh`).

See `acceptance/10_selfhost_compile_akuma.md` for the exact, copy-pasteable
preparation and the in-VM build steps.

---

## 3. Giving the build more RAM: the 6 GB → 16 GB story

### Symptom

Booting `MEMORY=8192` (8 GB) crashed in the **boot self-test suite**:

```
[TEST] map_user_page: map → verify → unmap → verify
[Exception] Sync from EL1: EC=0x25, ISS=0x7
  ELR=0x40336a4c, FAR=0x1c0180000          # FAR ≈ 7 GB
  EC=0x25 in kernel code — killing current process (EFAULT)
[SCHED] WARNING: yield_now with IRQs masked tid=0 ...   # boot thread dead → spin
```

`MEMORY` up to 6 GB was fine; ≥ ~7 GB crashed.

### Root cause — a boot self-test VA collision (not a kernel limit)

The MEMORY>2GB work (`docs/MEMORY_LAYOUT.md`) made the kernel/user VA split track
detected RAM: `kernel_va_end()` = `round_up(ram_base + ram_size, 1GB)`, and the
boot identity map is extended with 1 GB blocks to cover all RAM. Real user
processes are unaffected — `alloc_mmap` jumps over `[KERNEL_VA_START, kernel_va_end)`
to place user VAs above the (now larger) identity window.

But three boot self-tests in `src/tests.rs`
(`test_map_user_page_roundtrip`, `_preserves_irq_state`, `_race_leaks_frame`)
hardcoded a scratch VA of ~7.5–7.75 GB (`0x1_E000_0000` / `0x1_F000_0000`). That
is a safe "high user VA" only while RAM ≲ 7 GB. Once RAM is large enough that the
identity map *reaches* that address (MEMORY ≥ 8 GB), the scratch VA lands **inside
a 1 GB identity block**. `map_user_page` then tries to split that block into
L2/L3 tables and allocates the new page-table frames from the very 1 GB region it
just unmapped — so writing into a fresh table faults (translation fault, the
`FAR ≈ 7 GB`). The boot thread (tid 0) is killed and the scheduler spins.

### Fix

Move the three scratch VAs **above** the RAM identity map, matching the
convention the other PTE-walk tests already use (256 GB+, e.g.
`0x40_C000_0000`):

| test | old VA | new VA |
|---|---|---|
| `test_map_user_page_roundtrip` | `0x1_F000_0000` (7.75 GB) | `0x41_0000_0000` (260 GB) |
| `test_map_user_page_preserves_irq_state` | `0x1_E000_0000` (7.5 GB) | `0x41_4000_0000` (261 GB) |
| `test_map_user_page_race_leaks_frame` | `0x1_E000_0000` (7.5 GB) | `0x41_8000_0000` (262 GB) |

256 GB+ is above any plausible RAM identity map, so `map_user_page` builds fresh
tables under an empty L1 entry (no block to split) and the page-table frames it
allocates live in identity-mapped RAM. This was a **test bug**, not a kernel
capability limit — actual user mappings already worked at large RAM.

### Verified boot matrix

`scripts/boot_ram_sweep.sh` boots the release kernel at each size and checks it
reaches the in-kernel SSH server. (It must `grep -a` the logs: QEMU/HVF output
carries a stray control byte that makes plain `grep` treat the log as binary and
silently never match — this produced a false "all FAIL" the first time.)

Result (release kernel, `disk_selfhost.img`, HVF, June 2026 — after the test-VA fix):

| `MEMORY` | DTB-detected | boot → SSH |
|---|---|---|
| 6144M (6 GB)  | 6144 MB  | ✅ PASS |
| 8192M (8 GB)  | 8192 MB  | ✅ PASS  *(was the `map_user_page` crash; fixed)* |
| 10240M (10 GB)| 10240 MB | ✅ PASS |
| 12288M (12 GB)| 12288 MB | ✅ PASS |
| 14336M (14 GB)| 14336 MB | ✅ PASS |
| 16384M (16 GB)| 16384 MB | ✅ PASS |

All six detect the full RAM and reach `[SSH Server] Listening...`. Free RAM scales
with `MEMORY` (e.g. at 8 GB the PMM idles at ~1.90M / 2.10M pages free ≈ 7.2 GB
free). 16 GB was the top of the sweep, not a new ceiling — the boot map covers
1 GB blocks up to L1[511] (≈512 GB), so the architectural limit is far higher; the
practical limit is host RAM (this host: 48 GB).

---

## 4. Does the nightly toolchain actually run inside Akuma?

Yes, up to a point. Findings (June 2026, `disk_selfhost.img`, nightly
`rustc 1.98.0-nightly (9e2abe0c6 2026-06-16)`, host musl):

| Operation | Result |
|---|---|
| `rustc --version` | ✅ works at **every** `MEMORY` = 4/6/8/10/12/16 GB (`scripts/rustc_ram_sweep.sh`). Not RAM-capped. |
| `rustc --emit=obj hello.rs` (codegen only, **no linker**) | ✅ **completes**, produces a valid ELF `.o` (6344 B, magic `7f45 4c46`). ~24 s warm at 16 GB. |
| `rustc hello.rs -o hello` (codegen **+ link**) | ❌ **does not complete** — the link step kills the session (see §6). 0-byte output. |

So **codegen works end-to-end**; the original wall was the **link step** — fixed
in §5.

---

## 5. The link step: stdin-EOF was killing the session (FIXED)

`rustc hello.rs -o hello` *does* link inside Akuma (the chain
`rustc` → `cc` → `collect2` → `execve …/bin/ld` is visible in the kernel log).
It is slow — rustc forks to spawn the linker, and that fork CoW-shares rustc's
~75k-page address space (libLLVM); on single-core QEMU it runs tens of seconds
(`docs/RUST_TOOLCHAIN.md` §5b, `docs/COW_OPTIMIZATIONS.md`). But slow is fine. The
*blocker* was a session bug, not throughput:

**Root cause.** `ssh host cmd` closes its stdin immediately (sends
`SSH_MSG_CHANNEL_EOF`). `src/shell/mod.rs` treated that EOF as a disconnect and
**interrupted the foreground process** (a band-aid for an `exec cat` hang, issue
#5 in `docs/STABILITY_URGENT_ISSUES.md`). So every long non-interactive command
was cut at its first idle moment; the build then ran orphaned (which is why a
binary sometimes appeared if you polled long enough) but the session and its
streamed output were gone. Confirmed: the drop fired at stdin-EOF (14–59 s), far
under the 60 s read / 300 s idle timeouts — **never a timeout.**

**Fix (commit on this branch).** Split `channel_eof` (stdin done) from
`channel_closed` (real `CHANNEL_CLOSE` / `DISCONNECT` / TCP-EOF) in
`crates/akuma-ssh` + `src/ssh/protocol.rs`, and in the streaming loop
(`src/shell/mod.rs`):

- **stdin-EOF** → `process::close_process_stdin(pid)` (deliver EOF to the process
  so stdin-readers finish) and **keep streaming** until the process exits;
- **real disconnect** → deliver stdin-EOF then stop streaming, but **leave the
  process running** (orphaned, reattachable via `box grab`) — disconnecting does
  *not* kill it;
- **Ctrl-C (0x03)** → still the explicit kill.

A `close_process_stdin` that didn't wake a parked reader exposed a **lost-wakeup
race** in `read(stdin)` (`src/syscall/fs.rs`): the reader checked
`is_stdin_closed()`, *then* registered its waker, so a close in that window
parked it forever. Fixed by re-checking after registering the waker. Guards:
`test_channel_eof_distinct_from_close`, `test_streaming_exec_survives_stdin_eof`
(`src/ssh_tests.rs`).

**Result:** `ssh host '<build>'` now stays connected for the whole compile and
returns the artifact. Verified repeatedly: nightly `rustc -C linker=clang
hello.rs` holds the session ~120 s and the binary runs; apk rustc ~70 s.

### Mini-shell constraints (still true, useful to know)

| Want | Reality |
|---|---|
| `cmd &` (background) | ❌ `&` is passed as an argument |
| `busybox sh -c '…'` / `sh script.sh` | ❌ forking rustc from busybox `sh` **segfaults rustc** |
| `#!/bin/sh` wrapper script | ❌ `execve` doesn't honor shebang → `exit 127` |
| `2>file` (stderr redirect) | ❌ unsupported; `2` leaks as an arg. Only `>file` works |
| `busybox env VAR=… cmd` | ✅ sets env + `execve`s — use this to give rustc a `PATH` |

To compile inside the VM today: `/bin/busybox env PATH=/usr/local/bin:/usr/bin:/bin
HOME=/root /usr/local/bin/rustc -C linker=clang /root/hello.rs -o /root/hello`,
then run `/root/hello`.

---

## 6. Benchmarks (hello.rs)

`hello.rs` = `fn main(){ println!("Hello from Akuma!"); }`. Compile, then run.

| Environment | Toolchain | compile | runs? |
|---|---|---|---|
| **Mac native** (Apple Silicon) | rustc 1.95.0 | debug 0.67 s / `-O` 0.13 s | ✅ |
| **Docker Alpine** arm64 (native musl) | rustc 1.91.1 (apk) | debug 0.05 s / `-O` 0.04 s | ✅ |
| **Akuma 16 GB** — codegen only (`--emit=obj`) | nightly 1.98 | ~24 s | (.o) |
| **Akuma 16 GB** — full compile + link (apk) | apk 1.96 | **~70 s** | ✅ runs |
| **Akuma 16 GB** — full compile + link (nightly) | nightly 1.98 | **~120 s** | ✅ runs |

Akuma is **~150–1000× slower** than native — dominated by demand-paging the
305 MB `librustc_driver.so` + libLLVM off virtio-blk and the slow link-fork
(§5b). A single dependency-free `hello.rs` now compiles, links, and runs over one
SSH session.

> **TODO (still open):** (a) compile+run hello.rs across `MEMORY` 4→16 GB with
> timings, (b) find the RAM floor counting down from 1.5 GB, (c) tabulate vs
> Mac/Docker. `scripts/rustc_ram_sweep.sh` already boots each size and runs a
> probe — extend it to compile + run + time now that the link completes.

---

## 7. Future ideas (built-in shell / userspace gaps)

Surfaced while bringing up the toolchain — none block self-hosting, but each is a
papercut that "everyone expects" to work:

- **Split stdout / stderr.** The mini-shell only honors `>` (stdout); `2>` leaks
  `2` as an argument and `2>&1` is unsupported. Real shells separate the streams —
  needed for `cmd 2>err.log` and for tools that distinguish the two. Today rustc's
  errors can only be read off the live stderr channel on fast commands.
- **`/dev/null`.** No null device, so the common `… 2>/dev/null` / `> /dev/null`
  idioms don't work. Add a `/dev` null (and `/dev/zero`) device node.
- **`ln` in the built-in shell.** No link command; symlinks/hardlinks must be
  created out-of-band (host-side `populate_disk`). A built-in `ln`/`ln -s` would
  let in-VM setup (e.g. toolchain shims) be scripted.
- **Interactive stdin-readers over `ssh host cmd`.** With the §5 fix a
  stdin-reader (e.g. `cat`) exits cleanly when the client *sends* `CHANNEL_EOF`
  (piped input) or on real disconnect, but non-interactive `ssh host cat` where
  the client never signals EOF still just waits — and `CHANNEL_EOF`/`CHANNEL_DATA`
  delivery while the reader is parked is occasionally flaky. The canonical-mode
  stdin path could use a hardening pass.
- **A detached build runner.** `box use -d` + `box grab` exist (grab re-streams
  live output) but "log persistence while detached" is TBD. With the §5 fix the
  session survives a build, so this is now optional, but it'd make long
  multi-crate builds robust to client drops.
- **Fix all clippy warnings in the kernel (`src/`).** The pre-commit hook only
  lints `crates/*/` with `-D warnings`; the kernel crate is unchecked. The
  workspace enables `clippy::pedantic` + `nursery`, so `cargo clippy --release`
  reports ~3580 warnings on the `akuma` bin (~299 are default `clippy::all`, the
  rest pedantic/nursery — top offenders: `doc_markdown`, `items_after_statements`,
  `uninlined_format_args`, `ptr_as_ptr`, `manual_let_else`). ~2373 are
  machine-applicable via `cargo clippy --fix`. Worth burning down so the kernel
  can eventually be added to the pre-commit gate alongside the crates — and a few
  default lints (a dead `val = 1` write in `sync_tests.rs`, `unwrap()`-after-
  `is_err()` in `process_tests.rs`) are worth fixing on their own merits. Do it in
  batches by lint family, building under each profile (`release`/`size`/`extreme`)
  since much kernel code is `cfg`-gated and `--fix` only touches the active config.
- **Clean up `userspace/libakuma` warnings + clippy.** Building any libakuma-linked
  userspace crate (`hello`, `httpd`, …) currently emits ~7 `libakuma` warnings
  (e.g. an elided-lifetime suggestion on `Spinlock::lock`). Burn these down and add
  `userspace/libakuma` (and ideally the rest of the userspace workspace) to a
  `-D warnings` clippy pass, so the in-VM self-host build of userspace is clean.
- **Housekeeping: drop `sshd` and `needle-server` from the userspace workspace.**
  Candidates for removal from `userspace/Cargo.toml` `members` to slim the
  self-host build surface (`sshd` is the lone `net-async` consumer; revisit whether
  either is still needed in-tree).
- **In-VM `git` is broken for pulling — fix it.** Two separate problems block
  `git pull` of a branch into the on-disk `/root/akuma` (June 2026):
  1. **`git-remote-https` (apk git 2.54) SIGSEGVs** — `git fetch` over https dies
     with `git-remote-https died of signal 11`; the kernel log shows a wild
     instruction-abort (`[WILD-IA] ELR=0x0`, jump to null) in the helper. So
     git-over-https fails *even though cargo-over-https works* — different HTTP
     client (libcurl vs cargo's). Likely the same exec/relocation corruption class
     as the nightly-`cargo`/`rustc` `EC=0x0` crashes (§4, §8).
  2. **`scratch` (Akuma's git client, `/bin/git`) discrepancies with real git** —
     no `-C <dir>` flag; looks for lowercase `.git/head` instead of standard
     `.git/HEAD`; and relies on CWD, which is itself broken: **`cd X && cmd` runs
     `cmd` with CWD `/`** (chdir doesn't propagate through `exec` in Akuma — verified
     `sh -c 'cd /root/akuma && pwd'` prints `/`). Fixing the chdir/exec CWD
     inheritance is the prerequisite; then align scratch's `.git` layout + add `-C`
     so it can stand in for real git in-VM. Workaround for now: pull on the host and
     re-populate the disk, or apply changes to the on-disk tree directly.

### 7a. rustc compile time: it's ext2 read + library loading, not fork+exec or CPU

Measured on the **apk** toolchain (`/usr/bin/rustc` 1.96 stable; the nightly at
`/usr/local/bin` *segfaults* on a real compile — apk is the working one), 6 GB VM,
`hello.rs`, HVF (near-native CPU). Ablation of one full compile:

| phase | eager mmap | lazy mmap | note |
|---|---|---|---|
| rustc startup (`--version`) | 11.7s | **1.7s** | load+relocate `librustc_driver`+`libLLVM` |
| read libstd metadata | 7.8s | 1.8s | ext2 read of rlibs |
| codegen | 0.1s | 0.6s | **CPU is negligible** |
| link (gcc→ld) | 16.1s | 15.6s | spawn + read rlibs + write output |
| **full** | **35.6s** | **19.6s** | lazy output verified correct |

Findings:
- **Codegen (actual compile CPU) is ~0.1s — irrelevant.** rustc is slow here
  purely from moving bytes off ext2.
- **No effective cache (by default).** Repeated identical compiles take the *same*
  time (`--version`: 11.56 / 11.74 / 11.77s). The default ext2 "cache" is a 64-slot
  ring (`BLOCK_CACHE_ENTRIES`, ~64–256 KB) — far smaller than a build's working
  set — and file-backed mmap pages aren't cached by inode. Every spawn re-reads the
  whole toolchain from disk. The opt-in **`fs-cache`** feature replaces the ring
  with a RAM-sized clock cache that fixes this — see §7c (warm re-reads become 0
  disk reads; on HVF the wall-clock win is small because the disk isn't the
  bottleneck there).
- **Eager file-backed mmap was the big tax.** `libLLVM.so` (176 MB) +
  `librustc_driver.so` (63 MB) were read *in full* at `mmap()` time even though
  rustc touches only a fraction. Flipping `release` to lazy demand-paging (1 MB
  readahead) cut startup **6.9×** and the full compile **1.8×**. Raw sequential
  ext2 read is fine (~100 MB/s via `md5sum`); the eager-mmap *load path* was the
  problem, exactly as predicted.
- **Link dominates the lazy compile (~80%, 15.6s) — and it's libstd.**
  `hello.rs` is a *std* program (`println!`), so the link statically pulls in
  libstd. A `#![no_std]` binary links in ~5s vs ~15s (≈3× cheaper) — so for the
  actual akuma kernel (which *is* `#![no_std]`) per-crate link is cheap; the std
  link cost is an artifact of the hello-world test, not the real workload.
  - `-C prefer-dynamic` (link libstd as `.so`) did **not** cut compile time
    (~19.5s ≈ static), so the residual link cost is the gcc→collect2→ld toolchain
    load + C-runtime reads, not the libstd archive read specifically.
- **Alternative linkers make it WORSE on Akuma — do not use them.** A/B on the
  std hello (default = gcc + GNU ld @ 19.3s):
  - gcc + **lld** (`-fuse-ld=lld`): **>320 s (timeout)**
  - clang + **lld**: **240 s**, and the output ELF won't even load ("Invalid ELF")
  GNU ld (binutils) wins by 12–16×. lld is a large binary and its load/relocation
  pattern thrashes the cacheless ext2. clang-as-linker-driver is also broken here.

> **Measurement noise warning.** VM timings are unstable (single core, no cache,
> preemption): obj-only was measured at 4.0 s and 14.6 s in different runs. Only
> trust large effect sizes (lazy 1.8×, lld 12–16× worse). For fine-grained
> numbers, take min-of-N samples on a freshly-rebooted snapshot.

### 7b. How long to compile the whole kernel at current speed?

`cargo build --release -j1` for akuma is **127 packages** (`Cargo.lock`) → one
`rustc` per crate + build scripts + host proc-macros + one final link. The
killer is **no cache**: the ~1.7 s lazy `rustc` startup is paid afresh for *every*
crate (≈127 × 1.7 s ≈ **3.6 min just re-paging rustc's own `.so`s**), and each
crate re-reads its upstream rlibs from ext2. Adding per-crate codegen (most are
small no_std libs ~4–8 s; a few big ones — smoltcp, embedded-tls, the kernel crate
itself — run tens of seconds to minutes), a clean `-j1` build lands at a rough
**~30–60 minutes** (wide error bars; the kernel crate + the handful of large deps
dominate). This is why the **ext2 / page cache is the right next lever**: most of
that time is re-reading the *same* read-only toolchain and rlibs off disk N times.
The **`fs-cache`** feature (§7c) is the first cut at this lever.

Things to check later:
- **`release` lazy default — DONE/SHIPPED.** `MMAP_FILE_BACKED_LAZY = true` on all
  profiles now. Eager only wins when *all* mapped pages are touched (e.g. model
  weights), not for big partially-used libs.
- **Was `extreme` ever eager?** *No — resolved.* `build.rs` makes `extreme` imply
  `kernel_profile_size` (extreme ⇒ `OPT_LEVEL=z` ⇒ `size_profile` ⇒
  `kernel_profile_size` cfg), so the old `#[cfg(not(kernel_profile_size))]` already
  excluded it. `size` and `extreme` have used lazy all along; only `release` was
  eager. The gating is now written explicitly as `any(size, extreme)`.

### 7c. The `fs-cache` feature: a large clock block cache — SHIPPED (opt-in)

The 64-slot block ring (`BLOCK_CACHE_ENTRIES`, ~256 KB) is far smaller than a
build's working set, so it gives no reuse: every spawn re-streams the toolchain
off virtio-blk. The **`fs-cache`** cargo feature replaces it with a much larger,
clock-eviction block cache that keeps the read-only toolchain resident across the
many rustc/cc/ld spawns.

- **Build it in:** `cargo build --release --features fs-cache`. Off by default; not
  added to any default set and **never combined with `extreme`** (the 4 MB profile
  keeps its no-cache path). Plumbing mirrors `extreme`: kernel feature
  `fs-cache` → `akuma-ext2/fs-cache` → `build.rs` emits `cfg(ext2_fs_cache)`.
- **Sizing:** RAM-derived, set in `src/fs.rs::init()` before mount via
  `akuma_ext2::set_cache_cap_bytes(min(25% RAM, 512 MB))`. On a 6–16 GB self-host
  VM that's the full 512 MB — enough to hold the hot toolchain set.
- **Policy:** CLOCK / second-chance (one reference bit per slot via `Cell`, a
  rotating hand), so frequently-touched toolchain blocks survive while cold blocks
  stream past — a pure ring would evict the hot set as the working set overflows.
  Lookup is O(log n) via a `block_num → slot` `BTreeMap` (a linear scan over the
  ~131 072 slots of a 512 MB cache per block read would dwarf the disk read it
  avoids). Write-through with per-block invalidation is preserved.
- **Scope:** physical-block keyed; `read_block`, `read_at_by_inode` (the file-backed
  mmap fault path, previously cache-bypassing), **and metadata** — `read_inode`,
  `read_bgd`, `write_inode`, `write_bgd` — all consult and populate it. The metadata
  functions used to issue direct sub-block `dev.read_bytes` for each inode/descriptor;
  under the feature they read the *containing* block through the cache (`read_range_cached`),
  and writes go read-modify-write through `write_block` so a cached inode/BGD block
  can't go stale (the only correctness coupling — it's why metadata caching is part of
  `fs-cache`, not a separate switch). Reading the 4 KB block is also a *prefetch*: it
  pulls ~16 neighbouring inodes that a directory scan stats next. *Not* routed through
  the cache: the inode/block bitmaps already go via `read_block`; the superblock is
  read once at mount and never via `read_block`, so it stays direct.
- **Tests:** clock/second-chance/dedup/remove/floor unit tests in
  `crates/akuma-ext2/src/tests.rs`; the full fs read/write suite run under
  `--features fs-cache` exercises the cached inode/BGD RMW path (proves write
  invalidation). A boot self-test (`test_fs_cache_warm_reread_hits` in
  `src/process_tests.rs`, gated on `feature = "fs-cache"`) reads a temp file twice
  (data: `warm misses=0`) **and** re-resolves a path twice (metadata: `hit>0, miss=0`).

**Two workloads, two very different stories (`scripts/ext2_cache_bench.sh` and
`ext2_cache_meta_bench.sh`, MEMORY=6144M, HVF, `disk_selfhost.img`):**

*File data — mmap 320 MB `librustc_driver.so`, touch every page:*

| kernel | cold | warm | note |
|---|---|---|---|
| no cache (64-slot ring) | 4.47s | 4.48s | warm/cold = 1.00× (floor) |
| `--features fs-cache`   | 6.38s | 4.32s | warm fully cached (`warm misses=0`) |

Marginal on HVF — only ~6% on warm, and cold *regresses* ~+1.9s. Under HVF the
"disk" is a host file already in the Mac's page cache, so this demand-paged mmap is
**page-fault-bound, not disk-bound**; the extra copy into the cache is then mostly
overhead. (It's the IOP/latency win below that matters, not file-data bandwidth.)

*Metadata — `du -s /usr/local/lib/rustlib` (493 MB, thousands of files; pure stat
walk):*

| kernel / variant | cold | warm | speedup |
|---|---|---|---|
| no cache | 6.55s | 6.51s | 1.00× (floor) |
| `fs-cache`, **data blocks only** (inode/BGD bypass) | 6.79s | 6.76s | **1.00× — none** |
| `fs-cache`, **full** (inode/BGD cached) | 0.39s | 0.34s | **~19×** |

**This is the real result.** A metadata-heavy walk — exactly what a `cargo build`
*is* (open/stat hundreds of rlibs and re-walk the same prefix dirs across 127
spawns) — goes **~19× faster, even cold**, and the isolation row proves it's *all*
from the inode/BGD caching: caching directory *data* blocks alone does nothing,
because the dominant cost was thousands of tiny separate `dev.read_bytes` IOPs for
inodes/descriptors. The cache collapses them into a handful of 4 KB block reads
(each prefetching 16 inodes) plus cache hits. And this is *under HVF* — the win is
**IOP/latency**, not bandwidth, so it survives even when the host page-caches the
disk. Expect it to be larger still on TCG / real storage.

Takeaway: `fs-cache`'s payoff is **metadata / IOP reduction**, not file-data
bandwidth. Shipped opt-in (it's not in any default set and never combines with
`extreme`); on by intent for the self-host / cluster build path.

Still open / follow-ups:
- **Memory-pressure shrink.** The cache is bounded by the RAM-derived cap but does
  not yet shrink under `is_memory_low`. Wire a reclaim hook so a cache that grew to
  512 MB can be dropped if user demand-paging needs the pages.
- **Cold-path tax (file data).** The per-block `BTreeMap` insert + extra backing copy
  cost ~+40% on the first *bulk* read (the metadata path has no such regression —
  it's faster even cold). An open-addressing index would trim it.
- **Measure a full `cargo build -j1`** with the cache on, now that the metadata path
  — the build's real bottleneck — is cached.

### 7d. Compiling a userspace crate (libakuma `hello`) in-VM — IN PROGRESS

A much smaller self-host target than the kernel: compile a `userspace/` crate that
links `libakuma` directly in the VM (`hello` first, then `httpd`). Findings
(June 2026, fs-cache kernel, `disk_selfhost.img`, 8 GB):

- **Toolchain combo:** the **nightly `cargo`** (`/usr/local/bin/cargo`) **crashes**
  at startup in Akuma (`[Exception] Unknown from EL0: EC=0x0`, ~31 syscalls in) — the
  same "nightly segfaults" seen for a real `rustc` compile. The **apk `cargo` 1.96**
  (`/usr/bin/cargo`) runs fine, but apk's rust only ships the `aarch64-alpine-linux-musl`
  target. So the working combo is **apk cargo + `RUSTC=/usr/local/bin/rustc`** (nightly
  rustc has the `aarch64-unknown-none` std the userspace target needs):
  `busybox env PATH=/usr/local/bin:/usr/bin:/bin HOME=/root CARGO_HOME=/root/.cargo RUSTC=/usr/local/bin/rustc cargo build --release -p hello --manifest-path /root/akuma/userspace/Cargo.toml`
- **Workspace must be loadable.** Cargo requires *every* listed workspace member's
  `Cargo.toml` to be present just to *load* the workspace — so a missing submodule
  (e.g. `meow`) blocks building even unrelated crates like `hello`. Fixed by
  temporarily removing the submodule-backed members (`meow`, `tcc`, `llama.cpp`,
  `crush`, `nca`) from `userspace/Cargo.toml`'s `members`, removing the stale `xbps`
  submodule from `.gitmodules`, and repointing `meow`/`crush` from `git@github` to
  `https` (so the in-VM checkout needs no SSH key).
- **cargo-over-TLS works (the key result).** `cargo build -p hello` downloaded all
  deps from crates.io over the VM's network (HTTPS) into `/root/.cargo/registry` and
  began compiling them — no vendoring needed (updates §1.3).
- **The build-script wall.** The build then parks on `embedded-io-async`'s `build.rs`
  (a build script cargo compiles *and executes* on the build machine; it probes the
  compiler by spawning `rustc`). Fork+exec-of-rustc-from-a-build-script is the sticky
  point for in-VM builds. **Fixed by feature-gating** `embedded-io-async` in
  `libakuma` behind a `net-async` feature that is **off by default** (it only gates
  the `embedded_io_async::Error` impl in `net.rs`). Consumers that use the async IO
  traits opt in with `features = ["net-async"]` — currently just **`sshd`**
  (`embedded_io_async::{Read,Write}` + libakuma); `hello` and `httpd` use neither, so
  they get a minimal tree with no flags. A fresh `hello` build is then just **6
  crates, none with a rustc-probing build.rs**: `format_no_std`, `scopeguard`,
  `lock_api`, `talc`, `libakuma`, `hello`. (`embedded-io-async` may be dropped from
  `libakuma` entirely later if `sshd` can use its own copy.)
- **Runtime verified.** The shipped `/bin/hello` (libakuma-linked) runs correctly in
  the VM (`hello (1/10)`…`(10/10)`, `uptime≈expected`), confirming the libakuma
  runtime/ABI on Akuma — independent of the in-VM compile.
- **The next wall: `rustc` futex deadlock — ROOT-CAUSED & FIXED.** With the
  build-script crate gone, the build *does* reach codegen — and then **`rustc`
  hung on a futex**. Observed building `scopeguard` (the first dep): kernel PSTATS
  frozen across three 60 s samples — `rustc` PID stuck at `1448 syscalls`,
  `futex=78 (440716ms)`, `in_kernel ≈ 441 s`, zero forward progress; `cargo`
  parked waiting on it. It had paged in ~200 MB (the toolchain) and deadlocked
  *mid-compile*, not at startup. **Not OOM** (6.3 GB free of 8 GB).
  **Nondeterministic** and **got worse the deeper into the build it ran.**

  - **Root cause — `FUTEX_WAIT_BITSET` absolute timeout treated as relative**
    (NOT a lost-wakeup; the wait/wake core is sound — its sticky `WOKEN_STATES`
    flag, re-checked after `mark_thread_waiting`, closes the wait-entry window).
    Rust std emits an **absolute** `CLOCK_MONOTONIC` deadline (`now + dur`) via
    `FUTEX_WAIT_BITSET` **without** the `CLOCK_REALTIME` flag for *every* timed
    wait (`Condvar::wait_timeout`, `park_timeout`, `Mutex`/`Once` contention —
    exactly rustc's rayon/jobserver idle loops). Akuma's `sys_futex` only treated
    `FUTEX_WAIT_BITSET` timeouts as absolute when the realtime flag was set; the
    monotonic case fell through to the relative branch and added `uptime_us()` to
    an already-absolute, `uptime`-based deadline. Effective wait ≈ `2·uptime + dur`
    — and since it scales with current uptime, a short timed wait silently became
    an ~800 s one after the toolchain had paged in for ~400 s, presenting as a
    nondeterministic, deeper-is-worse "deadlock."
  - **Fix** (`src/syscall/sync.rs`): `FUTEX_WAIT_BITSET` deadlines are now
    absolute (Linux semantics) for **both** clocks. Monotonic == `uptime_us`, used
    directly; realtime is converted into uptime terms via `utc_time_us()`. Plain
    `FUTEX_WAIT` stays relative.
  - **Regression test** (`src/sync_tests.rs`):
    `test_futex_wait_bitset_monotonic_absolute_deadline` parks on an absolute
    monotonic deadline `uptime_now + 100 ms` and asserts the wait ends *near that
    deadline*, not ~uptime later. Verified to **fail on the old code** (panic:
    `overshoot 1,131,264 us` ≈ the uptime at call) and **pass after the fix**
    (overshoot ~7.5 ms). All 30 boot-suite futex tests pass.

- **The wall after that: the linker output was 0 bytes — `MAP_SHARED` writeback
  (IMPLEMENTED).** With the futex fix the build now compiles all 6 crates
  (`scopeguard`, `lock_api`, `talc`, `libakuma`, `format_no_std`, `hello`) and
  reaches the **link step** — further than the futex deadlock ever allowed. `cargo`
  reports `Finished`, but the linked `hello` landed on disk as **0 bytes**.
  - **Root cause.** `rust-lld` (the default linker for `aarch64-unknown-none`)
    writes its output through a writable `MAP_SHARED` file-backed `mmap`
    (LLVM `FileOutputBuffer`), then `munmap`s + `renameat`s it into place. Akuma
    had **no unified page cache**, so it silently downgraded writable `MAP_SHARED`
    file mappings to `MAP_PRIVATE` (`src/syscall/mem.rs`) — lld's writes went to
    private anonymous frames that were never flushed back, so the file stayed
    empty.
  - **Fix.** Implemented writable `MAP_SHARED` file-backed mappings with explicit
    writeback (`src/syscall/mem.rs`): such a mapping is now allocated **eagerly**
    (all pages resident, populated from the file, mapped RW) and tracked in
    `SHARED_FILE_MAPPINGS` by `(tgid, base_va)`. Its resident pages are copied back
    to the backing file on **`munmap`**, **`msync`** (was a no-op stub; now wired —
    the POSIX flush point lld uses), and **process exit** (`sys_exit`/
    `sys_exit_group`, so a process that drops the mapping by exiting still
    persists, and stale `(tgid, base)` entries can't mis-fire after tgid reuse).
    Read-only `MAP_SHARED` is unchanged (no writes → stays on the cheap lazy path).
    Eager-only: a writable `MAP_SHARED` that can't get its frames returns `ENOMEM`
    rather than silently dropping writes (so huge outputs under memory pressure are
    the known limit; fine for userspace self-host binaries).
  - **Test.** Boot self-test `test_shared_file_mmap_writeback`
    (`src/process_tests.rs`): fills two resident pages with a pattern, calls the
    writeback path, and verifies the file (incl. a partial last page) — `PASSED
    (wrote 4196 bytes, pattern verified)`.
  - **END-TO-END VERIFIED.** In-VM, `hello` now compiles (apk `cargo` +
    `RUSTC=`nightly, `--target aarch64-unknown-none`), links to a **4128-byte**
    binary (kernel log: `[mmap] … (shared-writable, writeback on)` →
    `[munmap] … shared-writeback … 4128 bytes`), and **runs correctly**:
    `hello (1/10)`…`(10/10)`, `done`, `uptime=9007ms expected=9000ms overhead=+7ms`.
    This is the first full in-VM self-host compile **and run** of a userspace crate.
  - **`httpd` too (VERIFIED, decisively).** The same flow compiles `httpd` in-VM
    (~30 s, writeback flushed a ~42 KB binary) and it **serves**. `httpd` was made
    to read its listen port from the `HTTP_PORT` env var (then argv[1], then the
    8080 default) so the freshly-built binary can run **alongside** the autostarted
    `/bin/httpd` (still on 8080) on a *different* port — proving the server that
    answers is unambiguously the new build, since the old binary hardcodes 8080 and
    has no env-port code. Running the rebuilt binary with `HTTP_PORT=4444`, it binds
    4444 and serves: in-VM `busybox wget http://127.0.0.1:4444/` → `index.html`
    (20595 B, exact match), `GET /nope.txt` → `404`, and host `curl localhost:4444`
    (forwarded) → `200`/20595 B — while `/bin/httpd` keeps serving 8080 independently.
    (Disk note: SSH-stdin file copy truncated the source; re-stage the file
    host-side via the Docker loop-mount, or `git`-pull on the disk, then rebuild.)
  - **In-VM build invocation (works today).** CWD is `/` over `ssh host cmd`
    (§7 chdir bug), so cargo can't find `userspace/.cargo/config.toml`; pass the
    target + rustflags via env instead, and call **apk** cargo explicitly (nightly
    cargo crashes at startup, §7d):
    `busybox env PATH=/usr/local/bin:/usr/bin:/bin HOME=/root CARGO_HOME=/root/.cargo RUSTC=/usr/local/bin/rustc CARGO_BUILD_TARGET=aarch64-unknown-none CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS=-Crelocation-model=static /usr/bin/cargo build --release -p hello --manifest-path /root/akuma/userspace/Cargo.toml`
    (note: cargo's incremental cache treats a previously-linked 0-byte `hello` as
    fresh; `rm -rf target/aarch64-unknown-none` to force the relink that exercises
    writeback).

**Disk refresh (how to get repo changes in-VM):** in-VM `git` can't pull (§7,
git-remote-https SIGSEGV; scratch broken), so re-stage `/root/akuma` host-side via
Docker — privileged Alpine, `mount -o loop disk_selfhost.img`, `git clone --depth 1
--branch <branch> https://github.com/netoneko/akuma.git /mnt/disk/root/akuma`,
`umount`. (`netoneko/akuma` is public; no token needed. No submodule init required —
the workspace `members` no longer reference them.)

**Next session:** the in-VM self-host compile **and run** now works end-to-end for
both **`hello`** and **`httpd`** (futex fix + `MAP_SHARED` writeback above). Next:
push toward larger crates / the kernel itself. Watch for: (a) the
eager-only writable-`MAP_SHARED` limit on large link outputs under memory pressure;
(b) the §7 chdir/CWD bug still forces the env-var build invocation; (c) the
intermittent `EC=0x0` exec corruption (§8) seen once when an SSH command violated
the mini-shell grammar. Committed: the futex fix + regression test (`f7ea7dc`),
plus the earlier workspace/gate work (faedd3a/6ba831a). Uncommitted: the
`MAP_SHARED` writeback (`src/syscall/mem.rs`, `mod.rs`, `proc.rs`) + its self-test
(`src/process_tests.rs`), these doc updates + `scripts/ext2_cache_*bench.sh`.

### 7e. First in-VM **kernel** `cargo build` attempt — reaches dep compilation, walls on `proc-macro2`'s build script (June 18 2026)

First attempt at building the **akuma kernel itself** inside the VM (the §7d work
proved userspace crates; this is the kernel). Kernel boot: `--release
--features fs-cache`; VM `MEMORY=12288M`, `disk_selfhost.img`, HVF.

**Setup that got it building.** The kernel can't be driven by the nightly `cargo`
(it still crashes at startup — see below), so the only working driver is the §7d
combo **apk `cargo` 1.96 + `RUSTC=`nightly**. apk cargo is *stable*, so it refuses
the workspace's nightly-only bits at manifest-parse time. Two edits to the on-disk
`/root/akuma/Cargo.toml` make it stable-parseable for a **release** build (release
is `panic="abort"`, so neither bit is actually needed by it):

1. delete the top line `cargo-features = ["panic-immediate-abort"]`;
2. change `profile.size`'s `panic = "immediate-abort"` → `panic = "abort"`.

Edit the image host-side via the Docker loop-mount (the §7d disk-refresh
mechanism); a backup is left at `Cargo.toml.selfhost-bak`. Then build with the
env-var invocation (CWD-is-`/` over ssh, §7, means `.cargo/config.toml` is not
read — pass target + the linker-script rustflag explicitly):

```
/bin/busybox env PATH=/usr/local/bin:/usr/bin:/bin HOME=/root CARGO_HOME=/root/.cargo \
  RUSTC=/usr/local/bin/rustc CARGO_BUILD_TARGET=aarch64-unknown-none \
  CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS=-Clink-arg=-T/root/akuma/linker.ld \
  /usr/bin/cargo build --release -p akuma --manifest-path /root/akuma/Cargo.toml -j1
```

**What worked (further than ever):**
- **apk cargo parses the kernel workspace** after the 2-line edit.
- **cargo's own git-over-HTTPS fetch works in-VM** — it cloned the `embedded-tls`
  GitHub fork (1437/1551 deltas, full checkout) and updated the crates.io index.
  Notable because §7's `git-remote-https` *binary* SIGSEGVs; cargo uses its own
  client (libgit2/gitoxide), which is fine. So git deps are **not** a blocker.
- **~11 leaf deps compile cleanly** (zeroize, typenum, version_check,
  generic-array, crypto-common, subtle, const-oid + their build scripts), driven
  by apk-cargo-orchestrates + nightly-`rustc`-per-crate, host target
  `aarch64-unknown-linux-musl`. Reached **step 12/147**.

**The wall — `proc-macro2`'s compiler-probing build script deadlocks.** This is
the §7d "build.rs that probes the compiler by spawning rustc **parks**" wall, now
pinned to a crate that is **unavoidable for the kernel** (every derive macro —
`zerocopy`, `thiserror`, `serde`-likes, etc. — depends on `proc-macro2`). The §7d
workaround (delete the offending crate) is not an option here.

Sequence from the kernel log: `[T200.20]` proc-macro2's `build-script-build` runs
→ `[T200.82]` it execs a child `rustc --cfg=procmacro2_build_probe` → `[T201.75]`
that probe rustc **exits cleanly** (teardown munmaps) → then **silence**; the build
sits at `12/147: proc-macro2(build)` indefinitely with the **CPU fully idle**
(heartbeat idle-loop spinning at 38M) — a hard **deadlock**, not slowness.

Diagnosis via the in-VM `ps` builtin + PSTATS:
- **cargo's main thread is blocked in `futex` (syscall 98)** — its coordinator
  waiting on a worker that never reports completion.
- **Two `rustc --cfg=procmacro2_build_probe` processes (pids 231, 233) are still
  alive but orphaned** — their parents (230, 232) have exited. They are parked in
  a **non-syscall kernel wait** (`ps` SYSCALL column `-`, i.e. `current_syscall ==
  !0`, not futex, not any syscall) while the CPU is idle. A userspace thread can
  only block via a syscall or a page fault, so this points at a **page-fault
  (demand-paging) wait** or a scheduler-level park that never gets woken —
  resolvable only with a live debugger.
- The pipe machinery is **not** the cause: per-spawn build-script pipes close
  cleanly (`[pipe] DESTROY`), and fork/exit pipe refcounting is symmetric
  (`clone_deep_for_fork` bumps; `close_all` on exit decrements — `fd.rs`). The
  jobserver pipe (id 63) shows an elevated `write_count` (~12, inherited by every
  forked child) but it drains, so it's a suspect to confirm, not the proven cause.

**Still-confirmed blocker:** nightly **`cargo`** still dies at startup with
`[Exception] Unknown from EL0: EC=0x0` (~31 syscalls in, after paging ~11 MB) —
unchanged by the futex/`MAP_SHARED` fixes. nightly **`rustc`** runs fine; only
nightly cargo crashes. This is why apk cargo is the driver.

**Next session — root-cause the build-script→rustc-probe deadlock.** It is THE
blocker for kernel self-host (any proc-macro pulls in `proc-macro2`). Plan:
build a *deterministic minimal repro* (a tiny crate whose `build.rs` just
`Command::new("rustc").arg("--version").output()`s, or replays proc-macro2's
probe), boot with `GDB=1`, and attach lldb (`docs/` lldb+gdbstub note) to a stuck
orphaned `rustc` probe to read its actual kernel wait state (page-fault vs.
futex/park) — that one fact picks the fix. Suspects to check statically
meanwhile: the demand-paging fault wait/wakeup path, and orphan reparenting
(`ps` shows orphans keeping a dead PPID — Akuma does not reparent to init).

### 7f. Minimal repro built — the wall is NOT "build.rs spawns rustc" (June 18 2026, IN PROGRESS)

Built the deterministic minimal repro the §7e plan called for, and it **narrowed
the cause significantly**. Repro crate is in the host tree at `userspace/selfhost_repro/`
(excluded from the userspace workspace — built only inside the VM)
(`Cargo.toml`, `lib.rs`, `build.rs`) and staged on `disk_selfhost.img` at
`/root/repro` via the Docker loop-mount.

**Build/run invocation (in-VM, over ssh on :2322 with `INSTANCE=1`):**
```
env PATH=/usr/local/bin:/usr/bin:/bin HOME=/root CARGO_HOME=/root/.cargo \
  RUSTC=/usr/local/bin/rustc \
  /usr/bin/cargo build -vv --manifest-path /root/repro/Cargo.toml
```
Note: the in-kernel SSH mini-shell does **not** support `2>&1`/fd redirection
(it creates a literal file named `&1`); run the command bare and let the host
capture both streams (e.g. Python `subprocess` `capture_output`). No `busybox
sh -c` wrapper needed.

**Result of the first repro (DID NOT deadlock).** A build.rs that does the
"obvious" probes — `rustc --version`, `rustc -vV`, and a stdin-piped
`rustc --emit=metadata -` compiling a snippet, **all for the host target** —
**completes cleanly** (`child exited ok=true`, build `Finished` in ~28 s). So the
wall is **not** the generic "a forked build script spawns a child rustc and waits"
shape. That shape works on Akuma.

**What's actually different about proc-macro2's probe** (read from the vendored
`selfhost_vendor/proc-macro2/build.rs` → `do_compile_probe`), any of which could
be the trigger:
- **`--target $TARGET`** where, for the kernel build, `TARGET=aarch64-unknown-none`
  (the bare-metal kernel target, **not** the host). The probe rustc therefore
  loads that target's sysroot (libcore etc.) — a **different file-backed mmap /
  demand-paging path** than the host-libstd path the first repro exercised. This
  is the prime suspect (the kernel build sets `CARGO_BUILD_TARGET=aarch64-unknown-none`,
  so build-script `TARGET` is the bare-metal triple).
- It appends **`CARGO_ENCODED_RUSTFLAGS`** (the `-Clink-arg=-T/root/akuma/linker.ld`
  rustflag from the env-var invocation) to the probe command.
- `--emit=dep-info,metadata` (writes a `.d` file too), `--cfg=procmacro2_build_probe`,
  `--cap-lints=allow`, compiling a real file containing `extern crate proc_macro;`.
- It `fs::create_dir`s an OUT_DIR/probe subdir and `fs::remove_dir_all`s it after.
- It runs the probe **up to twice** (with/without `RUSTC_BOOTSTRAP`) — matching
  the "two orphaned rustc probes" observation in §7e.

**Next step (repro v2, staged but NOT yet run).** `userspace/selfhost_repro/build.rs` was
rewritten to faithfully replay `do_compile_probe` — `--target aarch64-unknown-none`,
the linker rustflag via `CARGO_ENCODED_RUSTFLAGS`, `--emit=dep-info,metadata`,
`extern crate proc_macro`, the create_dir/remove_dir_all, run twice — with a
`[REPRO] …` marker before/after **each** step (write probe src, create_dir, spawn,
return-from-wait, remove_dir_all) so the kernel console pins the exact walling
step. Run it with `CARGO_BUILD_TARGET=aarch64-unknown-none` and the linker rustflag
so the build-script `TARGET` is the bare-metal triple:
```
env PATH=/usr/local/bin:/usr/bin:/bin HOME=/root CARGO_HOME=/root/.cargo \
  RUSTC=/usr/local/bin/rustc CARGO_BUILD_TARGET=aarch64-unknown-none \
  CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS=-Clink-arg=-T/root/akuma/linker.ld \
  /usr/bin/cargo build -vv --manifest-path /root/repro/Cargo.toml
```
(`REPRO_PROBES=N` controls how many times it loops the probe; `REPRO_MODE` from
the first version is gone.) If v2 reproduces, boot `INSTANCE=1 GDB=1` and attach
lldb (gdbstub :1235, ssh :2322) to the stuck rustc to read its kernel wait state
(page-fault wait vs futex/park) per the §7e plan. If it still does **not** wall,
the trigger is the **accumulated** state after ~11 prior crate compiles (a per-
spawn leak — child-channel registry / fault-set / pid), and the next repro is a
loop of N back-to-back rustc spawns, or a crate that path-depends on the vendored
`proc-macro2` directly (also in `selfhost_vendor/`).

Working notes/commands for resuming: kernel built (`cargo build --release`); boot
`MEMORY=6144 DISK=disk_selfhost.img INSTANCE=1 GDB=1 cargo run --release`; wait for
`SSH Server] Listening` then drive the build over ssh :2322 from Python.

### 7g. Repro v2 REPRODUCES the hang — root cause is a parked-thread missed wakeup, NOT the fault path (June 19 2026)

Repro v2 (`do_compile_probe` with `--target aarch64-unknown-none`) **deterministically
reproduces the hang within ~seconds** (no need to compile 11 deps first). It is the
real proc-macro2 wall.

**Findings (live, via in-VM `ps` + a serial `[THR-DUMP]` heartbeat dump):**

- **The hang is a PARK, not a spin.** lldb PC-sampling (60 samples) caught only
  background threads (`smoltcp_net::poll`, `ssh::server::run`, the SGI reschedule
  path) — never a cargo/rustc thread. The CPU genuinely idles. So the stuck user
  threads are blocked (`schedule_blocking`/WAITING), not busy-spinning.
- **Process tree when hung:** cargo main (`pid 95`) parked in `futex` (sc=98);
  build-script in `wait4`; the probe `rustc` chain (`pid 182 → 184 → 185 …`) and a
  cargo worker parked **not in any syscall** (`sc = -`).
- **`[THR-DUMP]` (saved kernel/user resume points) is the smoking gun:** **six**
  threads — cargo worker `pid 108`/`180` and the rustc probes `183/185/187/189` —
  are all in **WAITING** state parked at the **identical user PC `elr=0x30060cc4`**
  with `current_syscall = !0` (not inside a syscall). cargo main is separately
  parked in `futex` (sc=98). Six independent processes blocked at the *same* user
  instruction = a shared wait/park primitive (a futex/`sched_yield`/park call site
  in the rustc/musl binary) whose **wake is never delivered**.
- rustc first demand-pages its ~**221 MB** `librustc_driver` (`filesz=0xd38c000`)
  via ~1700 scattered instruction faults (normal, makes progress) and *then* parks.
  So the hang is downstream of paging, in the wait/wake handoff.

**This is (most likely) a futex / thread-park missed-wakeup**, exactly the
"sc=98 classic hang" class. The earlier `fault_mutex` poison fix (§ below) is a
real latent-deadlock fix but is **NOT** this bug (its `[FAULT-RECLAIM]` never fired
here).

**Fix committed regardless — `fault_mutex` poison recovery.** The per-page
demand-paging serialization (`Process.fault_mutex`) was a `BTreeSet<page_va>` with
no owner tracking: a thread that died mid-fault (RAII release guard never runs on
kernel thread teardown) left the slot poisoned, so siblings faulting that page spun
in `yield_now` forever. Now `BTreeMap<page_va, holder_tid>` with
`fault_slot_acquire`/`fault_slot_release` (crates/akuma-exec/src/process/children.rs):
a spinner reclaims the slot if the holder is `is_thread_terminated`, with a bounded
fallback for a wedged/recycled holder. Wired into all 3 fault sites in
`exceptions.rs` (CoW/DA/IA) + a `[FAULT-RECLAIM]` log. Regression self-test:
`test_fault_mutex_insert_remove` in `src/process_tests.rs`.

**Debugging aids added (keep until the futex bug is fixed):**
- `ps` builtin prints each process's saved kernel resume point (`x30`/`elr`).
- Heartbeat dumps `[THR-DUMP]` (per-thread state + pid + `current_syscall` +
  `x30`/`elr`/`sp`) every ~8 heartbeats when `waiting >= 2` —
  `threading::dump_thread_resume_points()`. Survives the SSH wedge (serial only).

**Next:** build a **userspace futex test suite** (multi-thread WAIT/WAKE,
WAIT_BITSET, requeue, wake-before-wait races, PI, timeouts) run as boot self-tests +
a standalone userspace binary, to pin the missed-wakeup. Suspect areas: the
futex hash-bucket match between waker and waiter, wake-before-wait (sticky wake),
and `schedule_blocking` wake delivery when `current_syscall` has been cleared.
Resolve user PC `0x30060cc4` against the in-VM rustc/musl binary to confirm it's the
futex wait call site.

#### 7g.1 — Refinement after futex tracing + binary analysis (June 19 2026, STILL OPEN)

Ran the reliable repro with `FUTEX_DBG_ENABLED=true` + a per-exit `[cct-exit]` log
+ a serial `[THR-DUMP]`. New facts:

- **cargo main is NOT stuck on a lost wake — it POLLS.** Its futex wait on
  `0x1e0e89fb0` is a *timed* wait that returns `ETIMEDOUT` every ~520 ms and
  re-waits forever. It's waiting for a child build unit that never reports done.
- **`clear_child_tid` / `pthread_join` wakes WORK.** Every `[cct-exit]` showed
  `mapped=true` and fired its `futex_wake`; whole rustc thread-groups (e.g. tgid
  110, pids 158–176 + leader 115) exited cleanly. So the earlier "gated futex_wake"
  hypothesis was **wrong** for the common case. (The fix — always `futex_wake`,
  gate only the user-memory write — was kept anyway: it's correct in principle and
  cheap. `src/syscall/proc.rs`, `crates/akuma-exec/src/process/mod.rs`.)
- **The truly-stuck threads park at a libc/ld-musl PC, not in futex.** `ps` +
  `[THR-DUMP]`: six processes — cargo's build-unit children (108, 180) and the
  **orphaned** rustc probes (183/185/187/189, parents 182/184/186/188 already
  exited) — are all in **WAITING** at the identical user PC `elr=0x30060cc4`,
  `current_syscall = !0`, and `last_syscall = 0` (per `ps` `-`). i.e. blocked in a
  syscall **without** `current_syscall` set, having (apparently) never completed
  one.
- **`/usr/local/bin/rustc` is a 72 KB dynamically-linked PIE** (interp
  `/lib/ld-musl-aarch64.so.1`, needs `librustc_driver-*.so` (~221 MB, the
  `filesz=0xd38c000` segment at `0x30100000`) + `libc.so`). `0x30060cc4` sits
  *below* the 221 MB lib → it's in **libc/ld-musl** — a musl syscall-return site
  shared across these statically-laid-out objects, which is why every stuck
  process shows the *same* PC.
- **Ruled out:** syscall `78` = `readlinkat` returning `EINVAL` in a loop at libc
  PC `0x30069828` is **expected** (the path isn't a symlink) — a startup red
  herring, not the hang.
- **Also confirmed:** the `fault_mutex` poison fix never engaged here
  (`[FAULT-RECLAIM]` never printed) — it's a real but *separate* latent bug.

**Working theory (unconfirmed):** the stuck processes are **forked/cloned children
of multithreaded rustc** parked at a libc syscall site and never woken — either a
classic fork-in-a-multithreaded-program lock inheritance (child waits on a libc
lock/futex whose owning thread doesn't exist in the child) or a `CLONE_VFORK`/
`posix_spawn` handshake the kernel mishandles. The repeated orphaning (parents
exited, children alive at the same PC) and Akuma's lack of orphan→init reparenting
(§7e) are consistent with this.

**Decisive next experiments:**
1. Resolve `0x30060cc4` to a musl symbol: extract `/lib/ld-musl-aarch64.so.1` from
   the disk (Docker loop-mount), find its load base from the boot mmap trace, and
   `llvm-objdump` the function — names the exact wait primitive (lock vs futex vs
   nanosleep vs clone-return).
2. lldb single-step one stuck child (`INSTANCE=1 GDB=1`) to read x8 (syscall nr)
   at the `svc` just before `0x30060cc4`, and whether it ever runs.
3. Check `CLONE_VFORK`/`posix_spawn` handling in `sys_clone` and the fork child's
   READY transition for a race.

**Test asset added:** `userspace/selfhost_repro/futextest.rs` — a self-contained
multi-thread/futex stress binary (spawn+join, fan-out, mutex+condvar, barrier,
wake-before-wait, park/unpark), built in-VM via `rustc -O futextest.rs`. NOTE:
building it in-VM currently **hangs rustc itself** (same bug), so cross-build the
binary on a Linux/aarch64 musl host and stage it, rather than building in-VM.

**Debug aids (gated OFF by default; flip on to resume):** `FUTEX_DBG_ENABLED` and
`DEADLOCK_THREAD_DUMP_ENABLED` in `src/config.rs`; the `ps` builtin's saved-kernel-PC
column and `threading::dump_thread_resume_points()` / `get_saved_kernel_resume()`.

---

## 8. Other known issues

- **Intermittent kernel crash during exec at high RAM.** Once, during an
  interactive 16 GB session with concurrent herd/httpd activity, exec'ing rustc
  wild-jumped into the `BLOCK_DEVICE` static (`.bss`) — `EC=0x0` undefined-instr,
  `ELR` in data. **Not reproducible** in isolation (10/10 repeat execs clean at
  16 GB; `rustc --version` clean at every size). Filed as a rare
  concurrency/corruption bug, not a RAM ceiling. (`11_selfhost.log`.)

---

## 9. References

- Playbook: `acceptance/10_selfhost_compile_akuma.md`
- Single-file bring-up + the fork/socketpair fixes + **fork-perf §5b**: `docs/RUST_TOOLCHAIN.md`
- VA split / identity-map extent + boot self-test VAs: `docs/MEMORY_LAYOUT.md`
- fork CoW cost + fix plan: `docs/COW_OPTIMIZATIONS.md`
- Boot RAM sweep: `scripts/boot_ram_sweep.sh`; rustc RAM sweep: `scripts/rustc_ram_sweep.sh`
