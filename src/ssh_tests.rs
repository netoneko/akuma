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
use core::sync::atomic::Ordering;

use crate::console;
use crate::ssh::server::{self, stats, test_note_session_close, test_note_session_open, SERVER_STEP, step};

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
        .map_or(SERVER_SRC.len(), |off| block_on_start + off);
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
             see SSH_STAGGERING.md and audit finding D2. Offending line: {raw_line:?}"
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

/// T5: `SERVER_STEP` defaults to `IDLE` before the accept loop runs and is
/// represented by `step::name()` with stable strings. Locks the contract
/// between the accept loop and the supervisor's `STALL DETAIL` line.
fn test_server_step_defaults_and_names() {
    let s = stats();
    assert_eq!(
        s.last_step, step::IDLE,
        "SERVER_STEP must be IDLE before run() is entered (got {})",
        s.last_step,
    );

    // Stable name strings — the supervisor logs these in `STALL DETAIL`.
    assert_eq!(step::name(step::IDLE), "idle");
    assert_eq!(step::name(step::TICK), "tick");
    assert_eq!(step::name(step::PRE_WITH_NETWORK), "pre_with_network");
    assert_eq!(step::name(step::POST_WITH_NETWORK), "post_with_network");
    assert_eq!(step::name(step::SPAWN), "spawn");
    assert_eq!(step::name(step::CREATE_LISTENER), "create_listener");
    assert_eq!(step::name(step::POLL), "poll");
    assert_eq!(step::name(step::YIELD), "yield");
    assert_eq!(step::name(255), "idle", "unknown step values fall back to idle");

    console::print("  [PASS] test_server_step_defaults_and_names\n");
}

/// T6: a write to `SERVER_STEP` round-trips through `stats()` so the
/// supervisor reads the value the accept loop last stamped.
fn test_server_step_round_trips_through_stats() {
    let before = SERVER_STEP.load(Ordering::Relaxed);
    SERVER_STEP.store(step::POLL, Ordering::Relaxed);
    let s = stats();
    assert_eq!(s.last_step, step::POLL, "stats() must reflect SERVER_STEP");
    // Restore so the heartbeat after tests doesn't show a misleading step.
    SERVER_STEP.store(before, Ordering::Relaxed);

    console::print("  [PASS] test_server_step_round_trips_through_stats\n");
}

/// T7: `network_holder_snapshot()` from akuma-net reports NONE when no
/// `with_network` / `poll` is active, and the supervisor's "free" check
/// uses the public `NETWORK_HOLDER_NONE` sentinel. Pins the contract.
fn test_network_holder_snapshot_idle() {
    use akuma_net::smoltcp_net::{network_holder_snapshot, NetSite, NETWORK_HOLDER_NONE};

    let (holder, _locked_at, _site, _polls_in, _polls_out) = network_holder_snapshot();
    // The boot path may have left a stale `locked_at`/`site` from the last
    // acquisition, so we only assert the holder is currently free. Tests
    // run between heartbeats and the `with_network` body is short.
    assert_eq!(
        holder, NETWORK_HOLDER_NONE,
        "NETWORK should be unlocked between calls; got holder={holder}"
    );

    // `NetSite::as_str` strings are stable contract with the supervisor.
    assert_eq!(NetSite::None.as_str(), "none");
    assert_eq!(NetSite::Poll.as_str(), "poll");
    assert_eq!(NetSite::WithNetwork.as_str(), "with_network");
    assert_eq!(NetSite::SocketClose.as_str(), "socket_close");
    assert_eq!(NetSite::UdpSocketClose.as_str(), "udp_socket_close");
    assert_eq!(NetSite::from_u8(99), NetSite::None);

    console::print("  [PASS] test_network_holder_snapshot_idle\n");
}

/// T8: `poll()` increments both POLL_ENTERED and POLL_EXITED in lockstep
/// during normal operation. If they diverge during a stall, candidate (b)
/// is implicated. Drive a few polls and assert the deltas match.
fn test_poll_entered_exited_balanced() {
    use akuma_net::smoltcp_net::{network_holder_snapshot, poll};

    let (_, _, _, in0, out0) = network_holder_snapshot();
    for _ in 0..4 {
        poll();
    }
    let (_, _, _, in1, out1) = network_holder_snapshot();
    assert_eq!(
        in1 - in0,
        out1 - out0,
        "POLL_ENTERED/EXITED must move in lockstep across normal polls (in: {in0} → {in1}, out: {out0} → {out1})",
    );
    assert!(in1 >= in0 + 4, "expected at least 4 polls (in0={in0} in1={in1})");

    console::print("  [PASS] test_poll_entered_exited_balanced\n");
}

