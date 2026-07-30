//! Pure SSH-2 channel-message builders: byte layout only, no I/O.
//!
//! Split out of `protocol.rs` so it can be unit-tested on the **host**. The rest
//! of sshd can't be: it reaches `libakuma`, which defines `#[panic_handler]` and
//! `#[global_allocator]` and therefore cannot link against a std target. Nothing
//! here touches `libakuma`, so with `--no-default-features` this module compiles
//! and tests natively:
//!
//! ```text
//! cargo test -p sshd --lib --no-default-features --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
//! ```
//!
//! `protocol.rs` keeps ownership of the session, crypto and socket; this module
//! owns only what the bytes must look like.

use akuma_ssh_crypto::crypto::{write_string, write_u32};
use alloc::vec;
use alloc::vec::Vec;

pub const SSH_MSG_CHANNEL_EOF: u8 = 96;
pub const SSH_MSG_CHANNEL_CLOSE: u8 = 97;
pub const SSH_MSG_CHANNEL_REQUEST: u8 = 98;

/// RFC 4254 §6.10 `exit-status` channel request — how the server tells the
/// client what the remote command's exit code was.
///
/// This message is why `ssh host cmd; echo $?` reports anything useful.
/// OpenSSH's client seeds its own exit status with **255** and overwrites it
/// only when this request arrives, so a server that omits it makes every
/// command — successful ones included — look like a connection failure.
///
/// Layout:
///
/// ```text
/// byte      SSH_MSG_CHANNEL_REQUEST (98)
/// uint32    recipient channel
/// string    "exit-status"
/// boolean   want_reply — MUST be false (§6.10 forbids a reply)
/// uint32    exit_status
/// ```
///
/// `exit_code` is masked to its low 8 bits, matching `waitpid`'s
/// `WEXITSTATUS`: a wait status carries no more than a byte of exit code.
pub fn exit_status_payload(channel: u32, exit_code: i32) -> Vec<u8> {
    let mut payload = vec![SSH_MSG_CHANNEL_REQUEST];
    write_u32(&mut payload, channel);
    write_string(&mut payload, b"exit-status");
    payload.push(0); // want_reply = false, required by §6.10
    write_u32(&mut payload, (exit_code & 0xFF) as u32);
    payload
}

/// RFC 4254 §6.10 signal name for a Linux/Akuma signal number — the value that
/// goes in an `exit-signal` request.
///
/// The spec lists bare names without the `SIG` prefix, and permits anything else
/// only in the local-extension form `sig-name@domain`. Signals outside that list
/// therefore get the extension form rather than being forced onto a wrong
/// standard name, so a client never displays a plausible lie like `TERM` for a
/// signal that wasn't `SIGTERM`.
pub fn signal_name(sig: u8) -> &'static str {
    match sig {
        1 => "HUP",
        2 => "INT",
        3 => "QUIT",
        4 => "ILL",
        6 => "ABRT",
        8 => "FPE",
        9 => "KILL",
        10 => "USR1",
        11 => "SEGV",
        12 => "USR2",
        13 => "PIPE",
        14 => "ALRM",
        15 => "TERM",
        _ => "UNKNOWN@akuma.os",
    }
}

/// RFC 4254 §6.10 `exit-signal` channel request — the counterpart to
/// `exit_status_payload` for a command **killed by a signal**.
///
/// A signal death has no exit code to report (`WEXITSTATUS` is 0, which would
/// read as success), so the spec gives it a separate message naming the signal.
/// Send this *instead of* `exit-status`, never both. OpenSSH's client responds
/// by reporting 255 and printing "Killed by signal N" — deliberately not a
/// small number, so a killed remote command can't be mistaken for a clean exit.
///
/// Layout:
///
/// ```text
/// byte      SSH_MSG_CHANNEL_REQUEST (98)
/// uint32    recipient channel
/// string    "exit-signal"
/// boolean   want_reply — MUST be false
/// string    signal name, no "SIG" prefix (e.g. "KILL")
/// boolean   core dumped
/// string    error message (UTF-8)
/// string    language tag
/// ```
///
/// `core_dumped` is always false: Akuma does not write core files.
pub fn exit_signal_payload(channel: u32, sig: u8, message: &str) -> Vec<u8> {
    let mut payload = vec![SSH_MSG_CHANNEL_REQUEST];
    write_u32(&mut payload, channel);
    write_string(&mut payload, b"exit-signal");
    payload.push(0); // want_reply = false, required by §6.10
    write_string(&mut payload, signal_name(sig).as_bytes());
    payload.push(0); // core_dumped = false
    write_string(&mut payload, message.as_bytes());
    write_string(&mut payload, b""); // language tag: none
    payload
}

