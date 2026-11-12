# Add a `sc-*` syscall feature

How to add a new kernel syscall family behind a `sc-*` feature gate and keep
all build configs in sync.

> The rump devbox build uses `--no-default-features` and re-adds
**specific** `sc-*` features. If you add a new family and forget one build
script, that build silently drops it.

## Steps

1. **Add the feature in `Cargo.toml`** (under the per-family gates section,
   `Cargo.toml:208-216`):
   ```toml
   sc-<name> = []
   ```
   Tier 1 (pure dead weight when off) vs Tier 2 (needs an ExecRuntime stub —
   see step 3). Default-on by adding it to the `linux-extra-syscalls` group (in `default`) —
   on **both** kernels: root `Cargo.toml` (which also forwards the amd64 half's feature) and
   `amd64/Cargo.toml`, so the group keeps one meaning.

2. **Add the submodule** `src/syscall/<name>.rs`, gated:
   ```rust
   #[cfg(feature = "sc-<name>")]
   mod name;
   ```
   Add the dispatch arms in `handle_syscall` (`src/syscall/mod.rs:656`).

3. **If Tier 2** (other code references it when off): add a no-op
   `ExecRuntime` callback stub in `src/main.rs` (e.g. `noop_u32` at :412, wired
   at :451 like `eventfd_close: noop_u32`). The stub runs when the feature is
   off so callers don't link-fail.

4. **The build scripts need nothing.** `scripts/build_devbox.sh` and
   `overlays/devbox/run.sh` select `linux-extra-syscalls`, so a family in the
   group reaches the devbox with no script change.

5. **Verify in every profile:**
   ```bash
   cargo build --release                                 # default (all on)
   scripts/build_devbox.sh                               # devbox
   scripts/build_size.sh                                 # size
   cargo build --no-default-features --features sc-<name>  # minimal
   ```

## The Tier 1 / Tier 2 distinction

| Tier | When off | Examples |
|---|---|---|
| 1 | Pure dead weight — nothing else references it | `sc-aio, sc-sysv-ipc, sc-framebuffer, sc-containers, sc-timerfd` |
| 2 | Other code references it → needs a no-op `ExecRuntime` stub | `sc-eventfd, sc-pidfd, sc-epoll` |

If unsure, make it Tier 2 (add the stub) — a spurious stub is harmless; a
missing one is a link error in the minimal build.

## Why `--no-default-features` matters

The devbox compiles smoltcp **out** (`--no-default-features`), so anything
smoltcp-coupled must be re-gated. A new syscall family that touches network
fds must respect the rump interception path
(`rump_proxy::intercept_box_syscall`) — see
[`../reference/subsystems/syscalls.md`](../reference/subsystems/syscalls.md)
and [`../reference/subsystems/rump-stack.md`](../reference/subsystems/rump-stack.md).

## Background

- `archive/SPLIT_SYSCALLS.md` — the split into `src/syscall/`.
- [`../reference/subsystems/syscalls.md`](../reference/subsystems/syscalls.md).
- [`../reference/subsystems/config-flags.md`](../reference/subsystems/config-flags.md).
