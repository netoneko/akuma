# `src/syscall/` → `akuma-syscalls-glue`

**2026-09-01.** The syscall dispatcher and its 23 per-family modules — ~17,000
lines, the largest thing left in the binary — became a crate. It carries
`#![forbid(unsafe_code)]`, which it already did as a module attribute, and needs
**seven function pointers** from the binary.

| | before | after |
|---|---:|---:|
| crates that forbid `unsafe` | 27 of 43 | **28 of 44** |
| enforced-safe code under `crates/` | 28,445 / 52,664 (54.0%) | **39,616 / 63,836 (62.1%)** |
| `src/` production `unsafe` | 3 | 3 |
| `extreme-size` | 732,664 B | **728,576 B** (−4,088) |
| host test suites | 86 | 88 |

The image got *smaller*. `akuma-vfs-glue` had cost exactly one page when its two
`/proc` flags stopped const-folding; `akuma-config` gave that back, and this move
returned the page.

## It was never about the syscall code

Four things had to move first, none of them syscall code. That is the whole
story of this extraction:

| blocker | refs | resolution |
|---|---:|---|
| `src/vfs/` ↔ `src/syscall/` **cycle** | 110 out, 10 back | [`AKUMA_VFS_GLUE_EXTRACTION.md`](AKUMA_VFS_GLUE_EXTRACTION.md), after `-log`/`-ipc` cut the back-edge |
| `crate::config` | 217 | [`AKUMA_CONFIG_EXTRACTION.md`](AKUMA_CONFIG_EXTRACTION.md) — a crate of `const`s, **not** a 26-field handover struct |
| `crate::process_tests::make_test_process` | 4 | moved to `akuma-exec::process` (43 lines, zero `crate::` refs) |
| `crate::fs` (13 syms), `crate::pmm` (11) | 60 | `akuma-vfs-glue::fs`, `akuma-exec::pmm` |

Then 539 references were repointed to crate paths — `safe_print`/`tprint` to
`akuma-primitives`, `irq::with_irqs_disabled` likewise, `crate::vfs` to
`akuma_vfs_glue`, `crate::block`/`crate::rng` to `akuma_virtio`, the four
`*_bkl_drop_enabled` to `akuma_bkl::policy` — and the outbound surface went from
**19 clusters / ~900 refs to 5 clusters / 48**.

**Resolve each symbol before writing a hook.** `crate::audio` looked like nine
hooks and was a re-export of `akuma_virtio::audio`. `crate::irq` looked like the
third-biggest cluster at 94 refs and was one function that already lived in
`akuma-primitives`. `crate::nic_profile` and `crate::bkl_profile` were doc
comments.

## The seven hooks

```rust
pub struct SyscallHooks {
    pub box_is_rump:           fn(u64) -> bool,
    pub mark_box_rump:         fn(u64),
    pub attach_server:         fn(u64, akuma_exec::process::Pid),
    pub intercept_box_syscall: fn(u64, &[u64; 6]) -> Option<u64>,
    pub rump_socket_readable:  fn(i32) -> bool,
    pub utc_time_us:           fn() -> Option<u64>,
    pub probed_core_count:     fn() -> usize,
}
```

Five are the rump sysproxy, whose state lives in `src/rump_proxy.rs`. The wall
clock needs the binary's boot uptime to turn monotonic microseconds into UTC. The
core count is the DTB probe. Unregistered is quiet, not fatal.

`src/syscall.rs` is 62 lines: `pub use akuma_syscalls_glue::*;`, `register()`,
and a `#[cfg(feature = "rump")]` pair of inert stubs so `register` stays one
expression on profiles without rump.

## Five things that went wrong, and what each teaches

### 1. Grep for `cfg!(...)`, not just `#[cfg(...)]`

The crate's `build.rs` forwards every `kernel_*` cfg its source reads. A first
pass used `grep -rohE 'cfg\(kernel_[a-z_]+'` and found **three**, missing four
`no-bkl` gates written as `cfg!(all(kernel_smp_shared, kernel_no_bkl_vfs))` — the
macro form, not the attribute. The build failed with **199** `unexpected cfg
condition name` errors, which at least was loud. Use
`grep -rohE 'kernel_[a-z_]+'` and filter by hand.

A missing one is otherwise silent: `cfg!(all(kernel_smp_shared,
kernel_no_bkl_vfs))` just reports the carve-out as absent.

