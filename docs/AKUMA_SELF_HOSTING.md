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

So **codegen works end-to-end**; the wall is the **link step**, and it is a
kernel-throughput problem, not a toolchain problem.

---

## 5. The link-step blocker (the project)

A full `rustc hello.rs -o hello` reproducibly fails to produce a binary. Traced
via the kernel serial log, the sequence is:

1. rustc codegens (works), then **forks to spawn the linker**:
   `rustc` → `cc` (gcc) → `collect2` → `execve /usr/aarch64-alpine-linux-musl/bin/ld`
   (confirmed in the log: `[FORK-DBG] replace_image …`, `execve(".../collect2")`,
   `execve(".../bin/ld")`).
2. That first fork copies/CoW-shares rustc's **~75k-page address space** (libLLVM
   mapped). On single-core QEMU this CoW takes ~30 s and **monopolizes the core**
   (`docs/RUST_TOOLCHAIN.md` §5b — the known-open fork-perf item, and
   `docs/COW_OPTIMIZATIONS.md`).
3. While the fork stalls the core, the **in-kernel SSH server thread is starved**;
   the connection drops (`Connection closed by remote host`) ~29–59 s in.
4. The dropped session **SIGHUPs the build** → the linker is killed → 0-byte output.
5. The build **does not survive the disconnect** — polled `/tmp/hr` for 744 s after
   a forced disconnect: it never appeared. So "fire and poll" does not work either.

### Why I can't just background it

The in-kernel SSH shell is a **mini-shell**, not a POSIX shell. Established by
experiment:

| Want | Reality |
|---|---|
| `cmd &` (background) | ❌ `&` is passed as an argument (`multiple input filenames … '&'`) |
| `busybox sh -c '…'` | ❌ forking rustc from busybox `sh` **segfaults rustc** (`EXIT=139`) |
| `busybox sh script.sh` | ❌ same fork-segfault path |
| `#!/bin/sh` linker/wrapper script | ❌ Akuma's `execve` doesn't honor shebang → `exit 127` |
| `2>file` (stderr redirect) | ❌ unsupported; `2` leaks as an arg. Only `>file` (stdout) works |
| `busybox env VAR=… cmd 2>f` | ⚠️ env works, but the multi-token line breaks `>` parsing |
| `busybox env VAR=… cmd` | ✅ sets env + `execve`s (no fork) — this is how to give rustc a `PATH` |

Practical upshot for driving it now: use `… 2>/dev/null` is out; capture rustc
errors only on **fast** commands (slow ones lose buffered output on the abrupt
session close). To give rustc a `PATH` for the linker, prefix with
`/bin/busybox env PATH=/usr/local/bin:/usr/bin:/bin`.

### The fix is one of (this is the project)

1. **Fork throughput (root cause, `docs/RUST_TOOLCHAIN.md` §5b).** A vfork-style
   fast path (share + suspend parent, drop the copy on `exec`) or coarse CoW
   (refcount L1/L2 page-table subtrees instead of per-page) would make the
   link-fork cheap, so it neither takes 30 s nor stalls the SSH server.
2. **Detached build execution.** A way to launch a build that survives session
   disconnect — e.g. a `herd`-supervised one-shot job, or a small persistent
   build-runner — then poll for completion.
3. **SSH-server resilience under a stalled core.** Keep the listener/keepalive
   alive (or the TCP connection from resetting) across a multi-second
   single-core monopoly so the session isn't dropped mid-link.

Until one lands, an in-VM `cargo build` of the kernel (hundreds of crates, each
forking a linker) cannot complete.

---

## 6. Benchmarks (hello.rs)

`hello.rs` = `fn main(){ println!("Hello from Akuma!"); }`. Compile time, then run.

| Environment | Toolchain | debug | `-O` | notes |
|---|---|---|---|---|
| **Mac native** (Apple Silicon, darwin) | rustc 1.95.0 | 0.67 s | 0.13 s | runs `Hello from Akuma!` |
| **Docker Alpine** arm64 (native musl) | rustc 1.91.1 (apk) | 0.05 s | 0.04 s | runs `Hello from Akuma!` |
| **Akuma** (16 GB) — codegen only (`--emit=obj`) | rustc 1.98.0-nightly | ~24 s | — | valid `.o`; **no link** |
| **Akuma** (16 GB) — full compile (+link) | rustc 1.98.0-nightly | — | — | ❌ blocked (§5) |

Akuma codegen alone is **~250–500× slower** than native, dominated by
demand-paging the 305 MB `librustc_driver.so` + libLLVM off virtio-blk. The
full compile is currently unmeasurable because the link can't complete. Re-run
the baselines: `scripts/rustc_ram_sweep.sh` (in-VM probe) and the Mac/Docker
one-liners in this section's history.

> **TODO (project):** once §5 is fixed — (a) compile+run hello.rs across
> `MEMORY` 4→16 GB with timings, (b) find the RAM floor counting down from
> 1.5 GB, (c) re-time against Mac/Docker. The harness for the RAM sweep already
> exists (`scripts/rustc_ram_sweep.sh`); extend it to compile+run+time once the
> link works.

---

## 7. Other known issues

- **Intermittent kernel crash during exec at high RAM.** Once, during an
  interactive 16 GB session with concurrent herd/httpd activity, exec'ing rustc
  wild-jumped into the `BLOCK_DEVICE` static (`.bss`) — `EC=0x0` undefined-instr,
  `ELR` in data. **Not reproducible** in isolation (10/10 repeat execs clean at
  16 GB; `rustc --version` clean at every size). Filed as a rare
  concurrency/corruption bug, not a RAM ceiling. (`11_selfhost.log`.)

---

## 8. References

- Playbook: `acceptance/10_selfhost_compile_akuma.md`
- Single-file bring-up + the fork/socketpair fixes + **fork-perf §5b**: `docs/RUST_TOOLCHAIN.md`
- VA split / identity-map extent + boot self-test VAs: `docs/MEMORY_LAYOUT.md`
- fork CoW cost + fix plan: `docs/COW_OPTIMIZATIONS.md`
- Boot RAM sweep: `scripts/boot_ram_sweep.sh`; rustc RAM sweep: `scripts/rustc_ram_sweep.sh`
