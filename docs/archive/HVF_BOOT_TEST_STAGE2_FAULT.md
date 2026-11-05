# The aarch64 boot self-tests aborted QEMU under HVF (2026-11-05)

**Status: fixed; the full boot suite now runs under HVF (166 PASS, 0 FAIL, filesystem tests 6/0).**

## Symptom

`cargo run --release` (or `scripts/cargo_runner.sh`) on an Apple Silicon host, default accelerator HVF: the kernel's boot self-tests stop after
`OK: fully unmapped source returns EFAULT` and QEMU dies with `Assertion failed: (isv), function hvf_handle_exception, file hvf.c, line 2437`
(exit 134). The runner already warns about this class of abort in its banner. `HVF=0` (TCG) ran the same tests without complaint, which is why
it went unnoticed.

## Cause

`src/tests.rs::test_safe_user_access_fault` step 3 ("fault part way through a long copy") started 1 KB below `kernel_va_end()` and copied 8 KB
across it, expecting the kernel identity map to end there. For a 1 GiB guest `kernel_va_end()` is `0x80000000`, but the boot map reaches further
than the guest's RAM, so the stretch past it is **mapped in the guest's page tables and has no RAM behind it**. A load there is a stage-2
translation fault: the guest never sees it (no EL1 data abort to handle), the hypervisor does, and QEMU's HVF backend asserts because the
faulting `ldp` carries no instruction syndrome (`ISV=0`) and it cannot emulate it as MMIO. Under TCG the same read returns zeros without any
fault, so the test could not have been checking what it said there either.

## Fix

The test builds its own cliff, which is a stage-1 fact and so the same on TCG, HVF and KVM: a fresh `UserAddressSpace` with one real page
mapped at `0x1000_0000` and its neighbour unmapped, activated with interrupts masked (a scheduler tick would swap `TTBR0` under the copy), a
copy that starts 1 KB below the page's end, then `deactivate()`. It now also checks the bytes before the cliff arrived, which the old version
did not. The fully-unmapped case (step 2) was never a problem: it reads a VA the guest's tables do not map at all.

## Verification

HVF boot of the test-enabled build, `INSTANCE=9`: all tests run to completion, the guest answers ssh, 166 `Result: PASS`, no `FAIL`.

## Open

Other tests may touch mapped-but-unbacked addresses; none did in this boot. A guest with more RAM maps differently, and only 1 GiB was tried.
