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

**Next session:** with the build-script crate gone from `hello`'s tree, resume the
in-VM `cargo build -p hello` (should now reach codegen + link without the build.rs
hang), run the produced binary, then repeat for `httpd`. Commit the local changes
(`.gitmodules`, `userspace/Cargo.toml` members, `libakuma` `net-async` gate + dep
made optional, `sshd` `features = ["net-async"]`).

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
