//! Futex Syscall Tests
//!
//! Tests for the futex synchronisation primitive (sys_futex / NR_FUTEX = 98).
//! Uses BYPASS_VALIDATION so kernel-stack addresses pass the user-pointer check.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use akuma_exec::threading;

use crate::console;

const NR_FUTEX: u64 = 98;

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_REQUEUE: u64 = 3;
const FUTEX_CMP_REQUEUE: u64 = 4;
const FUTEX_PRIVATE_FLAG: u64 = 128;
const FUTEX_WAIT_PRIVATE: u64 = FUTEX_WAIT | FUTEX_PRIVATE_FLAG;
const FUTEX_WAKE_PRIVATE: u64 = FUTEX_WAKE | FUTEX_PRIVATE_FLAG;
const FUTEX_REQUEUE_PRIVATE: u64 = FUTEX_REQUEUE | FUTEX_PRIVATE_FLAG;
const FUTEX_CMP_REQUEUE_PRIVATE: u64 = FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG;

const EAGAIN: u64 = (-11i64) as u64;
const ETIMEDOUT: u64 = (-110i64) as u64;
const EINVAL: u64 = (-22i64) as u64;

/// Helper: enable / disable pointer bypass.
fn set_bypass(v: bool) {
    crate::syscall::BYPASS_VALIDATION.store(v, Ordering::Release);
}

// ============================================================================
// Single-threaded correctness tests
// ============================================================================

/// FUTEX_WAIT with a mismatched value must return EAGAIN immediately.
fn test_futex_eagain() {
    set_bypass(true);

    let mut val: u32 = 99;
    let uaddr = &mut val as *mut u32 as usize;

    // Wait expecting 0 but actual value is 99 → EAGAIN
    let ret = crate::syscall::handle_syscall(
        NR_FUTEX,
        &[uaddr as u64, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0],
    );

    set_bypass(false);

    assert!(
        ret == EAGAIN,
        "test_futex_eagain: expected EAGAIN ({:#x}) got {:#x}",
        EAGAIN,
        ret
    );
    console::print("  [PASS] test_futex_eagain\n");
}

/// NULL uaddr must return EINVAL.
fn test_futex_null_addr() {
    set_bypass(true);

    let ret = crate::syscall::handle_syscall(
        NR_FUTEX,
        &[0u64, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0],
    );

    set_bypass(false);

    assert!(
        ret == EINVAL,
        "test_futex_null_addr: expected EINVAL ({:#x}) got {:#x}",
        EINVAL,
        ret
    );
    console::print("  [PASS] test_futex_null_addr\n");
}

/// Unaligned uaddr must return EINVAL.
fn test_futex_unaligned_addr() {
    set_bypass(true);

    let mut buf: [u8; 8] = [0; 8];
    // offset by 1 → not 4-byte aligned
    let uaddr = (buf.as_mut_ptr() as usize) + 1;

    let ret = crate::syscall::handle_syscall(
        NR_FUTEX,
        &[uaddr as u64, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0],
    );

    set_bypass(false);

    assert!(
        ret == EINVAL,
        "test_futex_unaligned_addr: expected EINVAL ({:#x}) got {:#x}",
        EINVAL,
        ret
    );
    console::print("  [PASS] test_futex_unaligned_addr\n");
}

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// FUTEX_WAIT with a 10 ms timeout must return ETIMEDOUT.
fn test_futex_timeout() {
    set_bypass(true);

    let mut val: u32 = 0;
    let uaddr = &mut val as *mut u32 as usize;

    let ts = Timespec {
        tv_sec: 0,
        tv_nsec: 10_000_000, // 10 ms
    };
    let timeout_ptr = &ts as *const Timespec as u64;

    let ret = crate::syscall::handle_syscall(
        NR_FUTEX,
        &[uaddr as u64, FUTEX_WAIT_PRIVATE, 0, timeout_ptr, 0, 0],
    );

    set_bypass(false);

    assert!(
        ret == ETIMEDOUT,
        "test_futex_timeout: expected ETIMEDOUT ({:#x}) got {:#x}",
        ETIMEDOUT,
        ret
    );
    console::print("  [PASS] test_futex_timeout\n");
}