/// T9: static guard on `handle_exec` in `src/ssh/protocol.rs`. Ensures that
/// the debug leftover `[DEBUG] Using buffered path` write (Issue #6 in
/// `docs/STABILITY_URGENT_ISSUES.md`) is never re-introduced. Any live
/// `[DEBUG]` write in `handle_exec` corrupts SSH exec output for all external
/// binaries.
fn test_exec_handler_no_debug_string() {
    const PROTO_SRC: &str = include_str!("ssh/protocol.rs");

    let handle_exec_start = PROTO_SRC
        .find("async fn handle_exec(")
        .expect("protocol.rs must define handle_exec");
    let handle_exec_end = PROTO_SRC[handle_exec_start..]
        .find("\nasync fn ")
        .map_or(PROTO_SRC.len(), |off| handle_exec_start + off);
    let body = &PROTO_SRC[handle_exec_start..handle_exec_end];

    for raw_line in body.lines() {
        let line = raw_line.trim_start();
        if line.starts_with("//") {
            continue;
        }
        let code = match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        };
        assert!(
            !code.contains("[DEBUG]"),
            "handle_exec must not emit [DEBUG] strings; \
             see STABILITY_URGENT_ISSUES.md Issue #6. Offending line: {raw_line:?}"
        );
    }

    console::print("  [PASS] test_exec_handler_no_debug_string\n");
}

/// Client stdin-EOF (`CHANNEL_EOF`, which `ssh host cmd` sends immediately) must
/// be distinguished from a real disconnect (`CHANNEL_CLOSE` / `DISCONNECT`).
/// Conflating them is what made long non-interactive commands (a build) die at
/// the first fork. Static guard on the protocol message handler.
fn test_channel_eof_distinct_from_close() {
    const PROTO_SRC: &str = include_str!("ssh/protocol.rs");

    // The two messages must be handled in separate match arms, not merged.
    assert!(
        PROTO_SRC.contains("SSH_MSG_CHANNEL_EOF =>"),
        "CHANNEL_EOF must have its own arm (stdin done != disconnect)"
    );
    // Only a real close/disconnect sets channel_closed.
    assert!(
        PROTO_SRC.contains("channel_closed = true"),
        "CHANNEL_CLOSE / DISCONNECT must set channel_closed"
    );
    // Regression guard: EOF and CLOSE must NOT share one arm again.
    assert!(
        !PROTO_SRC.contains("SSH_MSG_CHANNEL_EOF | SSH_MSG_CHANNEL_CLOSE"),
        "CHANNEL_EOF must not be merged with CHANNEL_CLOSE (kills long commands)"
    );

    console::print("  [PASS] test_channel_eof_distinct_from_close\n");
}

/// A streamed `ssh host cmd` must not be killed when the client closes its
/// stdin: stdin-EOF should deliver EOF to the process (`close_stdin`) and keep
/// streaming; only a real disconnect (`channel_closed`) ends the loop; only
/// Ctrl-C interrupts. Static guard on the streaming exec loop.
fn test_streaming_exec_survives_stdin_eof() {
    const SHELL_SRC: &str = include_str!("shell/mod.rs");

    let start = SHELL_SRC
        .find("pub async fn execute_external_interactive(")
        .expect("shell/mod.rs must define execute_external_interactive");
    let end = SHELL_SRC[start..]
        .find("\npub async fn ")
        .map_or(SHELL_SRC.len(), |off| start + off);
    let body = &SHELL_SRC[start..end];

    assert!(
        body.contains("close_process_stdin("),
        "stdin-EOF must be delivered (and the reader woken) via close_process_stdin(), not by killing the process"
    );
    assert!(
        body.contains("channel_closed()"),
        "the loop must end on a real disconnect (channel_closed), not on stdin-EOF"
    );

    // Regression guard: the old band-aid interrupted the process the instant
    // the client closed stdin (`if channel_eof() { set_interrupted; break }`),
    // which killed long non-interactive commands. Ensure stdin-EOF no longer
    // drives set_interrupted (whitespace-insensitive).
    let collapsed: alloc::string::String = body.split_whitespace().collect();
    assert!(
        !collapsed.contains("channel_eof(){channel.set_interrupted"),
        "stdin-EOF must NOT interrupt the process — regression of the long-build fix"
    );

    console::print("  [PASS] test_streaming_exec_survives_stdin_eof\n");
}

pub fn run_all_tests() {
    console::print("\n--- SSH Tests ---\n");
    test_block_on_uses_yield_now();
    test_stats_alive_flag_before_server_start();
    test_session_counters_balance();
    test_classify_session_exit_mapping();
    test_server_step_defaults_and_names();
    test_server_step_round_trips_through_stats();
    test_network_holder_snapshot_idle();
    test_poll_entered_exited_balanced();
    test_exec_handler_no_debug_string();
    test_channel_eof_distinct_from_close();
    test_streaming_exec_survives_stdin_eof();
    console::print("--- SSH tests complete ---\n");

    // Reset transient counters so the heartbeat in production isn't confused
    // by test-driven bumps. We do this by reading the deltas we introduced
    // and… actually, leave them: operators reading the heartbeat see
    // "open=N close=N panic=1" with active=0 and can deduce the test ran.
    // Keeping the test footprint observable is more useful than hiding it.
    let _ = server::stats();
}
