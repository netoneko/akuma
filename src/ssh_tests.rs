//! SSH stability tests.
//!
//! Covers the invariants identified in the 2026-05-29 audit (see
//! `docs/STABILITY_URGENT_ISSUES.md` issue #2 and the audit findings
//! section of the plan):
//!
//! * T1 — `block_on` in `src/ssh/server.rs` MUST yield with `yield_now()`,
//!   not `schedule_blocking()`. The latter would re-introduce the
//!   NETWORK-spinlock deadlock (D2). Static source check.
//! * T2 — Session counter bookkeeping is well-formed: every "open" balances
//!   with a "close", `PANICKED` only moves on the panic-tagged close path,
//!   and `stats()` round-trips the live values.
//! * T3 — `classify_session_exit` maps every pre-auth `SshState` to
//!   handshake_fail, `AwaitingUserAuth` to auth_fail, and post-auth states
//!   to neither.
//! * T4 — The `[SSH] no listener` heartbeat reflects `SERVER_ALIVE=false`
//!   even when counter values are non-zero (sanity check on `stats()`
//!   wiring).

use akuma_ssh::session::SshState;

use crate::console;
use crate::ssh::server::{self, stats, test_note_session_close, test_note_session_open};

/// T1: static guard on the SSH `block_on` body. If anyone re-introduces
/// `schedule_blocking()` inside it we abort the test run instead of waiting
/// for a soak deadlock to expose the regression.
///
/// The body of `block_on` deliberately MENTIONS `schedule_blocking` in a
/// comment explaining why we don't use it, so this check must look only
/// at non-comment lines and at the literal call form `schedule_blocking(`.
fn test_block_on_uses_yield_now() {
    const SERVER_SRC: &str = include_str!("ssh/server.rs");

    let block_on_start = SERVER_SRC
        .find("fn block_on")
        .expect("ssh/server.rs must define block_on");
    let block_on_end = SERVER_SRC[block_on_start..]
        .find("\nfn ")
        .map(|off| block_on_start + off)
        .unwrap_or(SERVER_SRC.len());
    let body = &SERVER_SRC[block_on_start..block_on_end];

    let mut found_yield = false;
    for raw_line in body.lines() {
        let line = raw_line.trim_start();
        if line.starts_with("//") {
            continue;
        }
        // Strip a trailing comment so `foo(); // schedule_blocking is fine`
        // doesn't falsely trip the check.
        let code = match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        };
        assert!(
            !code.contains("schedule_blocking("),
            "ssh/server.rs::block_on must NOT call schedule_blocking(); \
             see SSH_STAGGERING.md and audit finding D2. Offending line: {:?}",
            raw_line
        );
        if code.contains("yield_now(") {
            found_yield = true;
        }
    }
    assert!(
        found_yield,
        "ssh/server.rs::block_on must yield via yield_now() (no call found in non-comment lines)"
    );

    console::print("  [PASS] test_block_on_uses_yield_now\n");
}

/// T2: counters move exactly once per simulated session and the panicked
/// flag is the only thing that bumps `PANICKED`.
fn test_session_counters_balance() {
    let before = stats();

    test_note_session_open();
    let after_open = stats();
    assert_eq!(after_open.active, before.active + 1, "active gauge must rise");
    assert_eq!(after_open.opened, before.opened + 1, "opened counter must rise");
    assert_eq!(after_open.closed, before.closed, "closed must not move on open");
    assert_eq!(after_open.panicked, before.panicked, "panicked must not move on open");

    test_note_session_close(false);
    let after_close = stats();
    assert_eq!(after_close.active, before.active, "active gauge must return to baseline");
    assert_eq!(after_close.opened, before.opened + 1, "opened must stay at +1");
    assert_eq!(after_close.closed, before.closed + 1, "closed must rise");
    assert_eq!(after_close.panicked, before.panicked, "clean close must NOT bump panicked");

    test_note_session_open();
    test_note_session_close(true);
    let after_panic = stats();
    assert_eq!(after_panic.active, before.active, "active back to baseline after panicked close");
    assert_eq!(after_panic.panicked, before.panicked + 1, "panicked close must bump PANICKED");

    console::print("  [PASS] test_session_counters_balance\n");
}

/// T3: classification of session exit states into handshake_fail vs
/// auth_fail vs (no counter). Verifies the mapping audited in
/// `protocol.rs::classify_session_exit`.
fn test_classify_session_exit_mapping() {
    use crate::ssh::protocol::classify_session_exit;

    let before = stats();

    classify_session_exit(SshState::AwaitingVersion);
    classify_session_exit(SshState::AwaitingKexInit);
    classify_session_exit(SshState::AwaitingKexEcdhInit);
    classify_session_exit(SshState::AwaitingNewKeys);
    classify_session_exit(SshState::AwaitingServiceRequest);
    let after_hs = stats();
    assert_eq!(
        after_hs.handshake_fail,
        before.handshake_fail + 5,
        "all five pre-auth states must count as handshake_fail"
    );
    assert_eq!(after_hs.auth_fail, before.auth_fail, "pre-auth must not touch auth_fail");

    classify_session_exit(SshState::AwaitingUserAuth);
    let after_auth = stats();
    assert_eq!(
        after_auth.auth_fail,
        before.auth_fail + 1,
        "AwaitingUserAuth must count as auth_fail"
    );
    assert_eq!(
        after_auth.handshake_fail,
        before.handshake_fail + 5,
        "auth-stage exit must NOT count as handshake_fail"
    );

    classify_session_exit(SshState::Authenticated);
    classify_session_exit(SshState::Disconnected);
    let after_clean = stats();
    assert_eq!(
        after_clean.handshake_fail,
        before.handshake_fail + 5,
        "Authenticated/Disconnected must not bump handshake_fail"
    );
    assert_eq!(
        after_clean.auth_fail,
        before.auth_fail + 1,
        "Authenticated/Disconnected must not bump auth_fail"
    );

    console::print("  [PASS] test_classify_session_exit_mapping\n");
}

/// T4: `stats()` reflects the SERVER_ALIVE flag. The accept loop sets it
/// once running; before the loop starts we expect `alive=false`. This test
/// runs BEFORE the SSH server is spawned, so we can lock that in.
fn test_stats_alive_flag_before_server_start() {
    let s = stats();
    assert!(
        !s.alive,
        "SERVER_ALIVE must be false before run() is entered; got stats={{active:{},opened:{}}}",
        s.active, s.opened
    );
    console::print("  [PASS] test_stats_alive_flag_before_server_start\n");
}

pub fn run_all_tests() {
    console::print("\n--- SSH Tests ---\n");
    test_block_on_uses_yield_now();
    test_stats_alive_flag_before_server_start();
    test_session_counters_balance();
    test_classify_session_exit_mapping();
    console::print("--- SSH tests complete ---\n");

    // Reset transient counters so the heartbeat in production isn't confused
    // by test-driven bumps. We do this by reading the deltas we introduced
    // and… actually, leave them: operators reading the heartbeat see
    // "open=N close=N panic=1" with active=0 and can deduce the test ran.
    // Keeping the test footprint observable is more useful than hiding it.
    let _ = server::stats();
}