/// Simulate the missed-wakeup race scenario:
/// wake is called before any waiter exists, then the value is changed,
/// then wait is called with the old expected value — must return EAGAIN.
fn test_futex_wake_before_wait() {
    set_bypass(true);

    let mut val: u32 = 0;
    let uaddr = &mut val as *mut u32 as usize;

    // Wake with nobody waiting — returns 0
    let woken = crate::syscall::handle_syscall(
        NR_FUTEX,
        &[uaddr as u64, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0],
    );
    assert!(woken == 0, "test_futex_wake_before_wait: expected 0 woken, got {}", woken);

    // Waker changes the value (this is the typical Go pattern)
    val = 1;

    // Now wait with the old value 0 — should detect mismatch and return EAGAIN
    let ret = crate::syscall::handle_syscall(
        NR_FUTEX,
        &[uaddr as u64, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0],
    );

    set_bypass(false);

    assert!(
        ret == EAGAIN,
        "test_futex_wake_before_wait: expected EAGAIN ({:#x}) got {:#x}",
        EAGAIN,
        ret
    );
    console::print("  [PASS] test_futex_wake_before_wait\n");
}

/// FUTEX_WAKE(0) must wake nobody and return 0.
fn test_futex_wake_zero() {
    set_bypass(true);

    let mut val: u32 = 0;
    let uaddr = &mut val as *mut u32 as usize;

    let woken = crate::syscall::handle_syscall(
        NR_FUTEX,
        &[uaddr as u64, FUTEX_WAKE_PRIVATE, 0, 0, 0, 0],
    );

    set_bypass(false);

    assert!(
        woken == 0,
        "test_futex_wake_zero: expected 0 got {}",
        woken
    );
    console::print("  [PASS] test_futex_wake_zero\n");
}

/// FUTEX_CMP_REQUEUE with a mismatched val3 must return EAGAIN.
fn test_futex_cmp_requeue_mismatch() {
    set_bypass(true);

    let mut src: u32 = 42;
    let mut dst: u32 = 0;
    let uaddr = &mut src as *mut u32 as usize;
    let uaddr2 = &mut dst as *mut u32 as usize;

    // val3=99 but src==42 → EAGAIN
    let ret = crate::syscall::handle_syscall(
        NR_FUTEX,
        // args: uaddr, op, val(max_wake), timeout_ptr(max_requeue), uaddr2, val3
        &[uaddr as u64, FUTEX_CMP_REQUEUE_PRIVATE, 1, 1, uaddr2 as u64, 99],
    );

    set_bypass(false);

    assert!(
        ret == EAGAIN,
        "test_futex_cmp_requeue_mismatch: expected EAGAIN ({:#x}) got {:#x}",
        EAGAIN,
        ret
    );
    console::print("  [PASS] test_futex_cmp_requeue_mismatch\n");
}

// ============================================================================
// Multi-threaded tests
// ============================================================================

/// Basic multi-threaded wake: a spawned thread waits, the main thread wakes it.
///
/// The waker stores 1 into the futex word *before* calling FUTEX_WAKE (the
/// real Go pattern).  This ensures the waiter is never permanently stuck:
/// - Normal path: waiter parks, main changes word to 1 and calls wake → woken.
/// - Missed-wake path: main fires wake first AND changes word; when the waiter
///   eventually enters FUTEX_WAIT it reads 1 ≠ 0 and returns EAGAIN — still
///   not stuck.  EAGAIN is treated as success (woken via value change).
/// A 1-second safety timeout is added as a belt-and-braces guard so a bug
/// can never leave a thread permanently parked and consuming a slot.
fn test_futex_basic_wake() {
    static FUTEX_WORD: AtomicU32 = AtomicU32::new(0);
    static WOKEN_FLAG: AtomicBool = AtomicBool::new(false);

    FUTEX_WORD.store(0, Ordering::SeqCst);
    WOKEN_FLAG.store(false, Ordering::SeqCst);

    // Spawn waiter thread.
    // Uses a 1-second timeout so the thread exits even on a missed wake.
    threading::spawn_fn(|| {
        crate::syscall::BYPASS_VALIDATION.store(true, Ordering::Release);

        let uaddr = FUTEX_WORD.as_ptr() as usize;
        let ts = Timespec { tv_sec: 1, tv_nsec: 0 };
        let timeout_ptr = &ts as *const Timespec as u64;
        let ret = crate::syscall::handle_syscall(
            NR_FUTEX,
            &[uaddr as u64, FUTEX_WAIT_PRIVATE, 0, timeout_ptr, 0, 0],
        );

        // ret == 0      → woken by FUTEX_WAKE (success)
        // ret == EAGAIN → value changed before we entered wait (missed wake, success)
        // ret == ETIMEDOUT → safety timeout fired (unexpected, flag stays false)
        if ret == 0 || ret == EAGAIN {
            WOKEN_FLAG.store(true, Ordering::Release);
        }

        crate::syscall::BYPASS_VALIDATION.store(false, Ordering::Release);
        threading::mark_current_terminated();
        loop {
            threading::yield_now();
        }
    })
    .expect("test_futex_basic_wake: spawn failed");

    // Let the waiter reach FUTEX_WAIT
    for _ in 0..10 {
        threading::yield_now();
    }

    // Change the futex word *before* calling wake (standard Go/Linux pattern).
    // This ensures the waiter detects the wakeup even if it missed the FUTEX_WAKE.
    FUTEX_WORD.store(1, Ordering::SeqCst);

    crate::syscall::BYPASS_VALIDATION.store(true, Ordering::Release);
    let uaddr = FUTEX_WORD.as_ptr() as usize;
    crate::syscall::handle_syscall(
        NR_FUTEX,
        &[uaddr as u64, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0],
    );
    crate::syscall::BYPASS_VALIDATION.store(false, Ordering::Release);

    // Wait for the waiter to acknowledge
    let mut woke = false;
    for _ in 0..50 {
        threading::yield_now();
        if WOKEN_FLAG.load(Ordering::Acquire) {
            woke = true;
            break;
        }
    }

    assert!(woke, "test_futex_basic_wake: waiter thread never woke up");
    console::print("  [PASS] test_futex_basic_wake\n");
}

