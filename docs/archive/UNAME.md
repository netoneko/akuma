# uname(2) — Hardcoded Fields & Open Issues

> **Status: fixed 2026-08-04.** `release` and `version` are now build-derived; see
> "Fix (implemented)" at the bottom. Everything above that section describes the
> pre-fix state and is kept as the record of what was wrong.

`sys_uname` (`src/syscall/proc.rs:158-181`) fills in `struct utsname` with literal
byte-string constants — nothing is derived from the build or the crate manifest.

```rust
write_field(&mut kernel_buf, 0, b"Akuma");     // sysname
write_field(&mut kernel_buf, 1, b"akuma");     // nodename
write_field(&mut kernel_buf, 2, b"0.1.0");     // release
write_field(&mut kernel_buf, 3, b"Akuma OS");  // version
write_field(&mut kernel_buf, 4, b"aarch64");   // machine
write_field(&mut kernel_buf, 5, b"(none)");    // domainname
```

`busybox uname -a` prints these five fields plus a sixth, `"Linux"`, that comes
from busybox's own binary (bootstrap's `bin/busybox`, an unmodified Alpine
build) — it hardcodes an "operating system" string at compile time the same
way GNU coreutils does, and it is not read from Akuma's `sys_uname` at all.
Full output looks like:

```
Akuma akuma 0.1.0 Akuma OS aarch64 Linux
```

## Open Issues

### `release` doesn't match the crate version

**Symptom:** `uname -r` reports `0.1.0`, but the kernel crate's `Cargo.toml`
(`version = "0.0.7"`) says otherwise.

**Cause:** `"0.1.0"` is a plain literal, not `env!("CARGO_PKG_VERSION")` or
anything else tied to the build. Nobody has kept it in sync with the manifest.

### No injection path at all — build time or runtime

**Symptom:** There is no way to override `sysname`/`release`/`version` short of
editing the literal and rebuilding.

**Cause:**
- No `build.rs` step embeds `CARGO_PKG_VERSION`, a git SHA, or any other
  build-time value into these fields.
- `sethostname`/`setdomainname` (Linux syscalls 161/162) are not wired into
  the dispatch table (`src/syscall/mod.rs`) — only `UNAME` (read-only) is
  hooked up. So even the fields Linux normally lets userspace mutate at
  runtime (`nodename`, `domainname`) have no write path in Akuma.

### `version` is a static marketing string, not a build identifier

**Symptom:** `version` is always `"Akuma OS"` — it carries no information
about which commit or build produced the running kernel, unlike Linux where
this field typically holds a build timestamp/compiler string.

## Fix (implemented 2026-08-04)

Both build-derived fields landed as proposed; `sysname`/`nodename`/`domainname`
stayed static literals (there is still no `sethostname`/`setdomainname` write
path, so there is nothing for them to track).

**`release` — `env!("CARGO_PKG_VERSION")`** (`src/syscall/proc.rs`). No
`build.rs` step needed: cargo always sets it, so the proposed `"0.0.0"`
out-of-the-box literal was unnecessary. `uname -r` now reports `0.0.7`, the
kernel crate's actual manifest version, and cannot drift again.

**`version` — `concat!(env!("AKUMA_GIT_SHA"), "-", env!("AKUMA_BUILD_PROFILE"))`**,
both emitted by `build.rs`:

- `AKUMA_GIT_SHA` — `git rev-parse --short HEAD`, run from `build.rs` (the
  kernel is `no_std` and cannot shell out). Any failure — no git, no checkout,
  source tarball — degrades to `"unknown"` rather than failing the build.
  `cargo:rerun-if-changed` is emitted for `.git/HEAD` **and** for the ref file
  `HEAD` points at, so the SHA can't go stale behind a cache hit: the former
  covers branch switches, the latter covers new commits. Both are emitted only
  if they exist, since `rerun-if-changed` on a missing path makes cargo rebuild
  unconditionally.
- `AKUMA_BUILD_PROFILE` — reconstructed from the discriminators `build.rs`
  already computes (`extreme` → `extreme-size`, `OPT_LEVEL=z` → `size`,
  `CARGO_FEATURE_SMP_SHARED` → `release-smp-shared`, `CARGO_FEATURE_SMP` →
  `release-smp`, else `release`). Cargo's own `PROFILE` is useless here: it
  reads `"release"` for every profile that inherits `release`.

Verified by building two profiles at HEAD `b4e641b` and reading the `utsname`
literals straight out of each ELF:

| build | `strings` hit |
|---|---|
| `scripts/build_devbox_smoltcp.sh` | `akuma0.0.7b4e641b-release-smp-sharedaarch64(none)` |
| `cargo build --release` | `akuma0.0.7b4e641b-releaseaarch64(none)` |

So `busybox uname -a` now prints e.g.:

```
Akuma akuma 0.0.7 b4e641b-release-smp-shared aarch64 Linux
```

Note the profile name reports the *profile*, not the feature set — a
`devbox-smoltcp` kernel reads `release-smp-shared`, since that is the profile
`scripts/build_devbox_smoltcp.sh` builds with. The SHA disambiguates the rest.
