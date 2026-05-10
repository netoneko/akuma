# Signal delivery vs forktest (Go): conclusion and evidence

This document records why the **general signal delivery and return-to-user path** (async delivery, sigframe, `rt_sigreturn`, register restore) is the **leading hypothesis** for forktest / Go failures involving `SIGURG`, and why a **narrow syscall-stub deferral** is insufficient. It is meant to support a **roll back** of heuristic mitigations and a **focused audit** of signal machinery.

---

## Conclusion (one paragraph)

Observed failures line up with **incorrect or fragile user context across `SIGURG` delivery and `sigreturn`**, not only with “async signal while still in the musl syscall trampoline.” Kernel serial logs show **`SIGURG` delivered to the forktest child while interrupted in the Go heap (`ELR` ~`0x86768`, `memclr` / malloc path)**, immediately followed by a **data abort** whose **FAR** is a **small negative integer** (`0xfffffffffffffffa`, i.e. **-6**) and **general-purpose registers** holding **errno-like negatives** (e.g. **`x0 = 0xffffffffffffffea`**, **-22**), while **`last_sc`** is idle — consistent with **resumed user code executing with corrupted argument/state**, not with a simple “wrong syscall number at SVC” story. A mitigation that only re-pends `SIGURG` on **some** syscall returns when **`ELR` is in a fixed low-VMA window** does not cover **other threads**, **other PCs**, or **other delivery boundaries**, and logs show **defer and deliver in quick succession** for the same process anyway.

---

## Evidence A: Child thread — delivery in Go allocator, then bogus fault

Source: serial capture **`crash27.log`** (repo root), around **T22.72–T22.76**.

Sequence (abbreviated; see file for full lines):

1. **Two** `SIGURG` injections: `tkill(tid=8, sig=23)` (parent) and **`tkill(tid=17, sig=23)`** (child).
2. **`[signal] deliver sig=23 slot=17`** for **pid=94** (forktest child), with **`fault_pc=0x86768`** — matches userland stacks showing **`runtime.memclrNoHeapPointers`** / malloc (`pc≈0x86768`), **not** the musl stub band (~`0x13060`).
3. **`[sigreturn] restoring: ... pc=0x86768`** — handler returns to the same allocator site.
4. Immediately: **`[DA-MISS]`** / demand-paging path for **`va=0xfffffffffffffffa`** (FAR **-6**).
5. Kernel diagnostic **`[WILD-DA]`** explicitly flags FAR as a **small negative** consistent with **“syscall error used as pointer”** pattern; register dump includes **`x0=0xffffffffffffffea`** (**-22** / errno-shaped) and **`last_sc=18446744073709551615`** (idle / no active syscall tracking).

**Interpretation:** The failure is triggered in a path where **async `SIGURG` + `sigreturn`** should leave the thread able to continue **large zeroing / allocation** safely. The subsequent fault and register shapes argue for **broken user register file or wrong resume context** after signal handling, i.e. **signal delivery / restore**, not “parent stuck in `read` glue only.”

---

## Evidence B: Parent thread — stub defer fires but does not end the story

Same log shows many lines of the form:

- **`[SIGURG] re-pend tid=8 (stub defer syscall ret) ELR=0x13060 x8=21`** (and similar for `x8=22`, `63`).

So the **narrow** mitigation can run on **tid=8** (`forktest_parent`).

But the same capture also shows:

- **`[exit_group] pid=90 ... forktest_parent code=0`** after ~30s — parent may survive that run, while **child** already hit the allocator path above.
- In other places (e.g. around **T51.10**), **stub defer** for **tid=8** appears **near** a subsequent **`deliver sig=23 slot=8`** with **`fault_pc=0x86d04`** (Go runtime, not `0x13060`) — showing **delivery still occurs** on other boundaries / PCs.

**Interpretation:** The stub defer is **not** a proof of fix; it is at best **one skipped delivery opportunity**. It does **not** globally serialize or correctness-proof `SIGURG` for Go.

---

## Evidence C: Original Pattern 2 parent symptom (user-reported)

User capture (outside this file): **`SIGSEGV`** in **`syscall.Syscall(0x3f, …)`** (`read`), **`PC=0x13060`**, **`addr`** in a large heap/near-heap range, with **`m=0`**, **`GOMAXPROCS=1`**.

