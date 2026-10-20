# Note for crush: MSRPM/IOPM + SVM on the APs (2026-10-21)

Crush — the permission maps and the per-core enable are in. This increment
extends the rung-0 guest from "two vmmcalls, hlt" to a guest that exercises
**both permission maps**, and turns SVM on across all cores:

- **MSRPM** (§15.11) and **IOPM** (§15.10.1) as `.bss` statics in
  `amd64/src/kvm.rs`, 4-KiB-aligned and *physically contiguous* (that is the
  property §15.11/§15.10.1 demand and two frames do not have — the linker
  guarantees it of image objects; PA = VA − KERNEL_VMA). Two bits armed at
  compile time: EFER read, port `0x80`. The const builders and their
  offset tests live in `akuma-svm-amd64` (`msrpm::`, `iopm::`, 16 host tests).
- The guest grew `rdmsr(ECFER)` + `out 0x80, al` between its vmmcalls. The
  exit loop emulates RDMSR (RAX from the save area's EFER, EDX carried to the
  next VMRUN as an asm input — RDX is *shared*, not saved, Table B-2) and
  advances RIP for the IOIO exit. `vmrun` now declares the shared GPRs
  clobbered; **rbx/rbp cannot be inline-asm operands in Rust** (LLVM-reserved
  / frame pointer) — the seed guest never writes them, rung 2 needs a real
  register file.
- **SVM on the APs**: the BSP reserves all 16 host save areas up front
  (`reserve_hsaves`, deliberately leaked — no allocation during AP bring-up),
  and every AP enables through `kvm::ap_hook` called from `ap_entry64`
  (`amd64/src/smp.rs`). `EFER.SVME` is per-core MSR state.

Full record: `docs/archive/AKUMA_KVM.md` §2.7 + the `[10-21]` timeline block.

## The ask

Same drill as last time:

1. Rebuild and install: `kbuild`, then
   `cp target/x86_64-unknown-none/release/akuma-amd64 /boot/akuma-amd64`
   (done from inside Akuma, yours).
2. Reboot into **grub entry 27** (`overlays/ryzen/grub.cfg`, the `kvmtest`
   boot). Rehearsal first if you want it: `sh overlays/ryzen/qemu.sh 27`.
3. Read `dmesg` **immediately** — `[rtw]` spam eats the boot log within
   minutes.

## Expected on the metal

```
[KVM] Fn8000_000A: rev=... asids=32768 edx=1ebf1cff
[KVM] trivial guest: PASS (vmmcalls=2, msr=1, ioio=1, rax=0000000000001000)
  smp: cpu N online ...
  [KVM] cpu N svm enabled          <- one line per AP, after each "online"
```

The PASS line is new: `msr=1 ioio=1` are the two permission-map exits, and
`rax=0000000000001000` is `EFER.SVME` read back through the emulated RDMSR at
the first vmmcall checkpoint.

## If it fails, the signatures

- `exit 7Ch` with `rcx` ≠ `C000_0080` — an MSR the seed never touched got
  intercepted: MSRPM miswired (offset/bit math), or the auto-intercept rule
  biting (uncovered MSR + `MSR_PROT`).
- `checkpoint one: RAX != EFER[31:0]` — the `rdmsr` was not intercepted at
  all: `MSR_PROT` not set in the VMCB, `MSRPM_BASE_PA` wrong, or the armed
  bit landed on the wrong side (read vs write).
- everything passes but no second exit, or `ioio=0` — the IOPM bit for port
  `0x80` is not armed / not found: check `IOPM_BASE_PA` (bits 11:0 ignored,
  4-KiB aligned).
- an AP line `svm NOT enabled (no host save area)` — the reserve pool ran
  dry: count frames, `MAX_CPUS` is 16.
- box resets ~1 min after the PASS line — GIF again (should not be; `stgi`
  is still in the same `asm!` as `vmrun`), but check the CMOS breadcrumb
  (`crumb()` byte 0x7F, read from Pop via `/dev/port`): 8 = inside the exit
  loop, 9 = guest finished.

Known-untranscribed (recorded in §4 of the doc, not silently guessed): the
IOIO `EXITINFO1` bit layout and the `VMEXIT_MSR` `EXITINFO1`/`EXITINFO2`
fields — the handler reads RCX/RDX as shared GPRs instead. If metal hands us
a dump line, its `info1` is free data for the next extraction.

Keep the spec-first, host-tested, honest-about-what-is-unproven habit. Next
rung after this: Linux as the guest — long-mode guest state first.
