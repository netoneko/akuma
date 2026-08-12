# Self-hosting the userspace build: what breaks and how to get past it

Runbook-shaped notes from actually running `userspace/build.sh` **inside** a
running Akuma guest (a devbox-smoltcp VM, `/tmp/akuma` checkout), rather than
cross-compiling from the host. Distinct from
[`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md),
which self-hosts the **kernel** — this is the userspace workspace
(`userspace/build.sh`), built member-by-member with `--<name>-only`.

Session: 2026-08-12. Target members: `herd`, `sshd`, `box`, `paws`, `scratch`,
`wavplay`. All six eventually landed in `bootstrap/bin/`, but three separate
bugs had to be worked around first, in this order.

## 1. `sh build.sh` fails — needs bash

Busybox `sh` (ash) can't run `build.sh`: the script leans on bash indexed
arrays (`MEMBERS=(...)`, `"${MEMBERS[@]}"`, etc.) throughout, which ash
doesn't implement. `apk add bash` first, then `bash build.sh`.
Tracked as [`../archive/DEVBOX_ISSUES.md`](../archive/DEVBOX_ISSUES.md) Issue 7 —
worth eventually rewriting the script to be busybox-`sh`-compatible so a guest
doesn't need an extra package pull just to build.

## 2. `error[E0463]: can't find crate for `core`` — wrong toolchain on PATH

The guest's `apk`-installed `rustc`/`cargo` (`/usr/bin/*`, stable, currently
1.96.1) never had the `aarch64-unknown-none` target's `core`/`alloc` shipped —
`apk` doesn't carry a package for it, and there's no `rustup` on this image
(`apk add rustup` fails outright — not a real Alpine package) to add it.

The fix is **not** to install anything new: a complete nightly toolchain with
both `aarch64-unknown-linux-musl` and `aarch64-unknown-none` std already lives
at `/usr/local` (`rustc 1.99.0-nightly`), baked in by an earlier
`populate_disk.sh --with-rust-toolchain` pass (see that script's `RUST_CMD`
block — it installs nightly components straight from
`static.rust-lang.org/dist`, not via rustup). It just isn't ahead of `/usr/bin`
on `$PATH` by default:

```sh
export PATH=/usr/local/bin:/usr/bin:/bin:$PATH
```

Nightly (not stable) is what `userspace/build.sh` actually wants anyway: the
repo-root `rust-toolchain.toml` pins `channel = "nightly"` and — since
`userspace/` has no toolchain file of its own — a toolchain-aware invocation
resolves it by walking up to that root pin. Separately, `meow`'s build passes
`-Z build-std=core,alloc` (nightly-only) for its size-optimized binary. Most
*other* userspace crates would theoretically build against a stable
`aarch64-unknown-none` std if one existed, but nightly is simply what's
available and what the project's toolchain file already commits to.

## 3. Nightly cargo can't reach crates.io — but this is already root-caused

With the nightly toolchain on `PATH`, `cargo build` hung/failed fetching
dependencies:

```
warning: spurious network error (3 tries remaining): [7] Could not connect to server
  (Failed to connect to static.crates.io:443 after 310 ms: Could not connect to server)
...
error: failed to download from `https://static.crates.io/crates/picojson/0.2.3/download`
```

despite a plain `curl` to the same URL from the same shell working immediately.
**Don't re-diagnose this** — it's already fully root-caused in
[`../archive/CARGO_CRATES_IO_CONNECT_FAIL.md`](../archive/CARGO_CRATES_IO_CONNECT_FAIL.md):
nightly cargo's vendored, statically-linked libcurl (threaded DNS resolver +
bundled OpenSSL) never gets its non-blocking `connect()`s to complete against
Akuma's smoltcp kernel — confirmed 110 attempts / 0 completions in that doc's
repro — while apk's cargo (dynamic libcurl, c-ares resolver) does the identical
syscalls against the identical IPs and just works.

That doc's fix option 2 is the cheap one and is what worked here: use apk's
cargo **only** to populate the shared registry cache (both cargo binaries
default to the same `$HOME/.cargo` since neither sets `CARGO_HOME`), then let
nightly cargo build from the now-local cache with no further network calls:

```sh
cd userspace && /usr/bin/cargo fetch      # apk cargo — working network
export PATH=/usr/local/bin:/usr/bin:/bin:$PATH
bash build.sh --herd-only                 # nightly cargo — now offline-capable
```

`cargo fetch` doesn't need the `aarch64-unknown-none` sysroot (it resolves and
downloads, it doesn't codegen), so stable apk cargo doing this step is fine
even though it can't build the actual target.

## 4. Two flavors of in-VM crash — retry, don't debug

Once dependencies were cached, two of the six members still hit real crashes —
both matching *already-documented, open* memory-corruption bug classes in this
codebase, not anything new. **The fix in both cases was just retrying**;
crates that already compiled stay compiled, so a retry resumes rather than
restarts.

### 4a. `ld` segfaults linking a build script

First `--sshd-only` attempt:

```
   Compiling generic-array v0.14.7
