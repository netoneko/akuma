#[cfg(test)]
mod dns_tests {
    use crate::dns::DnsError;

    #[test]
    fn dns_error_messages() {
        assert_eq!(DnsError::LookupFailed.as_str(), "DNS lookup failed");
        assert_eq!(DnsError::NoConfig.as_str(), "Network not configured");
        assert_eq!(DnsError::InvalidHost.as_str(), "Invalid hostname");
        assert_eq!(DnsError::Timeout.as_str(), "DNS query timed out");
    }
}

#[cfg(test)]
mod socket_addr_tests {
    use crate::socket::{SocketAddrV4, SockAddrIn};

    #[test]
    fn socket_addr_v4_new() {
        let addr = SocketAddrV4::new([192, 168, 1, 1], 8080);
        assert_eq!(addr.ip, [192, 168, 1, 1]);
        assert_eq!(addr.port, 8080);
    }

    #[test]
    fn socket_addr_v4_loopback() {
        let addr = SocketAddrV4::new([127, 0, 0, 1], 22);
        assert_eq!(addr.ip, [127, 0, 0, 1]);
        assert_eq!(addr.port, 22);
    }

    #[test]
    fn sock_addr_in_roundtrip() {
        let original = SocketAddrV4::new([10, 0, 2, 15], 443);
        let sock_in = SockAddrIn::from_addr(&original);
        let converted = sock_in.to_addr();
        assert_eq!(original, converted);
    }

    #[test]
    fn sock_addr_in_network_byte_order() {
        let addr = SocketAddrV4::new([192, 168, 1, 1], 0x1234);
        let sock_in = SockAddrIn::from_addr(&addr);
        
        // Port should be big-endian
        assert_eq!(sock_in.sin_port, 0x1234u16.to_be());
        
        // Family should be AF_INET (2)
        assert_eq!(sock_in.sin_family, 2);
    }

    #[test]
    fn sock_addr_in_zero_port() {
        let addr = SocketAddrV4::new([0, 0, 0, 0], 0);
        let sock_in = SockAddrIn::from_addr(&addr);
        let converted = sock_in.to_addr();
        assert_eq!(converted.port, 0);
        assert_eq!(converted.ip, [0, 0, 0, 0]);
    }

    #[test]
    fn sock_addr_in_max_port() {
        let addr = SocketAddrV4::new([255, 255, 255, 255], 65535);
        let sock_in = SockAddrIn::from_addr(&addr);
        let converted = sock_in.to_addr();
        assert_eq!(converted.port, 65535);
        assert_eq!(converted.ip, [255, 255, 255, 255]);
    }
}

#[cfg(test)]
mod net_holder_tests {
    use crate::smoltcp_net::{network_holder_snapshot, NetSite, NETWORK_HOLDER_NONE};

    #[test]
    fn netsite_round_trips() {
        for v in 0u8..=5 {
            let site = NetSite::from_u8(v);
            // Re-encoding NetSite::None for unknown values is intentional.
            let expect_v = if v > 4 { 0 } else { v };
            assert_eq!(site as u8, expect_v, "round-trip mismatch for {v}");
        }
    }

    #[test]
    fn netsite_strings_are_stable() {
        assert_eq!(NetSite::None.as_str(), "none");
        assert_eq!(NetSite::Poll.as_str(), "poll");
        assert_eq!(NetSite::WithNetwork.as_str(), "with_network");
        assert_eq!(NetSite::SocketClose.as_str(), "socket_close");
        assert_eq!(NetSite::UdpSocketClose.as_str(), "udp_socket_close");
    }

    #[test]
    fn snapshot_reports_idle_before_any_acquire() {
        // No runtime is registered in the host test harness, so this only
        // verifies the static atomics' initial values. The kernel side
        // exercises real acquire/release via ssh_tests::test_poll_entered_exited_balanced.
        let (holder, _locked_at, site, polls_in, polls_out) = network_holder_snapshot();
        assert_eq!(holder, NETWORK_HOLDER_NONE);
        assert_eq!(site, NetSite::None);
        assert_eq!(polls_in, 0);
        assert_eq!(polls_out, 0);
    }
}


#[cfg(test)]
mod errno_tests {
    use crate::socket::libc_errno;

    /// Verify errno values match Linux AArch64 definitions.
    /// These must be exact to maintain ABI compatibility with musl/glibc.
    #[test]
    fn errno_values_match_linux() {
        assert_eq!(libc_errno::EPERM, 1);
        assert_eq!(libc_errno::ENOENT, 2);
        assert_eq!(libc_errno::ESRCH, 3);
        assert_eq!(libc_errno::EINTR, 4);
        assert_eq!(libc_errno::EIO, 5);
        assert_eq!(libc_errno::ENOEXEC, 8);
        assert_eq!(libc_errno::EBADF, 9);
        assert_eq!(libc_errno::ECHILD, 10);
        assert_eq!(libc_errno::EAGAIN, 11);
        assert_eq!(libc_errno::ENOMEM, 12);
        assert_eq!(libc_errno::EACCES, 13);
        assert_eq!(libc_errno::EFAULT, 14);
        assert_eq!(libc_errno::EEXIST, 17);
        assert_eq!(libc_errno::EINVAL, 22);
        assert_eq!(libc_errno::EMFILE, 24);
        assert_eq!(libc_errno::EPIPE, 32);
        assert_eq!(libc_errno::ERANGE, 34);
        assert_eq!(libc_errno::EDESTADDRREQ, 89);
        assert_eq!(libc_errno::EADDRINUSE, 98);
        assert_eq!(libc_errno::ENETDOWN, 100);
        assert_eq!(libc_errno::ECONNABORTED, 103);
        assert_eq!(libc_errno::ENOTCONN, 107);
        assert_eq!(libc_errno::ETIMEDOUT, 110);
        assert_eq!(libc_errno::ECONNREFUSED, 111);
        assert_eq!(libc_errno::EINPROGRESS, 115);
    }
}

