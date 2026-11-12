# amd64: the framebuffer back buffer deadlocked the boot on a 4K panel

**Date:** 2026-11-11. **Machine:** the trashcan (amd64 metal, 3840x2160 framebuffer, GRUB/multiboot2).
**Status:** fix written, clippy clean, PVH gates green. **Not yet booted under multiboot2**, see Verification.

## Symptom

At 21:27 on 2026-11-10 the multiarch A/B rebooted the trashcan into `6a817703`, and it never came
back: no ping, and ARP stayed "incomplete". After a power cycle the screen stops at the memory
bring-up:

```
  heap: 0x000000002212d000 + 1024 MiB ... ok
  pmm:  arena 0x0000000000100000 + 16879 MiB spanning 6 RAM region(s)
  pmm:  16321 MiB RAM, 3776802 free frames (14753 MiB)
```

The `[fb] double buffer on|off` line that should come next never appears, and neither does any
panic or fault. The core spins silently.

## Cause

`kmain_mb2` calls `fb_enable_double_buffer()` right after `mem::init_reserving`. That function
came from main's console back-buffer change (`b50111a0`), which reached this branch in the merge
`67c15a4f`. It took `CONSOLE.lock()` and then allocated the RAM copy of the screen while holding
the lock: `pitch / 4 * height` dwords, which is 16384 x 2160 bytes, about 34 MB.

`akuma-alloc` prints `[HEAP] NMB used` from inside the allocation the first time usage crosses
13 MB, then every 5 MB after that (`talc_alloc`, unconditional). This allocation crosses the
threshold. The print goes through `serial::puts` to `multiboot2::mirror_byte`, which takes
`CONSOLE.lock()` again on the same core. `spinning_top::Spinlock` is not reentrant, so the core
deadlocks with interrupts masked and nothing left to draw.

Why nothing caught it:

- PVH boots (QEMU TCG, Firecracker, KVM) have no framebuffer console, so `CONSOLE` is `None`
  and the function returns before allocating.
- At 1920x1080 the copy is about 8 MB, below the 13 MB first threshold, so no print fires.
  Only a large panel crosses it.

## Fix

`fb_enable_double_buffer` reads the copy's shape under the lock (`Framebuffer::back_shape`),
releases the lock, allocates (`Back::new`, still fallible), then locks again to install the copy,
repaint and flush. The rule this establishes, "never allocate under a console sink's lock",
is in `docs/reference/subsystems/console.md` § Printing rules.

## Verification

- Clippy `-D warnings` clean on `akuma-amd64`.
- The amd64 TCG battery (PVH, which does not reach this function) is unchanged.
- **Not run:** a multiboot2 boot with a framebuffer above 13 MB. The GRUB rig on the ryzen's
  Pop install is down, and this Mac has OVMF but no GRUB. The trashcan itself is the test: boot
  the fixed kernel there **one-shot** (`grub-reboot`), so a repeat hang falls back to the default
  entry on the next power cycle instead of sticking.

## Still open

- Any other allocation under `CONSOLE.lock()` has the same shape. The framebuffer console grows
  its cell grid on a margin change (`CSI ? 9001`); that path has not been audited.
- The fix also belongs on `main`, where `b50111a0` landed.
