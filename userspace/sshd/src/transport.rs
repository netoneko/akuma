//! Pure transport-layer rules: which message may arrive in which phase of a
//! connection, what a well-formed binary packet looks like, and which channel
//! requests the server actually serves. No I/O, so host-testable like
//! `wire.rs` and `session_policy.rs`:
//!
//! ```text
//! cargo test -p sshd --lib --no-default-features --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
//! ```
//!
//! Split out of `protocol.rs` (2026-11-16, `docs/ISSUES_2026_11_16.md` S1-S3)
//! because the daemon used to dispatch every message whatever state the
//! connection was in: a client that finished the key exchange could open a
//! channel and send `exec` without ever authenticating, and sshd ran the
//! command as root. The table below is the gate `handle_message` now asks
//! before it acts on anything.

/// Where a connection is in RFC 4253/4252/4254's sequence. Mirrors
/// `protocol.rs`'s `SshState` one to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Before the client's version line.
    Version,
    /// Waiting for the client's `KEXINIT`.
    KexInit,
    /// Waiting for `KEX_ECDH_INIT`.
    KexEcdhInit,
    /// Our `NEWKEYS` is out; waiting for the client's.
    NewKeys,
    /// Keys are in place; waiting for `SERVICE_REQUEST ssh-userauth`.
    ServiceRequest,
    /// Waiting for a successful `USERAUTH_REQUEST`.
    UserAuth,
    /// Authenticated: the connection protocol (RFC 4254) is open.
    Authenticated,
}

pub const MSG_DISCONNECT: u8 = 1;
pub const MSG_IGNORE: u8 = 2;
pub const MSG_UNIMPLEMENTED: u8 = 3;
pub const MSG_DEBUG: u8 = 4;
pub const MSG_SERVICE_REQUEST: u8 = 5;
pub const MSG_KEXINIT: u8 = 20;
pub const MSG_NEWKEYS: u8 = 21;
pub const MSG_KEX_ECDH_INIT: u8 = 30;
pub const MSG_USERAUTH_REQUEST: u8 = 50;
/// RFC 4254 §4/§5: everything from `GLOBAL_REQUEST` (80) to
/// `CHANNEL_FAILURE` (100) is the connection protocol.
pub const MSG_CONNECTION_FIRST: u8 = 80;
pub const MSG_CONNECTION_LAST: u8 = 100;

/// Whether a message of type `msg` may be acted on in `phase`.
///
/// `false` means the connection must be closed: every arrival here that is
/// out of order is either a broken client or an attack, and the dangerous
/// one is the connection protocol (80..=100) before [`Phase::Authenticated`]
/// — which is exactly what the daemon used to serve.
///
/// Transport-generic messages (`DISCONNECT`, `IGNORE`, `UNIMPLEMENTED`,
/// `DEBUG`) are legal in every phase after the version exchange (RFC 4253
/// §11). A second `KEXINIT` after the first key exchange is a re-key, which
/// this server does not implement; it is refused here so the caller can say
/// so instead of misparsing the client's next encrypted packet as plaintext.
#[must_use]
pub const fn message_allowed(phase: Phase, msg: u8) -> bool {
    if matches!(phase, Phase::Version) {
        return false;
    }
    if matches!(msg, MSG_DISCONNECT | MSG_IGNORE | MSG_UNIMPLEMENTED | MSG_DEBUG) {
        return true;
    }
    match phase {
        Phase::Version => false,
        Phase::KexInit => msg == MSG_KEXINIT,
        Phase::KexEcdhInit => msg == MSG_KEX_ECDH_INIT,
        Phase::NewKeys => msg == MSG_NEWKEYS,
        Phase::ServiceRequest => msg == MSG_SERVICE_REQUEST,
        Phase::UserAuth => msg == MSG_USERAUTH_REQUEST,
        Phase::Authenticated => msg >= MSG_CONNECTION_FIRST && msg <= MSG_CONNECTION_LAST,
    }
}

/// The one service a client may ask for before authenticating (RFC 4252 §3).
pub const USERAUTH_SERVICE: &[u8] = b"ssh-userauth";

/// The largest `packet_length` accepted, OpenSSH's `PACKET_MAX_SIZE`
/// (256 KiB). RFC 4253 §6.1 requires 35000; anything bigger than this is a
/// client trying to make the server buffer it.
///
/// Before this bound the length field was trusted outright: a pre-auth
/// client could announce a 4 GiB packet and stream bytes at the server,
/// which buffered all of them waiting for the end.
pub const MAX_PACKET_LEN: usize = 256 * 1024;