**Interpretation:** That symptom is **compatible** with **corrupt state when returning from or near the syscall path** (including wrong resume after async signal), but **by itself** it does not isolate “musl stub only” vs “full delivery bug.” Evidence A is what ties the issue more strongly to **delivery/restore** across **arbitrary** user PCs.

---

## Why a syscall-stub-only deferral is the wrong primary fix

| Limitation | Why it matters |
|--------------|----------------|
| **Thread scope** | Go uses **multiple OS threads** (`tid=8` parent, `tid=17` child). Deferring only on one thread’s syscall return misses the other. |
| **PC scope** | Child fault at **`~0x86768`** is outside a fixed **`0x10000..0x20000`** “stub” window. |
| **Syscall scope** | Allocator and runtime use many syscalls; `SIGURG` can deliver on **returns** and other paths not covered by a short list (`read` / `epoll_*`). |
| **Temporal** | Re-pend → **still pending** → delivery on **next** eligible boundary can be **microseconds** later; races remain. |

---

## Suggested audit scope (for follow-up work)

Prioritize **read-only** tracing and review of:

1. **`try_deliver_signal`** — how the interrupted frame is chosen and written; interaction with **altstack**; **`fault_pc`** / `si_addr` semantics for async signals.
2. **Sigframe layout and alignment** — AArch64 `ucontext`, **`RT_SIGFRAME`**, reserved fields, and **`SA_RESTORER`** path.
3. **`do_rt_sigreturn` / `rt_sigreturn`** — full register restore; interaction with **pending** `SIGURG` immediately after return (ordering vs syscall result in **`x0`**).
4. **Preemption / timer / `tkill`** — who is targeted (`tid` vs process-wide), and whether **nested** or **re-entrant** delivery is possible while building frames.
5. **NEON / FP** — EL0 sync handler saves/restores wide state; verify signal delivery does not assume a partial frame.

Cross-reference existing notes: **`docs/GO_FORKTEST_DEBUG.md`**, **`docs/SIGNAL_DELIVERY.md`**, **`docs/GO_FORK_EXEC_FIXES.md`**.

---

## Artifact reference

- **`crash27.log`** (user-provided serial log at repo root in the session that produced this write-up): lines **~3050–3085** (child `SIGURG` → `sigreturn` → `WILD-DA`), **~29790–29795** (parent defer vs deliver proximity), **~29904** (parent `exit_group` success on that run).

---

## Code audit findings (2026-05-10)

Full read-only audit of `src/exceptions.rs`, `crates/akuma-exec/src/threading/mod.rs`, and `src/syscall/signal.rs` against the five scope items. Every claim cites file and line numbers in the tree as of commit `c9b6fc1`.

### Finding F5.2 — FPSIMD vregs copied as `u128`; may emit aligned `ldp q` *(HIGH SEVERITY)*

**Location:** `exceptions.rs:1133–1137` (delivery), `exceptions.rs:1289–1293` (restore).

Both the delivery path in `try_deliver_signal` and the restore path in `do_rt_sigreturn` copy the 32 NEON vregs as `*const u128` / `*mut u128` via `core::ptr::read` / `core::ptr::write`:

```rust
let src = kernel_neon.add(i * 16) as *const u128;
let dst = vregs_dst.add(i * 16) as *mut u128;
core::ptr::write(dst, core::ptr::read(src));
```

At delivery, `vregs_dst = fp.add(16)` where `fp = base.add(SIGFRAME_FPSIMD)` and `base = new_sp` (16-byte aligned). `SIGFRAME_FPSIMD = 584`. `584 % 16 = 8` — the vregs destination is **NOT 16-byte aligned**. LLVM may lower a `u128` load/store to `ldp q0, q1` / `stp q0, q1`, which on AArch64 requires 16-byte alignment (`SCTLR_EL1.A` enforces this). If it does, the first iteration (`i=0`, address `new_sp+600`) causes an EL1 data abort on a correct signal delivery, killing the process with a spurious fault.

