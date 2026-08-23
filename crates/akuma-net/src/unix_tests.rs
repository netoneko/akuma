//! Host tests for the AF_UNIX state machine (`crate::unix`).
//!
//! These exist because the two defects the AF_UNIX audit found —
//! `SOCK_SEQPACKET` silently merging messages, and `sendmsg` writing only the
//! first iovec — were both *silent*: the caller got a plausible short count and
//! no error. Neither is detectable from a kernel log, and both survived for as
//! long as they did because nothing could assert the property without booting a
//! VM. Every test below is written against a specific way to lose or duplicate
//! user data, and the ones that would be invisible in production are marked as
//! such in their doc comments.
//!
//! Run: `cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)`

use crate::socket::libc_errno;
use crate::unix::{
    AF_UNIX, Channel, ConnectOutcome, DEFAULT_BACKLOG, MAX_BACKLOG, Pending, Record, Shutdown,
    SOCKADDR_UN_LEN, SUN_PATH_LEN, SUN_PATH_OFFSET, SockAddrUn, SockState, SockType, Ucred,
    UnixName, UnixTable, plan_read, plan_write,
};
use alloc::vec;
use alloc::vec::Vec;

// ============================================================================
// Helpers
// ============================================================================

/// Build the raw bytes userspace would pass for a pathname bind, and the
/// `addrlen` it would pass with them. Deliberately *not* using
/// `SockAddrUn::encode` — a codec tested against its own output proves nothing.
fn raw_path(path: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&AF_UNIX.to_ne_bytes());
    v.extend_from_slice(path);
    v.push(0);
    v
}

fn raw_abstract(name: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&AF_UNIX.to_ne_bytes());
    v.push(0);
    v.extend_from_slice(name);
    v
}

fn creds(pid: u32) -> Ucred {
    Ucred { pid, uid: 1000 + pid, gid: 100 }
}

// ============================================================================
// sockaddr_un codec
// ============================================================================

mod addr {
    use super::*;

    #[test]
    fn decode_pathname() {
        let raw = raw_path(b"/tmp/probe.sock");
        assert_eq!(
            SockAddrUn::decode(&raw).unwrap(),
            UnixName::Path(b"/tmp/probe.sock".to_vec())
        );
    }

    /// `addrlen == 2` is "unnamed", not "the empty path". An unbound
    /// `socketpair` endpoint reports exactly this from `getsockname`, and
    /// treating it as a zero-length pathname would make `bind` create a file
    /// named "".
    #[test]
    fn decode_addrlen_two_is_unnamed() {
        let raw = AF_UNIX.to_ne_bytes();
        assert_eq!(SockAddrUn::decode(&raw).unwrap(), UnixName::Unnamed);
    }

    #[test]
    fn decode_rejects_short_and_wrong_family() {
        assert_eq!(SockAddrUn::decode(&[]).unwrap_err(), libc_errno::EINVAL);
        assert_eq!(SockAddrUn::decode(&[1]).unwrap_err(), libc_errno::EINVAL);
        // AF_INET
        let raw = 2u16.to_ne_bytes();
        assert_eq!(
            SockAddrUn::decode(&raw).unwrap_err(),
            libc_errno::EAFNOSUPPORT
        );
    }

    /// A path that fills all 108 bytes of `sun_path` with **no terminator** is
    /// legal on Linux. This is why the decoder cannot be a `CStr` conversion:
    /// a NUL scan with no bound reads past the caller's buffer.
    #[test]
    fn decode_unterminated_path_stops_at_addrlen() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&AF_UNIX.to_ne_bytes());
        raw.extend(core::iter::repeat_n(b'x', SUN_PATH_LEN));
        assert_eq!(raw.len(), SOCKADDR_UN_LEN);
        let name = SockAddrUn::decode(&raw).unwrap();
        assert_eq!(name, UnixName::Path(vec![b'x'; SUN_PATH_LEN]));
    }

    /// An `addrlen` larger than `sizeof(sockaddr_un)` must be clamped, not
    /// trusted. Linux ignores the excess; reading it would be an out-of-bounds
    /// read driven entirely by a userspace integer.
    #[test]
    fn decode_clamps_oversized_addrlen() {
        let mut raw = raw_path(b"/tmp/s");
        raw.resize(SOCKADDR_UN_LEN + 64, b'Z');
        let name = SockAddrUn::decode(&raw).unwrap();
        // The trailing Zs past the NUL are not part of the name.
        assert_eq!(name, UnixName::Path(b"/tmp/s".to_vec()));
    }

    #[test]
    fn decode_abstract() {
        let raw = raw_abstract(b"akuma-probe");
        assert_eq!(
            SockAddrUn::decode(&raw).unwrap(),
            UnixName::Abstract(b"akuma-probe".to_vec())
        );
    }

    /// An abstract name is delimited by `addrlen`, **not** by a NUL — it may
    /// contain NULs, and `strlen` would truncate it to nothing. Two distinct
    /// abstract names that share a prefix up to a NUL must stay distinct, or
    /// one daemon can hijack another's socket.
    #[test]
    fn decode_abstract_keeps_embedded_nuls() {
        let a = SockAddrUn::decode(&raw_abstract(b"a\0b")).unwrap();
        let b = SockAddrUn::decode(&raw_abstract(b"a\0c")).unwrap();
        assert_eq!(a, UnixName::Abstract(b"a\0b".to_vec()));
        assert_ne!(a, b, "embedded NULs were truncated — names collided");
    }

    /// The zero-length abstract name (`addrlen == 3`, `sun_path[0] == 0`) is a
    /// real, bindable name on Linux and is *not* the same as unnamed.
    #[test]
    fn decode_zero_length_abstract_is_not_unnamed() {
        let name = SockAddrUn::decode(&raw_abstract(b"")).unwrap();
        assert_eq!(name, UnixName::Abstract(Vec::new()));
        assert_ne!(name, UnixName::Unnamed);
    }

    /// A pathname and an abstract name with the same bytes are different
    /// namespaces and must never collide in the name table.
    #[test]
    fn path_and_abstract_are_distinct_namespaces() {
        assert_ne!(
            UnixName::Path(b"foo".to_vec()),
            UnixName::Abstract(b"foo".to_vec())
        );
    }

    // ---- addrlen the encoder reports ---------------------------------------

    /// Linux's three `addrlen` answers, which differ by whether the delimiter
    /// counts: unnamed is 2, an abstract name counts its leading NUL and has no
    /// terminator, a path counts its terminator. A client that reads
    /// `sun_path` for `addrlen - 2` bytes gets a stray NUL or a truncated name
    /// if any of these is off by one.
    #[test]
    fn encode_addrlen_matches_linux() {
        assert_eq!(SockAddrUn::encode(&UnixName::Unnamed).len, SUN_PATH_OFFSET);
        assert_eq!(
            SockAddrUn::encode(&UnixName::Abstract(b"abc".to_vec())).len,
            SUN_PATH_OFFSET + 1 + 3
        );
        assert_eq!(
            SockAddrUn::encode(&UnixName::Path(b"/tmp/s".to_vec())).len,
            SUN_PATH_OFFSET + 6 + 1
        );
    }

    #[test]
    fn encode_marks_abstract_with_leading_nul() {
        let sa = SockAddrUn::encode(&UnixName::Abstract(b"abc".to_vec()));
        assert_eq!(sa.bytes[SUN_PATH_OFFSET], 0, "abstract marker missing");
        assert_eq!(&sa.bytes[SUN_PATH_OFFSET + 1..SUN_PATH_OFFSET + 4], b"abc");
    }

    #[test]
    fn encode_family_is_af_unix() {
        let sa = SockAddrUn::encode(&UnixName::Path(b"/x".to_vec()));
        assert_eq!(u16::from_ne_bytes([sa.bytes[0], sa.bytes[1]]), AF_UNIX);
    }

    /// The property that matters for `getsockname` → `connect` round trips: a
    /// client that reads an address off one socket and connects to it must
    /// reach the same name.
    #[test]
    fn encode_decode_roundtrip() {
        for name in [
            UnixName::Unnamed,
            UnixName::Path(b"/tmp/probe.sock".to_vec()),
            UnixName::Abstract(b"akuma".to_vec()),
            UnixName::Abstract(Vec::new()),
            UnixName::Abstract(b"a\0b".to_vec()),
            UnixName::Path(vec![b'y'; SUN_PATH_LEN - 1]),
        ] {
            let sa = SockAddrUn::encode(&name);
            let back = SockAddrUn::decode(sa.as_slice()).unwrap();
            assert_eq!(back, name, "round trip lost {name:?}");
        }
    }

    /// A name longer than `sun_path` can hold must be truncated, not panic on
    /// a slice overflow — the length comes from userspace.
    #[test]
    fn encode_truncates_oversized_name() {
        let sa = SockAddrUn::encode(&UnixName::Path(vec![b'z'; 500]));
        assert!(sa.len <= SOCKADDR_UN_LEN);
        let sa = SockAddrUn::encode(&UnixName::Abstract(vec![b'z'; 500]));
        assert!(sa.len <= SOCKADDR_UN_LEN);
    }

    #[test]
    fn is_filesystem_only_for_paths() {
        assert!(UnixName::Path(b"/x".to_vec()).is_filesystem());
        assert!(!UnixName::Abstract(b"x".to_vec()).is_filesystem());
        assert!(!UnixName::Unnamed.is_filesystem());
    }

    /// The guard that makes it impossible to create a filesystem node for an
    /// abstract name: there is no path to create it at.
    #[test]
    fn path_bytes_is_none_for_abstract() {
        assert!(UnixName::Abstract(b"x".to_vec()).path_bytes().is_none());
        assert!(UnixName::Unnamed.path_bytes().is_none());
        assert_eq!(UnixName::Path(b"/x".to_vec()).path_bytes(), Some(&b"/x"[..]));
    }
}