/// Why a packet was refused. Any of these ends the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    /// `packet_length` is larger than [`MAX_PACKET_LEN`].
    TooLong,
    /// `padding_length` leaves no room for the message-type byte.
    BadPadding,
    /// The MAC did not verify.
    BadMac,
}

impl PacketError {
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::TooLong => "packet length over the 256 KiB limit",
            Self::BadPadding => "padding length leaves no payload",
            Self::BadMac => "MAC verification failed",
        }
    }
}

/// Check a packet's announced length before buffering it.
pub const fn check_packet_len(packet_len: usize) -> Result<(), PacketError> {
    if packet_len > MAX_PACKET_LEN { Err(PacketError::TooLong) } else { Ok(()) }
}

/// The payload length of a packet whose `packet_length` and `padding_length`
/// fields are as given (RFC 4253 §6: `packet_length = 1 + payload + padding`).
///
/// The payload is at least one byte, the message type. A `padding_length`
/// equal to `packet_length - 1` used to pass the old check and give an empty
/// payload, and the slice `[6..5]` that followed panicked the session process.
pub const fn payload_len(packet_len: usize, padding_len: usize) -> Result<usize, PacketError> {
    if packet_len > MAX_PACKET_LEN {
        return Err(PacketError::TooLong);
    }
    // 1 byte of padding_length itself, at least 1 byte of payload.
    if padding_len + 2 > packet_len {
        return Err(PacketError::BadPadding);
    }
    Ok(packet_len - padding_len - 1)
}

/// RFC 4253 §4.2: the identification line is at most 255 bytes, CR LF
/// included. A client that sends more without a newline is refused rather
/// than buffered until the grace timer runs out.
pub const MAX_VERSION_LINE: usize = 255;

/// The channel type this server serves (RFC 4254 §6.1). Everything else
/// (`direct-tcpip` from `ssh -L`, `x11`, `auth-agent@openssh.com`) gets an
/// `OPEN_FAILURE` instead of being confirmed as if it were a session.
pub const SESSION_CHANNEL: &[u8] = b"session";

/// RFC 4254 §5.1 `SSH_OPEN_UNKNOWN_CHANNEL_TYPE`.
pub const OPEN_UNKNOWN_CHANNEL_TYPE: u32 = 3;
/// RFC 4254 §5.1 `SSH_OPEN_RESOURCE_SHORTAGE` — a second channel on one
/// connection (OpenSSH `ControlMaster` multiplexing): one session per
/// connection is what the daemon serves.
pub const OPEN_RESOURCE_SHORTAGE: u32 = 4;

/// Channel requests the server acts on, before the shell or command starts.
/// `env` is accepted and ignored, as OpenSSH does with its default
/// `AcceptEnv` (nothing): the client's `SendEnv LANG LC_*` must not fail the
/// session.
///
/// Anything else — `subsystem` (`sftp`, which modern `scp` uses), `x11-req`,
/// `auth-agent-req@openssh.com` — used to be answered `CHANNEL_SUCCESS` and
/// then ignored, so `scp` waited for an SFTP server that never spoke until
/// the idle watch killed it. It now gets `CHANNEL_FAILURE` and the client
/// reports it at once.
#[must_use]
pub fn channel_request_supported(req: &[u8]) -> bool {
    matches!(req, b"shell" | b"exec" | b"pty-req" | b"env" | b"window-change")
}

