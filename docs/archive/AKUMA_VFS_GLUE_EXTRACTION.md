# `src/vfs/` → `akuma-vfs-glue`

**2026-09-01.** The kernel-side VFS layer left the binary: the global mount
table, the per-box namespace map, the ext2-over-virtio adapter and the whole
synthetic `/proc`. 2,935 lines, `#![forbid(unsafe_code)]`, zero `unsafe` sites
before or after.

| | before | after |
|---|---:|---:|
| crates that forbid `unsafe` | 23 of 39 | **26 of 42** |
| enforced-safe code under `crates/` | 25,658 / 49,706 (51.6%) | **28,135 / 52,202 (53.9%)** |
| host test suites | 78 | **84** |
| `src/` production `unsafe` sites | 3 | 3 |
| `extreme-size` | 728,568 B | 732,664 B (**+4,096**) |

The `unsafe` count did not move and was never the point. This extraction is
**structural**: it exists to break a dependency cycle that kept two directories
— 19,600 lines between them — pinned inside the binary crate.

## Why `akuma-vfs` was not enough

`akuma-vfs` already existed and already held the *vocabulary*: the `Filesystem`
trait, `DirEntry`, `Metadata`, `FsError`, `MountTable`, path canonicalisation.
What stayed in `src/vfs/` was the kernel's single **instance** of all that, and
instances are what pull in the rest of the world:

- `MOUNT_TABLE` — one `Spinlock<Option<MountTable>>` for the machine.
- `BOX_NAMESPACES` / `SPAWN_NS_OVERRIDE` — which mounts a container can see, and
  the per-thread override `with_fs` consults during spawn.
- `KernelBlockDevice` — `akuma_ext2::BlockDevice` over a registered virtio-blk
  index.
- `proc.rs`, 1,544 lines of synthetic files.

Measured: of the 37 `crate::vfs::` *functions* `src/syscall/` calls, **30 are
defined in `src/vfs/*.rs`** and 0 are re-exports of `akuma-vfs`. So "just use
`akuma-vfs`" was never available; the layer had to move as a layer.

## The cycle

```
src/syscall/  ──110 refs, 50 symbols──▶  src/vfs/
src/vfs/      ── 10 refs,  3 symbols──▶  src/syscall/
```

Cargo cannot express that, so neither directory could leave. The back-edge was
small and one-sided — all ten references in `proc.rs`, all three symbols being
`/proc` *reading* registries that happen to live under `src/syscall/`:

| symbol | refs |
|---|---:|
| `crate::syscall::log::get_formatted` | 8 |
| `crate::syscall::log::list_pids_with_logs` | 1 |
| `crate::syscall::msgqueue::list_msg_queues` | 1 |

Neither registry is syscall *dispatch*. They are state syscalls own and `/proc`
publishes. So they went first, into two crates of their own
([`SRC_SYSCALL_EXTRACTION.md`](SRC_SYSCALL_EXTRACTION.md) §7.1–7.2), and the
back-edge went to **0**. Only then could this layer move.

The alternative — inverting `/proc` onto a registration hook so providers
register themselves — was rejected: the registries are not polymorphic and have
exactly one implementation each, so the indirection would buy nothing and cost a
vtable on a `/proc` read.

## What it still needs from the binary: four function pointers

```rust
pub struct VfsGlueHooks {
    pub audio_is_available: fn() -> bool,
    pub fs_exists: fn(&str) -> bool,
    pub probed_core_count: fn() -> usize,
    pub utc_time_us: fn() -> Option<u64>,
}
```

**Four, not twenty, and the difference was found by resolving symbols rather than
counting references.** The survey priced `crate::vfs`'s dependencies at 20
distinct symbols across 10 clusters. Four of those clusters turned out to be
re-exports of crates that already existed:

| looked binary-local | actually |
|---|---|
| `crate::block` — `device_name`, `read_bytes_at`, `write_bytes_at` | `akuma_virtio::block` (a `pub(crate) use` in `main.rs`) |
| `crate::pmm::stats` | `akuma_pmm::stats` |
| `crate::file_page_cache::{invalidate_inode, len}` | `akuma_fpcache::` |
| `crate::timer::uptime_us` | `akuma_primitives::clock::uptime_us` |

What survived is genuinely the binary's: an inline `pub(crate) mod audio` in
`main.rs`, `src/fs.rs`'s path probe, the DTB-probed SMP core count, and the wall
clock — which needs the binary's boot uptime to turn monotonic microseconds into
UTC, so `akuma_timer::utc_time_us(boot_uptime_us)` cannot be called directly.

Unregistered is quiet, not fatal: every getter has a defined answer with no hooks
installed, so host tests and early boot read a coherent (if empty) `/proc`
instead of panicking. Same contract as `akuma_primitives::console::print_str`.

**This is the reusable lesson.** Reference counts mislead in both directions:
`crate::irq` at 94 references is a single function that already lives in
`akuma-primitives`, while `crate::vfs` at 110 references is what made the move
impossible. Price the work in *distinct symbols, resolved to their real home*,
and check whether any are types — a hooks struct cannot carry a type.

