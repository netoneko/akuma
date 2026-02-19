# DOOM on Akuma

A `no_std` port of DOOM running as a userspace ELF binary on the Akuma bare-metal ARM64 kernel.

## Overview

DOOM runs on Akuma using [doomgeneric](https://github.com/ozkl/doomgeneric), a portable C DOOM engine. The C code is cross-compiled for `aarch64-unknown-none` via the `cc` crate and linked into a Rust `no_std` userspace binary that provides platform callbacks and C library stubs.

**Display:** ANSI truecolor half-block art rendered over SSH. Also outputs to ramfb framebuffer (visible if QEMU is started with `-display gtk`; headless by default).

**Input:** SSH terminal keystrokes translated to DOOM keycodes.

## Architecture

```
┌──────────────────────────────────┐
│  doomgeneric (C)                 │
│  - Game logic, renderer, WAD I/O │
├──────────────────────────────────┤
│  Platform layer (Rust, main.rs)  │
│  - DG_Init, DG_DrawFrame,       │
│    DG_GetKey, DG_SleepMs        │
│  - ANSI terminal renderer       │
│  - Key translation (SSH → DOOM) │
├──────────────────────────────────┤
│  C stubs (stubs.c)              │
│  - malloc/free → Rust allocator │
│  - fopen/fread → Akuma syscalls │
│  - printf/snprintf/vsnprintf    │
│  - strncpy, memcpy, memcmp, etc │
├──────────────────────────────────┤
│  libakuma (syscall interface)    │
│  - fb_init, fb_draw (ramfb)     │
│  - open, read_fd, lseek, fstat  │
│  - sleep_ms, uptime, exit       │
│  - poll_input_event             │
├──────────────────────────────────┤
│  Akuma kernel                    │
│  - ramfb driver (fw_cfg + DMA)  │
│  - VirtIO block (disk.img)      │
│  - ext2 filesystem              │
│  - Process/ELF loader            │
│  - SSH server (streaming exec)  │
└──────────────────────────────────┘
```

## Building

```bash
# Build the DOOM userspace binary
cd userspace && cargo build --release -p doom

# Deploy to disk image (doom is not in build.sh — must be deployed separately)
cd ..
debugfs -w -R "rm /bin/doom" disk.img 2>/dev/null
debugfs -w -R "write userspace/target/aarch64-unknown-none/release/doom /bin/doom" disk.img
# doom1.wad (shareware) must also be on disk.img at /doom1.wad

# Build the kernel and run (headless, no QEMU window)
cargo run --release
```

## Running

### Via SSH (interactive, with controls)

```bash
ssh -t -o StrictHostKeyChecking=no \
    -o UserKnownHostsFile=/dev/null \
    -o ServerAliveInterval=15 \
    -o ServerAliveCountMax=20 \
    user@localhost -p 2222 "doom"
```

**Important flags:**
- `-t` forces PTY allocation, which puts the client terminal in raw mode so keystrokes are sent immediately (not line-buffered). Without this, you'd have to press Enter after every key.
- `ServerAliveInterval` keeps the connection alive during gameplay.

### Via QEMU window (optional)

DOOM also renders to the ramfb framebuffer. To see this, start QEMU with `-display gtk` instead of `-display none` in `.cargo/config.toml` or `scripts/run.sh`.

## Controls

| Key       | Action          |
|-----------|-----------------|
| W / ↑     | Move forward    |
| S / ↓     | Move backward   |
| A / ←     | Turn left       |
| D / →     | Turn right      |
| Space     | Fire            |
| E         | Use / Open door |
| Q         | Run (shift)     |
| Enter     | Menu confirm    |
| Escape    | Menu / Pause    |
| Tab       | Automap         |
| 1-7       | Select weapon   |

Movement, fire, use, run, and tab are **holdable** — keep the key pressed and DOOM will continue the action. The engine detects terminal autorepeat and simulates key-held state with a 150ms release timeout.

## SSH ANSI Rendering

DOOM's 320×200 framebuffer is rendered as ANSI truecolor half-block art in the SSH terminal:

- **Resolution:** 80×24 characters (fits standard 80×24 terminals via `▀` half-blocks)
- **Color:** 24-bit truecolor with color deduplication (skips escape sequences when adjacent cells share colors, cutting frame size by ~40-60%)
- **Frame rate:** ~7 fps (every 5th game tick)
- **Throttling:** 30ms sleep after each frame write to let the SSH channel drain
- **Buffer:** 90KB static buffer — no per-frame heap allocation
- **Screen management:** First frame sends `\x1b[2J\x1b[3J` (clear display + scrollback), every frame sends `\x1b[?25l\x1b[H` (hide cursor + home) — widely compatible VT100 sequences
- **Line endings:** `akuma_print` converts bare `\n` to `\r\n` to avoid staircase rendering caused by a raw-mode timing race between the process and shell output handler

## Key Implementation Details

### WAD File Loading

The shareware `doom1.wad` (~4MB) is stored on `disk.img` (ext2). Since Akuma's `sys_read` reads the entire file on each call, the WAD is memory-mapped at startup: `W_StdC_OpenFile` allocates a single buffer via `malloc`, loads the full WAD into it, and subsequent `W_StdC_Read` calls use `memcpy` from this buffer.

See: `userspace/doom/doomgeneric/w_file_stdc.c`

### C Library Stubs

DOOM's C code expects standard library functions. These are provided by `userspace/doom/stubs/stubs.c`:

- **Memory:** `malloc`, `free`, `realloc`, `calloc` → delegate to Rust allocator via `akuma_malloc`/`akuma_free`
- **File I/O:** `fopen`, `fclose`, `fread`, `fwrite`, `fseek`, `ftell` → Akuma syscalls via `akuma_open`/`akuma_read`/`akuma_lseek`
- **Formatting:** `printf`, `snprintf`, `vsnprintf` with full format specifier support including integer precision (`%.3d`)
- **String:** `strncpy`, `strlen`, `strcmp`, `memcpy`, `memset`, `memcmp`
- **Math:** `abs`, basic operations

### Cross-Compilation

The `build.rs` compiles doomgeneric C sources using the `cc` crate with:
- Target: `aarch64-unknown-none`
- Compiler: `aarch64-none-elf-gcc` (ARM GNU toolchain)
- Flags: `-ffreestanding -fno-builtin -nostdinc`
- Custom include path for stub headers (`stubs/headers/`)

### SSH Keepalive Fix

Long-running processes like DOOM that don't write to stdout cause the SSH socket to appear idle. Two fixes were needed:

1. **Socket timeout:** Increased from 60s to 3600s in `src/ssh/server.rs`
2. **Global request handling:** Added `SSH_MSG_GLOBAL_REQUEST` handler in `SshChannelStream::handle_channel_message` to respond to `keepalive@openssh.com` requests during interactive exec sessions

## Bugs Fixed During Development

| Bug | Root Cause | Fix |
|-----|-----------|-----|
| SIGSEGV in `R_Init` at `lump[106].wad_file` | `strncpy` stub had off-by-one: wrote 9 bytes for 8-byte target, corrupting adjacent struct field | Rewrote `strncpy` with correct for-loop implementation |
| `STCFN33 not found` crash | `vsnprintf` didn't handle integer precision (`.3` in `%.3d`), producing "STCFN33" instead of "STCFN033" | Added `precision` parameter to `format_int` helper |
| Extreme slowness during WAD loading | `sys_read` reads entire file on each call; hundreds of lump reads = GBs of redundant I/O | Memory-mapped WAD: load once into `malloc` buffer, `memcpy` on reads |
| SSH disconnect after ~60s | TCP socket timeout of 60s with no stdout activity | Increased timeout to 3600s |
| SSH disconnect after ~120s | Server didn't respond to SSH keepalive global requests during exec | Added `SSH_MSG_GLOBAL_REQUEST` → `SSH_MSG_REQUEST_FAILURE` reply in channel message handler |
| Kernel OOM panic with ANSI rendering | ANSI frames produced faster than SSH could drain; process channel buffer grew unboundedly | Static buffer + frame skip + 30ms drain sleep after each frame |
| ANSI art scrolls instead of rendering in-place | Shell checks `raw_mode` at read time, but process sets it before shell reads first output batch — all init text gets bare `\n`, causing staircase that pushes cursor off-screen | `akuma_print` now converts `\n`→`\r\n`; Rust prints use `\r\n`; first frame clears display+scrollback (`\x1b[2J\x1b[3J`) |

## File Layout

```
userspace/doom/
├── Cargo.toml              # Package config, cc build dependency
├── build.rs                # Cross-compiles doomgeneric C sources
├── src/
│   └── main.rs             # Entry point, platform callbacks, ANSI renderer
├── stubs/
│   ├── stubs.c             # C stdlib implementations
│   └── headers/            # Minimal C headers (stdio.h, stdlib.h, etc.)
└── doomgeneric/            # Vendored doomgeneric C sources
    ├── doomgeneric.c/h     # Platform abstraction layer
    ├── w_wad.c             # WAD file handling
    ├── w_file_stdc.c       # File I/O with memory-mapped WAD
    ├── r_data.c            # Renderer data init
    └── ...                 # Other DOOM engine sources
```
