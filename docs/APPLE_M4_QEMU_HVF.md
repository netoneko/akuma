# QEMU HVF Acceleration on Apple Silicon (M4)

## Overview

QEMU's Hypervisor.framework (HVF) backend runs the guest at near-native speed on
Apple Silicon by using the M4 CPU directly instead of TCG emulation.

**Current status:** HVF mode is **non-functional** with stock QEMU 10.x due to a
GIC MMIO bug (see below). You must either use TCG (`-accel tcg`, default) or
build QEMU from source with the ISV=0 fix patches applied.

## How to Enable

### Step 1: Build patched QEMU

Stock QEMU 10.x crashes when the guest accesses the GIC distributor under HVF.
An RFC patch series by Joelle van Dyne fixes this by falling back to TCG
single-step emulation for instructions that produce ISV=0 in the data-abort ESR.

```bash
# Fork and clone QEMU
git clone https://gitlab.com/qemu-project/qemu.git
cd qemu
git checkout v10.0.0   # or whatever version you use

# Download and apply the patch series:
#   "[PATCH RFC 0/4] hvf: use TCG emulation to handle data aborts"
#   Mailing list: https://www.mail-archive.com/qemu-devel@nongnu.org/msg1094011.html
#
# Individual patches:
#   1/4 cpu-exec: support single-step without debug
#       https://www.mail-archive.com/qemu-devel@nongnu.org/msg1094012.html
#   2/4 cpu-target: support emulation from non-TCG accels
#       https://www.mail-archive.com/qemu-devel@nongnu.org/msg1094008.html
#   3/4 hvf: arm: emulate instruction when ISV=0  (the key fix)
#       https://www.mail-archive.com/qemu-devel@nongnu.org/msg1094009.html
#   4/4 hw/arm/virt: enable VGA  (optional, not needed)
#       https://www.mail-archive.com/qemu-devel@nongnu.org/msg1094010.html
#
# To download patches from mail-archive: click the message link, then use
# the "raw" or "mbox" download link. Save each as 01.patch, 02.patch, etc.
# Then:
git am 01.patch 02.patch 03.patch

# Build QEMU
mkdir build && cd build
../configure --target-list=aarch64-softmmu --enable-hvf
make -j$(nproc)

# Verify your binary:
./qemu-system-aarch64 --version
```

### Step 2: Configure Akuma

```bash
# In src/config.rs, set:
pub const QEMU_HVF_FIX_ENABLED: bool = true;

# In scripts/cargo_runner.sh, change -accel tcg to:
-accel hvf

# Build and run:
MEMORY=2048 cargo run --release
```

### Alternative: Minimal one-line QEMU patch

If you only need Akuma (which already has kernel-side workarounds for page table
flushes and the virtual timer), you can apply a simpler patch. In
`target/arm/hvf/hvf.c`, in the `EC_DATAABORT` case, replace:

```c
assert(isv);
```

with:

```c
if (!isv) {
    break;  /* retry — stage-2 mapping already fixed by fault handler */
}
```

This makes ISV=0 data aborts retry the instruction instead of aborting. For RAM
accesses (page table walks, dirty tracking) this works because QEMU has already
fixed the stage-2 mapping. For MMIO (GIC distributor), Akuma skips GIC init
under HVF so this path is never hit.

**Caveat:** This does NOT fix MMIO ISV=0 — it only helps RAM-based ISV=0 faults.
For full GIC support under HVF, the full Joelle van Dyne patch series is needed.

## What Breaks Under HVF and Why

### 1. Page Table Cache Coherency (`protect_kernel_code`)

**Symptom:** QEMU aborts immediately after "Kernel code protection enabled":
```
Assertion failed: (isv), function hvf_handle_exception, file hvf.c, line 1883.
```

**Cause:** `protect_kernel_code()` allocates new L2/L3 page tables and fills them
with `write_volatile`. Under TCG, QEMU reads guest memory directly so writes are
immediately visible. Under HVF, the real M4 hardware page table walker reads from
physical memory. If the D-cache hasn't been cleaned to PoC, the walker reads stale
data, faults during the walk (ISV=0 in ESR because the fault is from the walker
itself, not a user instruction), and QEMU's `assert(isv)` fires.

**Fix:** Issue `DC CIVAC` (data cache clean+invalidate by VA to PoC) on every
cache line of every new page table page before the `DSB ISH / TLBI / ISB` sequence.
The flush is gated on `hvf_fix: bool` passed from `config::QEMU_HVF_FIX_ENABLED`.

**Files:** `crates/akuma-exec/src/mmu/mod.rs` — `protect_kernel_code()`,
`flush_page_to_poc()`.

---

### 2. GIC MMIO Access (QEMU 10.x Bug — BLOCKING)

**Symptom:** QEMU aborts inside `gic::init()`:
```
Assertion failed: (isv), function hvf_handle_exception, file hvf.c, line 1883.
```