// ============================================================================
// Socket types
// ============================================================================

mod sock_type {
    use super::*;

    #[test]
    fn from_raw_accepts_the_three_linux_types() {
        assert_eq!(SockType::from_raw(1), Some(SockType::Stream));
        assert_eq!(SockType::from_raw(2), Some(SockType::Dgram));
        assert_eq!(SockType::from_raw(5), Some(SockType::SeqPacket));
        assert_eq!(SockType::from_raw(3), None); // SOCK_RAW
        assert_eq!(SockType::from_raw(0), None);
    }

    /// `getsockopt(SO_TYPE)` answered a hardcoded `1` for every non-AF_INET fd
    /// before this. A client that picks its framing off the answer then reads a
    /// `SEQPACKET` socket as a byte stream.
    #[test]
    fn to_raw_roundtrips_from_raw() {
        for raw in [1, 2, 5] {
            assert_eq!(SockType::from_raw(raw).unwrap().to_raw(), raw);
        }
    }

    #[test]
    fn framing_and_connection_orientation() {
        assert!(!SockType::Stream.is_framed());
        assert!(SockType::Dgram.is_framed());
        assert!(SockType::SeqPacket.is_framed());

        assert!(SockType::Stream.is_connection_oriented());
        assert!(SockType::SeqPacket.is_connection_oriented());
        assert!(!SockType::Dgram.is_connection_oriented());
    }
}

// ============================================================================
// Framing: the silent-corruption class
// ============================================================================

mod framing {
    use super::*;

    /// A byte stream coalesces: two 10-byte sends then one 20-byte read
    /// returns 20. This is the *correct* behaviour for `SOCK_STREAM` and the
    /// baseline the SEQPACKET test below diverges from.
    #[test]
    fn stream_coalesces_across_writes() {
        let plan = plan_read(SockType::Stream, None, 20, 20, false);
        assert_eq!(plan.take, 20);
        assert!(!plan.truncated);
        assert!(!plan.consume_record);
    }

    /// **The defect this module was written to fix.** Two 10-byte sends then
    /// one 20-byte read on a `SOCK_SEQPACKET` socket must return **10** — one
    /// message. The pipe-backed implementation returned 20, silently merging
    /// two messages, and a client that framed on the return value had no way to
    /// notice. Nothing about this is visible in a kernel log.
    #[test]
    fn seqpacket_preserves_message_boundaries() {
        let plan = plan_read(SockType::SeqPacket, Some(10), 20, 20, false);
        assert_eq!(plan.take, 10, "message boundary lost — two messages merged");
        assert_eq!(plan.discard, 0);
        assert!(!plan.truncated);
        assert!(plan.consume_record);
    }

    /// A record read into a too-small buffer is consumed **whole**: the caller
    /// gets `buflen` bytes, `MSG_TRUNC`, and the tail is destroyed. Leaving the
    /// tail for the next read is what a byte stream does, and doing it here
    /// would make every subsequent record boundary wrong by the shortfall.
    #[test]
    fn seqpacket_truncated_read_discards_the_tail() {
        let plan = plan_read(SockType::SeqPacket, Some(10), 10, 4, false);
        assert_eq!(plan.take, 4);
        assert_eq!(plan.discard, 6, "record tail left behind — framing desync");
        assert!(plan.truncated);
        assert!(plan.consume_record);
    }

