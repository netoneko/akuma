# Trimming Fat Part 2

Continued removal of components that add build complexity and maintenance burden without contributing to the core OS goals.

## Removed: DOOM

**What:** `userspace/doom/` — a `no_std` port of DOOM using [doomgeneric](https://github.com/ozkl/doomgeneric), rendered as ANSI truecolor half-block art over SSH and to ramfb framebuffer.

**Why removed:**
- Large vendored C source tree (`doomgeneric/`) with a custom build pipeline (`aarch64-none-elf-gcc` via `cc` crate)
- Requires a separate deploy step (not in `build.sh`) and a shareware WAD file on disk
- Not related to OS functionality — a demo/novelty, not infrastructure
- Several kernel workarounds were added specifically for DOOM (SSH keepalive timeout increases, SSH global request handler, window adjust handler) that add noise to the kernel

**Files removed:**
- `userspace/doom/` (entire directory)
- `"doom"` entry from `userspace/Cargo.toml`
- DOOM references from `README.md`

**Files updated:**
- `docs/DOOM.md` — kept for historical reference, marked as removed

## Removed: `-L qemu-roms` QEMU flag

**What:** The `-L qemu-roms` flag passed to `qemu-system-aarch64`, pointing at a local directory of QEMU firmware/ROM files.

**Why removed:** Only needed for the ramfb framebuffer used by DOOM's display output. With DOOM gone, the flag is dead weight.

**Files updated:**
- `scripts/run.sh`
- `scripts/cargo_runner.sh`
- `.claude/settings.local.json` (allowlist entries)

## Removed: sqld

**What:** `userspace/sqld/` — a TCP-based SQLite daemon exposing a network interface for executing SQL queries against a local SQLite database.

**Why removed:** Application-level service with no tie to core OS functionality. Adding a full SQLite amalgamation and custom network protocol is significant build weight for a feature that isn't part of the OS itself.

**Files removed:**
- `userspace/sqld/` (entire directory)
- `"sqld"` entry from `userspace/Cargo.toml`
- `"sqld"` from both MEMBERS and BINARIES arrays in `userspace/build.sh`
- sqld row from `README.md` capabilities table and architecture diagram

**Files updated:**
- `docs/SQLD.md` — kept for historical reference, marked as removed
- `docs/C_STUBS.md` — noted that sqld stubs no longer exist
- `docs/QJS.md` — noted sqld removal in the comparison section

## Removed: xbps

**What:** `xbps` — the Void Linux package manager, used to install packages from Void Linux repositories.

**Why removed:** Maintaining two package managers (xbps and apk) was redundant; apk (Alpine Linux) covers the same use case with a smaller footprint.

## Removed: scratch

**What:** `userspace/scratch/` — a custom `no_std` Git client implementing Git Smart HTTP protocol (clone, fetch, pull, push, commit, branch, tag, status).

**Why removed:** With Alpine apk available, `git` can be installed via `apk add git`, providing a full standard Git implementation without maintaining a bespoke no_std client. The custom implementation also required a 256+ KB stack for zlib decompression and had ongoing compatibility issues.

**Replacement:** `apk add git-core`

**Files removed:**
- `userspace/scratch/` (entire directory)
- `"scratch"` from `userspace/Cargo.toml` and both arrays in `userspace/build.sh`
- scratch row from `README.md` and architecture diagram; updated `CLAUDE.md` table

**Files updated:**
- `userspace/meow/src/tools/git.rs` — all `scratch <cmd>` calls replaced with `git <cmd>`
- `userspace/meow/src/config.rs` — removed "via scratch" from Git tools section header
- `docs/SCRATCH.md` — kept for historical reference, marked as removed
- `docs/SCRATCH_CLONE_DECOMPRESSION_FIX.md` — kept for historical reference, marked as removed