### 2. Features must be declared *and* forwarded

Eleven Cargo features gate code in this crate. Declaring them on the crate is
half the job; the bin must forward each one. The symptom of forgetting is
`could not find 'pidfd' in 'syscall'` against a `pub mod pidfd;` you can see in
the source — a feature-gated item, not a visibility problem.

### 3. `rustc-env` does not cross crates

`version.rs` and `uname` read `env!("AKUMA_GIT_SHA")` / `env!("AKUMA_BUILD_PROFILE")`,
set by the **binary's** `build.rs`. They stopped resolving the moment this became
a crate.

The first fix threaded the packed version through a hook — the binary computing a
value purely so a crate could read it back. **The derivation moved down here
instead**, and the binary's emission was deleted: nothing else in the tree read
either variable, so there is still exactly one `git rev-parse` in the build.

One trap in the move: `rerun-if-changed` paths are relative to the *crate's*
manifest dir, so `.git/HEAD` would name
`crates/akuma-syscalls-glue/.git/HEAD` — which does not exist, and the staleness
would be silent. It is `../../.git/HEAD`.

### 4. `dead_code = "deny"` is a workspace lint, and a lib is stricter than a bin

Three `net.rs` functions had only `#[cfg(feature = "smoltcp")]` callers while
being ungated themselves. That survived in the binary and does not in a crate;
they now carry the same gate as their callers, which is what they should always
have had.

The bigger version of this: **clippy lints fire on public library items that
never fired on a binary's.** `too_long_first_doc_paragraph` (28 sites),
`must_use_candidate` (40), `redundant_pub_crate` (52), `pub_underscore_fields`.
Three others — `cast_ptr_alignment`, `cast_possible_wrap`, `inline_always` —
were **already allowed by `src/main.rs` on this exact code**, so carrying those
allows over suppresses nothing new. The doc paragraphs were *fixed*, not allowed,
for the same reason `akuma-config`'s were: a blanket allow is how
`#![allow(dead_code)]` hid 14 dead consts for three weeks.

### 5. A bare `cargo build -p` is not a configuration anybody ships

With no features the socket layer is half-unreachable and `-D dead-code` fires on
code every real build contains. The crate has `default = ["smoltcp"]` so the
pre-commit hook's per-crate clippy compiles something coherent — and the bin
therefore uses `default-features = false`, exactly as it does for `akuma-net`, or
an `extreme-size` build would silently gain smoltcp.

## Verification

- Builds: `--release`, `extreme-size`, `devbox-smoltcp`, `devbox` (rump),
  **firecracker**.
- `cargo clippy -D warnings`: **all 44 crates on the host target**, plus the bin
  at `release` and `extreme-size` — the full pre-commit sequence.
- 88 host suites, 0 failures; `cloc_akuma.py --self-test` passes.
- QEMU `SMP=4 MEMORY=2048M`: **265 pass / 0 fail**, `Procfs mounted at /proc`,
  `smp_shared_cores_online PASSED (3/3 secondaries)`, and
  **`akuma_get_version PASSED`** — the build identity survived the move to this
  crate's own `build.rs`.

## What is left in `src/`

28 top-level files. The four shims are 121 lines between them: `syscall.rs` (62),
`vfs.rs` (40), `config.rs` (11), `fs.rs` (8).

`src/` production `unsafe` is unchanged at **3**, of which one is a real
operation (`akuma_fdt::locate`). The remaining hooks are the thing to look at
next: seven here, four in `akuma-vfs-glue`, and the `ExceptionHooks` set — a
consolidation pass across those would shrink the seam further.

## Background

- [`SRC_SYSCALL_EXTRACTION.md`](SRC_SYSCALL_EXTRACTION.md) — the survey that
  planned this, and the merge deliberation for `akuma-syscalls` (still: no).
- [`AKUMA_CONFIG_EXTRACTION.md`](AKUMA_CONFIG_EXTRACTION.md),
  [`AKUMA_VFS_GLUE_EXTRACTION.md`](AKUMA_VFS_GLUE_EXTRACTION.md) — the two
  prerequisites.
- [`SYSCALL_UNSAFE_CLEANUP.md`](SYSCALL_UNSAFE_CLEANUP.md) — how this subtree got
  to zero `unsafe` in the first place.
- [`../reference/crate-safety.md`](../reference/crate-safety.md) — the census.