error: linking with `cc` failed: exit status: 1
  = note: collect2: fatal error: ld terminated with signal 11 [Segmentation fault]
error: could not compile `generic-array` (build script) due to 1 previous error
```

`generic-array`'s `build.rs` compiles a tiny host-triple helper binary, and the
native linker (`ld`, via `cc`/`collect2`) segfaulted. A second attempt got past
this crate entirely — same class as `debug-thread-spawn-segv.md`'s "freshly
cloned thread SIGSEGVs at a fixed PC", not reproduced deterministically enough
here to pin down further.

### 4b. cargo segfaults on its own exit, *after* a successful build

Second `--sshd-only` attempt:

```
    Finished `release` profile [optimized] target(s) in 34.96s
build.sh: line 102:   861 Segmentation fault         cargo build --release -p "$m"
```

The link had already finished and the ELF was already on disk
(`target/aarch64-unknown-none/release/sshd`, correct size, fresh mtime) —
`cargo` itself crashed during its own post-build teardown. This is
[`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md)'s
**Defect B** (`[WILD-DA]` null-pointer read in `Rc::drop`, cargo's heap
corrupted from underneath it, not a cargo bug) — open, not yet root-caused
further than "page management, not cargo."

**Trap:** `build.sh` has `set -e`, so a `139` exit here aborts the script
*before* it reaches the `cp` into `../bootstrap/bin` — even though the binary
it needed to copy was already valid and complete. Check
`target/aarch64-unknown-none/release/<member>`'s mtime against the crash
before assuming the build produced nothing; if it's there, just `cp` it
yourself rather than re-running the whole compile.

### 4c. rustc itself panics mid-compile

First `--box-only` attempt:

```
thread '<unnamed>' (28) panicked at .../library/alloc/src/raw_vec/mod.rs:28:5:
capacity overflow
build.sh: line 102:   426 Segmentation fault         cargo build --release -p "$m"
```

rustc's own allocator panicked (a corrupted `Vec` capacity, not a real
allocation request that large) partway through compiling `box`, then
segfaulted. Same family as 4b — heap corruption underneath a rustc/cargo
process, not a logic bug in either. A clean retry compiled `box` in 10.57s
with no recurrence.

## 5. Extra crate-only member: `apk`'s busybox trigger error

Unrelated to the build itself but hit in the same session while installing
`xz`/`bash` via `apk`: every `apk add`/`apk fix` that touches the `busybox`
package prints `1 error` from its own post-install trigger, even though the
package installs correctly (binary works, applet symlinks resolve fine — not
a recurrence of the dangling-symlink bug). Not yet root-caused; see
[`../archive/DEVBOX_ISSUES.md`](../archive/DEVBOX_ISSUES.md) Issue 8.

## Recipe (what actually worked, end to end)

```sh
apk add bash                              # build.sh needs real bash, not busybox sh
export PATH=/usr/local/bin:/usr/bin:/bin:$PATH   # prefer the nightly toolchain already at /usr/local

cd userspace
/usr/bin/cargo fetch                      # apk cargo: working network, populates the shared registry cache

for m in herd sshd box paws scratch wavplay; do
    bash build.sh --$m-only
    # if it segfaults AFTER "Finished ... profile ...", check target/aarch64-unknown-none/release/$m
    # before retrying — it may already be built; just missing the bootstrap/bin copy.
done
```

## Background

- [`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md) —
  the kernel-side self-host build; §"Defect B" is the same cargo-heap-corruption
  class hit here in §4b/4c.
- [`../archive/CARGO_CRATES_IO_CONNECT_FAIL.md`](../archive/CARGO_CRATES_IO_CONNECT_FAIL.md) —
  full root-cause of §3, including the ruled-out hypotheses and the fix-options
  priority list this doc's §3 draws from.
- [`../archive/DEVBOX_ISSUES.md`](../archive/DEVBOX_ISSUES.md) — Issue 7
  (`build.sh` needs bash) and Issue 8 (`apk` busybox trigger "1 error"), both
  found in this same session.
- [`../runbooks/debug-thread-spawn-segv.md`](../runbooks/debug-thread-spawn-segv.md) —
  the broader open bug class §4a/4c belong to.
- `scripts/populate_disk.sh` (`RUST_CMD` block) — how the `/usr/local` nightly
  toolchain used in §2 actually gets installed onto an image.
