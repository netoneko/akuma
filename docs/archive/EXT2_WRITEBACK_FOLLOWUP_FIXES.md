# ext2 write-back follow-up: defects found and fixed (2026-08-26)

Status: **history.** Written after picking up the in-flight ext2 write-back work
(`EXT2_WRITEBACK_DESIGN.md`, commits `f0edc4e0` / `084c9e43`) to finish it:
clean up lint debt, verify the claims, and land D-4. Nine defects surfaced, most
of them outside ext2 itself. This records what was wrong, how it was found, and
what it means — the fixes are in the tree.

Companion docs: `EXT2_WRITEBACK_DESIGN.md` (the decisions and the measurements),
`crates/akuma-ext2/README.md` § Performance (the numbers),
`docs/runbooks/selfhost-kernel-build.md` (updated by items 4–6).

---

## 1. `extreme-size` did not build

`scripts/build_extreme_size.sh` failed with:

```
error: function `sync_all_filesystems` is never used  --> src/vfs/mod.rs:167
       requested on the command line with `-D dead-code`
```

D-7 added `sync_all_filesystems` and called it from `sys_reboot`. But
`extreme-size` builds `--no-default-features`, which drops `sc-reboot` — so on
that profile the function had no caller at all. Fixed by gating it
`#[cfg(feature = "sc-reboot")]` with a comment saying why.

**Lesson:** a new kernel function with exactly one caller must be checked against
the profile that drops that caller. `cargo build --release` passing says nothing
about `extreme-size`.

## 2. Three `#[allow(...)]` that were papering over fixable code

The write-back work added `#[allow(unused_variables)]` to `read_range` and
`write_range`, and `#[allow(clippy::needless_return)]` to `write_block`. All
three removed:

- `state` is unused only in the `kernel_profile_extreme` arm, so that arm now
  says `let _ = state;`. A function-wide allow would have hidden a genuinely
  unused variable added later.
- `write_block`'s allow came with a comment claiming the `return` was
  "load-bearing: the two cfg blocks are mutually exclusive, so without it the
  dead arm's value would have to typecheck in both profiles." **That is not how
  `cfg` works** — only one block survives compilation, so the survivor is
  already the tail expression. `read_range`/`write_range` immediately above it
  use exactly that shape. Dropping the `return` compiles clean on both profiles.

## 3. Six new clippy warnings, and a test that tested nothing

`cargo clippy -p akuma-ext2 --all-targets` went from HEAD's 18 warnings to 24.
All six new ones fixed (`needless_lifetimes`, `uninlined_format_args`,
`useless_format`, 3x `identity_op`, `unnecessary_wraps`).

The `identity_op` warnings were sitting in dead code, which is how the real
defect surfaced: `writeback_mixed_workload_persists_coherently` extends
`/mix/f3` with a partial-block write, then deletes it in the same
`(0..9).step_by(3)` loop — so the match arm verifying that extension was
unreachable, and the "extend a couple (partial blocks both ends)" comment was
only half true. Changed the delete set to `step_by(4)` (f0/f4/f8) so f3/f5/f7 —
the three the mutation phase touches — survive to be checked.

**Lesson:** a clippy warning inside a test's expectation block is worth reading
as "is this code even reached?", not just silencing.

## 4. `ext2probe-host` could never be built `--release`

```
error: struct `ClockBlockCache` is never constructed
error: type alias `DevFlush` is never used
```

Root cause in `crates/akuma-ext2/build.rs`: it emitted
`cfg(kernel_profile_extreme)` whenever `OPT_LEVEL == "z"`, *regardless of the
`extreme` feature*. The `userspace/` workspace's release profile is
`opt-level = "z"`, so akuma-ext2 built from there believed it was the extreme
kernel — no block cache — while `fs-cache` still compiled `ClockBlockCache`,
which then had nothing to construct it.

The unconditional test was a leftover: it dated from when a second size profile
(`size`) existed and wanted the same behaviour. That profile was removed
2026-08-10, and both `Cargo.toml` and the sibling build scripts already document
the intended rule — root `Cargo.toml` on `[profile.extreme-size]` says
"build.rs keys the extreme behaviour off the `extreme` FEATURE, not the
profile". The code just did not match. Now it requires `CARGO_FEATURE_EXTREME`.

**Verified behaviour-neutral for every shipped target:** the `extreme-size`
kernel binary is byte-identical before and after (md5 `9d94eafd…`), because
`build_extreme_size.sh` passes `extreme` and `opt-level = "z"` together.

Pre-existing at `4b086f3d`, confirmed by stashing — this was not introduced by
the write-back work, only exposed by it.

**Lesson:** inferring a *kernel profile* from `OPT_LEVEL` misfires for any other
size-optimised consumer of the crate. Key profile cfgs off features.

## 5. `cargo build --release` left a stale `akuma.bin`

`akuma.bin` — the flat image QEMU actually boots — was produced only by
`scripts/cargo_runner.sh`, i.e. only on `cargo run`. A `cargo build --release`
left the *previous* build's `.bin` in place, looking current. This corrupted a
real ext2 A/B during this very session: the "new" arm was measured against the
other arm's kernel image until the mtime was checked.

Fixed with a `-C linker=` wrapper (`scripts/link_kernel.sh`, wired in
`.cargo/config.toml`): it execs the real `rust-lld`, then objcopies. The link
step is the only hook that runs after the ELF exists and still inside
`cargo build` — **cargo has no post-build hook**, and `build.rs` runs *before*
its own crate is compiled and linked, so nothing else can post-process the
binary. objcopy + the size ceiling are factored into `scripts/mkbin.sh`, shared
by the wrapper, `cargo_runner.sh` and `dropoff_kernel.sh`.