/// The bytes the bridge may read from the child's stdout in one go, given
/// the client's remaining receive window and the read buffer's size.
///
/// A pty session's output is cooked (`\n` -> `\r\n`), which can double it,
/// so only half the window is offered when `cook` is set. Zero means "wait
/// for a `WINDOW_ADJUST`": RFC 4254 §5.2 forbids sending past the window, and
/// OpenSSH answers a server that does with `channel 0: rcvd too much data`
/// and drops the excess.
#[must_use]
pub fn stdout_read_budget(remote_window: u32, buf_len: usize, cook: bool) -> usize {
    let w = remote_window as usize;
    let w = if cook { w / 2 } else { w };
    w.min(buf_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Phase; 7] = [
        Phase::Version,
        Phase::KexInit,
        Phase::KexEcdhInit,
        Phase::NewKeys,
        Phase::ServiceRequest,
        Phase::UserAuth,
        Phase::Authenticated,
    ];

    #[test]
    fn the_connection_protocol_needs_authentication() {
        // The 2026-11-16 bypass: CHANNEL_OPEN (90) and CHANNEL_REQUEST (98)
        // were served right after NEWKEYS.
        for phase in ALL {
            for msg in MSG_CONNECTION_FIRST..=MSG_CONNECTION_LAST {
                assert_eq!(
                    message_allowed(phase, msg),
                    phase == Phase::Authenticated,
                    "{phase:?} msg {msg}"
                );
            }
        }
    }

    #[test]
    fn each_handshake_step_takes_exactly_its_message() {
        let steps = [
            (Phase::KexInit, MSG_KEXINIT),
            (Phase::KexEcdhInit, MSG_KEX_ECDH_INIT),
            (Phase::NewKeys, MSG_NEWKEYS),
            (Phase::ServiceRequest, MSG_SERVICE_REQUEST),
            (Phase::UserAuth, MSG_USERAUTH_REQUEST),
        ];
        for (phase, want) in steps {
            for msg in 5..=255u8 {
                assert_eq!(message_allowed(phase, msg), msg == want, "{phase:?} msg {msg}");
            }
        }
    }

    #[test]
    fn rekey_and_late_auth_are_refused_after_authentication() {
        assert!(!message_allowed(Phase::Authenticated, MSG_KEXINIT));
        assert!(!message_allowed(Phase::Authenticated, MSG_USERAUTH_REQUEST));
        assert!(!message_allowed(Phase::Authenticated, MSG_NEWKEYS));
    }

    #[test]
    fn transport_generic_messages_are_always_fine_after_the_version() {
        for phase in ALL.into_iter().skip(1) {
            for msg in [MSG_DISCONNECT, MSG_IGNORE, MSG_UNIMPLEMENTED, MSG_DEBUG] {
                assert!(message_allowed(phase, msg), "{phase:?} msg {msg}");
            }
        }
        assert!(!message_allowed(Phase::Version, MSG_DISCONNECT));
    }

    #[test]
    fn packet_lengths() {
        assert_eq!(check_packet_len(MAX_PACKET_LEN), Ok(()));
        assert_eq!(check_packet_len(MAX_PACKET_LEN + 1), Err(PacketError::TooLong));
        assert_eq!(check_packet_len(u32::MAX as usize), Err(PacketError::TooLong));
    }

    #[test]
    fn payload_has_room_for_the_message_type() {
        // packet_length 12, padding 4: 1 + 7 + 4.
        assert_eq!(payload_len(12, 4), Ok(7));
        // Exactly the type byte.
        assert_eq!(payload_len(6, 4), Ok(1));
        // The case that panicked: padding = packet_length - 1, empty payload.
        assert_eq!(payload_len(5, 4), Err(PacketError::BadPadding));
        assert_eq!(payload_len(4, 4), Err(PacketError::BadPadding));
        assert_eq!(payload_len(0, 0), Err(PacketError::BadPadding));
        assert_eq!(payload_len(1, 0), Err(PacketError::BadPadding));
        assert_eq!(payload_len(2, 0), Ok(1));
        assert_eq!(payload_len(MAX_PACKET_LEN + 1, 4), Err(PacketError::TooLong));
    }

    #[test]
    fn channel_requests() {
        for ok in [&b"shell"[..], b"exec", b"pty-req", b"env", b"window-change"] {
            assert!(channel_request_supported(ok));
        }
        for no in [&b"subsystem"[..], b"x11-req", b"auth-agent-req@openssh.com", b"signal", b""] {
            assert!(!channel_request_supported(no));
        }
    }

    #[test]
    fn stdout_budget_respects_the_window() {
        assert_eq!(stdout_read_budget(0, 1024, false), 0);
        assert_eq!(stdout_read_budget(100, 1024, false), 100);
        assert_eq!(stdout_read_budget(1 << 20, 1024, false), 1024);
        // Cooking can double the bytes: offer half.
        assert_eq!(stdout_read_budget(100, 1024, true), 50);
        assert_eq!(stdout_read_budget(1, 1024, true), 0);
        assert_eq!(stdout_read_budget(u32::MAX, 1024, true), 1024);
    }
}