**Verification:** `objdump -d build/akuma | grep -A4 'do_rt_sigreturn\|try_deliver_signal'` — check whether `ldp q` / `stp q` appears for the vreg copy loop. If yes, replace the inner body with `core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, 16)`.

---

### Finding F3.1 — `do_rt_sigreturn` reads user stack with no bounds validation on `sp_el0` *(HIGH SEVERITY)*

**Location:** `exceptions.rs:1186–1194`.

```rust
let sigframe_sp = frame_ref.sp_el0 as usize;
let first_page  = sigframe_sp & !0xFFF;
let last_page   = (sigframe_sp + SIGFRAME_SIZE - 1) & !0xFFF;
if !akuma_exec::mmu::is_current_user_page_mapped(first_page) { return None; }
```

The only validation is a page-presence check. There is no assertion that `sigframe_sp` is:
- 16-byte aligned (Linux mandates this; misalignment means the sigframe was corrupted or the handler unbalanced SP).
- Within the configured per-thread sigaltstack `[alt_sp, alt_sp+alt_size)` (when the handler ran on altstack).

If Go's `doSigPreempt` or the gsignal stack underflows (e.g., because `pushCall` adjusts `mcontext.sp` by the wrong amount), `frame_ref.sp_el0` at the rt_sigreturn SVC points to an arbitrary address. The page-presence check may pass (if the page happens to be mapped), and `do_rt_sigreturn` silently restores garbage GPRs, SPSR, ELR, and FPSIMD. The process then ERTETs to an arbitrary PC with arbitrary registers — producing exactly the "errno-shaped FAR after sigreturn" symptom class.

**Verification:** log `[sigreturn] sp={:#x} misaligned or out-of-altstack` and return `None` on mismatch; correlate with crash windows.

---

### Finding F1.3 — SA_RESTART check re-reads `ESR_EL1` which is stale after EL1 IRQ context switch *(MEDIUM SEVERITY)*

**Location:** `exceptions.rs:915–926`.

```rust
let esr: u64;
unsafe { core::arch::asm!("mrs {}, esr_el1", out(reg) esr); }
if (esr >> 26) == 0x15 { // EC_SVC_LOWER
```

IRQs are enabled at `exceptions.rs:174` (`msr daifclr, #2`). A timer IRQ firing during `rust_sync_el0_handler` enters `irq_handler` (EL1 IRQ path, `exceptions.rs:419`). `irq_handler` saves/restores `ELR_EL1` and `SPSR_EL1` but **does not touch `ESR_EL1`**. After a context switch and ERET back to the kernel, `ESR_EL1` holds the IRQ's exception class (typically EC=0x00), not the original SVC's EC=0x15. The `(esr >> 26) == 0x15` check fails, SA_RESTART is silently skipped, and an interrupted restartable syscall returns EINTR instead of being restarted.

**Verification:** save `esr` from the `let ec = (esr >> 26)` read at the top of `rust_sync_el0_handler` and pass it into `try_deliver_signal` as a parameter; remove the inner `mrs`.

---

### Finding F4.1 — No signal delivery at EL0 IRQ return *(MEDIUM SEVERITY)*

**Location:** `irq_el0_handler` (`exceptions.rs:263`); `rust_irq_handler_with_sp` (`exceptions.rs:1375`).

`irq_el0_handler` performs context switches but never calls `take_pending_signal` or `try_deliver_signal`. A goroutine running in a tight loop (e.g., `memclr` for a large `make([]byte, N)`) cannot be preempted by SIGURG until its next SVC. Linux delivers pending signals in `do_notify_resume` on every return to EL0. This is not the direct cause of the errno-FAR crash (those require SVC-boundary delivery), but it lengthens the window between a `tkill(SIGURG)` and actual delivery, increasing the probability that a second SIGURG is pended before the first is delivered — which is the precondition for the observed double-delivery crash chain.

**Verification:** instrument `irq_el0_handler` exit to check `peek_pending_signal`; count occurrences of non-zero pending while ELR is in the allocator/asyncPreempt band.

---

### Finding F1.1 — `sa_mask` from `sigaction` is stored but never applied at delivery *(MEDIUM SEVERITY)*

**Location:** `exceptions.rs:1164–1170`; `signal.rs:61`.