The link output is `deps/akuma-<hash>` (cargo *uplifts* it to `<profile>/akuma`
afterwards), so the wrapper converts from the file that exists at link time and
writes to where the uplifted ELF will land, sidestepping the ordering.

**Residual hole, documented in both scripts:** on an uplift-from-cache there is
no link, so the wrapper does not run. That is why `cargo_runner.sh` still
regenerates unconditionally.

## 6. The linker wrapper would have broken the self-hosted build

The first draft was `#!/bin/bash` using `${BASH_SOURCE[0]}`. The guest rootfs
has **no bash** — only busybox `/bin/sh` — and `.cargo/config.toml` applies to
in-guest builds of this tree too, so the wrapper would have failed to exec and
taken the entire self-hosted kernel build (acceptance/10) down with it.

Both scripts are now POSIX `sh`, matching `dropoff_kernel.sh`'s existing
convention. Verified three ways under busybox: produces a byte-identical `.bin`;
with **no objcopy at all** it warns and **exits 0** so the link still succeeds;
and it picks the 1 MB vs 4 MB ceiling from the path without ever failing a link.
Also executed inside the live Akuma guest.

Related: objcopy discovery. `dropoff_kernel.sh` looked for `rust-objcopy` then
`llvm-objcopy` and gave up — but the self-host image gets neither. It gets GNU
`objcopy` from apk `binutils`, which `populate_disk.sh --with-rust-toolchain`
already installs. `mkbin.sh` now tries all three, and GNU `objcopy -O binary`
was verified **byte-identical** to `rust-objcopy` on the kernel ELF
(md5 `cc25e983…`, 3 322 096 B). No new packages needed.

## 7. Why the flat `.bin` exists at all (asked, and answered with evidence)

"Can QEMU just boot the ELF?" — no. QEMU's aarch64 `-kernel` treats a plain ELF
as a bare-metal image: it jumps to the entry point with `x0 = 0` and passes no
device tree. The flat image carries an ARM64 Image header (`text_offset = 1 MB`,
`linker.ld`), which puts QEMU on the Linux boot protocol instead. Measured, same
kernel and same flags:

| | `.bin` | ELF |
|---|---|---|
| `DTB ptr from boot (x0 arg)` | `0x48000000` | `0x0` |
| FDT | found | `no FDT; keeping bootstrap device map` |
| SMP | probed | `not probed; staying single-core` |
| outcome | boots to sshd | **kernel OOM panic**, `src/allocator.rs:513` |

The `.bin` is not packaging — it is the boot protocol. Recorded so the question
does not get re-litigated from first principles.

## 8. Wall-clock on this host drifts ~2x between sessions

The write-back A/B measured `create` at 1858 ms (A-D) vs 1433 ms (write-back).
Hours later, on the same commit, the same probe measured 779 ms. **The machine
itself was ~2x faster**, so the first D-4 numbers looked like a spectacular win
until a fresh baseline arm was run back-to-back and showed the truth (see
`EXT2_WRITEBACK_DESIGN.md` § D-4).

This is the trap `crates/akuma-ext2/README.md` § Performance already warns about,
hit again. The rules that survived it:

- **Absolute millisecond figures are session-relative.** Only compare arms
  measured back-to-back in one session.
- **Report ranges, not just medians**, and state whether they are disjoint. The
  write-back A/B's claim rests on 6/6 disjoint ranges; D-4's rests on 1/6.
- A device-I/O count from `ext2probe-host` is deterministic and carries a claim
  on its own; wall-clock does not.

## 9. D-4's premise was wrong (and the real read bottleneck)

D-4 (zero-copy reads) is implemented and kept, but it does **not** speed up
reads: a focused warm-read benchmark came out 1.90 s on both arms. The design
text assumed the per-block memcpy was a visible share of `seq_read`; at ~46 us
per 4 KB block accessed, the ~1 us copy is not.

`Filesystem::read_at` re-resolves the **path** on every `read(2)` —
`src/syscall/fs.rs` passes `&f.path`, so each call runs a full
`lookup_path_internal` directory walk plus `read_inode`, and allocates a temp
buffer of up to 64 KB. That is where the time goes. The next read lever is
per-fd inode caching (resolve at `open(2)`, let `read(2)` use
`read_at_by_inode`, which the mmap/exec fault path already does), not the block
cache.

---

## Verification performed

- `cargo test -p akuma-ext2`: 76/76, and 76/76 again with `--features fs-cache`.
- `cargo clippy -p akuma-ext2 --all-targets`: back to HEAD's 18 pre-existing
  warnings; every warning the write-back work added is gone.
- `cargo build --release`, `scripts/build_extreme_size.sh`: clean; extreme
  binary byte-identical across the build.rs change.
- `ext2probe-host` device-I/O A/B; `ext2probe` guest A/B (3 runs per arm, arms
  confirmed distinct by `akuma.bin` size) for both write-back and D-4.
- Verification-ladder step 5's functional half, previously owed and now run
  against the write-back + D-4 kernel: nested dirs 5 deep, 50 files of
  11–4768 bytes created and content-verified byte-for-byte, rename with content
  intact, re-verify after `sync`, `rm -rf` leaving zero residual entries. All
  passed.
