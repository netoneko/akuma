//! Rump sysproxy / scheduling regression guards.
//!
//! Static + behavioural invariants for the rump-default devbox performance
//! fixes (Phase 2a: rump proxy kthread claims the network-thread boost slot;
//! Phase 2b: rump-aware shorter blocking-poll cadence). Mirrors the
//! source-text-check pattern in `ssh_tests::test_block_on_uses_yield_now`.
//!
//! All tests are kernel-space (called from `main` at boot under
//! `#[cfg(feature = "rump")]`), not host-testable — they assert on files in
//! `src/` and on pure functions in `crate::syscall::poll`.

use crate::console;

/// T1: under `rump-default`, `run_async_main` must NOT register itself as the
/// network thread — the rump proxy kthread owns that slot (T2). If anyone
/// removes the `#[cfg(not(feature = "rump-default"))]` gate we'd silently
/// regress the devbox "starve under CPU-bound load" fix
/// (`overlays/devbox/README.md:297-307`).
fn test_run_async_main_skips_network_thread_id_under_rump_default() {
    const MAIN_SRC: &str = include_str!("main.rs");

    let fn_start = MAIN_SRC
        .find("fn run_async_main")
        .expect("main.rs must define run_async_main");
    let fn_body_end = MAIN_SRC[fn_start..]
        .find("\nfn ")
        .map_or(MAIN_SRC.len(), |off| fn_start + off);
    let body = &MAIN_SRC[fn_start..fn_body_end];

    // Locate the set_network_thread_id call inside run_async_main.
    let call_idx = body
        .find("set_network_thread_id(")
        .expect("run_async_main must register the network thread (gated)");

    // Walk backwards from the call over non-comment, non-blank lines until we
    // find the nearest attribute (attrs must precede the item/call closely).
    let before = &body[..call_idx];
    let mut found_cfg = false;
    for raw_line in before.lines().rev() {
        let line = raw_line.trim_start();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if line.starts_with("#[cfg(") {
            found_cfg = line.contains("not(feature = \"rump-default\")");
        }
        break;
    }
    assert!(
        found_cfg,
        "run_async_main's set_network_thread_id call must be gated by \
         #[cfg(not(feature = \"rump-default\"))] so the rump proxy kthread owns \
         the boost slot under devbox"
    );

    console::print("  [PASS] test_run_async_main_skips_network_thread_id_under_rump_default\n");
}

/// T2: under `rump-default`, `start_default_stack` must register rump_server's
/// main thread (the fiber-scheduler OS thread) as the network thread — NOT the
/// proxy handshake kthread (which parks after `Client::connect` and does zero
/// per-call work). The registration happens right after spawn, using the TID
/// returned by `spawn_process_with_channel`, before `attach_server` is called.
fn test_start_default_stack_registers_rump_server_tid() {
    const PROXY_SRC: &str = include_str!("rump_proxy.rs");

    let fn_start = PROXY_SRC
        .find("pub fn start_default_stack")
        .expect("rump_proxy.rs must define start_default_stack");
    let fn_body_end = PROXY_SRC[fn_start..]
        .find("\npub fn ")
        .or_else(|| PROXY_SRC[fn_start..].find("\nfn "))
        .map_or(PROXY_SRC.len(), |off| fn_start + off);
    let body = &PROXY_SRC[fn_start..fn_body_end];

    // The spawn must capture the TID (not discard it as `_tid`).
    assert!(
        !body.contains("Ok((_tid,") || body.contains("Ok((tid,"),
        "start_default_stack must capture rump_server's TID from spawn_process_with_channel, \
         not discard it as _tid"
    );

    // The set_network_thread_id call must be present AND must use the captured
    // server TID (not current_thread_id, which would be the kthread).
    let reg_idx = body
        .find("set_network_thread_id(server_tid)")
        .expect(
            "start_default_stack must call set_network_thread_id(server_tid) — registering \
             rump_server's main thread as the network thread, not the calling kthread. \
             The proxy kthread parks after handshake and does no per-call work.",
        );
    let _ = reg_idx; // silence unused warning in release

    // The kthread body in attach_server must NOT contain its own registration.
    let attach_start = PROXY_SRC
        .find("pub fn attach_server")
        .expect("rump_proxy.rs must define attach_server");
    let attach_body = &PROXY_SRC[attach_start..];
    let spawn_idx = attach_body
        .find("threading::spawn_fn")
        .expect("attach_server must spawn the handshake kthread");
    let kthread_body = &attach_body[spawn_idx..];
    // Find the next function after the kthread body to bound our search.
    let kthread_end = kthread_body
        .find("\n    });\n")
        .map(|off| spawn_idx + off)
        .unwrap_or(attach_body.len());
    let kthread_section = &attach_body[spawn_idx..kthread_end];
    assert!(
        !kthread_section.contains("set_network_thread_id(current_thread_id())"),
        "attach_server's kthread must NOT register itself as the network thread — \
         it parks after the handshake. The registration belongs in start_default_stack."
    );

    console::print("  [PASS] test_start_default_stack_registers_rump_server_tid\n");
}

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

pub fn run_all_tests() {
    console::print("\n--- Rump Tests ---\n");
    test_run_async_main_skips_network_thread_id_under_rump_default();
    test_start_default_stack_registers_rump_server_tid();
    test_effective_poll_interval_default_for_non_rump();
    test_effective_poll_interval_rump_uses_shorter_floor();
    test_effective_poll_interval_rump_meaningfully_shorter();
    console::print("--- Rump tests complete ---\n");
}