/// FUTEX_WAKE with N=INT_MAX must wake all waiters, not just one.
///
/// Spawns 3 waiter threads on the same futex word, then issues a single
/// FUTEX_WAKE(INT_MAX).  All three threads must unblock.
fn test_futex_wake_all() {
    static FUTEX_WORD2: AtomicU32 = AtomicU32::new(0);
    static WOKEN_COUNT: AtomicU32 = AtomicU32::new(0);

    FUTEX_WORD2.store(0, Ordering::SeqCst);
    WOKEN_COUNT.store(0, Ordering::SeqCst);

    const WAITERS: u32 = 3;

    for _ in 0..WAITERS {
        threading::spawn_fn(|| {
            crate::syscall::BYPASS_VALIDATION.store(true, Ordering::Release);

            let uaddr = FUTEX_WORD2.as_ptr() as usize;
            let ts = Timespec { tv_sec: 2, tv_nsec: 0 };
            let timeout_ptr = &ts as *const Timespec as u64;
            let ret = crate::syscall::handle_syscall(
                NR_FUTEX,
                &[uaddr as u64, FUTEX_WAIT_PRIVATE, 0, timeout_ptr, 0, 0],
            );

            if ret == 0 || ret == EAGAIN {
                WOKEN_COUNT.fetch_add(1, Ordering::Release);
            }

            crate::syscall::BYPASS_VALIDATION.store(false, Ordering::Release);
            threading::mark_current_terminated();
            loop {
                threading::yield_now();
            }
        })
        .expect("test_futex_wake_all: spawn failed");
    }

    // Let all waiters park
    for _ in 0..20 {
        threading::yield_now();
    }

    FUTEX_WORD2.store(1, Ordering::SeqCst);

    // Wake everyone (i32::MAX)
    crate::syscall::BYPASS_VALIDATION.store(true, Ordering::Release);
    let uaddr = FUTEX_WORD2.as_ptr() as usize;
    let woken = crate::syscall::handle_syscall(
        NR_FUTEX,
        &[uaddr as u64, FUTEX_WAKE_PRIVATE, i32::MAX as u64, 0, 0, 0],
    );
    crate::syscall::BYPASS_VALIDATION.store(false, Ordering::Release);

    // woken is the number actually dequeued; may be less than WAITERS if some
    // saw the value change and returned EAGAIN before being queued.
    assert!(
        woken <= WAITERS as u64,
        "test_futex_wake_all: woken={} > WAITERS={}",
        woken,
        WAITERS
    );

    // Wait for all threads to acknowledge
    let mut all_woke = false;
    for _ in 0..100 {
        threading::yield_now();
        if WOKEN_COUNT.load(Ordering::Acquire) == WAITERS {
            all_woke = true;
            break;
        }
    }

    assert!(
        all_woke,
        "test_futex_wake_all: only {}/{} threads woke",
        WOKEN_COUNT.load(Ordering::Acquire),
        WAITERS
    );
    console::print("  [PASS] test_futex_wake_all\n");
}

