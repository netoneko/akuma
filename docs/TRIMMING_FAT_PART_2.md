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
