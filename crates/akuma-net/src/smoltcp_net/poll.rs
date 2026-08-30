//! `poll()` — one lap of the stack, and the `NETWORK` critical section.

use super::*;

// Public API
// ============================================================================

/// What the DHCP socket reported during a `poll()`, recorded inside the
/// `NETWORK` critical section and logged **after** it is released.
///
/// Logging from inside that section is a deadlock: `poll()` holds a `Spinlock`
/// with preemption disabled, and the console takes `CONSOLE_LOCK` — establishing
/// a NETWORK -> CONSOLE order that any CONSOLE -> NETWORK path (or a print from
/// IRQ context) closes into a cycle. These log statements were harmless for years
/// because `akuma-net` built with `log`'s `max_level_off`, which compiled them
/// out entirely; they became real console I/O the moment a sink was installed.
#[derive(Copy, Clone)]
pub(crate) enum DhcpReport {
    Configured { addr: smoltcp::wire::Ipv4Cidr, addr_full: bool, loopback_full: bool },
    Deconfigured { fallback_full: bool, loopback_full: bool },
}

#[allow(clippy::cast_possible_wrap)]
pub fn poll() -> bool {
    POLL_ENTERED.fetch_add(1, Ordering::Relaxed);
    let poll_t = nicstat::start();
    // Filled inside the critical section, emitted once it has been released.
    let mut dhcp_report: Option<DhcpReport> = None;
    let mut corrupt_handles: u32 = 0;
    let socket_state_changed = {
        // Hold preemption disabled for the whole NETWORK critical section so the
        // spinlock is never stranded across a context switch (fatal under the
        // BKL — see `PreemptGuard`). No-op on non-smp-shared builds.
        let _pg = PreemptGuard::new();
        // Time the acquisition separately: `NETWORK` is a plain (unfair)
        // `Spinlock`, so under 4-core contention a waiter can be starved, and
        // `poll_us` alone cannot distinguish that from a slow poll or a slow
        // wake pass.
        let wait_t = nicstat::start();
        let mut guard = NETWORK.lock();
        nicstat::record_poll_wait(wait_t);
        mark_acquire(NetSite::Poll);
        let result = if let Some(net) = guard.as_mut() {
            let timestamp = Instant::from_micros((runtime().uptime_us)() as i64);
            
            let p1 = net.iface.poll(timestamp, &mut net.device, &mut net.sockets);
            
            // Handle DHCP
            let mut dhcp_changed = false;
            if let Some(handle) = net.dhcp_handle {
                let event = net.sockets.get_mut::<dhcpv4::Socket>(handle).poll();
                if let Some(event) = event {
                    match event {
                        dhcpv4::Event::Configured(config) => {
                            // `addrs` was just cleared, so these cannot fail —
                            // but panicking here would kill the kernel over a
                            // DHCP lease, so degrade instead. Failures are
                            // recorded and reported outside the lock.
                            let mut addr_full = false;
                            let mut loopback_full = false;
                            net.iface.update_ip_addrs(|addrs| {
                                addrs.clear();
                                addr_full = addrs.push(IpCidr::Ipv4(config.address)).is_err();
                                loopback_full = addrs
                                    .push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8))
                                    .is_err();
                            });
                            dhcp_report = Some(DhcpReport::Configured {
                                addr: config.address,
                                addr_full,
                                loopback_full,
                            });
                            if let Some(router) = config.router {
                                let _ = net.iface.routes_mut().add_default_ipv4_route(router);
                            }

                            DHCP_CONFIGURED.store(true, Ordering::Release);
                        }
                        dhcpv4::Event::Deconfigured => {
                            DHCP_CONFIGURED.store(false, Ordering::Release);
                            let mut fallback_full = false;
                            let mut loopback_full = false;
                            net.iface.update_ip_addrs(|addrs| {
                                addrs.clear();
                                fallback_full = addrs
                                    .push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24))
                                    .is_err();
                                loopback_full = addrs
                                    .push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8))
                                    .is_err();
                            });
                            dhcp_report =
                                Some(DhcpReport::Deconfigured { fallback_full, loopback_full });
                            let _ = net.iface.routes_mut().add_default_ipv4_route(smoltcp::wire::Ipv4Address::new(10, 0, 2, 2));
                        }
                    }
                    dhcp_changed = true;
                }
            }

            // Re-poll after DHCP reconfiguration so the stack immediately processes
            // any in-flight packets (e.g. loopback TCP handshake) with the updated
            // IP configuration. Without this, the address change isn't picked up
            // until the next external poll() call, which can cause loopback TCP
            // connections to stall (server stuck in SynReceived).
            if dhcp_changed {
                let timestamp = Instant::from_micros((runtime().uptime_us)() as i64);
                net.iface.poll(timestamp, &mut net.device, &mut net.sockets);
            }

            // Garbage collect pending removals.
            // Force-abort sockets stuck in non-Closed states for longer than
            // SOCKET_GC_TIMEOUT_US to prevent slot exhaustion.
            //
            // Runs on EVERY poll. Rate-limiting it to 100 ms was tried
            // 2026-08-20 — reasonable on the face of it, since everything it
            // collects is on a 10 s (smoltcp `CLOSE_DELAY`) or 30 s clock — and
            // measured NO DIFFERENCE, so it was reverted as unearned complexity
            // rather than as a regression.
            //
            // Normalised by traffic (the only fair comparison — poll count tracks
            // packet count), gated vs ungated at ~12,160 rx packets/window:
            // 6.96 vs 6.87/6.70 polls per packet, 10.7 vs 11.0/11.5 us per poll.
            // That is noise.
            //
            // Why it is cheaper than it looks: `SocketSet::get` is an array index,
            // not a scan, so the sweep is ~100 cheap operations per poll. Counting
            // operations is not measuring them — see AKUMA_NET_ISSUES.md §10.
            let now_us = (runtime().uptime_us)();
            let mut i = 0;
            while i < net.pending_removal.len() {
                let (handle, added_at) = net.pending_removal[i];
                if !is_valid_handle(handle) {
                    corrupt_handles = corrupt_handles.saturating_add(1);
                    net.pending_removal.swap_remove(i);
                    continue;
                }
                let state = net.sockets.get::<tcp::Socket>(handle).state();
                let timed_out = now_us.saturating_sub(added_at) > SOCKET_GC_TIMEOUT_US;
                if state == tcp::State::Closed || timed_out {
                    if timed_out && state != tcp::State::Closed {
                        net.sockets.get_mut::<tcp::Socket>(handle).abort();
                    }
                    net.sockets.remove(handle);
                    SOCKETS_LIVE.fetch_sub(1, Ordering::Relaxed);
                    net.pending_removal.swap_remove(i);
                } else {
                    i += 1;
                }
            }

            // Bound `SynSent`. An entry leaves this list the moment the socket
            // is no longer shaking hands — connected, reset, or closed — so the
            // deadline never applies to an established connection. See
            // `CONNECT_TIMEOUT_US`.
            let mut i = 0;
            while i < net.connecting.len() {
                let (handle, started_at) = net.connecting[i];
                if !is_valid_handle(handle) {
                    net.connecting.swap_remove(i);
                    continue;
                }
                if net.sockets.get::<tcp::Socket>(handle).state() != tcp::State::SynSent {
                    net.connecting.swap_remove(i);
                    continue;
                }
                if now_us.saturating_sub(started_at) > CONNECT_TIMEOUT_US {
                    // `abort()` moves it to `Closed`, which surfaces to the
                    // caller as `EPOLLHUP`. The socket is flagged first so
                    // `SO_ERROR` can answer `ETIMEDOUT` rather than the
                    // `ECONNREFUSED` a plain `Closed` would imply — a connect
                    // that was never answered is not a connect that was refused,
                    // and telling them apart is the difference between reading a
                    // log correctly and not.
                    crate::socket::mark_connect_timed_out(handle);
                    net.sockets.get_mut::<tcp::Socket>(handle).abort();
                    net.connecting.swap_remove(i);
                    continue;
                }
                i += 1;
            }

            // Publish the set size: `iface.poll()` above walks all of it, so this
            // is the scaling term behind `poll_us`.
            // O(1): `SOCKETS_LIVE` is maintained on add/remove. It used to be
            // `iter().count()` here, which made the meter a material part of what
            // it measured — ~0.9 us/poll at 128 slots and ~14 us at 2048, which
            // inflated the first 2048-slot experiment.
            nicstat::record_sockets_live(sockets_live());

            if matches!(p1, PollResult::SocketStateChanged) {
                POLL_COUNT.fetch_add(1, Ordering::Release);
                true
            } else {
                false
            }
        } else {
            false
        };
        mark_release();
        drop(guard);
        POLL_EXITED.fetch_add(1, Ordering::Relaxed);
        result
    };
    // NETWORK lock is released here — safe to acquire SOCKET_TABLE.
    // Acquiring SOCKET_TABLE while holding NETWORK causes AB-BA deadlock
    // with socket_can_recv_tcp et al. which hold SOCKET_TABLE→NETWORK.
    //
    // The console is the same hazard and belongs in the same paragraph: printing
    // takes `CONSOLE_LOCK`, so a `log::` call inside the section above would
    // establish NETWORK -> CONSOLE. Everything recorded in there is emitted here
    // instead. See `DhcpReport`.
    match dhcp_report {
        Some(DhcpReport::Configured { addr, addr_full, loopback_full }) => {
            crate::safe_print!(32, "[SmolNet] DHCP configured\n");
            if addr_full {
                crate::safe_print!(80, "[SmolNet] could not install DHCP address: list full\n");
            }
            if loopback_full {
                crate::safe_print!(80, "[SmolNet] could not install loopback address: list full\n");
            }
            crate::safe_print!(64, "[SmolNet] IP: {addr}\n");
        }
        Some(DhcpReport::Deconfigured { fallback_full, loopback_full }) => {
            crate::safe_print!(80, "[SmolNet] DHCP deconfigured - reverting to static fallback\n");
            if fallback_full {
                crate::safe_print!(88, "[SmolNet] could not install static fallback address: list full\n");
            }
            if loopback_full {
                crate::safe_print!(80, "[SmolNet] could not install loopback address: list full\n");
            }
        }
        None => {}
    }
    if corrupt_handles > 0 {
        crate::safe_print!(72, "[NET] {corrupt_handles} corrupt handle(s) dropped in poll GC\n");
    }

    if socket_state_changed {
        // Walks EVERY socket slot (MAX_SOCKETS) taking each one's waker lock,
        // with SOCKET_TABLE held. Timed separately from the poll itself.
        let wake_t = nicstat::start();
        crate::socket::with_table(|table| {
            for slot in table.iter().flatten() {
                slot.wake_all();
            }
        });
        nicstat::record_poll_wake(wake_t);
    }

    nicstat::record_poll(poll_t, socket_state_changed);
    socket_state_changed
}

pub fn with_network<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut NetworkState) -> R,
{
    // Preemption disabled for the whole hold: the NETWORK spinlock must never be
    // stranded across a context switch under the BKL (see `PreemptGuard`).
    let _pg = PreemptGuard::new();
    let mut guard = NETWORK.lock();
    mark_acquire(NetSite::WithNetwork);
    let result = guard.as_mut().map(f);
    mark_release();
    drop(guard);
    result
}