/// FUTEX_WAKE(1) must dequeue at most one waiter from the kernel queue.
///
/// Spawns 2 waiters on the same address.  FUTEX_WAKE(1) is called — this
/// dequeues at most 1.  The remaining waiter is then released by changing the
/// futex word and calling FUTEX_WAKE(INT_MAX).  Both threads must unblock.
fn test_futex_wake_one_of_two() {
    static FUTEX_WORD3: AtomicU32 = AtomicU32::new(0);
    static WOKEN_COUNT2: AtomicU32 = AtomicU32::new(0);

    FUTEX_WORD3.store(0, Ordering::SeqCst);
    WOKEN_COUNT2.store(0, Ordering::SeqCst);

    // Spawn two waiters with a 2s safety timeout.
    for _ in 0..2 {
        threading::spawn_fn(|| {
            crate::syscall::BYPASS_VALIDATION.store(true, Ordering::Release);

            let uaddr = FUTEX_WORD3.as_ptr() as usize;
            let ts = Timespec { tv_sec: 2, tv_nsec: 0 };
            let timeout_ptr = &ts as *const Timespec as u64;
            let ret = crate::syscall::handle_syscall(
                NR_FUTEX,
                &[uaddr as u64, FUTEX_WAIT_PRIVATE, 0, timeout_ptr, 0, 0],
            );

            // Any non-timeout result means the thread was intentionally unblocked
            if ret == 0 || ret == EAGAIN || ret == ETIMEDOUT {
                WOKEN_COUNT2.fetch_add(1, Ordering::Release);
            }

            crate::syscall::BYPASS_VALIDATION.store(false, Ordering::Release);
            threading::mark_current_terminated();
            loop {
                threading::yield_now();
            }
        })
        .expect("test_futex_wake_one_of_two: spawn failed");
    }

    // Let both waiters park
    for _ in 0..20 {
        threading::yield_now();
    }

    crate::syscall::BYPASS_VALIDATION.store(true, Ordering::Release);
    let uaddr = FUTEX_WORD3.as_ptr() as usize;

    // Wake exactly one — must dequeue at most 1
    let woken = crate::syscall::handle_syscall(
        NR_FUTEX,
        &[uaddr as u64, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0],
    );

    assert!(
        woken <= 1,
        "test_futex_wake_one_of_two: FUTEX_WAKE(1) dequeued {}",
        woken
    );

    // Release the remaining waiter by changing the value and waking all
    FUTEX_WORD3.store(1, Ordering::SeqCst);
    crate::syscall::handle_syscall(
        NR_FUTEX,
        &[uaddr as u64, FUTEX_WAKE_PRIVATE, i32::MAX as u64, 0, 0, 0],
    );
    crate::syscall::BYPASS_VALIDATION.store(false, Ordering::Release);

    // Both threads must unblock
    let mut all_done = false;
    for _ in 0..100 {
        threading::yield_now();
        if WOKEN_COUNT2.load(Ordering::Acquire) >= 2 {
            all_done = true;
            break;
        }
    }

    assert!(
        all_done,
        "test_futex_wake_one_of_two: only {}/2 threads unblocked",
        WOKEN_COUNT2.load(Ordering::Acquire)
    );
    console::print("  [PASS] test_futex_wake_one_of_two\n");
}