## Three ways it went wrong

### 1. The missing `build.rs`, which would have been silent

`proc.rs` has:

```rust
#[cfg(kernel_smp_shared)]
fn active_core_count() -> usize { ... }   // and a `1` fallback for the other arm
```

and that number sizes the per-core CPU-time accounting `/proc` reports. **Cfgs do
not travel with code.** A crate carved out of `src/` inherits every `kernel_*`
cfg its code reads and receives none of them, so without a `build.rs` forwarding
`smp-shared`, this compiles the fallback *even under real SMP* — `/proc` divides
by one core on a four-core machine, with no build error and no runtime error.
`akuma-exec` shipped exactly this bug for its `kernel_profile_extreme` gates and
says so in its own `build.rs` header.

Anything moved out of `src/` needs its cfgs audited, not just its imports.

### 2. Feature gates read as missing functions

Eight `#[cfg(feature = "sc-containers")]`, one `sc-reboot`, one `sc-sysv-ipc`.
The symptom is `cannot find function ... in module crate::vfs` pointing at a
`pub fn` that plainly exists three lines below in the source you are looking at.
Declare the features on the new crate and forward them from the bin.

### 3. Config-by-handover stops const-folding

`src/config.rs` forces `PROC_SYSCALL_LOG_ENABLED` and `PROC_SYSVIPC_ENABLED` to
`false` on `kernel_profile_extreme`. While they were `const`, the `/proc`
renderers behind them were **deleted from the image**. Handing them over as
runtime config at `set_config` time turned both into loads and retained both
renderers.

The fix is a deliberate duplication:

```rust
#[cfg(kernel_profile_extreme)]
const fn cfg_proc_syscall_log_enabled() -> bool { false }

#[cfg(not(kernel_profile_extreme))]
fn cfg_proc_syscall_log_enabled() -> bool { CFG.get().is_some_and(|c| c.proc_syscall_log_enabled) }
```

`src/config.rs` remains authoritative for the value on every other profile; the
crate hardcodes only the profile's forced-off case, which `config.rs` itself
documents. The alternative is paying image space for a renderer that profile can
never reach.

**Measure the floor with `cargo clean -p akuma` on both arms.** An incremental
rebuild reported byte-identical sizes for HEAD and the extraction and would have
told me there was no regression at all. The real delta is exactly one page —
alignment, not 4 KB of code — 0.13% of the headroom under a 3,070 KB image with a
4 MB floor. `akuma-fpcache`'s extraction documented its own +304 B for the same
reason; **every const that becomes runtime config stops folding whatever it
gates**, so check the floor on any extraction that moves a `bool` out of
`src/config.rs`.

## `src/vfs.rs`, the shim that stayed

Two jobs, both the binary's:

1. **`src/config.rs` stays the single source of truth.** `register()` reads the
   six consts and hands them over as a `VfsGlueConfig`. Do not put a second copy
   in the crate.
2. Installs the four hooks.

Everything else is `pub use akuma_vfs_glue::*;`, so no call site in
`src/syscall/` changed.

## Verification

- Builds: `--release`, `extreme-size`, **`devbox-smoltcp` and `devbox` (rump)**.
  All four, because a mid-session break hit only the `no-tests` profiles — see
  below.
- Clippy clean at `--release`; 84 host suites green; `cloc_akuma.py --self-test`
  passes.
- QEMU `SMP=4 MEMORY=2048M`: **265 pass / 0 fail**, `[FS] Procfs mounted at
  /proc`, `smp_shared_cores_online PASSED (3/3 secondaries)`.
- Firecracker under Lima nested virt: `PASSED=305 FAILED=1`, and the failure is
  `thread_slot_reclaim_on_spawn` — **A/B'd against a stashed-clean tree, both
  arms byte-identical**, so pre-existing. Recorded in
  `overlays/devbox-firecracker/README.md`.

### The break worth remembering

The shim first re-exported the boot-suite-only helpers (`msgqueue_add_recv_poller`
and seven siblings) ungated. `-D unused-imports` then failed **`devbox-smoltcp`
and `devbox`** — which build `no-tests` and call none of them — while `--release`
and `extreme-size` stayed green. A break that hits two of four targets and spares
the two you habitually run is the worst shape to find late. The helpers are
`#[cfg(kernel_tests)]` now, and the rule is: **build all four targets after
touching a re-export.**

## Background

- [`SRC_SYSCALL_EXTRACTION.md`](SRC_SYSCALL_EXTRACTION.md) — the survey this
  came out of, the two prerequisite crates (§7.1–7.2), and what still blocks
  `src/syscall/` itself.
- [`AKUMA_EXCEPTIONS_EXTRACTION.md`](AKUMA_EXCEPTIONS_EXTRACTION.md) — the
  `ExceptionHooks` model these four function pointers follow.
- [`AKUMA_FPCACHE_EXTRACTION.md`](AKUMA_FPCACHE_EXTRACTION.md) — the
  config-handover pattern, and the first time its size cost was measured.
- [`../reference/crate-safety.md`](../reference/crate-safety.md) — the census.