/// RFC 4254 §5.3 `SSH_MSG_CHANNEL_EOF`: no more data will be sent on this
/// channel. Sent before the close so the client knows the output is complete.
pub fn channel_eof_payload(channel: u32) -> Vec<u8> {
    let mut payload = vec![SSH_MSG_CHANNEL_EOF];
    write_u32(&mut payload, channel);
    payload
}

/// RFC 4254 §5.3 `SSH_MSG_CHANNEL_CLOSE`: the channel is finished. The client
/// returns once it sees this, using the status from `exit_status_payload`.
pub fn channel_close_payload(channel: u32) -> Vec<u8> {
    let mut payload = vec![SSH_MSG_CHANNEL_CLOSE];
    write_u32(&mut payload, channel);
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use akuma_ssh_crypto::crypto::{read_string, read_u32};

    /// Full byte-for-byte layout of an `exit-status` request, which is the part
    /// a real client actually parses. Spelled out literally rather than rebuilt
    /// with the same helpers the code under test uses, so a change in framing
    /// has to fail here.
    #[test]
    fn exit_status_payload_has_exact_rfc_layout() {
        assert_eq!(
            exit_status_payload(0, 0),
            vec![
                98, // SSH_MSG_CHANNEL_REQUEST
                0, 0, 0, 0, // recipient channel = 0
                0, 0, 0, 11, // string length of "exit-status"
                b'e', b'x', b'i', b't', b'-', b's', b't', b'a', b't', b'u', b's',
                0, // want_reply = false
                0, 0, 0, 0, // exit_status = 0
            ]
        );
    }

    /// want_reply must stay false: §6.10 forbids replying to `exit-status`, and
    /// a client that got asked for one would treat it as a protocol error.
    #[test]
    fn exit_status_never_requests_a_reply() {
        for code in [0, 1, 42, 127, 255] {
            let payload = exit_status_payload(7, code);
            // The byte after the "exit-status" string: 1 tag + 4 channel
            // + 4 length + 11 name.
            assert_eq!(payload[20], 0, "want_reply set for code {code}");
        }
    }

    /// Round-trip through the readers a client would use, over codes that cover
    /// success, shell failure, "command not found" and the top of the range.
    #[test]
    fn exit_status_round_trips_channel_and_code() {
        for (channel, code) in [(0u32, 0i32), (1, 1), (7, 42), (3, 127), (0xDEADBEEF, 255)] {
            let payload = exit_status_payload(channel, code);
            let mut off = 1; // skip the message tag
            assert_eq!(read_u32(&payload, &mut off), Some(channel));
            assert_eq!(read_string(&payload, &mut off), Some(&b"exit-status"[..]));
            off += 1; // want_reply
            assert_eq!(read_u32(&payload, &mut off), Some(code as u32));
            assert_eq!(off, payload.len(), "trailing bytes after exit_status");
        }
    }

    /// The wire field is one byte wide (`WEXITSTATUS`), so anything outside
    /// 0..=255 must be masked rather than silently truncated by a cast — 256
    /// in particular has to arrive as 0, not as a corrupt 4-byte value.
    #[test]
    fn exit_status_masks_code_to_low_byte() {
        let cases = [(256i32, 0u32), (257, 1), (-1, 255), (300, 44), (0x1FF, 255)];
        for (code, want) in cases {
            let payload = exit_status_payload(0, code);
            let mut off = 1;
            let _ = read_u32(&payload, &mut off);
            let _ = read_string(&payload, &mut off);
            off += 1;
            assert_eq!(read_u32(&payload, &mut off), Some(want), "code {code}");
        }
    }

    /// Full layout of an `exit-signal` request, including both trailing strings
    /// — a client that parses the fields in order will desync if either the
    /// core-dumped boolean or the language tag is dropped.
    #[test]
    fn exit_signal_payload_has_exact_rfc_layout() {
        let payload = exit_signal_payload(0, 9, "hi");
        let mut want = vec![98u8];
        want.extend_from_slice(&[0, 0, 0, 0]); // channel
        want.extend_from_slice(&[0, 0, 0, 11]); // "exit-signal"
        want.extend_from_slice(b"exit-signal");
        want.push(0); // want_reply = false
        want.extend_from_slice(&[0, 0, 0, 4]); // "KILL"
        want.extend_from_slice(b"KILL");
        want.push(0); // core_dumped = false
        want.extend_from_slice(&[0, 0, 0, 2]); // "hi"
        want.extend_from_slice(b"hi");
        want.extend_from_slice(&[0, 0, 0, 0]); // language tag = ""
        assert_eq!(payload, want);
    }

    #[test]
    fn exit_signal_round_trips_every_field() {
        let payload = exit_signal_payload(3, 11, "boom");
        let mut off = 1;
        assert_eq!(read_u32(&payload, &mut off), Some(3));
        assert_eq!(read_string(&payload, &mut off), Some(&b"exit-signal"[..]));
        assert_eq!(payload[off], 0, "want_reply must be false");
        off += 1;
        assert_eq!(read_string(&payload, &mut off), Some(&b"SEGV"[..]));
        assert_eq!(payload[off], 0, "core_dumped must be false");
        off += 1;
        assert_eq!(read_string(&payload, &mut off), Some(&b"boom"[..]));
        assert_eq!(read_string(&payload, &mut off), Some(&b""[..]));
        assert_eq!(off, payload.len(), "trailing bytes after exit_signal");
    }

    /// The names RFC 4254 §6.10 actually lists, spelled without the SIG prefix.
    #[test]
    fn signal_names_match_the_rfc_list() {
        let expected = [
            (1u8, "HUP"), (2, "INT"), (3, "QUIT"), (4, "ILL"), (6, "ABRT"),
            (8, "FPE"), (9, "KILL"), (10, "USR1"), (11, "SEGV"), (12, "USR2"),
            (13, "PIPE"), (14, "ALRM"), (15, "TERM"),
        ];
        for (sig, name) in expected {
            assert_eq!(signal_name(sig), name, "signal {sig}");
            assert!(!name.starts_with("SIG"), "{name} must not carry the SIG prefix");
        }
    }

    /// A signal with no standard name must use the `@domain` extension form
    /// rather than borrowing a standard name it isn't.
    #[test]
    fn unlisted_signals_use_the_extension_form() {
        for sig in [5u8, 7, 16, 31, 64] {
            let name = signal_name(sig);
            assert!(name.contains('@'), "signal {sig} name {name:?} must be an extension");
        }
        // And must never collide with a listed name.
        for sig in [5u8, 7, 16, 31, 64] {
            for listed in ["HUP", "INT", "QUIT", "ILL", "ABRT", "FPE", "KILL",
                           "USR1", "SEGV", "USR2", "PIPE", "ALRM", "TERM"] {
                assert_ne!(signal_name(sig), listed);
            }
        }
    }

    /// `exit-status` and `exit-signal` share the CHANNEL_REQUEST tag, so the
    /// request-name string is the only thing distinguishing them on the wire.
    #[test]
    fn exit_status_and_exit_signal_differ_by_request_name() {
        let status = exit_status_payload(1, 0);
        let signal = exit_signal_payload(1, 9, "");
        assert_eq!(status[0], signal[0], "both are CHANNEL_REQUEST");
        let mut so = 1;
        let mut go = 1;
        let _ = read_u32(&status, &mut so);
        let _ = read_u32(&signal, &mut go);
        assert_eq!(read_string(&status, &mut so), Some(&b"exit-status"[..]));
        assert_eq!(read_string(&signal, &mut go), Some(&b"exit-signal"[..]));
    }

    #[test]
    fn channel_eof_and_close_are_tag_plus_channel() {
        assert_eq!(channel_eof_payload(0), vec![96, 0, 0, 0, 0]);
        assert_eq!(channel_close_payload(0), vec![97, 0, 0, 0, 0]);
        assert_eq!(channel_eof_payload(0x01020304), vec![96, 1, 2, 3, 4]);
        assert_eq!(channel_close_payload(0x01020304), vec![97, 1, 2, 3, 4]);
    }

    /// The three messages are distinct types; a copy-paste slip that made the
    /// close look like an EOF would hang the client waiting for a close.
    #[test]
    fn teardown_messages_have_distinct_tags() {
        let ch = 5;
        let tags = [
            exit_status_payload(ch, 0)[0],
            channel_eof_payload(ch)[0],
            channel_close_payload(ch)[0],
        ];
        assert_eq!(tags, [98, 96, 97]);
    }
}
