# Self-host: compile the Akuma kernel inside Akuma

Runbook for compiling the Akuma kernel *inside* Akuma (the self-hosting
milestone). **This is NOT the devbox** — self-hosting uses the default-smoltcp
build + a nightly toolchain on a separate large disk.

> The devbox (`build-devbox.md`) is the rump-only dogfooding image with apk
> stable toolchain. Self-hosting has actually compiled the kernel (147/147
> units) and the self-built kernel boots.

## Status (2026-08-05) — two builds, only one of them green

Keep these apart; they fail for different reasons and the same word
("self-host") is used for both.

| | `cargo build --release -j1` (§1-§4 below) | in-VM `-j4` `release-smp-shared` + `devbox-smoltcp` |
|---|---|---|
| kernel *source* compiles on the host | yes — clean, clippy clean, 483 host tests pass | same source |
| in-VM build | reaches the ELF | **blocked** |
| blocker | — | freshly-cloned rustc threads SIGSEGV at a fixed PC ⇒ [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md) |

The `-j4` variant is the one under active investigation:

```sh
cargo build -p akuma --profile release-smp-shared \
    --features devbox-smoltcp,no-tests -j4
```

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

## Verify

- `/usr/local/bin/rustc --version` contains `"nightly"`.
- Success = produced ELF at `target/aarch64-unknown-none/release/akuma`.
- Record the highest milestone reached: manifest parse → build.rs/proc-macro →
  deps → akuma crate → rust-lld link.

## Key constraints

- **Nightly is mandatory** (panic-immediate-abort cargo-feature). Host must be
  `aarch64-unknown-linux-musl`.
- **`cargo build --release`** is the realistic target — no `build-std` needed.
- **`fs-cache` feature** is the key perf lever: metadata ~19× faster. Consider
  adding it (opt-in, not in the devbox set). See
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
| `cargo --version` crash (EC=0x0) | nightly cargo traps HVF CNTP | Use apk cargo + nightly rustc; or `HVF=0` |
| `error: could not compile … (signal: 11)`, or all rustc processes frozen with `pthread_join` waiters | freshly-cloned thread SIGSEGVs at a fixed PC | **OPEN** — [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md) |

## Background

- `archive/AKUMA_SELF_HOSTING.md` — the full progression §1–§7j (SELF-HOSTED).
- [`acceptance/10_selfhost_compile_akuma.md`](../../acceptance/10_selfhost_compile_akuma.md).
- `scripts/loop_selfhost_kernelbuild.py` — retry loop.
