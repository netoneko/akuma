# The framebuffer path was removed (2026-08-31)

`src/fw_cfg.rs`, `src/ramfb.rs` and `src/syscall/fb.rs` are gone, along with the
`sc-framebuffer` feature, the `kernel_framebuffer` cfg, `FW_CFG_PA`,
`HAS_FRAMEBUFFER`, `DEV_FW_CFG_VA` and the `libakuma` wrappers. This kernel now
has **no graphics output of any kind** — virtio-gpu never existed here either.

The syscall numbers **321, 322, 323 are reserved, not free** (see §5).

## 1. Why

It was a dead subsystem held open by its own initializer. Measured before
deleting, first-party trees only:

- **`fw_cfg` had exactly one consumer**: `ramfb.rs`, at two call sites —
  `find_file("etc/ramfb")` and `write_entry`.
- **`ramfb` had three**: `main.rs`'s boot-time `init(320, 200)`, the three
  `sys_fb_*` syscalls, and a boot self-test.
- **The syscalls had none.** `fb_init`/`fb_draw`/`fb_info` were exposed in
  `userspace/libakuma/src/lib.rs` and **no first-party userspace binary called
  any of them** — checked across all 30 members of the `userspace/` workspace.
  The only consumer there had ever been DOOM, removed in the trim-fat pass
  (`DOOM.md`).
- **The device was attached on one path only**: `-device ramfb` in
  `scripts/cargo_runner.sh`, TCG builds only (HVF dropped it deliberately). No
  devbox runner attached it.
- **Nothing was ever displayed**: the runner always passes `-display none`.

So the pixels went nowhere, on the one configuration that had a device at all.

### The ACPI question

fw_cfg's other entries were checked too, since a config channel would have been
a reason to keep the device. On QEMU virt with `-device ramfb` the directory has
ten entries (dumped by instrumenting the walk and booting):

| entry | sel | size |
|---|---|---|
| `bios-geometry` | 0x20 | 0 |
| `bootorder` | 0x21 | 0 |
| `etc/acpi/rsdp` | 0x22 | 36 |
| `etc/acpi/tables` | 0x23 | 131072 |
| `etc/boot-fail-wait` | 0x24 | 4 |
| `etc/ramfb` | 0x25 | 28 |
| `etc/smbios/smbios-anchor` | 0x26 | 24 |
| `etc/smbios/smbios-tables` | 0x27 | 302 |
| `etc/table-loader` | 0x28 | 3200 |
| `etc/tpm/log` | 0x29 | 0 |

Two are empty, one was ours, and seven are ACPI/SMBIOS — which **this kernel has
never parsed**. A grep for `acpi|madt|rsdp|xsdt|fadt` across `src/` and
`crates/` returns exactly one hit, a comment in `akuma-boot` saying there is no
ACPI. Akuma is DTB-driven end to end (`rust_start(dtb_ptr)` →
`fdt::Fdt::from_ptr`), on Firecracker as well.

Worth recording, since it came up: those tables are **not fixed**. QEMU generates
them from machine config — MADT carries one GIC CPU interface entry per `-smp`
core, ranges follow `-m`. `etc/table-loader` is the giveaway: it is QEMU's ACPI
linker-loader command stream (`ALLOCATE` / `ADD_POINTER` / `ADD_CHECKSUM`) that
firmware executes to place, patch and checksum the blobs. Fixed tables would ship
in a ROM. None of which matters here — fw_cfg is a *pull* interface, so unread
entries cost nothing, and we only ever selected one key.

### What was given up

A real capability, and it should be named honestly: `-fw_cfg
name=opt/akuma/foo,file=...` injects arbitrary named blobs, and `find_file` plus
the read path already handled them. That is a host→guest boot-config channel
needing no disk, no network and no cmdline parsing — something Akuma lacks
(nothing reads `-append`/`bootargs` today).

It was dropped anyway because it is **QEMU-only**. Firecracker has no fw_cfg
node, which is the documented `FAR=0x8000012008` data abort
(`AKUMA_FIRECRACKER_KVM.md`). Building a config channel that one of the two
supported platforms structurally cannot use is the wrong foundation; the DTB
(`/chosen/bootargs`) is the portable answer, and Firecracker builds one.

## 2. What went, exactly

Deleted: `src/fw_cfg.rs`, `src/ramfb.rs`, `src/syscall/fb.rs`,
`docs/reference/subsystems/drivers/fw_cfg.md`,
`docs/reference/subsystems/syscalls/fb.md`.