    /// `MSG_PEEK` must leave the record completely intact — no discard, no pop.
    /// A peek that consumed the tail would turn a diagnostic read into data
    /// loss.
    #[test]
    fn peek_leaves_the_record_intact() {
        let plan = plan_read(SockType::SeqPacket, Some(10), 10, 4, true);
        assert_eq!(plan.take, 4);
        assert_eq!(plan.discard, 0);
        assert!(plan.truncated);
        assert!(!plan.consume_record);
    }

    /// A framed socket with no queued record reads nothing — and critically,
    /// that is *not* the same as a zero-length datagram (next test).
    #[test]
    fn framed_read_with_no_record_takes_nothing() {
        let plan = plan_read(SockType::Dgram, None, 0, 64, false);
        assert_eq!(plan.take, 0);
        assert!(!plan.consume_record);
    }

    /// A zero-length datagram is a real, deliverable message. `recv` returns 0
    /// for it, and 0 also means EOF — so the record must be *consumed*, or the
    /// receiver spins on it forever. This is the difference between "nothing
    /// arrived" and "an empty message arrived".
    #[test]
    fn zero_length_datagram_is_a_real_message() {
        let plan = plan_write(SockType::Dgram, 0, 65536, 65536).unwrap();
        assert_eq!(plan.bytes, 0);
        assert!(plan.push_record, "zero-length datagram was not recorded");

        let read = plan_read(SockType::Dgram, Some(0), 0, 64, false);
        assert_eq!(read.take, 0);
        assert!(read.consume_record, "zero-length datagram would be re-read forever");
    }

    /// A stream write is allowed — and expected — to be short. `write(2)` is
    /// specified that way and the caller loops.
    #[test]
    fn stream_write_may_be_short() {
        let plan = plan_write(SockType::Stream, 1000, 400, 65536).unwrap();
        assert_eq!(plan.bytes, 400);
        assert!(!plan.push_record);
    }

    /// **The sync invariant.** A framed write that does not fit *entirely* is
    /// `EAGAIN`, never a short count. There is no way to record "two thirds of
    /// a message": the next boundary would be wrong by the shortfall and the
    /// channel could never resynchronise.
    #[test]
    fn framed_write_is_all_or_nothing() {
        assert_eq!(
            plan_write(SockType::SeqPacket, 1000, 400, 65536).unwrap_err(),
            libc_errno::EAGAIN,
            "a framed socket accepted a partial message"
        );
        assert_eq!(
            plan_write(SockType::Dgram, 1000, 400, 65536).unwrap_err(),
            libc_errno::EAGAIN
        );
        // Exactly fitting is fine.
        assert_eq!(plan_write(SockType::Dgram, 400, 400, 65536).unwrap().bytes, 400);
    }

    /// A datagram larger than the send buffer can *never* fit, so waiting for
    /// room is waiting forever. `EMSGSIZE` immediately, not `EAGAIN`.
    #[test]
    fn oversized_datagram_is_emsgsize_not_eagain() {
        assert_eq!(
            plan_write(SockType::Dgram, 70000, 65536, 65536).unwrap_err(),
            libc_errno::EMSGSIZE
        );
        // A stream has no such limit — it just writes what fits.
        assert_eq!(
            plan_write(SockType::Stream, 70000, 65536, 65536).unwrap().bytes,
            65536
        );
    }

    /// A full stream buffer is `EAGAIN`, but a zero-length stream write on a
    /// full buffer is a no-op success — `write(fd, buf, 0)` must not fail.
    #[test]
    fn stream_write_full_buffer() {
        assert_eq!(
            plan_write(SockType::Stream, 10, 0, 65536).unwrap_err(),
            libc_errno::EAGAIN
        );
        assert_eq!(plan_write(SockType::Stream, 0, 0, 65536).unwrap().bytes, 0);
    }
}

// ============================================================================
// Channels: bytes and boundaries staying in sync
// ============================================================================

mod channels {
    use super::*;

    /// `Channel`'s `Default` is hand-written, and this is why: a derived one
    /// would give `sndbuf == 0`, so every datagram send on a channel created via
    /// `entry().or_default()` would fail `EMSGSIZE` — a total, silent loss of
    /// function for that socket, with nothing in the code to point at.
    #[test]
    fn channel_default_equals_new() {
        assert_eq!(Channel::default().sndbuf, Channel::new().sndbuf);
        assert!(Channel::default().sndbuf > 0, "a default channel cannot send");
    }

    #[test]
    fn new_channel_is_empty_with_default_sndbuf() {
        let ch = Channel::new();
        assert_eq!(ch.queued_bytes(), 0);
        assert_eq!(ch.queued_records(), 0);
        assert_eq!(ch.pending_bytes, 0);
        assert_eq!(ch.sndbuf, crate::unix::DEFAULT_SNDBUF);
    }

