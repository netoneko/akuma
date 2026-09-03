//! Rump sysproxy / scheduling regression guards.
//!
//! Static + behavioural invariants for the rump-default devbox performance
//! fixes (Phase 2a: `start_default_stack` registers rump_server's TID as the
//! network thread; Phase 2b: rump-aware shorter blocking-poll cadence).
//! Mirrors the source-text-check pattern in
//! `ssh_tests::test_block_on_uses_yield_now`.
//!
//! All tests are kernel-space (called from `main` at boot under
//! `#[cfg(feature = "rump")]`), not host-testable — they assert on pure
//! functions in `crate::syscall::poll` and on rump fd lifetime.
//!
//! The two SOURCE-TEXT tests (T1 `run_async_main` cfg gate, T2
//! `start_default_stack` TID registration) moved to host tests in
//! `akuma-kernel-glue`'s `source_shape` module on 2026-09-03. They used
//! `include_str!` on `kernel-glue/src/lib.rs` (126 KB) and `rump_proxy.rs`
//! (78 KB), putting ~200 KB of source text into the kernel image's `.rodata`;
//! and being gated with this module they never ran in the devbox builds that
//! actually use `rump-default`. On host they cost no image bytes and run for
//! every profile.

use crate::console;



/// T3: the effective poll cadence must be the default 10 ms when no rump fd is
/// being polled — preserves the `KNOWN_ISSUES.md` #6/#7 fix for non-rump
/// paths (pipes, eventfds, timerfds).
fn test_effective_poll_interval_default_for_non_rump() {
    let interval = crate::syscall::poll::effective_poll_interval_us(false);
    assert_eq!(
        interval, 10_000,
        "non-rump poll cadence must stay at 10 ms (KNOWN_ISSUES #6/#7)"
    );
    console::print("  [PASS] test_effective_poll_interval_default_for_non_rump\n");
}

/// T4: with a rump fd being polled, the cadence must drop to the tighter 1 ms
/// floor. Rump sockets have no push readiness yet, so this is the only lever
/// that keeps their per-round-trip cost bounded.
fn test_effective_poll_interval_rump_uses_shorter_floor() {
    let interval = crate::syscall::poll::effective_poll_interval_us(true);
    assert_eq!(
        interval, 1_000,
        "rump poll cadence must drop to 1 ms (the Phase 2b fix)"
    );
    console::print("  [PASS] test_effective_poll_interval_rump_uses_shorter_floor\n");
}

/// T5: regression guard against accidental bumps. The rump floor must remain
/// a meaningful tightening (at least 5×) of the default; if someone raises it
/// to e.g. 5 ms the test trips. The chosen value (1 ms) reflects that the
/// sysproxy round-trip itself is in the hundreds-of-µs range, so a 1 ms
/// re-poll floor matches the underlying cost without padding it 10×.
fn test_effective_poll_interval_rump_meaningfully_shorter() {
    let rump = crate::syscall::poll::effective_poll_interval_us(true);
    let default = crate::syscall::poll::effective_poll_interval_us(false);
    assert!(
        rump * 5 <= default,
        "rump poll cadence ({rump}µs) must be at least 5× tighter than default ({default}µs) \
         — otherwise the Phase 2b fix has been effectively reverted"
    );
    console::print("  [PASS] test_effective_poll_interval_rump_meaningfully_shorter\n");
}

/// T6: a rump socket must survive `fork` — the reference the child's descriptor
/// takes has to outlive the parent's `close`.
///
/// Drives `rump_proxy`'s reference count directly through the exact sequence
/// `sshd`'s process-per-session accept loop produces: accept (init), fork
/// (clone), parent `drop(stream)` (drop → must NOT be the last), child exit
/// (drop → must be the last, so the NetBSD `close` finally goes out).
///
/// Before the count existed, the parent's close was reported as last and
/// `proxy_close` tore the socket down inside `rump_server` while the child was
/// still mid-kex; every rump-devbox ssh session died with
/// `kex_exchange_identification: Connection reset by peer`. See
/// `docs/archive/RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`.
fn test_rump_fd_ref_survives_fork() {
    // A (box_id, rump_fd) pair no live socket can be using.
    const BOX: u64 = 0xFFFF_FF01;
    const FD: i32 = 0x7EED;

    crate::rump_proxy::rump_fd_ref_init(BOX, FD);
    crate::rump_proxy::rump_fd_ref_clone(BOX, FD); // fork duplicates the descriptor
    assert!(
        !crate::rump_proxy::rump_fd_ref_drop(BOX, FD),
        "parent's post-fork close must NOT be the last reference — closing the rump \
         socket here is what killed every forked ssh session at kex"
    );
    assert!(
        crate::rump_proxy::rump_fd_ref_drop(BOX, FD),
        "child's close must be the last reference, or the rump fd leaks in rump_server"
    );

    // Two boxes' servers hand out the same small fd numbers; the count must not
    // conflate them, or a close in one box is deferred by a reference in another.
    crate::rump_proxy::rump_fd_ref_init(BOX, FD);
    crate::rump_proxy::rump_fd_ref_init(BOX + 1, FD);
    assert!(
        crate::rump_proxy::rump_fd_ref_drop(BOX, FD),
        "per-box counts must be independent"
    );
    assert!(crate::rump_proxy::rump_fd_ref_drop(BOX + 1, FD));

    // An untracked pair (a socket predating tracking) must still close, not leak.
    assert!(
        crate::rump_proxy::rump_fd_ref_drop(BOX, FD + 1),
        "an untracked rump fd must report its close as the last one"
    );
    console::print("  [PASS] test_rump_fd_ref_survives_fork\n");
}

pub fn run_all_tests() {
    console::print("\n--- Rump Tests ---\n");
    test_rump_fd_ref_survives_fork();
    test_effective_poll_interval_default_for_non_rump();
    test_effective_poll_interval_rump_uses_shorter_floor();
    test_effective_poll_interval_rump_meaningfully_shorter();
    console::print("--- Rump tests complete ---\n");
}
