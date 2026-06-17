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
(`embedded-tls`). cargo-over-TLS *inside Akuma* is unproven and may be the first
wall, so the recommended setup **vendors** deps on the host (`cargo vendor`,
~44 MB) and points `.cargo/config.toml` at `vendored-sources` so the in-VM build
is fully offline. (If you clone the repo fresh inside the disk instead, the build
will try the network — see the playbook's "offline fallback" note.)

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