    /// A plain stream read drains the counter, so the accounting returns to
    /// zero without any record ever existing.
    #[test]
    fn plain_stream_read_drains_pending_bytes() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        t.commit_write(7, 40, false, Vec::new());
        assert!(t.commit_read(7, 25, false).is_empty());
        assert_eq!(t.channel(7).unwrap().pending_bytes, 15);
        assert!(t.commit_read(7, 15, false).is_empty());
        assert_eq!(t.channel(7).unwrap().queued_bytes(), 0);
    }

    /// A plain stream write allocates **no record** — just a counter bump.
    ///
    /// This is the allocation-free stream path: the record queue is a
    /// `VecDeque`, so the first push heap-allocates, and a `SOCK_STREAM` socket
    /// would pay that plus per-write bookkeeping for metadata no reader
    /// consults (`plan_read` sizes stream reads from the pipe's available
    /// bytes, not from records). rustc's spawn handshake runs through this path
    /// on every link, so it is worth the extra field.
    #[test]
    fn plain_stream_writes_allocate_no_record() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        t.commit_write(7, 10, false, Vec::new());
        t.commit_write(7, 30, false, Vec::new());
        let ch = t.channel(7).unwrap();
        assert_eq!(ch.queued_records(), 0, "a plain stream write allocated a record");
        // The bytes are still accounted for — in the counter, not a record.
        assert_eq!(ch.pending_bytes, 40);
        assert_eq!(ch.queued_bytes(), 40, "byte accounting was lost");
    }

    /// **Ordering, not just accounting.** Bytes written *before* an
    /// fd-carrying message must be consumable without releasing the
    /// descriptors. Without the `pending_bytes` counter those earlier bytes
    /// leave no trace, the fds' record sits at the front of the queue, and a
    /// reader draining only the earlier bytes pops it — receiving descriptors
    /// it has not been told about yet.
    #[test]
    fn plain_bytes_before_an_fd_message_do_not_release_it_early() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        t.commit_write(7, 10, false, Vec::new()); // plain, no record
        t.commit_write(7, 4, false, vec![42]);    // carries an fd
        // The plain bytes were materialised into their own record ahead of it.
        assert_eq!(t.channel(7).unwrap().queued_records(), 2);
        assert_eq!(t.channel(7).unwrap().queued_bytes(), 14);

        // Reading exactly the leading bytes must yield NO descriptors.
        assert!(
            t.commit_read(7, 10, false).is_empty(),
            "descriptors released before the reader reached their message"
        );
        // Reading into the fd-carrying record then delivers them.
        assert_eq!(t.commit_read(7, 4, false), vec![42]);
    }

    /// The bytes/boundaries invariant, on a channel where boundaries exist:
    /// whatever the channel believes is queued must equal what the pipe holds.
    /// A drift is a framing desync, so it is asserted directly.
    #[test]
    fn framed_bytes_stay_exact() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        t.commit_write(7, 10, true, Vec::new());
        t.commit_write(7, 30, true, Vec::new());
        assert_eq!(t.channel(7).unwrap().queued_bytes(), 40);
    }

    /// Once a stream carries ancillary data it *does* keep records, because the
    /// descriptors have to stay anchored at their own point in the byte stream —
    /// and plain bytes written afterwards must stay behind them.
    #[test]
    fn stream_coalesces_behind_an_ancillary_record() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        t.commit_write(7, 10, false, vec![9]);
        t.commit_write(7, 30, false, Vec::new());
        assert_eq!(t.channel(7).unwrap().queued_records(), 2);
        assert_eq!(t.channel(7).unwrap().queued_bytes(), 40);
    }

    #[test]
    fn framed_writes_keep_one_record_each() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        t.commit_write(7, 10, true, Vec::new());
        t.commit_write(7, 30, true, Vec::new());
        assert_eq!(t.channel(7).unwrap().queued_records(), 2);
        assert_eq!(t.channel(7).unwrap().queued_bytes(), 40);
        assert_eq!(t.front_record(7), Some(10));
    }

    #[test]
    fn framed_read_pops_exactly_one_record() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        t.commit_write(7, 10, true, Vec::new());
        t.commit_write(7, 30, true, Vec::new());
        t.commit_read(7, 10, true);
        assert_eq!(t.front_record(7), Some(30));
        assert_eq!(t.channel(7).unwrap().queued_records(), 1);
    }

    /// A stream read consumes bytes off the front of the record queue, and a
    /// read that spans a record boundary must leave the *remainder* of the
    /// second record, not drop it.
    #[test]
    fn stream_read_walks_records_by_byte_count() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        // A record exists because the first write carried ancillary data; the
        // second coalesces behind it rather than into `pending_bytes`.
        t.commit_write(7, 10, false, vec![42]);
        t.commit_write(7, 30, false, Vec::new());
        assert_eq!(t.channel(7).unwrap().queued_records(), 2);
        t.commit_read(7, 25, false);
        assert_eq!(
            t.channel(7).unwrap().queued_bytes(),
            15,
            "byte accounting drifted across a record boundary"
        );
    }

    /// A stream read that lands exactly on a boundary must not leave a
    /// zero-length record behind — that would look like a zero-length datagram
    /// to anything reading `front_record`. Uses an ancillary-anchored record,
    /// since a plain stream write creates none.
    #[test]
    fn stream_read_on_exact_boundary_leaves_nothing() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        t.commit_write(7, 10, false, vec![3]);
        t.commit_read(7, 10, false);
        assert_eq!(t.channel(7).unwrap().queued_records(), 0);
        assert_eq!(t.channel(7).unwrap().queued_bytes(), 0);
    }

    /// Ancillary data belongs to a **record**, not to the socket. If it were
    /// attached to the socket, the second message's fds would be delivered with
    /// the first — a receiver acting on a descriptor it has not been told about.
    #[test]
    fn ancillary_stays_with_its_own_record() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        t.commit_write(7, 4, true, vec![10, 11]);
        t.commit_write(7, 4, true, vec![20]);
        let first = t.commit_read(7, 4, true);
        assert_eq!(first, vec![10, 11]);
        let second = t.commit_read(7, 4, true);
        assert_eq!(second, vec![20], "ancillary data crossed a message boundary");
    }

    /// A stream write carrying ancillary data cannot coalesce into the previous
    /// record — the fds have to stay at their own point in the stream.
    #[test]
    fn stream_write_with_ancillary_starts_a_new_record() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        t.commit_write(7, 10, false, Vec::new());
        t.commit_write(7, 10, false, vec![5]);
        assert_eq!(t.channel(7).unwrap().queued_records(), 2);
        assert_eq!(t.channel(7).unwrap().pending_bytes, 0, "pending bytes not materialised");
    }

    /// **The leak this design's worst failure mode.** A channel torn down with
    /// unread records holds the only reference to every `SCM_RIGHTS` descriptor
    /// in them. `detach_channel` must hand them all back so the caller can
    /// close them; dropping them silently leaks fds, and no probe can observe
    /// that from userspace.
    #[test]
    fn detaching_a_channel_returns_every_in_flight_fd() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        t.commit_write(7, 4, true, vec![10, 11]);
        t.commit_write(7, 4, true, vec![20]);
        let mut orphans = t.detach_channel(7);
        orphans.sort_unstable();
        assert_eq!(orphans, vec![10, 11, 20], "in-flight descriptors leaked");
        assert!(t.channel(7).is_none());
    }

    #[test]
    fn detaching_an_unknown_channel_is_harmless() {
        let mut t = UnixTable::new();
        assert!(t.detach_channel(999).is_empty());
    }

    /// A record that was read normally must not also come back from teardown —
    /// that would be a double close.
    #[test]
    fn read_records_do_not_reappear_at_teardown() {
        let mut t = UnixTable::new();
        t.attach_channel(7);
        t.commit_write(7, 4, true, vec![10]);
        assert_eq!(t.commit_read(7, 4, true), vec![10]);
        assert!(t.detach_channel(7).is_empty(), "descriptor delivered twice");
    }
}

// ============================================================================
// Names
// ============================================================================

mod names {
    use super::*;