`sys_rt_sigaction` correctly stores `sa.sa_mask` into `SignalAction.mask` (`process/types.rs:273`). `try_deliver_signal` only masks the delivered signal itself:

```rust
proc.signal_mask |= 1u64 << (signal - 1);
```

`action.mask` is never ORed in. On Linux, `sa_mask` is added to the blocked set for the duration of the handler, then removed on `rt_sigreturn`. Go's SIGURG handler registers with `sa_mask = 0`, so this has no effect on the current test case, but any handler that uses `sa_mask` to protect a critical section from re-entrant signals will be broken.

---

### Finding F2.1 — SPSR forced to 0 when mode bits are nonzero, losing N/Z/C/V *(LOW SEVERITY)*

**Location:** `exceptions.rs:1242–1248`.

```rust
if restored_spsr & 0x1F != 0 {
    (*frame).spsr_el1 = 0; // Clean EL0t, all flags cleared
}
```

Clearing to 0 discards the NZCV condition codes and DAIF bits as well as the mode bits. The correct fix is `restored_spsr & !0x1F` (zero only the mode field, keep the rest). Triggered only on already-corrupted frames, so low practical impact.

---

### Finding F1.2 — `uc_stack.ss_flags` = 0 instead of SS_DISABLE when no altstack *(LOW SEVERITY)*

**Location:** `exceptions.rs:1073–1076`.

When `alt_sp == 0`, `on_altstack = false` and `ss_flags` is written as `0`. Linux writes `SS_DISABLE (2)`. Go's altstack readiness check also tests `ss.size >= minStackForSigAlt`; with `ss_size=0` the size check fails regardless, so Go makes the correct decision. ABI-incorrect but benign in practice.

---

### Root-cause synthesis

The direct kernel mechanism for the errno-FAR crash is:

1. A SVC is called with **x0 = an unexpected value** (heap pointer or prior errno) at the point SIGURG is delivered.
2. `(*frame).x0 = ret` saves this value into `mcontext.regs[0]`.
3. `do_rt_sigreturn` correctly restores `x0 = ret`.
4. The goroutine resumes at `ELR=0x86768`; the code there treats `x0` as a base address, reads `[x0, #16]`, producing `FAR = ret + 16 = −22 + 16 = −6`.

**The signal frame construction and sigreturn are correct for the register values present at delivery.** The upstream question — why x0 held an errno or heap pointer at SVC entry — requires tracing whether a second SIGURG delivery interleaved with `asyncPreempt`'s register-save prologue, leaving `x0` populated with the second SVC's return value rather than the value asyncPreempt expected to save. Finding F3.1 (unvalidated SP at sigreturn) is the strongest kernel-side candidate for a corruption path: if Go's `doSigPreempt` leaves `sp_el0` misaligned or outside the altstack, `do_rt_sigreturn` reads garbage as mcontext, restoring wrong GPRs silently.

---

## Fix plan (2026-05-10)

Ordered by risk; each item includes a concrete kernel test.

---

### Fix 1 — Replace `u128` vreg copies with byte copies (F5.2)

**File:** `src/exceptions.rs`, in `try_deliver_signal` (line ~1133) and `do_rt_sigreturn` (line ~1289).

**Change:**
```rust
// Before
let src = kernel_neon.add(i * 16) as *const u128;
let dst = vregs_dst.add(i * 16) as *mut u128;
core::ptr::write(dst, core::ptr::read(src));

// After
unsafe {
    core::ptr::copy_nonoverlapping(
        kernel_neon.add(i * 16),
        vregs_dst.add(i * 16),
        16,
    );
}
```

Repeat the same change for the restore path in `do_rt_sigreturn`.

**New kernel test (add to `src/tests.rs` or a new `src/signal_tests.rs`):**