**Cause:** This is a QEMU 10.x bug (GitLab issue #2312). Under HVF on Apple
Silicon, all accesses to the GIC distributor PA (`0x0800_0000`) produce VM exits
with `ISV=0` in ESR, regardless of the instruction type (STR, LDR, STP — all
fail). This is not about instruction encoding — the GIC memory region itself is
intercepted by a different hypervisor mechanism than regular MMIO devices. QEMU's
HVF handler cannot decode ISV=0 faults and asserts.

We confirmed that replacing `write_volatile` with explicit `str w, [x]` inline
assembly (which always produces ISV=1 for normal MMIO) does NOT help — the GIC
region is special.

**Kernel-side workaround:** Skip `gic::init()` when `QEMU_HVF_FIX_ENABLED`.
All GIC functions (`enable_irq`, `trigger_sgi`, `acknowledge_irq`,
`end_of_interrupt`) are guarded by `GIC_INITIALIZED: AtomicBool`.

**Consequence:** Without GIC, there is:
- No interrupt delivery (timer IRQ 27 never fires)
- No preemptive scheduling (no timer-driven context switch)
- No cooperative scheduling (`yield_now()` calls `trigger_sgi` → no-op)
- No sleeping thread wakeups (`nanosleep` blocks forever)

This makes HVF mode **non-functional** for any multi-threaded or process-based
operation. The fix requires the patched QEMU (see "How to Enable" above).

**QEMU fix:** The RFC patch series "[PATCH RFC 0/4] hvf: use TCG emulation to
handle data aborts" by Joelle van Dyne (Feb 2025) adds a TCG single-step
fallback for ISV=0 data aborts. With this patch, GIC distributor MMIO works
normally under HVF and all kernel functionality is restored.

**Files:** `src/gic.rs` — `GIC_INITIALIZED`, guards on all public functions.
`src/main.rs` — conditional `gic::init()` call.

---

### 3. Physical Timer Registers Trapped by EL2

**Symptom:** Kernel exception EC=0x0 executing `MSR CNTP_CVAL_EL0, X9` during
"Enabling timer...".

**Cause:** Under HVF the hypervisor at EL2 traps EL1 access to the physical timer
registers (`CNTP_CTL_EL0`, `CNTP_CVAL_EL0`, `CNTPCT_EL0`). The guest OS is
expected to use the virtual timer (`CNTV_*`) instead.

**Fix:** Added `TIMER_USING_VIRTUAL: AtomicBool` to `src/timer.rs`. When set,
`enable_timer_interrupts()`, `timer_irq_handler()`, and `read_counter()` use
`CNTV_CTL_EL0`, `CNTV_CVAL_EL0`, and `CNTVCT_EL0` respectively.

Under HVF, `timer::set_use_virtual_timer()` is called before enabling the timer.
The scheduler and kernel-timer async wakeups are both routed through virtual timer
IRQ 27 (instead of the physical timer IRQ 30 used under TCG).

**Files:** `src/timer.rs` — `TIMER_USING_VIRTUAL`, `set_use_virtual_timer()`.
`src/main.rs` — conditional IRQ registration and `set_use_virtual_timer()` call.

---

## IRQ Routing Differences

| Feature | TCG (default) | HVF (patched QEMU) |
|---------|--------------|---------------------|
| GIC init | Full | Full (with QEMU ISV=0 fix) |
| Scheduler timer | IRQ 30 / `CNTP` | IRQ 27 / `CNTV` |
| Kernel timer (async) | IRQ 27 / `CNTV` | IRQ 27 / `CNTV` (shared) |
| SGI scheduling | Enabled | Enabled (with QEMU ISV=0 fix) |

## Known Remaining Issues / Fixed Bugs

### `DC CVAU` on User VA from Kernel Context (test bug, fixed)

**Symptom:** Tests `mprotect: flag update RW -> RX with cache maintenance` and
`mprotect: IC IALLU on 256-page region completes` crash with EC=0x25 (data abort
from current EL) at user addresses `0xC000_0000` / `0xD000_0000`.

**Cause:** Both tests allocate a `UserAddressSpace`, map pages into it, then issue
`dc cvau` using the **user virtual address** (`test_va`). But these tests run in the
kernel thread (Thread 0) with the kernel's boot TTBR0 active — and the user VA is
only mapped in the `UserAddressSpace` object's page table, not the kernel's own.

Under TCG, `dc cvau` on an unmapped VA is silently ignored. On real AArch64
hardware (HVF), cache-maintenance-by-VA goes through the current TTBR0 translation
exactly like a data access: if the VA is not mapped, it generates a translation
fault (EC=0x25, "data abort from current EL").

**Fix:** Use `phys_to_virt(frame.addr)` (the physical identity address, always
mapped in the kernel's TTBR0) instead of the user VA for the `dc cvau` loop.
Also gate `dc cvau` in `sys_mprotect` on `is_mapped(va)` to skip lazy pages.

**Files:** `src/tests.rs` — `test_mprotect_flag_update_with_cache_maintenance()`,
`test_mprotect_large_region_completes()`. `src/syscall/mem.rs` — `sys_mprotect()`.

## QEMU Version

Tested with QEMU 10.2.0 on macOS (Apple M4).

## References

- QEMU GitLab Issue #2312: https://gitlab.com/qemu-project/qemu/-/issues/2312
- RFC patch (cover): https://www.mail-archive.com/qemu-devel@nongnu.org/msg1094011.html
- Key patch (3/4): https://www.mail-archive.com/qemu-devel@nongnu.org/msg1094009.html