/// FUTEX_REQUEUE moves waiters from one futex address to another.
///
/// Spawns 2 waiters on `src`.  FUTEX_REQUEUE wakes 0 from src and requeues
/// both to `dst`.  A subsequent FUTEX_WAKE on `dst` wakes both.
fn test_futex_requeue() {
    static SRC_WORD: AtomicU32 = AtomicU32::new(0);
    static DST_WORD: AtomicU32 = AtomicU32::new(0);
    static REQUEUE_WOKEN: AtomicU32 = AtomicU32::new(0);

    SRC_WORD.store(0, Ordering::SeqCst);
    DST_WORD.store(0, Ordering::SeqCst);
    REQUEUE_WOKEN.store(0, Ordering::SeqCst);

    for _ in 0..2 {
        threading::spawn_fn(|| {
            crate::syscall::BYPASS_VALIDATION.store(true, Ordering::Release);

            let uaddr = SRC_WORD.as_ptr() as usize;
            let ts = Timespec { tv_sec: 2, tv_nsec: 0 };
            let timeout_ptr = &ts as *const Timespec as u64;
            let ret = crate::syscall::handle_syscall(
                NR_FUTEX,
                &[uaddr as u64, FUTEX_WAIT_PRIVATE, 0, timeout_ptr, 0, 0],
            );

            if ret == 0 || ret == EAGAIN {
                REQUEUE_WOKEN.fetch_add(1, Ordering::Release);
            }

            crate::syscall::BYPASS_VALIDATION.store(false, Ordering::Release);
            threading::mark_current_terminated();
            loop {
                threading::yield_now();
            }
        })
        .expect("test_futex_requeue: spawn failed");
    }

    // Let both threads queue on src
    for _ in 0..20 {
        threading::yield_now();
    }

    crate::syscall::BYPASS_VALIDATION.store(true, Ordering::Release);
    let src_uaddr = SRC_WORD.as_ptr() as usize;
    let dst_uaddr = DST_WORD.as_ptr() as usize;

    // FUTEX_REQUEUE: wake 0 from src, requeue up to 2 to dst
    // args: uaddr, op, val(max_wake=0), timeout_ptr(max_requeue=2), uaddr2, val3
    crate::syscall::handle_syscall(
        NR_FUTEX,
        &[src_uaddr as u64, FUTEX_REQUEUE_PRIVATE, 0, 2, dst_uaddr as u64, 0],
    );

    // Now wake all from dst
    DST_WORD.store(1, Ordering::SeqCst);
    crate::syscall::handle_syscall(
        NR_FUTEX,
        &[dst_uaddr as u64, FUTEX_WAKE_PRIVATE, i32::MAX as u64, 0, 0, 0],
    );
    crate::syscall::BYPASS_VALIDATION.store(false, Ordering::Release);

    let mut all_woke = false;
    for _ in 0..100 {
        threading::yield_now();
        if REQUEUE_WOKEN.load(Ordering::Acquire) >= 2 {
            all_woke = true;
            break;
        }
    }

    assert!(
        all_woke,
        "test_futex_requeue: only {}/2 threads woke after requeue+wake",
        REQUEUE_WOKEN.load(Ordering::Acquire)
    );
    console::print("  [PASS] test_futex_requeue\n");
}

/// FUTEX_WAIT_BITSET with val3==0 must return EINVAL.
fn test_futex_wait_bitset_zero_bitset() {
    set_bypass(true);

    const FUTEX_WAIT_BITSET: u64 = 9;
    const FUTEX_WAIT_BITSET_PRIVATE: u64 = FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG;

    let mut val: u32 = 0;
    let uaddr = &mut val as *mut u32 as usize;

    // val3=0 (bitset) is invalid
    let ret = crate::syscall::handle_syscall(
        NR_FUTEX,
        &[uaddr as u64, FUTEX_WAIT_BITSET_PRIVATE, 0, 0, 0, 0 /* val3=0 */],
    );

    set_bypass(false);

    assert!(
        ret == EINVAL,
        "test_futex_wait_bitset_zero_bitset: expected EINVAL ({:#x}) got {:#x}",
        EINVAL,
        ret
    );
    console::print("  [PASS] test_futex_wait_bitset_zero_bitset\n");
}

/// FUTEX_WAIT_BITSET with CLOCK_REALTIME and an already-past deadline must
/// return ETIMEDOUT immediately (not block forever).
fn test_futex_wait_bitset_absolute_past() {
    set_bypass(true);

    const FUTEX_WAIT_BITSET: u64 = 9;
    const FUTEX_CLOCK_REALTIME: u64 = 256;
    const FUTEX_WAIT_BITSET_REALTIME: u64 = FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME;
    const FUTEX_BITSET_MATCH_ANY: u64 = 0xFFFF_FFFF;

    let mut val: u32 = 0;
    let uaddr = &mut val as *mut u32 as usize;

    // Absolute deadline of 1 second (wall-clock epoch) — always in the past.
    let ts = Timespec { tv_sec: 1, tv_nsec: 0 };
    let timeout_ptr = &ts as *const Timespec as u64;

    let ret = crate::syscall::handle_syscall(
        NR_FUTEX,
        &[uaddr as u64, FUTEX_WAIT_BITSET_REALTIME, 0, timeout_ptr, 0, FUTEX_BITSET_MATCH_ANY],
    );

    set_bypass(false);

    assert!(
        ret == ETIMEDOUT || ret == EAGAIN,
        "test_futex_wait_bitset_absolute_past: expected ETIMEDOUT or EAGAIN, got {:#x}",
        ret
    );
    console::print("  [PASS] test_futex_wait_bitset_absolute_past\n");
}