#[cfg(test)]
mod socket_constants_tests {
    use crate::socket::{socket_const, EPHEMERAL_PORT_START, EPHEMERAL_PORT_END, MAX_SOCKETS};

    #[test]
    fn socket_type_constants() {
        assert_eq!(socket_const::AF_INET, 2);
        assert_eq!(socket_const::SOCK_STREAM, 1);
        assert_eq!(socket_const::SOCK_DGRAM, 2);
    }

    #[test]
    fn ephemeral_port_range_valid() {
        // IANA ephemeral port range
        assert!(EPHEMERAL_PORT_START >= 49152);
        // EPHEMERAL_PORT_END is u16, so always <= 65535
        assert_eq!(EPHEMERAL_PORT_END, 65535);
        assert!(EPHEMERAL_PORT_START < EPHEMERAL_PORT_END);
    }

    #[test]
    fn max_sockets_reasonable() {
        // Should support at least a modest number of concurrent connections
        assert!(MAX_SOCKETS >= 64);
        // But not be unreasonably large for embedded/kernel use
        assert!(MAX_SOCKETS <= 1024);
    }
}

/// The `connect(2)` state machine and the `bind` port rule.
///
/// These are the two bugs that made `redis-cli` unable to reach a `redis-server`
/// on the same box (docs/archive/LONG_ROAD_TO_REDIS.md §9): a redial reported
/// ECONNREFUSED, and a port-0 bind poisoned the following connect. Both were
/// invisible because every connect failure returned the same errno, so these
/// tests pin the *distinctions* as much as the happy path.
#[cfg(all(test, feature = "smoltcp"))]
mod connect_state_tests {
    use crate::socket::{bind_port_for, connect_outcome, connect_step, ConnectStep, libc_errno};
    use smoltcp::socket::tcp::State;

    #[test]
    fn a_fresh_socket_dials() {
        assert_eq!(connect_step(State::Closed), ConnectStep::Dial);
    }

    /// The redial that used to fail. hiredis (so `redis-cli`) issues
    /// connect -> EINPROGRESS -> poll -> connect, and that second call must not
    /// reach `smoltcp::connect`, which rejects a non-Closed socket outright.
    #[test]
    fn a_connect_in_flight_is_not_redialled() {
        assert_eq!(connect_step(State::SynSent), ConnectStep::InProgress);
        assert_eq!(connect_step(State::SynReceived), ConnectStep::InProgress);
    }

    #[test]
    fn an_established_socket_reports_success_not_refused() {
        assert_eq!(connect_step(State::Established), ConnectStep::AlreadyConnected);
    }

    /// A half-closed socket is reusable for a fresh dial, not "in progress" —
    /// classifying these as InProgress would hang a caller forever waiting on a
    /// connection that is going away.
    #[test]
    fn teardown_states_dial_again() {
        for state in [
            State::CloseWait,
            State::Closing,
            State::FinWait1,
            State::FinWait2,
            State::LastAck,
            State::TimeWait,
            State::Listen,
        ] {
            assert_eq!(connect_step(state), ConnectStep::Dial, "{state:?}");
        }
    }

    #[test]
    fn a_connection_that_came_up_is_ok_even_if_the_wait_reported_an_error() {
        // The socket can establish in the same poll round that the deadline
        // expires; the state is the authority, not the timer.
        assert_eq!(
            connect_outcome(Err(libc_errno::ETIMEDOUT), Some(State::Established)),
            Ok(())
        );
        assert_eq!(connect_outcome(Ok(()), Some(State::Established)), Ok(()));
    }

    /// The distinction that was missing: a socket that reached Closed was
    /// refused; one still half-open at the deadline timed out. Both used to
    /// report ECONNREFUSED, which is what hid the port-0 bind bug.
    #[test]
    fn refused_and_timed_out_are_different_errnos() {
        assert_eq!(
            connect_outcome(Ok(()), Some(State::Closed)),
            Err(libc_errno::ECONNREFUSED)
        );
        assert_eq!(
            connect_outcome(Ok(()), Some(State::SynSent)),
            Err(libc_errno::ETIMEDOUT)
        );
    }

    #[test]
    fn an_interrupted_wait_keeps_its_own_errno() {
        assert_eq!(
            connect_outcome(Err(libc_errno::EINTR), Some(State::SynSent)),
            Err(libc_errno::EINTR)
        );
    }

    /// No network at all is ENETDOWN, not a connection error.
    #[test]
    fn a_missing_stack_is_enetdown() {
        assert_eq!(connect_outcome(Ok(()), None), Err(libc_errno::ENETDOWN));
    }

    #[test]
    fn an_explicit_bind_port_is_kept_and_costs_no_ephemeral() {
        let mut allocated = false;
        let port = bind_port_for(8080, || {
            allocated = true;
            49152
        });
        assert_eq!(port, 8080);
        assert!(!allocated, "an explicit port must not consume an ephemeral");
    }

    /// Port 0 means "pick one for me". Storing the literal 0 is what made the
    /// next connect hand smoltcp `local_port = 0` and get EADDRNOTAVAIL.
    #[test]
    fn a_zero_bind_port_becomes_an_ephemeral() {
        assert_eq!(bind_port_for(0, || 49152), 49152);
        assert_ne!(bind_port_for(0, || 49152), 0);
    }
}