    #[test]
    fn bind_then_lookup() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Stream, creds(1));
        let name = UnixName::Abstract(b"svc".to_vec());
        t.bind(s, name.clone()).unwrap();
        assert!(t.is_bound(&name));
        assert_eq!(t.get(s).unwrap().state, SockState::Bound);
        assert_eq!(t.name_count(), 1);
    }

    #[test]
    fn bind_twice_on_one_name_is_eaddrinuse() {
        let mut t = UnixTable::new();
        let a = t.alloc(SockType::Stream, creds(1));
        let b = t.alloc(SockType::Stream, creds(2));
        let name = UnixName::Path(b"/tmp/s".to_vec());
        t.bind(a, name.clone()).unwrap();
        assert_eq!(t.bind(b, name).unwrap_err(), libc_errno::EADDRINUSE);
    }

    /// **A daemon must be able to restart.** Once the holder closes, the name is
    /// free. A name left behind by a dead socket makes every subsequent `bind`
    /// fail with `EADDRINUSE` forever, which for a service means it can never
    /// come back without a reboot.
    #[test]
    fn name_is_free_again_after_its_holder_closes() {
        let mut t = UnixTable::new();
        let name = UnixName::Path(b"/tmp/s".to_vec());
        let a = t.alloc(SockType::Stream, creds(1));
        t.bind(a, name.clone()).unwrap();
        t.close(a);
        assert!(!t.is_bound(&name));
        assert_eq!(t.name_count(), 0, "name-table entry leaked past its socket");

        let b = t.alloc(SockType::Stream, creds(2));
        t.bind(b, name).unwrap();
    }

    /// Closing a socket must only release the name if the name still points at
    /// *it*. A daemon that died and restarted has rebound the same name to a new
    /// socket; releasing it on the old socket's teardown would silently unbind
    /// the live one.
    #[test]
    fn closing_a_rebound_socket_does_not_unbind_the_live_holder() {
        let mut t = UnixTable::new();
        let name = UnixName::Path(b"/tmp/s".to_vec());
        let old = t.alloc(SockType::Stream, creds(1));
        t.bind(old, name.clone()).unwrap();
        // Simulate "the name was force-rebound": drop the old holder's claim,
        // rebind to a new socket, then let the old socket finish closing.
        t.close(old);
        let new = t.alloc(SockType::Stream, creds(2));
        t.bind(new, name.clone()).unwrap();
        // A second (stale) close of the old id must not touch the new binding.
        t.close(old);
        assert!(t.is_bound(&name));
        assert_eq!(t.get(new).unwrap().name, name);
    }

    #[test]
    fn bind_unnamed_is_einval() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Stream, creds(1));
        assert_eq!(t.bind(s, UnixName::Unnamed).unwrap_err(), libc_errno::EINVAL);
    }

    #[test]
    fn rebinding_a_bound_socket_is_einval() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Stream, creds(1));
        t.bind(s, UnixName::Abstract(b"a".to_vec())).unwrap();
        assert_eq!(
            t.bind(s, UnixName::Abstract(b"b".to_vec())).unwrap_err(),
            libc_errno::EINVAL
        );
    }

    #[test]
    fn bind_on_a_missing_socket_is_ebadf() {
        let mut t = UnixTable::new();
        assert_eq!(
            t.bind(404, UnixName::Abstract(b"a".to_vec())).unwrap_err(),
            libc_errno::EBADF
        );
        // The failed bind must not have taken the name.
        assert_eq!(t.name_count(), 0);
    }
}

// ============================================================================
// listen
// ============================================================================

mod listen {
    use super::*;

    #[test]
    fn listen_requires_a_bound_name() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Stream, creds(1));
        assert_eq!(t.listen(s, 5).unwrap_err(), libc_errno::EINVAL);
    }

    #[test]
    fn dgram_cannot_listen() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Dgram, creds(1));
        t.bind(s, UnixName::Abstract(b"d".to_vec())).unwrap();
        assert_eq!(t.listen(s, 5).unwrap_err(), libc_errno::EOPNOTSUPP);
    }

    /// `listen(fd, 0)` must not produce a listener that can never accept
    /// anything. Linux clamps up; a literal 0 would make the socket refuse
    /// every connection with `EAGAIN` forever.
    #[test]
    fn zero_and_negative_backlog_clamp_up() {
        let mut t = UnixTable::new();
        for backlog in [0, -1] {
            let s = t.alloc(SockType::Stream, creds(1));
            t.bind(s, UnixName::Abstract(vec![backlog as u8])).unwrap();
            t.listen(s, backlog).unwrap();
            assert_eq!(t.get(s).unwrap().backlog_max, DEFAULT_BACKLOG);
        }
    }

    /// The backlog length is attacker-controlled, so it is capped whatever
    /// `listen(2)` asks for.
    #[test]
    fn huge_backlog_is_capped() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Stream, creds(1));
        t.bind(s, UnixName::Abstract(b"s".to_vec())).unwrap();
        t.listen(s, 1_000_000).unwrap();
        assert_eq!(t.get(s).unwrap().backlog_max, MAX_BACKLOG);
    }

    #[test]
    fn listening_socket_reports_state_and_readiness() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::SeqPacket, creds(1));
        t.bind(s, UnixName::Abstract(b"s".to_vec())).unwrap();
        t.listen(s, 4).unwrap();
        assert_eq!(t.get(s).unwrap().state, SockState::Listening);
        assert!(!t.get(s).unwrap().accept_ready(), "empty backlog is not ready");
    }
}

// ============================================================================
// connect / accept rendezvous
// ============================================================================

mod rendezvous {
    use super::*;

    fn listener(t: &mut UnixTable, name: &[u8], backlog: i32) -> u32 {
        let s = t.alloc(SockType::Stream, creds(1));
        t.bind(s, UnixName::Abstract(name.to_vec())).unwrap();
        t.listen(s, backlog).unwrap();
        s
    }

    /// A name nobody has bound is `ECONNREFUSED`, not `ENOENT` and not a hang.
    /// This is the stale-socket-file case every daemon's restart path runs
    /// through, and a client must be told "nobody home" so it can retry or fail.
    #[test]
    fn connect_to_unbound_name_is_econnrefused() {
        let mut t = UnixTable::new();
        let c = t.alloc(SockType::Stream, creds(2));
        assert_eq!(
            t.connect(c, &UnixName::Abstract(b"nope".to_vec()), creds(2))
                .unwrap_err(),
            libc_errno::ECONNREFUSED
        );
    }

