# QEMU HVF ISV Assertion Bug

## Problem

Running Akuma with `-accel hvf` crashes QEMU on boot:

```
Assertion failed: (isv), function hvf_handle_exception, file hvf.c, line 1883.
```

Without HVF, QEMU falls back to TCG (software emulation), which is ~3000x slower for NEON-heavy workloads like llama.cpp inference. See `LLAMA_CPP_AKUMA_VS_ALPINE_PERFORMANCE_GAP.md` for details.

## Root Cause

QEMU's HVF backend has an `assert(isv)` in the `EC_DATAABORT` handler with a TODO comment acknowledging the gap:

```c
/*
 * TODO: ISV will be 0 for SIMD or SVE accesses.
 * Inject the exception into the guest.
 */
assert(isv);
```

Per the ARM Architecture Reference Manual, the ISV (Instruction Syndrome Valid) bit in ESR_EL2 is NOT set for:

- STP/LDP (pair instructions, any register size)
- Pre/post-indexed load/stores (writeback variants)
- Any 128-bit SIMD/FP access (STR/LDR of Q-registers)
- ST1/LD1 (NEON element/structure loads)

When a stage 2 VM exit (e.g. dirty page tracking) coincides with one of these instructions, QEMU cannot decode the access from the syndrome alone and aborts. Akuma's exception handlers use `stp q0, q1, [sp, #offset]` extensively for NEON save/restore, triggering this.

Replacing `stp` with single-register `str q0` would NOT help — 128-bit accesses never set ISV regardless of instruction form. `stp d0, d1` (64-bit pair) also fails because STP never sets ISV.

Upstream issue: https://gitlab.com/qemu-project/qemu/-/issues/2312

## Workarounds

### 1. Patch QEMU locally (recommended)

Replace the `assert(isv)` with a retry. When ISV=0 and the access is to RAM, the stage 2 mapping has already been fixed by QEMU's fault handler — retrying the instruction succeeds.

In `target/arm/hvf/hvf.c`, `hvf_handle_exception()`, `EC_DATAABORT` case, replace:

```c
assert(isv);
```

with:

```c
if (!isv) {
    break;  /* Retry — stage 2 mapping already fixed */
}
```

Build QEMU from source with this change (`brew install --build-from-source qemu` after patching, or build from the GitLab repo).

### 2. Remove `-device ramfb`

The ramfb framebuffer device likely enables dirty-page tracking, which write-protects guest RAM pages at stage 2 and causes VM exits on first write. Since `-display none` is already set, ramfb is unnecessary.

In `scripts/run.sh`, remove:
```
-device ramfb
```

### 3. Pre-fault kernel stack pages

Touch all kernel stack pages before installing exception vectors to ensure they are mapped in stage 2. This prevents stage 2 faults during NEON save in exception handlers.

```rust
// Before exceptions::init()
unsafe {
    let stack_base = 0x40700000u64;
    let stack_top  = 0x40800000u64;
    let mut addr = stack_base;
    while addr < stack_top {
        core::ptr::write_volatile(addr as *mut u8, 0);
        addr += 4096;
    }
}
```

Only helps if the boot stack is the trigger; other pages could also fault.

### 4. Use `-cpu host` instead of `-cpu max`

`-cpu max` enables features the host may not fully support, potentially causing QEMU to set up extra memory protections. `-cpu host` passes through only actual hardware capabilities.

### 5. Upstream fix

File or comment on the existing QEMU issue. The TODO in the source says exactly what needs to happen. KVM already handles non-ISV data aborts by fetching and decoding the guest instruction or injecting an external abort.

## Recommended approach

Try removing ramfb first (one-line change). If that doesn't fix it, patch QEMU locally — the change is minimal and well-understood.