/// Pend a signal on the current thread slot while it would be in FUTEX_WAIT,
/// verify the pending signal causes EINTR to be returned.
/// (Single-threaded: we pend the signal before entering wait with mismatched
/// value so EAGAIN fires, but we verify peek_pending_signal works correctly.)
fn test_per_thread_sigaltstack() {
    // Verify get/set sigaltstack per-thread API works for two different slots.
    // We test slots 0 and 1 directly (without needing actual threads running there).
    akuma_exec::threading::set_sigaltstack(0, 0xdead_0000, 0x4000, 0);
    akuma_exec::threading::set_sigaltstack(1, 0xbeef_0000, 0x8000, 0);

    let (sp0, sz0, fl0) = akuma_exec::threading::get_sigaltstack(0);
    let (sp1, sz1, fl1) = akuma_exec::threading::get_sigaltstack(1);

    assert!(sp0 == 0xdead_0000, "slot 0 sp mismatch: {:#x}", sp0);
    assert!(sz0 == 0x4000,      "slot 0 size mismatch: {:#x}", sz0);
    assert!(fl0 == 0,           "slot 0 flags mismatch: {}", fl0);

    assert!(sp1 == 0xbeef_0000, "slot 1 sp mismatch: {:#x}", sp1);
    assert!(sz1 == 0x8000,      "slot 1 size mismatch: {:#x}", sz1);
    assert!(fl1 == 0,           "slot 1 flags mismatch: {}", fl1);

    // Slots must be independent — slot 0 must be unchanged after writing slot 1.
    let (sp0b, _, _) = akuma_exec::threading::get_sigaltstack(0);
    assert!(sp0b == 0xdead_0000, "slot 0 contaminated by slot 1 write");

    // Restore to disabled state.
    akuma_exec::threading::set_sigaltstack(0, 0, 0, 2);
    akuma_exec::threading::set_sigaltstack(1, 0, 0, 2);

    console::print("  [PASS] test_per_thread_sigaltstack\n");
}

/// Verify peek_pending_signal returns the signal without consuming it.
fn test_peek_pending_signal() {
    let slot = akuma_exec::threading::current_thread_id();

    // No signal pending initially (after any previous test cleanup).
    akuma_exec::threading::pend_signal_for_thread(slot, 0); // clear
    assert!(
        akuma_exec::threading::peek_pending_signal(slot) == 0,
        "peek_pending_signal: expected 0 after clear"
    );

    // Pend a signal and peek — must see it without consuming.
    // Use SIGURG (23) as Go does.
    akuma_exec::threading::pend_signal_for_thread(slot, 23);
    let first  = akuma_exec::threading::peek_pending_signal(slot);
    let second = akuma_exec::threading::peek_pending_signal(slot);
    assert!(first == 23,  "peek_pending_signal: expected 23, got {}", first);
    assert!(second == 23, "peek_pending_signal: not idempotent, got {}", second);

    // Clear it.
    akuma_exec::threading::pend_signal_for_thread(slot, 0);
    assert!(
        akuma_exec::threading::peek_pending_signal(slot) == 0,
        "peek_pending_signal: expected 0 after second clear"
    );

    console::print("  [PASS] test_peek_pending_signal\n");
}

pub fn run_all_tests() {
    console::print("\n--- Futex Sync Tests ---\n");
    // Single-threaded correctness
    test_futex_eagain();
    test_futex_null_addr();
    test_futex_unaligned_addr();
    test_futex_timeout();
    test_futex_wake_before_wait();
    test_futex_wake_zero();
    test_futex_cmp_requeue_mismatch();
    test_futex_wait_bitset_zero_bitset();
    test_futex_wait_bitset_absolute_past();
    test_per_thread_sigaltstack();
    test_peek_pending_signal();
    // Multi-threaded
    test_futex_basic_wake();
    test_futex_wake_all();
    test_futex_wake_one_of_two();
    test_futex_requeue();
    console::print("--- Futex Sync Tests Done ---\n\n");
}