    /// Bound but never `listen`ed — a daemon that crashed between the two
    /// syscalls. Indistinguishable from unbound to a client, and it must be:
    /// same errno.
    #[test]
    fn connect_to_bound_but_unlistening_is_econnrefused() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Stream, creds(1));
        t.bind(s, UnixName::Abstract(b"svc".to_vec())).unwrap();
        let c = t.alloc(SockType::Stream, creds(2));
        assert_eq!(
            t.connect(c, &UnixName::Abstract(b"svc".to_vec()), creds(2))
                .unwrap_err(),
            libc_errno::ECONNREFUSED
        );
    }

    /// A full backlog is **transient**, so it must be `EAGAIN` — a blocking
    /// client should wait for the server to catch up, not give up. Reporting
    /// `ECONNREFUSED` here would make a busy server look like a dead one.
    #[test]
    fn full_backlog_is_eagain_not_econnrefused() {
        let mut t = UnixTable::new();
        let l = listener(&mut t, b"svc", 1);
        let c1 = t.alloc(SockType::Stream, creds(2));
        t.connect(c1, &UnixName::Abstract(b"svc".to_vec()), creds(2)).unwrap();
        let c2 = t.alloc(SockType::Stream, creds(3));
        assert_eq!(
            t.connect(c2, &UnixName::Abstract(b"svc".to_vec()), creds(3))
                .unwrap_err(),
            libc_errno::EAGAIN
        );
        assert_eq!(t.get(l).unwrap().backlog.len(), 1);
    }

    #[test]
    fn connect_queues_and_accept_claims_it() {
        let mut t = UnixTable::new();
        let l = listener(&mut t, b"svc", 4);
        let c = t.alloc(SockType::Stream, creds(2));
        let out = t.connect(c, &UnixName::Abstract(b"svc".to_vec()), creds(2)).unwrap();
        let ConnectOutcome::Queued { listener: got_l, server_sock } = out else {
            panic!("expected Queued, got {out:?}");
        };
        assert_eq!(got_l, l);
        assert!(t.get(l).unwrap().accept_ready());

        let p: Pending = t.accept(l).unwrap();
        assert_eq!(p.server_sock, server_sock);
        assert_eq!(p.client_sock, c);
        assert!(!t.get(l).unwrap().accept_ready(), "backlog not drained");
    }

    /// The client's endpoint exists — and is writable — from `connect` time,
    /// before the server calls `accept`. Linux behaves this way and clients rely
    /// on it: a request written immediately after `connect` must be buffered,
    /// not lost.
    #[test]
    fn client_endpoint_is_connected_before_accept() {
        let mut t = UnixTable::new();
        listener(&mut t, b"svc", 4);
        let c = t.alloc(SockType::Stream, creds(2));
        t.connect(c, &UnixName::Abstract(b"svc".to_vec()), creds(2)).unwrap();
        assert_eq!(t.get(c).unwrap().state, SockState::Connected);
        assert!(t.get(c).unwrap().peer.is_some());
    }

    #[test]
    fn accept_on_empty_backlog_is_eagain() {
        let mut t = UnixTable::new();
        let l = listener(&mut t, b"svc", 4);
        assert_eq!(t.accept(l).unwrap_err(), libc_errno::EAGAIN);
    }

    #[test]
    fn accept_on_a_non_listener_is_einval() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Stream, creds(1));
        assert_eq!(t.accept(s).unwrap_err(), libc_errno::EINVAL);
    }

    /// Connecting a `SOCK_STREAM` to a `SOCK_SEQPACKET` name is `EPROTOTYPE`:
    /// the name exists and the protocol exists, but the framing does not match,
    /// and letting it through would give the two ends different ideas about
    /// where messages end.
    #[test]
    fn mismatched_socket_types_are_eprototype() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::SeqPacket, creds(1));
        t.bind(s, UnixName::Abstract(b"svc".to_vec())).unwrap();
        t.listen(s, 4).unwrap();
        let c = t.alloc(SockType::Stream, creds(2));
        assert_eq!(
            t.connect(c, &UnixName::Abstract(b"svc".to_vec()), creds(2))
                .unwrap_err(),
            libc_errno::EPROTOTYPE
        );
    }

    #[test]
    fn connect_to_unnamed_is_einval() {
        let mut t = UnixTable::new();
        let c = t.alloc(SockType::Stream, creds(2));
        assert_eq!(
            t.connect(c, &UnixName::Unnamed, creds(2)).unwrap_err(),
            libc_errno::EINVAL
        );
    }

    /// `getpeername` on the client must report the listener's name; the server
    /// side of an accepted connection reports the *listener's* name as its own,
    /// which is what Linux does and what a server logging its own address needs.
    #[test]
    fn names_are_visible_from_both_ends() {
        let mut t = UnixTable::new();
        let name = UnixName::Abstract(b"svc".to_vec());
        let l = listener(&mut t, b"svc", 4);
        let c = t.alloc(SockType::Stream, creds(2));
        t.connect(c, &name, creds(2)).unwrap();
        let p = t.accept(l).unwrap();
        assert_eq!(t.get(c).unwrap().peer_name, name);
        assert_eq!(t.get(p.server_sock).unwrap().name, name);
    }

    /// A `SOCK_DGRAM` connect sets a default destination and nothing else — no
    /// backlog, no accept, no rendezvous.
    #[test]
    fn dgram_connect_only_records_a_peer() {
        let mut t = UnixTable::new();
        let srv = t.alloc(SockType::Dgram, creds(1));
        t.bind(srv, UnixName::Abstract(b"log".to_vec())).unwrap();
        let c = t.alloc(SockType::Dgram, creds(2));
        let out = t.connect(c, &UnixName::Abstract(b"log".to_vec()), creds(2)).unwrap();
        assert_eq!(out, ConnectOutcome::DgramPeerSet { peer: srv });
        assert_eq!(t.get(c).unwrap().state, SockState::Connected);
        assert!(t.get(srv).unwrap().backlog.is_empty());
    }
}

// ============================================================================
// SOCK_DGRAM destination resolution
// ============================================================================

mod dgram {
    use super::*;

    fn bound_dgram(t: &mut UnixTable, name: &[u8], queue: u32) -> u32 {
        let s = t.alloc(SockType::Dgram, creds(1));
        t.bind(s, UnixName::Abstract(name.to_vec())).unwrap();
        t.attach_dgram_queue(s, queue);
        s
    }

    #[test]
    fn sendto_resolves_to_the_targets_queue() {
        let mut t = UnixTable::new();
        let srv = bound_dgram(&mut t, b"log", 42);
        let (sock, queue) = t
            .resolve_dgram_dest(&UnixName::Abstract(b"log".to_vec()))
            .unwrap();
        assert_eq!(sock, srv);
        assert_eq!(queue, 42, "sender did not find the receiver's queue");
    }

    /// Nothing bound is `ECONNREFUSED`, not a hang and not `ENOENT`. This is
    /// `syslog(3)` with no syslogd running — the single most common datagram
    /// case there is, and it must fail fast so the caller's log line is dropped
    /// rather than its thread parked.
    #[test]
    fn sendto_unbound_name_is_econnrefused() {
        let t = UnixTable::new();
        assert_eq!(
            t.resolve_dgram_dest(&UnixName::Path(b"/dev/log".to_vec()))
                .unwrap_err(),
            libc_errno::ECONNREFUSED
        );
    }

