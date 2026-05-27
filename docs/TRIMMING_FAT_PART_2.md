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
