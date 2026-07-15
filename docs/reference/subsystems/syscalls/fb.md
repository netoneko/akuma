# fb syscalls

fb_init / fb_draw / fb_info — thin syscall wrappers over the `ramfb`
(fw_cfg + DMA) framebuffer driver. Source: `src/syscall/fb.rs`. Gated by the
`sc-framebuffer` feature (Tier 1: dead weight when off, see
[`../syscalls.md`](../syscalls.md) and
[`../config-flags.md`](../config-flags.md)).

> **Stability: A (stable, dormant).** No functional changes since the
> initial syscall split (Mar 2026); only a clippy touch since. DOOM
> (`docs/archive/DOOM.md`) is the only known consumer, and its use of these
> three calls has been trouble-free.

## fb_init / fb_draw / fb_info

`sys_fb_init(width, height)` (`fb.rs:6`) rejects `0` or out-of-range
dimensions (`> 1920×1080`) with `EINVAL`, then calls `ramfb::init()`;
`EIO` on driver failure.

`sys_fb_draw(buf_ptr, buf_len)` (`fb.rs:17`) requires `ramfb::is_initialized()`
first (`EIO` otherwise), then copies the user buffer into the framebuffer in
**1 MB kernel-buffer chunks** rather than one giant `copy_from_user_safe` —
avoids a multi-megabyte kernel-heap spike for a full 1920×1080×4 frame. A
partial-copy fault after at least one chunk succeeded returns the partial
byte count instead of `EFAULT`/`EIO`, so a caller can tell "some pixels
landed" from "none did."

`sys_fb_info(info_ptr)` (`fb.rs:48`) copies out `ramfb::FBInfo` (width,
height, stride, XRGB8888 fourcc) or `EIO` if the framebuffer was never
initialized.

All three validate pointers with `validate_user_ptr` before touching user
memory and return errno-encoded (`-errno`) values via `libc_errno`, same
convention as the rest of the syscall table.

## Background

- `archive/DOOM.md` — the framebuffer's one real consumer; syscall numbers
  321–323 and the fw_cfg/DMA `ramfb` driver this wraps.