    /// A datagram aimed at a stream socket's name is `EPROTOTYPE`. The name
    /// exists and belongs to something, but delivering the bytes would put them
    /// in a queue framed by different rules.
    #[test]
    fn sendto_a_stream_socket_is_eprototype() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Stream, creds(1));
        t.bind(s, UnixName::Abstract(b"svc".to_vec())).unwrap();
        assert_eq!(
            t.resolve_dgram_dest(&UnixName::Abstract(b"svc".to_vec()))
                .unwrap_err(),
            libc_errno::EPROTOTYPE
        );
    }

    /// A bound datagram socket with no queue cannot receive, and the sender is
    /// told "refused" — from its side that is indistinguishable from nothing
    /// being there, and it is actionable, unlike a kernel-internal error.
    #[test]
    fn sendto_a_queueless_socket_is_econnrefused() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Dgram, creds(1));
        t.bind(s, UnixName::Abstract(b"q".to_vec())).unwrap();
        // No attach_dgram_queue.
        assert_eq!(
            t.resolve_dgram_dest(&UnixName::Abstract(b"q".to_vec()))
                .unwrap_err(),
            libc_errno::ECONNREFUSED
        );
    }

    #[test]
    fn sendto_unnamed_destination_is_edestaddrreq() {
        let t = UnixTable::new();
        assert_eq!(
            t.resolve_dgram_dest(&UnixName::Unnamed).unwrap_err(),
            libc_errno::EDESTADDRREQ
        );
    }

    /// `send(2)` with no destination on an **unconnected** datagram socket is
    /// `EDESTADDRREQ`: the call is missing an address, which is a different
    /// thing from the socket being broken, and a caller can fix it.
    #[test]
    fn send_without_a_destination_or_a_peer_is_edestaddrreq() {
        let mut t = UnixTable::new();
        let c = t.alloc(SockType::Dgram, creds(2));
        assert_eq!(t.dgram_default_dest(c).unwrap_err(), libc_errno::EDESTADDRREQ);
    }

    /// After `connect`, a bare `send` goes to the recorded peer's queue.
    #[test]
    fn connected_dgram_send_uses_the_peers_queue() {
        let mut t = UnixTable::new();
        bound_dgram(&mut t, b"log", 42);
        let c = t.alloc(SockType::Dgram, creds(2));
        t.connect(c, &UnixName::Abstract(b"log".to_vec()), creds(2)).unwrap();
        assert_eq!(t.dgram_default_dest(c).unwrap(), 42);
    }

    /// **A datagram socket's `tx` is not a send path.**
    ///
    /// Both `rx` and `tx` point at the socket's own single receive queue, purely
    /// so the fd teardown path stays uniform: closing a `UnixSocket` fd does
    /// `pipe_close_read(rx)` + `pipe_close_write(tx)`, so a queue recorded only
    /// in `rx` would keep `write_count == 1` forever and the pipe would never be
    /// destroyed — one leaked pipe per datagram socket, with nothing pointing at
    /// it.
    #[test]
    fn dgram_queue_is_recorded_in_both_directions_for_teardown() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Dgram, creds(1));
        t.attach_dgram_queue(s, 77);
        assert_eq!(t.get(s).unwrap().rx, 77);
        assert_eq!(
            t.get(s).unwrap().tx,
            77,
            "tx unset — pipe_close_write would never run and the queue would leak"
        );
        assert!(t.channel(77).is_some(), "no framing metadata for the queue");
    }
}

// ============================================================================
// Credentials
// ============================================================================

mod credentials {
    use super::*;

    /// Both ends of a `socketpair` report each other's credentials.
    ///
    /// They belong to the same process, so this is the calling process's own
    /// pid. Leaving `peer_creds` at its default made `SO_PEERCRED` answer
    /// **pid 0** on every socketpair — caught by `nettest-unix peercred`
    /// against the Linux control arm, not by any test written in advance, which
    /// is why the probe checks a value it could have taken on trust.
    #[test]
    fn socketpair_ends_see_each_others_credentials() {
        let mut t = UnixTable::new();
        let a = t.alloc(SockType::Stream, creds(11));
        let b = t.alloc(SockType::Stream, creds(11));
        t.pair(a, b, 10, 11);
        assert_eq!(t.get(a).unwrap().peer_creds, creds(11));
        assert_eq!(t.get(b).unwrap().peer_creds, creds(11));
        assert_ne!(
            t.get(a).unwrap().peer_creds,
            Ucred::default(),
            "SO_PEERCRED would report pid 0"
        );
    }

    /// Captured at **connect**, not at send. A daemon that authorises by uid
    /// must see the uid the client had when it connected; otherwise a client can
    /// connect privileged, drop privileges, and keep the daemon's trust — or
    /// connect unprivileged and later appear privileged.
    #[test]
    fn peer_credentials_are_from_connect_time() {
        let mut t = UnixTable::new();
        let l = t.alloc(SockType::Stream, creds(1));
        t.bind(l, UnixName::Abstract(b"svc".to_vec())).unwrap();
        t.listen(l, 4).unwrap();

        let c = t.alloc(SockType::Stream, creds(7));
        let connect_time = creds(7);
        t.connect(c, &UnixName::Abstract(b"svc".to_vec()), connect_time).unwrap();
        let p = t.accept(l).unwrap();

        assert_eq!(p.client_creds, connect_time);
        assert_eq!(t.get(p.server_sock).unwrap().peer_creds, connect_time);
        // And the client learns the server's.
        assert_eq!(t.get(c).unwrap().peer_creds, creds(1));
    }
}

// ============================================================================
// Shutdown
// ============================================================================

mod shutdown {
    use super::*;

    #[test]
    fn how_values_map_to_halves() {
        let mut s = Shutdown::default();
        s.apply(0).unwrap();
        assert_eq!(s, Shutdown { rd: true, wr: false });

        let mut s = Shutdown::default();
        s.apply(1).unwrap();
        assert_eq!(s, Shutdown { rd: false, wr: true });

        let mut s = Shutdown::default();
        s.apply(2).unwrap();
        assert_eq!(s, Shutdown { rd: true, wr: true });
    }

    /// `shutdown` was a `return 0` stub for every non-AF_INET fd, so an invalid
    /// `how` reported success. A caller that passes a bad constant should learn
    /// about it rather than silently continue with a socket it thinks is
    /// half-closed.
    #[test]
    fn invalid_how_is_einval() {
        let mut s = Shutdown::default();
        assert_eq!(s.apply(3).unwrap_err(), libc_errno::EINVAL);
        assert_eq!(s.apply(-1).unwrap_err(), libc_errno::EINVAL);
        assert_eq!(s, Shutdown::default(), "a failed shutdown changed state");
    }