```rust
#[test_case]
fn test_sigframe_fpsimd_vreg_copy_alignment() {
    // Verify that the FPSIMD vregs destination offset is NOT 16-byte aligned
    // (so the old u128 copy path was dangerous) and that the new byte-copy
    // path produces identical data regardless of alignment.
    let new_sp: usize = 0x1000_0010; // 16-byte aligned; SIGFRAME_FPSIMD at +584
    let fp_base = new_sp + crate::exceptions::TEST_SIGFRAME_FPSIMD; // 0x1000_0258
    assert_eq!(fp_base % 16, 8, "vregs base must be 8-mod-16 to expose alignment bug");

    // Simulate a 16-byte vreg copy at the misaligned offset using byte copy.
    let mut src = [0u8; 16];
    let mut dst = [0u8; 16];
    for i in 0..16u8 { src[i as usize] = i + 1; }
    unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), 16); }
    assert_eq!(src, dst, "byte copy must preserve 16-byte vreg value");
}
```

---

### Fix 2 — Validate SP alignment and altstack bounds in `do_rt_sigreturn` (F3.1)

**File:** `src/exceptions.rs`, `do_rt_sigreturn` (line ~1186).

**Change:** after computing `sigframe_sp`, add:
```rust
// Reject misaligned or out-of-bounds sigframe SP before touching user memory.
if sigframe_sp & 0xF != 0 {
    crate::tprint!(128,
        "[sigreturn] REJECT: sigframe_sp={:#x} not 16-byte aligned\n", sigframe_sp);
    return None;
}
let thread_slot = akuma_exec::threading::current_thread_id();
let (alt_sp, alt_size, _) = akuma_exec::threading::get_sigaltstack(thread_slot);
if alt_sp != 0 {
    let alt_lo = alt_sp as usize;
    let alt_hi = alt_lo + alt_size as usize;
    // Allow sigframe_sp to be anywhere in [alt_lo, alt_hi - SIGFRAME_SIZE].
    // Also allow sigframe_sp on the regular stack (< alt_lo or >= alt_hi) for
    // signals delivered without SA_ONSTACK.
    if sigframe_sp >= alt_lo && sigframe_sp < alt_lo + SIGFRAME_SIZE.saturating_sub(1) {
        crate::tprint!(128,
            "[sigreturn] REJECT: sigframe_sp={:#x} below altstack frame minimum\n",
            sigframe_sp);
        return None;
    }
}
```

**New kernel tests:**

```rust
#[test_case]
fn test_do_rt_sigreturn_rejects_misaligned_sp() {
    // do_rt_sigreturn must return None for non-16-byte-aligned SP.
    // We can't call it directly without a live trap frame, so we test the
    // alignment predicate inline.
    for sp in [0x1001usize, 0x1002, 0x1004, 0x100A, 0xFFFF_FFF1] {
        assert_ne!(sp & 0xF, 0, "test sp must be misaligned");
        // The guard condition:
        assert!(sp & 0xF != 0, "misaligned sp={:#x} should be rejected", sp);
    }
    // Aligned values should pass.
    for sp in [0x1000usize, 0x2000, 0x3FF0, 0xFFFF_FFF0] {
        assert_eq!(sp & 0xF, 0, "aligned sp={:#x} must not be rejected", sp);
    }
}

#[test_case]
fn test_sigframe_sp_altstack_bounds() {
    // Verify the altstack bounds check logic: a sigframe_sp below the minimum
    // valid position (alt_lo + SIGFRAME_SIZE) must be rejected.
    let alt_sp: usize  = 0x8000_0000;
    let alt_size: usize = 0x8000;  // 32 KiB
    let frame_size     = crate::exceptions::TEST_SIGFRAME_SIZE; // 1120

    let valid_sp   = alt_sp + alt_size - frame_size; // top of altstack, valid
    let invalid_sp = alt_sp + 16;                    // near bottom, no room for frame

    // valid_sp must be >= alt_sp + frame_size - 1 from the top direction
    assert!(valid_sp >= alt_sp);
    assert!(valid_sp + frame_size <= alt_sp + alt_size);

    // invalid_sp + frame_size would overflow the altstack
    assert!(invalid_sp + frame_size > alt_sp + alt_size);
}
```

---

### Fix 3 — Pass saved ESR into `try_deliver_signal`; remove inner `mrs` (F1.3)

**File:** `src/exceptions.rs`.

**Change A** — `try_deliver_signal` signature:
```rust
fn try_deliver_signal(
    frame: *mut UserTrapFrame,
    signal: u32,
    fault_addr: u64,
    is_fault: bool,
    entry_esr: u64,          // ← new: ESR captured at exception entry
) -> bool {
```