Edited: `src/main.rs` (two `mod`s, the boot init block), `src/syscall/mod.rs`
(`mod fb` + three dispatch arms), `src/platform.rs` (`FW_CFG_PA` ×2,
`HAS_FRAMEBUFFER` ×2, two device-map `push`es), `crates/akuma-primitives/src/addr.rs`
(`DEV_FW_CFG_VA` + its `DEV_WINDOW_SPANS` row), `crates/akuma-mmu/src/types.rs`
(re-export), `crates/akuma-syscalls-linux/src/{nr,lib}.rs`, `build.rs`
(`kernel_framebuffer` + check-cfg), `Cargo.toml` (`sc-framebuffer` + default
set), `userspace/libakuma/src/lib.rs` (`FBInfo` + three wrappers + three
numbers), `scripts/cargo_runner.sh` (`FB_ARGS`), and the four feature lists in
`overlays/devbox{,-smoltcp}/run.sh`, `scripts/build_devbox.sh`,
`scripts/sched_audit_matrix.py` — those last four would otherwise fail with
"feature `sc-framebuffer` does not exist".

`0x80_0001_2000` is now a **hole** in the device window. It was left as a hole
deliberately: the spans are just addresses in a 2 MB window, and re-packing them
to close a gap would churn every device for nothing.

## 3. What this cost the BKL driver carve-out

`no-bkl-drivers` (Phase 6) rests on each driver's own fine-grained Spinlock
standing in for the BKL — `RNG_DEVICE`, `SOUND_DEVICE`, and formerly ramfb's
`FB_STATE`.

`test_drivers_bkl_drop` **lost its only real guarded path.** Every `getrandom`
leg fails `validate_user_ptr` *before* `DriverBklGuard` is constructed, so those
legs only ever proved the guard is not opened on early-error paths.
`sys_fb_init` was the one driver syscall that took dimensions rather than
pointers, so it could be called from a boot test without a mapped user page —
which made it the only leg that opened the window and proved it closed.

What remains is early-error paths plus the kill switch. **Do not read a pass as
"the dropped window closes."** Restoring that coverage needs a driver syscall
callable without a mapped user buffer; there is none today. This is stated in the
test's own doc comment so the next reader does not have to find this file.

## 4. Immediately before: the unsafe cleanup (same day)

The pair was cleaned up hours before it was deleted (commit `afbbc3bb`), which
looks wasted but produced two findings worth keeping:

- `fw_cfg.rs` went 6 `unsafe` → 1; `ramfb.rs` went 4 → 0 and took
  `#![forbid(unsafe_code)]`, the second enforced ban outside `crates/` after
  `src/syscall/`.
- **`write_entry` swallowed DMA errors.** The spin loop broke on `ctrl == 0`
  *or* the `ERROR` bit and returned `()`, so a failed configure was
  indistinguishable from success and `ramfb::init` still reported `Ok`. Never
  fixed; recorded here because the shape — *break on either outcome, report
  neither* — is a pattern worth recognising elsewhere.
- The DMA descriptor's `control` field was polled with `read_volatile` while
  QEMU wrote it by DMA. That is a data race however it is spelled; it became an
  `AtomicU32` with an `Acquire` load.

## 5. If a framebuffer comes back

Take **321, 322, 323 again**. They are reserved rather than free, and
`crates/akuma-syscalls-linux/src/nr.rs` says so in place of the old constants:
a `libakuma` built before this removal still encodes those numbers in its
`fb_init`/`fb_draw`/`fb_info` wrappers, and any binary sitting on an existing
disk image carries them baked in. Reusing them for something else would hand
such a binary a silent wrong syscall instead of `ENOSYS`.

Prefer virtio-gpu over ramfb if the target is still QEMU, or a DTB-described
`simple-framebuffer` if it needs to work on Firecracker. Reviving `fw_cfg`
itself is only worth it for the `opt/` config channel described in §1, and only
once the QEMU-only limitation is acceptable.

## Background

- `DOOM.md` — the framebuffer's only real consumer, removed earlier.
- `AKUMA_FIRECRACKER_KVM.md` — the `FAR=0x8000012008` data abort that first
  showed fw_cfg was QEMU-only.
- `BKL_DRIVERS_CARVE_OUT.md`, `BKL_FINE_GRAINED_LOCKING_PLAN.md` §6 — the
  carve-out §3 discusses.
- `SYSCALL_UNSAFE_CLEANUP.md`, `INLINE_ASM_CLEANUP.md` — the `forbid` discipline
  §4's cleanup was following.
- `TRIM_FAT_MMIO_NEWTYPE.md` — `MmioReg`, whose generic width existed because
  fw_cfg needed u8/u16/u64 registers.