    /// Shutting down twice is idempotent, not an error — a wrapper that calls
    /// it in both an explicit close and a destructor must not fail the second
    /// time.
    #[test]
    fn repeated_shutdown_is_idempotent() {
        let mut s = Shutdown::default();
        s.apply(1).unwrap();
        s.apply(1).unwrap();
        assert_eq!(s, Shutdown { rd: false, wr: true });
        s.apply(2).unwrap();
        assert_eq!(s, Shutdown { rd: true, wr: true });
    }
}

// ============================================================================
// Lifecycle and leaks
// ============================================================================

mod lifecycle {
    use super::*;

    /// Ids start at 1 so that `0` is usable as "no socket" on the kernel's
    /// `FileDescriptor::UnixSocket { sock }` field — a socketpair created
    /// before the table existed carries `sock: 0`.
    #[test]
    fn ids_start_at_one() {
        let mut t = UnixTable::new();
        assert_eq!(t.alloc(SockType::Stream, creds(1)), 1);
    }

    /// `dup`/`dup2`/`F_DUPFD`/`fork` all make a real second reference. The first
    /// close must not destroy the entry underneath the other fd — the same
    /// invariant `test_socketpair_close_refcount` asserts for the pipes.
    #[test]
    fn refcount_survives_the_first_close() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Stream, creds(1));
        t.clone_ref(s);
        t.close(s);
        assert!(t.get(s).is_some(), "entry destroyed under a live second fd");
        t.close(s);
        assert!(t.get(s).is_none());
        assert!(t.is_empty());
    }

    /// **The listener-teardown leak.** A listener closed with queued
    /// connections holds the only reference to each server-side endpoint.
    /// `close` must hand them back or those entries — and two pipes each —
    /// leak, while the clients park forever on a connection nobody will accept.
    #[test]
    fn closing_a_listener_returns_its_queued_server_sockets() {
        let mut t = UnixTable::new();
        let l = t.alloc(SockType::Stream, creds(1));
        t.bind(l, UnixName::Abstract(b"svc".to_vec())).unwrap();
        t.listen(l, 4).unwrap();

        let mut expected = Vec::new();
        for pid in 2..5u32 {
            let c = t.alloc(SockType::Stream, creds(pid));
            let out = t.connect(c, &UnixName::Abstract(b"svc".to_vec()), creds(pid)).unwrap();
            if let ConnectOutcome::Queued { server_sock, .. } = out {
                expected.push(server_sock);
            }
        }
        let mut orphans = t.close(l);
        orphans.sort_unstable();
        expected.sort_unstable();
        assert_eq!(orphans, expected, "queued server endpoints leaked");
    }

    /// The full round trip returns the table to baseline. Every accumulating
    /// leak class in this design — a name-table entry, a socket entry, a queued
    /// server endpoint — shows up only as a drift here, which is why the
    /// `stress` probe mode exists.
    #[test]
    fn a_full_connect_accept_close_cycle_leaks_nothing() {
        let mut t = UnixTable::new();
        for round in 0..20u32 {
            let l = t.alloc(SockType::Stream, creds(1));
            t.bind(l, UnixName::Abstract(b"svc".to_vec())).unwrap();
            t.listen(l, 4).unwrap();
            let c = t.alloc(SockType::Stream, creds(2));
            t.connect(c, &UnixName::Abstract(b"svc".to_vec()), creds(2)).unwrap();
            let p = t.accept(l).unwrap();
            t.pair(c, p.server_sock, 100 + round, 200 + round);

            t.close(c);
            t.close(p.server_sock);
            t.close(l);
            t.detach_channel(100 + round);
            t.detach_channel(200 + round);

            assert!(t.is_empty(), "socket entries leaked on round {round}");
            assert_eq!(t.name_count(), 0, "name leaked on round {round}");
        }
    }

    /// When one end goes away the other must learn about it, or a send there
    /// writes into a channel with no reader and the caller never gets `EPIPE`.
    #[test]
    fn closing_one_end_disconnects_the_peer() {
        let mut t = UnixTable::new();
        let a = t.alloc(SockType::Stream, creds(1));
        let b = t.alloc(SockType::Stream, creds(2));
        t.pair(a, b, 10, 11);
        assert_eq!(t.get(b).unwrap().peer, Some(a));
        t.close(a);
        assert_eq!(t.get(b).unwrap().peer, None);
        assert_eq!(t.get(b).unwrap().state, SockState::Disconnected);
    }

    #[test]
    fn closing_an_unknown_socket_is_harmless() {
        let mut t = UnixTable::new();
        assert!(t.close(404).is_empty());
    }

    /// `socketpair` semantics: both endpoints connected, pipes crossed, and a
    /// channel registered for each direction so the pair gets real framing
    /// instead of the byte-stream approximation.
    #[test]
    fn pair_crosses_the_pipes_and_registers_both_channels() {
        let mut t = UnixTable::new();
        let a = t.alloc(SockType::SeqPacket, creds(1));
        let b = t.alloc(SockType::SeqPacket, creds(1));
        t.pair(a, b, 10, 11);

        assert_eq!((t.get(a).unwrap().rx, t.get(a).unwrap().tx), (10, 11));
        assert_eq!((t.get(b).unwrap().rx, t.get(b).unwrap().tx), (11, 10));
        assert!(t.channel(10).is_some());
        assert!(t.channel(11).is_some());
        assert_eq!(t.get(a).unwrap().state, SockState::Connected);
        assert_eq!(t.get(b).unwrap().state, SockState::Connected);
    }

    /// A fresh socket is unnamed and unconnected, and reports so — this is what
    /// `getsockname` on a `socketpair` endpoint must return (`addrlen == 2`),
    /// where the pre-existing code returned `EBADF`.
    #[test]
    fn fresh_socket_is_unnamed_and_unbound() {
        let mut t = UnixTable::new();
        let s = t.alloc(SockType::Stream, creds(1));
        let sock = t.get(s).unwrap();
        assert_eq!(sock.name, UnixName::Unnamed);
        assert_eq!(sock.state, SockState::Unbound);
        assert_eq!(sock.refs, 1);
        assert_eq!(SockAddrUn::encode(&sock.name).len, SUN_PATH_OFFSET);
    }

    #[test]
    fn record_default_is_an_empty_message() {
        let r = Record::default();
        assert_eq!(r.len, 0);
        assert!(r.anc_fds.is_empty());
    }
}