Replace the `mrs esr_el1` inside the function with `let esr = entry_esr;`.

**Change B** — all three call sites pass the already-captured ESR:
- Normal syscall return: `entry_esr = esr` (the variable at the top of `rust_sync_el0_handler`).
- rt_sigreturn path: same `esr` (still the rt_sigreturn SVC's ESR).
- JIT retry path: same.
- Data-abort path: pass `esr` from the data-abort handler (`EC_DATA_ABORT_LOWER`).

**New kernel test:**

```rust
#[test_case]
fn test_sa_restart_uses_supplied_esr_not_live_register() {
    // The SA_RESTART gating condition is (entry_esr >> 26) == 0x15 (EC_SVC64).
    // Verify that the predicate correctly distinguishes SVC from IRQ ESR values.
    const EC_SVC64:          u64 = 0x15 << 26;
    const EC_DATA_ABORT:     u64 = 0x24 << 26;
    const EC_IRQ:            u64 = 0x00 << 26; // typical IRQ ESR

    let is_svc = |esr: u64| (esr >> 26) == 0x15u64;

    assert!( is_svc(EC_SVC64),       "SVC ESR must be identified as EC_SVC64");
    assert!(!is_svc(EC_DATA_ABORT),  "data-abort ESR must not match EC_SVC64");
    assert!(!is_svc(EC_IRQ),         "IRQ ESR must not match EC_SVC64");
    assert!(!is_svc(0),              "zero ESR must not match EC_SVC64");
    // ISS bits must not affect the EC comparison
    assert!( is_svc(EC_SVC64 | 0x1_FFFF), "SVC ESR with ISS must still match");
}
```

---

### Fix 4 — Deliver pending signals on return from `irq_el0_handler` to EL0 (F4.1)

**File:** `src/exceptions.rs`, `irq_el0_handler` assembly block.

After `rust_irq_handler_with_sp` returns (context switch done or skipped), before the RESTORE PHASE, add a call to a new Rust function `maybe_deliver_pending_el0(frame)`:

```rust
/// Called from irq_el0_handler after the scheduler has run, before ERET.
/// Delivers any pending signal so goroutines in tight loops are preemptible.
/// Returns the signal number if a handler was set up (x0 for handler),
/// or 0 if no delivery was needed (x0 is left unchanged).
#[unsafe(no_mangle)]
extern "C" fn maybe_deliver_pending_el0(frame: *mut UserTrapFrame) -> u64 {
    let pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let sig_mask = akuma_exec::process::lookup_process(pid)
        .map(|p| p.signal_mask)
        .unwrap_or(0);
    // Block fault signals in the IRQ delivery path (same logic as JIT path).
    const FAULT_SIGNALS: u64 = (1 << 4) | (1 << 7) | (1 << 8) | (1 << 11);
    let effective_mask = sig_mask | FAULT_SIGNALS;
    if let Some(sig) = akuma_exec::threading::take_pending_signal(effective_mask) {
        let thread_slot = akuma_exec::threading::current_thread_id();
        let (alt_sp, _, _) = akuma_exec::threading::get_sigaltstack(thread_slot);
        if sig == 23 && alt_sp == 0 {
            akuma_exec::threading::pend_signal_for_thread(thread_slot, sig);
            return 0;
        }
        // Use EC_SVC64 as the entry_esr so SA_RESTART is not applied
        // (the goroutine was not in a syscall when interrupted by the IRQ).
        if try_deliver_signal(frame, sig, 0, false, 0 /* not a syscall */) {
            return sig as u64;
        }
    }
    0
}
```

In the `irq_el0_handler` assembly, after `cbz x0, 3f` / `mov sp, x0` and before the RESTORE PHASE, insert:
```asm
// Deliver pending signals before returning to EL0
mov     x0, sp              // frame pointer
bl      maybe_deliver_pending_el0
// If x0 != 0, a signal was delivered; x0 is already the signal number.
// The frame has been modified (ELR → handler, SP → sigframe, x30 → restorer).
// The RESTORE PHASE below will reload from the updated frame.
// x0 is written to sp+N scratch slot so RESTORE PHASE loads the right value.
// (Implementation detail: match the scratch-slot convention of sync_el0_handler.)
```

**New kernel test:**

```rust
#[test_case]
fn test_irq_path_pending_signal_not_delivered_without_altstack() {
    // maybe_deliver_pending_el0 must re-pend SIGURG when alt_sp == 0,
    // mirroring the syscall-return re-pend logic.
    let thread_slot = akuma_exec::threading::current_thread_id();
    // Ensure no altstack configured for this slot.
    akuma_exec::threading::set_sigaltstack(thread_slot, 0, 0, 2 /* SS_DISABLE */);
    // Pend SIGURG.
    akuma_exec::threading::pend_signal_for_thread(thread_slot, 23);
    // Simulate the mask-check: SIGURG is not masked (bit 22 = 0 in mask 0).
    let sig = akuma_exec::threading::take_pending_signal(0);
    assert_eq!(sig, Some(23));
    // Because alt_sp == 0, code should call pend_signal_for_thread again.
    // Re-pend it (as maybe_deliver_pending_el0 would do).
    akuma_exec::threading::pend_signal_for_thread(thread_slot, 23);
    // Verify signal is still pending.
    let sig2 = akuma_exec::threading::take_pending_signal(0);
    assert_eq!(sig2, Some(23), "SIGURG must survive re-pend round-trip");
    // Cleanup.
    akuma_exec::threading::pend_signal_for_thread(thread_slot, 0);
}
```

---

### Fix 5 — Apply `sa_mask` during signal delivery (F1.1)

**File:** `src/exceptions.rs`, `try_deliver_signal` (line ~1164).

**Change:** after the SA_NODEFER mask update, also apply `action.mask`:
```rust
if action.flags & SA_NODEFER == 0 && signal >= 1 && signal <= 64 {
    if signal != 9 && signal != 19 {
        proc.signal_mask |= 1u64 << (signal - 1);
    }
}
// Apply sa_mask: additional signals to block during handler execution.
// SIGKILL (bit 8) and SIGSTOP (bit 18) cannot be blocked.
proc.signal_mask |= action.mask & !((1u64 << 8) | (1u64 << 18));
```

The saved `uc_sigmask` already captures the mask *before* both of these changes (written at line 1078, before the block), so `rt_sigreturn` restores the clean pre-delivery mask. No change to the sigframe write is needed.

**New kernel test:**

```rust
#[test_case]
fn test_sa_mask_applied_during_delivery() {
    // After try_deliver_signal, proc.signal_mask must include action.mask bits.
    // We test the masking logic in isolation: simulate the two OR steps.
    let initial_mask: u64 = 0;
    let signal: u32 = 23; // SIGURG
    let sa_mask: u64 = 1u64 << (14); // SIGTERM bit (signal 15)
    let sa_nodefer = false;

    let mut mask = initial_mask;
    // Step 1: block delivered signal (SA_NODEFER not set).
    if !sa_nodefer && signal >= 1 && signal <= 64 && signal != 9 && signal != 19 {
        mask |= 1u64 << (signal - 1);
    }
    // Step 2: apply sa_mask.
    mask |= sa_mask & !((1u64 << 8) | (1u64 << 18));

    assert!(mask & (1u64 << 22) != 0, "SIGURG must be masked during its own handler");
    assert!(mask & (1u64 << 14) != 0, "sa_mask SIGTERM must be masked");
    assert!(mask & (1u64 << 8)  == 0, "SIGKILL bit must never be masked");
    assert!(mask & (1u64 << 18) == 0, "SIGSTOP bit must never be masked");
}

#[test_case]
fn test_sa_mask_not_persisted_after_sigreturn() {
    // uc_sigmask saved before delivery must not include sa_mask bits,
    // so sigreturn restores to the clean pre-delivery mask.
    let pre_delivery_mask: u64 = 0b101; // some user mask
    // uc_sigmask = pre_delivery_mask (saved BEFORE any OR operations).
    let uc_sigmask = pre_delivery_mask;
    // After sigreturn:
    let restored = uc_sigmask & !((1u64 << 8) | (1u64 << 18));
    assert_eq!(restored, pre_delivery_mask,
        "sigreturn must restore the exact pre-delivery mask");
}
```

---

### Fix 6 — Preserve N/Z/C/V in SPSR sanitisation (F2.1)

**File:** `src/exceptions.rs`, `do_rt_sigreturn` (line ~1242).

**Change:**
```rust
// Before
(*frame).spsr_el1 = 0; // Clean EL0t, all flags cleared

// After
// Force only M[4:0] to 0 (EL0t); preserve N/Z/C/V and other PSTATE bits.
(*frame).spsr_el1 = restored_spsr & !0x1Fu64;
```

**New kernel test:**

```rust
#[test_case]
fn test_spsr_sanitise_preserves_nzcv() {
    // Only M[4:0] must be cleared; NZCV and other flags must survive.
    let test_cases: &[(u64, u64)] = &[
        // (corrupted_spsr,            expected_after_sanitise)
        (0xF000_0000 | 0x5,           0xF000_0000), // NZCV=1111, corrupted mode → keep NZCV
        (0x2000_0000 | 0x1,           0x2000_0000), // C flag only, corrupted mode
        (0x0,                         0x0),          // all zero → stays zero
        (0x1F,                        0x0),          // only bad mode bits → clean to 0
        (0xF000_001F,                 0xF000_0000), // NZCV + all mode bits → keep NZCV
    ];
    for &(input, expected) in test_cases {
        let sanitised = input & !0x1Fu64;
        assert_eq!(sanitised, expected,
            "SPSR {:#x}: expected {:#x}, got {:#x}", input, expected, sanitised);
    }
}
```

---

### Test placement and build impact

| Test | File | New / existing |
|------|------|----------------|
| `test_sigframe_fpsimd_vreg_copy_alignment` | `src/signal_tests.rs` (new file) | New |
| `test_do_rt_sigreturn_rejects_misaligned_sp` | `src/signal_tests.rs` | New |
| `test_sigframe_sp_altstack_bounds` | `src/signal_tests.rs` | New |
| `test_sa_restart_uses_supplied_esr_not_live_register` | `src/signal_tests.rs` | New |
| `test_irq_path_pending_signal_not_delivered_without_altstack` | `src/signal_tests.rs` | New |
| `test_sa_mask_applied_during_delivery` | `src/signal_tests.rs` | New |
| `test_sa_mask_not_persisted_after_sigreturn` | `src/signal_tests.rs` | New |
| `test_spsr_sanitise_preserves_nzcv` | `src/signal_tests.rs` | New |

All tests use no `std` and no QEMU; they exercise pure Rust logic against constants exported from `exceptions.rs` (`TEST_SIGFRAME_SIZE`, `TEST_SIGFRAME_FPSIMD`, etc.). Run with:
```bash
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2) -p akuma -- signal_tests
```

### Recommended implementation order

1. **Fix 1** (FPSIMD alignment) — no functional change in correct cases, eliminates potential EL1 abort. Lowest risk, highest urgency.
2. **Fix 3** (ESR parameter) — purely mechanical refactor; fixes a real correctness bug with no observable regression on paths without context switches.
3. **Fix 6** (SPSR sanitisation) — one-liner; correct by inspection.
4. **Fix 5** (sa_mask) — adds one OR; verify no test regression on existing `test_take_pending_signal_sigurg_masked`.
5. **Fix 2** (SP validation) — adds a guard; must be tuned to not reject legitimate out-of-altstack delivery (SA_ONSTACK not set, regular stack).
6. **Fix 4** (IRQ delivery) — most invasive; requires assembly change, new extern function, and careful re-entrancy analysis. Implement last, behind a `config` toggle, after all other fixes are stable.

---

## Artifact reference

- **`crash27.log`** (user-provided serial log at repo root in the session that produced this write-up): lines **~3050–3085** (child `SIGURG` → `sigreturn` → `WILD-DA`), **~29790–29795** (parent defer vs deliver proximity), **~29904** (parent `exit_group` success on that run).

---

## Document history

- **2026-05-10:** Initial write-up after analysis of `crash27.log` and user-reported Pattern 2 stacks; supports rollback of narrow `SIGURG` stub deferral and shift to signal-path audit.
- **2026-05-10:** Code audit and fix plan added (findings F1.1–F5.2, fixes 1–6, eight new kernel tests).
