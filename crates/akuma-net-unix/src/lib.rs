//! AF_UNIX socket state machine.
//!
//! # Why this is its own crate
//!
//! It lived in `akuma-net` until 2026-08-30, and the move is a decoupling, not
//! a tidy-up. AF_UNIX is not networking in `akuma-net`'s sense — no NIC, no IP,
//! no port, no smoltcp — it is IPC over the kernel's pipes. Keeping it beside
//! the TCP/IP stack cost two things:
//!
//! - The **rump-only devbox** had to pull the whole of `akuma-net` (with
//!   `default-features = false`) just to reach AF_UNIX, because box 0's
//!   `rump_server` answers every proxied syscall over a `UnixSocket` at fd 3
//!   (`src/rump_proxy.rs`).
//! - It was 2,476 of `akuma-net`'s 8,845 lines — 28% of a crate whose size was
//!   the reason a split was being considered at all
//!   (`docs/archive/AKUMA_NET_SPLIT.md` §5.1 extraction A).
//!
//! The cut cost one import line: this module's only coupling to `akuma-net` was
//! `crate::socket::libc_errno`, itself a re-export of
//! [`akuma_primitives::errno`], which is now the dependency directly. There is
//! no `smoltcp`, no `alloc`-free constraint, and no `unsafe` anywhere in it.
//!
//! # Why it is not in `src/syscall/`
//!
//! Before this module, the kernel's entire AF_UNIX implementation was
//! `sys_socketpair` plus special-case arms in eight syscalls, all of it in
//! kernel-only code that no host test can reach
//! (`docs/archive/UNIX_SOCKET_IMPROVEMENTS.md` §1). The consequence was not
//! just missing coverage: the two arms of `sys_sendmsg` disagreed about whether
//! to coalesce iovecs, and `SOCK_SEQPACKET` silently merged messages, for two
//! years, because nothing could assert either property without booting a VM.
//!
//! The *decisions* AF_UNIX has to make — is this connect refused, does this
//! datagram fit, where does this record end, which endpoint does a shutdown
//! make readable — need no NIC, no timer, and no page table. So they live here,
//! as a pure state machine over plain integers, and
//! `cargo test -p akuma-net-unix` asserts them in a second. The kernel keeps only what it must: user-pointer
//! copies, fd allocation, waker registration, and the VFS calls.
//!
//! # The bytes/boundaries split
//!
//! This module does **not** buffer payload. The bytes live in the kernel's
//! pipes (`src/syscall/pipe.rs`), which already have a capacity cap, endpoint
//! refcounts, a poller/waker set and EOF semantics that work under SMP —
//! re-implementing that would re-litigate solved problems. What this module
//! owns is the *metadata running parallel to those bytes*: which name is bound,
//! who is connected to whom, and — for `SOCK_SEQPACKET`/`SOCK_DGRAM` — where
//! each record ends.
//!
//! Keeping the two in sync is the one real hazard, and it has a single rule:
//! **a datagram-type send must be all-or-nothing.** `pipe_write` is allowed to
//! accept a prefix; if a caller recorded the full length after a partial write,
//! every subsequent record boundary would be wrong by the shortfall and the
//! stream would never resynchronise. [`plan_write`] is what enforces that — it
//! returns `EAGAIN` for a datagram that does not fit *entirely*, rather than a
//! short count.
//!
//! # No feature gates
//!
//! AF_UNIX must exist on the rump-only devbox build (see the top of this file),
//! so nothing here is conditional. That is now a property of the crate rather
//! than a `#[cfg]` discipline inside a bigger one — which is most of the point.

#![cfg_attr(not(test), no_std)]
// Unsafe-free by design, and `forbid` so no module can opt back in with a
// local `allow`. Same reasoning as `akuma-net-yarn` and `akuma-syscalls-sync`,
// and spelled here rather than in Cargo.toml for the same reason: a
// crate-local `[lints]` table and `[lints] workspace = true` are mutually
// exclusive. This crate has no buffer to alias and no device to hand a pointer
// to — it is a state machine over integers and `alloc` collections — so there
// is nothing here that could ever earn an exemption.
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use akuma_primitives::errno as libc_errno;

#[cfg(test)]
mod tests;

// ============================================================================
// Constants
// ============================================================================

/// `AF_UNIX` / `AF_LOCAL`.
pub const AF_UNIX: u16 = 1;

/// `sizeof(struct sockaddr_un.sun_path)` on Linux. Not negotiable — it is the
/// on-the-wire size userspace passes, and a name may occupy all 108 bytes with
/// no terminator (see [`SockAddrUn::decode`]).
pub const SUN_PATH_LEN: usize = 108;

/// `offsetof(struct sockaddr_un, sun_path)` — the two `sun_family` bytes. An
/// `addrlen` of exactly this means "unnamed", which is what an unbound
/// socketpair endpoint reports from `getsockname`.
pub const SUN_PATH_OFFSET: usize = 2;

/// Full `sizeof(struct sockaddr_un)`.
pub const SOCKADDR_UN_LEN: usize = SUN_PATH_OFFSET + SUN_PATH_LEN;

/// Default listen backlog when `listen(fd, 0)` is called. Linux clamps 0 up to
/// a small non-zero value rather than making the listener useless; so do we.
pub const DEFAULT_BACKLOG: usize = 8;

/// Hard ceiling on a listener's pending-connection queue.
///
/// Whatever `listen(2)` asks for. Each queued entry costs two kernel pipes'
/// worth of bookkeeping once accepted, and the queue's length is
/// attacker-controlled, so it is capped.
pub const MAX_BACKLOG: usize = 128;

/// Default `SO_SNDBUF`, and so the largest single datagram accepted.
///
/// Matches the kernel's `PIPE_CAPACITY`, because the bytes go into a pipe: a
/// datagram larger than the pipe can ever hold could never be sent atomically,
/// so it must be rejected with `EMSGSIZE` up front rather than blocking
/// forever.
pub const DEFAULT_SNDBUF: usize = 65536;

// ============================================================================
// Socket type
// ============================================================================

/// The three AF_UNIX socket types, and the only place their differing framing
/// rules are written down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SockType {
    /// `SOCK_STREAM` (1): a byte stream. Writes coalesce, reads take whatever
    /// is available up to the buffer size. No record boundaries.
    Stream,
    /// `SOCK_DGRAM` (2): unreliable-ordered datagrams, may be unconnected
    /// (`sendto` with a destination name). Boundaries preserved; a zero-length
    /// datagram is a real, deliverable message.
    Dgram,
    /// `SOCK_SEQPACKET` (5): connection-oriented datagrams. Boundaries
    /// preserved, like `Dgram`; connection semantics, like `Stream`.
    SeqPacket,
}

impl SockType {
    /// Decode the `type` argument of `socket(2)`/`socketpair(2)`, with the
    /// `SOCK_NONBLOCK`/`SOCK_CLOEXEC` flag bits already masked off by the
    /// caller.
    #[must_use]
    pub const fn from_raw(base_type: i32) -> Option<Self> {
        match base_type {
            1 => Some(Self::Stream),
            2 => Some(Self::Dgram),
            5 => Some(Self::SeqPacket),
            _ => None,
        }
    }

    /// The value `getsockopt(SO_TYPE)` must report. Answering a hardcoded `1`
    /// here — which the pre-existing `sys_getsockopt` did for every non-AF_INET
    /// fd — makes a `SEQPACKET` or `DGRAM` socket misreport its own type, and a
    /// client that switches framing on the answer then reads the stream wrong.
    #[must_use]
    pub const fn to_raw(self) -> i32 {
        match self {
            Self::Stream => 1,
            Self::Dgram => 2,
            Self::SeqPacket => 5,
        }
    }

    /// Whether this type preserves message boundaries. The single predicate
    /// every framing decision in this module turns on.
    #[must_use]
    pub const fn is_framed(self) -> bool {
        matches!(self, Self::Dgram | Self::SeqPacket)
    }

    /// Whether this type uses `connect`/`accept` rendezvous. `Dgram` may be
    /// connected (to set a default peer) but does not have to be, and never has
    /// a listener.
    #[must_use]
    pub const fn is_connection_oriented(self) -> bool {
        matches!(self, Self::Stream | Self::SeqPacket)
    }
}

// ============================================================================
// Addresses
// ============================================================================

/// A bound AF_UNIX name.
///
/// Two namespaces, deliberately kept as separate variants rather than one byte
/// string with a convention: the abstract namespace has no filesystem presence
/// at all (no inode, no permissions, no `unlink`), and conflating the two is
/// how a `bind` ends up creating a file for a name that should not have one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnixName {
    /// Never bound. `getsockname` reports `addrlen == 2` and no path — what
    /// Linux reports for a fresh `socket()` or a `socketpair` endpoint.
    Unnamed,
    /// Linux's abstract namespace: `sun_path[0] == '\0'`, the remaining
    /// `addrlen - 3` bytes are the name **verbatim**, embedded NULs and all.
    /// No filesystem involvement.
    Abstract(Vec<u8>),
    /// A filesystem pathname, NUL-terminated in `sun_path` (or occupying all
    /// 108 bytes with no terminator).
    Path(Vec<u8>),
}

impl UnixName {
    /// True for a name that needs a filesystem node created at `bind` and
    /// removed by `unlink`.
    #[must_use]
    pub const fn is_filesystem(&self) -> bool {
        matches!(self, Self::Path(_))
    }

    /// The path bytes, for the caller that has to touch the VFS. `None` for
    /// unnamed and abstract names — which is the point: a caller cannot
    /// accidentally create a file for an abstract name, because there is no
    /// path to create it at.
    #[must_use]
    pub fn path_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Path(p) => Some(p),
            _ => None,
        }
    }
}

/// Linux `struct sockaddr_un`, as it crosses the syscall boundary.
///
/// Kept as a plain byte buffer plus a length rather than a `#[repr(C)]` struct
/// with a `[u8; 108]` field, because every interesting case is about the
/// *length*: userspace passes `addrlen` separately, and `addrlen` — not a NUL
/// scan — is what delimits an abstract name.
#[derive(Debug, Clone, Copy)]
pub struct SockAddrUn {
    /// The raw `sockaddr_un` bytes, zero-padded.
    pub bytes: [u8; SOCKADDR_UN_LEN],
    /// The `addrlen` that accompanies them. Always `>= SUN_PATH_OFFSET` for a
    /// value produced by [`SockAddrUn::encode`].
    pub len: usize,
}

impl Default for SockAddrUn {
    fn default() -> Self {
        Self { bytes: [0u8; SOCKADDR_UN_LEN], len: SUN_PATH_OFFSET }
    }
}

impl SockAddrUn {
    /// Parse a `sockaddr_un` that userspace passed with `addrlen == raw.len()`.
    ///
    /// Every case here is a real client behaviour, and each one is a way to
    /// read out of bounds or invent a name that was not asked for:
    ///
    /// - `addrlen < 2` — cannot even hold `sun_family`: `EINVAL`.
    /// - `sun_family != AF_UNIX`: `EAFNOSUPPORT`.
    /// - `addrlen == 2` — unnamed. `bind` to it is the Linux "autobind"
    ///   request, which this module reports as `EINVAL` rather than inventing
    ///   an abstract name; `connect` to it is `EINVAL`.
    /// - `sun_path[0] == 0` — abstract. The name is the remaining
    ///   `addrlen - 3` bytes **verbatim**: no NUL scan, because an abstract
    ///   name may legally contain NULs, and `strlen` would truncate it.
    /// - otherwise — a pathname, delimited by the first NUL **or** by
    ///   `addrlen`, whichever comes first. A client that fills all 108 bytes
    ///   with no terminator is legal, and the reason this is not `CStr::from`.
    pub fn decode(raw: &[u8]) -> Result<UnixName, i32> {
        if raw.len() < SUN_PATH_OFFSET {
            return Err(libc_errno::EINVAL);
        }
        let family = u16::from_ne_bytes([raw[0], raw[1]]);
        if family != AF_UNIX {
            return Err(libc_errno::EAFNOSUPPORT);
        }
        // Never trust `addrlen` past the struct: a caller may pass a larger
        // value than `sizeof(sockaddr_un)` and Linux ignores the excess.
        let end = raw.len().min(SOCKADDR_UN_LEN);
        let path = &raw[SUN_PATH_OFFSET..end];
        if path.is_empty() {
            return Ok(UnixName::Unnamed);
        }
        if path[0] == 0 {
            // Abstract. An all-zero `sun_path` with addrlen == 3 is the
            // zero-length abstract name, which Linux accepts and which is
            // distinct from unnamed — keep it.
            return Ok(UnixName::Abstract(path[1..].to_vec()));
        }
        let n = path.iter().position(|&b| b == 0).unwrap_or(path.len());
        Ok(UnixName::Path(path[..n].to_vec()))
    }

    /// Render a name back into a `sockaddr_un` + `addrlen` for
    /// `getsockname`/`getpeername`/`recvfrom`.
    ///
    /// The `addrlen` returned is what Linux reports, and the three cases differ:
    /// unnamed is `2`, an abstract name is `3 + name.len()` (the leading NUL
    /// counts, no terminator), and a path is `2 + path.len() + 1` (the
    /// terminator counts).
    #[must_use]
    pub fn encode(name: &UnixName) -> Self {
        let mut out = Self::default();
        out.bytes[0..2].copy_from_slice(&AF_UNIX.to_ne_bytes());
        match name {
            UnixName::Unnamed => out.len = SUN_PATH_OFFSET,
            UnixName::Abstract(n) => {
                let n = &n[..n.len().min(SUN_PATH_LEN - 1)];
                // bytes[2] stays 0 — that leading NUL *is* the abstract marker.
                out.bytes[SUN_PATH_OFFSET + 1..SUN_PATH_OFFSET + 1 + n.len()].copy_from_slice(n);
                out.len = SUN_PATH_OFFSET + 1 + n.len();
            }
            UnixName::Path(p) => {
                let p = &p[..p.len().min(SUN_PATH_LEN - 1)];
                out.bytes[SUN_PATH_OFFSET..SUN_PATH_OFFSET + p.len()].copy_from_slice(p);
                // The NUL is already there (buffer is zeroed) and is counted.
                out.len = SUN_PATH_OFFSET + p.len() + 1;
            }
        }
        out
    }

    /// The populated prefix, which is what gets copied out to userspace.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len.min(SOCKADDR_UN_LEN)]
    }
}

// ============================================================================
// Shutdown
// ============================================================================

/// Which halves of a socket `shutdown(2)` has retired.
///
/// This is not cosmetic bookkeeping: it is the input to the readiness
/// predicates, and the AF_INET side of exactly this got four defects wrong at
/// once (`docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`) — a socket that
/// reported read-closed when it was not made a tokio client park forever. The
/// rule that keeps that from recurring is that "readable" and "at EOF" are
/// different answers, and only this state can tell them apart on a socket whose
/// pipe still has bytes in it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shutdown {
    /// `SHUT_RD`: reads return 0 once drained; new data is discarded.
    pub rd: bool,
    /// `SHUT_WR`: writes return `EPIPE`; the peer sees EOF.
    pub wr: bool,
}

impl Shutdown {
    /// Apply a `shutdown(2)` `how` argument. Returns `EINVAL` for anything
    /// outside `SHUT_RD`/`SHUT_WR`/`SHUT_RDWR`.
    pub fn apply(&mut self, how: i32) -> Result<(), i32> {
        match how {
            0 => self.rd = true,
            1 => self.wr = true,
            2 => {
                self.rd = true;
                self.wr = true;
            }
            _ => return Err(libc_errno::EINVAL),
        }
        Ok(())
    }
}

// ============================================================================
// Credentials
// ============================================================================

/// `struct ucred` — what `SO_PEERCRED` and `SCM_CREDENTIALS` report.
///
/// Captured at **connect** time, not at send time. That is the security-relevant
/// part: a daemon that authorises a client by uid must see the uid the client
/// had when it connected, so a client cannot connect as root, drop privileges,
/// and have the daemon observe the lower uid (or the reverse — connect
/// unprivileged and later appear privileged). Linux has this property and
/// anything that gates on peer uid depends on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ucred {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

// ============================================================================
// Records: the boundaries running parallel to the pipe's bytes
// ============================================================================

/// One queued message on a channel.
///
/// `len` is a byte count in the *pipe*, not storage here — see the module
/// docs' bytes/boundaries split. For `SOCK_STREAM` records still exist (so
/// ancillary data has something to attach to) but reads coalesce across them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    /// Payload length in bytes. May be 0: a zero-length datagram is a real,
    /// deliverable message and must be distinguishable from EOF.
    pub len: usize,
    /// Descriptors passed with this record via `SCM_RIGHTS`, as opaque tokens
    /// the kernel resolves back to `FileDescriptor` values.
    ///
    /// Attached to the **record**, not to the socket. Attaching them to the
    /// socket would deliver the second message's fds with the first, which is
    /// both wrong and a confused-deputy bug — a receiver would act on a
    /// descriptor it has not yet been told about.
    pub anc_fds: Vec<u32>,
}

/// The framing metadata for one direction of one connection, keyed in
/// [`UnixTable`] by the id of the pipe carrying its bytes.
#[derive(Debug, Clone)]
pub struct Channel {
    /// Queued records, oldest first. Empty for an idle channel.
    pub records: VecDeque<Record>,
    /// Stream bytes ahead of `records` that carry no ancillary data and so
    /// needed no [`Record`] of their own.
    ///
    /// This exists so the plain `SOCK_STREAM` path can stay allocation-free
    /// (see [`UnixTable::commit_write`]) **without** losing the ordering
    /// between bytes and descriptors. Without it, bytes written before an
    /// `SCM_RIGHTS` message would be invisible, the fds' record would sit at
    /// the front of the queue, and a reader draining just those earlier bytes
    /// would be handed the descriptors one message too soon — a receiver acting
    /// on a descriptor it has not been told about.
    ///
    /// The channel layout is therefore always: `pending_bytes` first, then
    /// `records`. Plain writes coalesce into whichever of the two is at the
    /// tail; a write carrying descriptors materialises `pending_bytes` into a
    /// record first, which is the only place the plain path can ever allocate,
    /// and only on the first descriptor a channel carries.
    pub pending_bytes: usize,
    /// `SO_SNDBUF`: the byte ceiling, and for a datagram type the largest
    /// single message accepted.
    pub sndbuf: usize,
}

/// Deliberately hand-written rather than derived.
///
/// A derived `Default` would give `sndbuf == 0`, and every datagram send on
/// that channel would then fail `EMSGSIZE` — a total, silent loss of function
/// for one socket, reachable from any `entry().or_default()`. Making `default`
/// and [`Channel::new`] the same value removes that trap instead of documenting
/// it.
impl Default for Channel {
    fn default() -> Self {
        Self::new()
    }
}

impl Channel {
    #[must_use]
    pub fn new() -> Self {
        Self { records: VecDeque::new(), pending_bytes: 0, sndbuf: DEFAULT_SNDBUF }
    }

    /// Total bytes this channel believes are in flight. Must equal the pipe's
    /// buffered byte count; a divergence is a framing desync and the reason
    /// datagram writes are all-or-nothing.
    #[must_use]
    pub fn queued_bytes(&self) -> usize {
        self.pending_bytes + self.records.iter().map(|r| r.len).sum::<usize>()
    }

    /// Number of queued records — what a `SOCK_DGRAM` receiver can still read.
    #[must_use]
    pub fn queued_records(&self) -> usize {
        self.records.len()
    }
}

/// What a send should do, once [`plan_write`] has decided it is legal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WritePlan {
    /// Bytes to hand to `pipe_write`. For a framed type this always equals the
    /// caller's full length — a datagram is never split.
    pub bytes: usize,
    /// Whether to push a [`Record`] for these bytes. False only for a
    /// `SOCK_STREAM` write, which has no boundaries to record.
    pub push_record: bool,
}

/// Decide what a send of `len` bytes does, given the room left in the pipe.
///
/// This is the function that keeps the bytes and the boundaries in sync, and
/// the asymmetry between the two arms is the whole point:
///
/// - **`Stream`**: a short write is correct and normal. Accept `min(len, room)`
///   and let the caller loop, exactly as `write(2)` is specified.
/// - **`Dgram`/`SeqPacket`**: a short write is *unrepresentable*. There is no
///   way to record "two thirds of a message"; the next boundary would be wrong
///   by the shortfall and the channel would never resynchronise. So a message
///   that does not fit entirely is `EAGAIN` (the caller blocks or reports it),
///   and one that can never fit — larger than `sndbuf` — is `EMSGSIZE`
///   immediately rather than a permanent block.
///
/// A zero-length send is legal for both, and for a framed type it produces a
/// real record: `recv` must be able to return 0 for a zero-length datagram
/// without that meaning EOF.
pub fn plan_write(ty: SockType, len: usize, room: usize, sndbuf: usize) -> Result<WritePlan, i32> {
    if ty.is_framed() {
        if len > sndbuf {
            // Can never fit, however long we wait.
            return Err(libc_errno::EMSGSIZE);
        }
        if len > room {
            return Err(libc_errno::EAGAIN);
        }
        return Ok(WritePlan { bytes: len, push_record: true });
    }
    if len == 0 {
        return Ok(WritePlan { bytes: 0, push_record: false });
    }
    if room == 0 {
        return Err(libc_errno::EAGAIN);
    }
    Ok(WritePlan { bytes: len.min(room), push_record: false })
}

/// What a receive should do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadPlan {
    /// Bytes to copy out to the caller's buffer.
    pub take: usize,
    /// Bytes to read from the pipe and **throw away**: the tail of a record
    /// that did not fit. Zero for a stream.
    pub discard: usize,
    /// Set `MSG_TRUNC` in `msg_flags`: a record was truncated.
    pub truncated: bool,
    /// Pop the front record after the copy.
    pub consume_record: bool,
}

/// Decide what a receive of at most `buflen` bytes does.
///
/// The framed arm is where the pre-existing implementation was wrong, and the
/// assertion that catches it is the `discard` field. On a real `SOCK_SEQPACKET`
/// socket, reading a 10-byte record into a 4-byte buffer returns 4 and
/// **destroys the remaining 6** — the record is consumed whole. Leaving them
/// for the next read is what a byte stream does, and it is what Akuma's
/// pipe-backed approximation did: two 10-byte sends then a 20-byte read
/// returned 20 instead of 10, so a client that framed on the return value
/// silently merged two messages.
///
/// `peek` (`MSG_PEEK`) suppresses both the discard and the record pop, so the
/// next read sees the record intact.
#[must_use]
pub fn plan_read(
    ty: SockType,
    front_record: Option<usize>,
    avail: usize,
    buflen: usize,
    peek: bool,
) -> ReadPlan {
    if ty.is_framed() {
        let Some(rec) = front_record else {
            return ReadPlan::default();
        };
        let take = rec.min(buflen);
        let discard = if peek { 0 } else { rec - take };
        return ReadPlan {
            take,
            discard,
            truncated: rec > buflen,
            consume_record: !peek,
        };
    }
    ReadPlan {
        take: avail.min(buflen),
        discard: 0,
        truncated: false,
        consume_record: false,
    }
}

// ============================================================================
// The socket entry
// ============================================================================

/// Lifecycle position of a unix socket.
///
/// Kept explicit because the errno a `connect`/`accept`/`send` must return is a
/// function of it, and "unbound" vs "bound but not listening" produce
/// *different* answers that a client's restart path depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SockState {
    /// Created, never bound, never connected.
    Unbound,
    /// `bind` succeeded; no `listen` yet. A `connect` here is `ECONNREFUSED`,
    /// which is exactly the stale-socket-file case: the daemon died, the node
    /// (or table entry) outlived it, and a client must be told "nobody home"
    /// rather than hanging.
    Bound,
    /// `listen` succeeded. Has a backlog; is not itself readable/writable for
    /// data.
    Listening,
    /// Connected to a peer, or (for `Dgram`) has a default peer set.
    Connected,
    /// Peer gone or `shutdown(SHUT_RDWR)`; still open as an fd.
    Disconnected,
}

/// A pending connection sitting in a listener's backlog: the client side has
/// already been given its endpoint, and `accept` only has to claim the server
/// side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    /// The server-side socket id created at `connect` time.
    pub server_sock: u32,
    /// The connecting client's socket id, for `getpeername` on the accepted fd.
    pub client_sock: u32,
    /// Credentials captured from the connecting process — see [`Ucred`].
    pub client_creds: Ucred,
}

/// One AF_UNIX socket.
#[derive(Debug, Clone)]
pub struct UnixSock {
    pub ty: SockType,
    pub state: SockState,
    /// This socket's own bound name, reported by `getsockname`.
    pub name: UnixName,
    /// The peer's bound name, reported by `getpeername`. An accepted server
    /// endpoint's peer is usually `Unnamed` (clients rarely bind), which is
    /// what Linux reports too.
    pub peer_name: UnixName,
    /// Pipe this endpoint reads from (`0` = none).
    pub rx: u32,
    /// Pipe this endpoint writes to (`0` = none).
    pub tx: u32,
    /// The connected peer's socket id, if any.
    pub peer: Option<u32>,
    pub shutdown: Shutdown,
    pub creds: Ucred,
    pub peer_creds: Ucred,
    /// Pending connections; non-empty only while `Listening`.
    pub backlog: VecDeque<Pending>,
    /// Effective backlog ceiling, from `listen(2)` clamped to [`MAX_BACKLOG`].
    pub backlog_max: usize,
    /// How many fds reference this entry. `dup`/`fork` bump it; the entry is
    /// removed when it reaches zero.
    pub refs: u32,
}

impl UnixSock {
    #[must_use]
    pub fn new(ty: SockType, creds: Ucred) -> Self {
        Self {
            ty,
            state: SockState::Unbound,
            name: UnixName::Unnamed,
            peer_name: UnixName::Unnamed,
            rx: 0,
            tx: 0,
            peer: None,
            shutdown: Shutdown::default(),
            creds,
            peer_creds: Ucred::default(),
            backlog: VecDeque::new(),
            backlog_max: DEFAULT_BACKLOG,
            refs: 1,
        }
    }

    /// Whether a listener has a connection ready for `accept`. This is the one
    /// genuinely *new* readiness predicate AF_UNIX adds, and it must report
    /// identically through `poll`, `select` and `epoll` — the AF_INET side of
    /// that contract is what the `_exceptfds_ptr` bug violated
    /// (`docs/runbooks/cargo-cannot-reach-crates-io.md` §3).
    #[must_use]
    pub fn accept_ready(&self) -> bool {
        self.state == SockState::Listening && !self.backlog.is_empty()
    }
}

// ============================================================================
// The table
// ============================================================================

/// Outcome of a `connect(2)` attempt against the name table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectOutcome {
    /// Queued on the listener's backlog. The caller must create the two pipes,
    /// wire both endpoints, and wake anything parked in `accept`.
    Queued { listener: u32, server_sock: u32 },
    /// A `SOCK_DGRAM` connect: no rendezvous, just a recorded default peer.
    DgramPeerSet { peer: u32 },
}

/// Every AF_UNIX socket in the system, the names they are bound to, and the
/// framing metadata for their channels.
///
/// Deliberately *not* a global: the kernel owns exactly one of these behind a
/// lock, and host tests own as many as they like. A `static` here would make
/// the state machine untestable in parallel, which is the mistake this module
/// exists to avoid.
#[derive(Debug, Default)]
pub struct UnixTable {
    socks: BTreeMap<u32, UnixSock>,
    /// Bound name → socket id. Covers both namespaces; the `UnixName` variant
    /// keeps them from colliding, so an abstract `"foo"` and a path `"foo"` are
    /// different keys.
    names: BTreeMap<UnixName, u32>,
    /// Framing metadata keyed by the pipe id carrying the bytes.
    channels: BTreeMap<u32, Channel>,
    next_id: u32,
}

impl UnixTable {
    #[must_use]
    pub fn new() -> Self {
        Self {
            socks: BTreeMap::new(),
            names: BTreeMap::new(),
            channels: BTreeMap::new(),
            next_id: 1,
        }
    }

    // ---- lifecycle ---------------------------------------------------------

    /// Allocate an unbound socket. Ids start at 1 so `0` is usable as "none"
    /// on the kernel's `FileDescriptor::UnixSocket { sock }` field.
    pub fn alloc(&mut self, ty: SockType, creds: Ucred) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.socks.insert(id, UnixSock::new(ty, creds));
        id
    }

    #[must_use]
    pub fn get(&self, id: u32) -> Option<&UnixSock> {
        self.socks.get(&id)
    }

    pub fn get_mut(&mut self, id: u32) -> Option<&mut UnixSock> {
        self.socks.get_mut(&id)
    }

    /// Number of live socket entries. A leak check: after every fd is closed
    /// this must return to its baseline, and the accumulating-leak classes in
    /// this design (a closed listener with queued connects, an unread record
    /// holding `SCM_RIGHTS` fds) are only visible as a drift here.
    #[must_use]
    pub fn len(&self) -> usize {
        self.socks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.socks.is_empty()
    }

    /// Number of bound names. Must also return to baseline: a name left behind
    /// by a closed listener makes the next `bind` fail with `EADDRINUSE`
    /// forever, which for a daemon means it can never restart.
    #[must_use]
    pub fn name_count(&self) -> usize {
        self.names.len()
    }

    /// Take one reference. Mirrors `pipe_clone_ref` — `dup`, `dup2`, `F_DUPFD`
    /// and `fork` all produce a real second reference, and the first close must
    /// not destroy the entry underneath the other fd.
    pub fn clone_ref(&mut self, id: u32) {
        if let Some(s) = self.socks.get_mut(&id) {
            s.refs = s.refs.saturating_add(1);
        }
    }

    /// Drop one reference; tear the socket down at zero.
    ///
    /// Returns the ids of any **server-side sockets still sitting in this
    /// socket's backlog**, which the caller must also close. That return value
    /// is the leak this function exists to prevent: a listener destroyed with N
    /// queued connections holds the only reference to N server endpoints, and
    /// dropping the listener without them leaks N entries and two pipes each,
    /// while the N clients park forever on a connection nobody will ever accept.
    pub fn close(&mut self, id: u32) -> Vec<u32> {
        // Field-split so the name can be *borrowed* out of `socks` while
        // `names` is mutated. Cloning it instead would heap-allocate on every
        // close of a named socket, for a value used only as a map key.
        let Self { socks, names, .. } = self;
        let Some(s) = socks.get_mut(&id) else {
            return Vec::new();
        };
        s.refs = s.refs.saturating_sub(1);
        if s.refs > 0 {
            return Vec::new();
        }
        // `collect` from an empty iterator does not allocate, so the common
        // case — a socket with no queued connections — costs nothing here.
        let orphans: Vec<u32> = s.backlog.iter().map(|p| p.server_sock).collect();
        // Only release the name if it still maps to *this* socket. A daemon
        // that died and restarted may have rebound the same name to a new
        // socket, and releasing it here would silently unbind the live one.
        if s.name != UnixName::Unnamed && names.get(&s.name) == Some(&id) {
            names.remove(&s.name);
        }
        self.socks.remove(&id);
        // Tell the peer its other end is gone, so a `send` there reports
        // `EPIPE`/`ECONNRESET` instead of writing into a channel with no reader.
        for s in self.socks.values_mut() {
            if s.peer == Some(id) {
                s.peer = None;
                s.state = SockState::Disconnected;
            }
        }
        orphans
    }

    // ---- names -------------------------------------------------------------

    /// `bind(2)`.
    ///
    /// `EINVAL` for an unnamed address (Linux's autobind, deliberately not
    /// implemented — see [`SockAddrUn::decode`]) or for a socket that is
    /// already bound. `EADDRINUSE` for a name another live socket holds; a
    /// name whose holder has closed is free again, which is what lets a daemon
    /// restart.
    pub fn bind(&mut self, id: u32, name: UnixName) -> Result<(), i32> {
        if name == UnixName::Unnamed {
            return Err(libc_errno::EINVAL);
        }
        if self.names.contains_key(&name) {
            return Err(libc_errno::EADDRINUSE);
        }
        let s = self.socks.get_mut(&id).ok_or(libc_errno::EBADF)?;
        if s.name != UnixName::Unnamed {
            return Err(libc_errno::EINVAL);
        }
        s.name = name.clone();
        if s.state == SockState::Unbound {
            s.state = SockState::Bound;
        }
        self.names.insert(name, id);
        Ok(())
    }

    /// Whether a name is currently bound by a live socket. The kernel needs
    /// this to decide whether a *filesystem* node it found is a live socket or
    /// a stale file left by a dead daemon.
    #[must_use]
    pub fn is_bound(&self, name: &UnixName) -> bool {
        self.names.contains_key(name)
    }

    /// `listen(2)`. `EINVAL` for a socket that was never bound (there is
    /// nothing for a client to address) or for `SOCK_DGRAM`, which has no
    /// connection to queue.
    pub fn listen(&mut self, id: u32, backlog: i32) -> Result<(), i32> {
        let s = self.socks.get_mut(&id).ok_or(libc_errno::EBADF)?;
        if !s.ty.is_connection_oriented() {
            return Err(libc_errno::EOPNOTSUPP);
        }
        if s.name == UnixName::Unnamed {
            return Err(libc_errno::EINVAL);
        }
        // Linux clamps a 0 or negative backlog up rather than creating a
        // listener that can never accept anything.
        let want = if backlog <= 0 { DEFAULT_BACKLOG } else { backlog as usize };
        s.backlog_max = want.min(MAX_BACKLOG);
        s.state = SockState::Listening;
        Ok(())
    }

    // ---- rendezvous --------------------------------------------------------

    /// `connect(2)` against a name.
    ///
    /// The three refusal cases are distinct on purpose:
    ///
    /// | situation | errno | why it matters |
    /// |---|---|---|
    /// | no socket bound to the name | `ECONNREFUSED` | a stale socket file; the client must retry or give up, not hang |
    /// | bound but never `listen`ed | `ECONNREFUSED` | a daemon that crashed between bind and listen looks the same to a client |
    /// | backlog full | `EAGAIN` | transient; a blocking client should wait, not fail |
    ///
    /// Returning `ENOENT` or hanging for the first two is what makes a daemon
    /// restart look like a network outage.
    pub fn connect(
        &mut self,
        id: u32,
        name: &UnixName,
        creds: Ucred,
    ) -> Result<ConnectOutcome, i32> {
        if *name == UnixName::Unnamed {
            return Err(libc_errno::EINVAL);
        }
        let ty = self.socks.get(&id).ok_or(libc_errno::EBADF)?.ty;
        let target = *self.names.get(name).ok_or(libc_errno::ECONNREFUSED)?;

        if !ty.is_connection_oriented() {
            // SOCK_DGRAM connect only records a default destination.
            let t = self.socks.get(&target).ok_or(libc_errno::ECONNREFUSED)?;
            if t.ty != SockType::Dgram {
                return Err(libc_errno::EPROTOTYPE);
            }
            let peer_name = name.clone();
            let s = self.socks.get_mut(&id).ok_or(libc_errno::EBADF)?;
            s.peer = Some(target);
            s.peer_name = peer_name;
            s.state = SockState::Connected;
            return Ok(ConnectOutcome::DgramPeerSet { peer: target });
        }

        {
            let t = self.socks.get(&target).ok_or(libc_errno::ECONNREFUSED)?;
            if t.ty != ty {
                return Err(libc_errno::EPROTOTYPE);
            }
            if t.state != SockState::Listening {
                return Err(libc_errno::ECONNREFUSED);
            }
            if t.backlog.len() >= t.backlog_max {
                return Err(libc_errno::EAGAIN);
            }
        }

        // The server side of the connection exists from `connect` time, not
        // from `accept` time: the client is handed a working endpoint
        // immediately (Linux behaviour — a client may write before the server
        // accepts, and those bytes must be buffered, not lost).
        let listener_creds = self.socks[&target].creds;
        let listener_name = self.socks[&target].name.clone();
        let server_sock = self.alloc(ty, listener_creds);
        {
            let srv = self.socks.get_mut(&server_sock).ok_or(libc_errno::EBADF)?;
            srv.state = SockState::Connected;
            srv.peer = Some(id);
            srv.name = listener_name;
            srv.peer_creds = creds;
        }
        {
            let cli = self.socks.get_mut(&id).ok_or(libc_errno::EBADF)?;
            cli.state = SockState::Connected;
            cli.peer = Some(server_sock);
            cli.peer_name = name.clone();
            cli.peer_creds = listener_creds;
        }
        let listener = self.socks.get_mut(&target).ok_or(libc_errno::EBADF)?;
        listener.backlog.push_back(Pending {
            server_sock,
            client_sock: id,
            client_creds: creds,
        });
        Ok(ConnectOutcome::Queued { listener: target, server_sock })
    }

    /// Resolve a `sendto` destination name to the socket that owns it and the
    /// pipe carrying its receive queue.
    ///
    /// # Why a datagram socket has ONE queue, not a pipe per peer
    ///
    /// A `SOCK_DGRAM` socket is written to by anyone who knows its name, with no
    /// rendezvous and no per-peer state — that is the whole point of the type,
    /// and it is what `/dev/log` relies on (musl's `syslog(3)` is a connect and
    /// a send, from every process on the system). Modelling it as a pipe pair
    /// per sender would mean allocating two pipes on every first send from a new
    /// peer, and a receiver would then have to poll all of them. Linux gives
    /// each socket a single receive queue and so does this: the destination's
    /// `rx` pipe *is* its queue, and a sender writes one record into it.
    ///
    /// The errnos are the ones a sender can act on:
    ///
    /// - no socket bound to the name → `ECONNREFUSED` (a stale node, or a
    ///   syslogd that is not running), never a hang.
    /// - bound, but not a datagram socket → `EPROTOTYPE`: the name exists and
    ///   belongs to something, but sending a datagram at a stream listener
    ///   would put bytes into a queue framed by different rules.
    pub fn resolve_dgram_dest(&self, name: &UnixName) -> Result<(u32, u32), i32> {
        if *name == UnixName::Unnamed {
            return Err(libc_errno::EDESTADDRREQ);
        }
        let target = *self.names.get(name).ok_or(libc_errno::ECONNREFUSED)?;
        let s = self.socks.get(&target).ok_or(libc_errno::ECONNREFUSED)?;
        if s.ty != SockType::Dgram {
            return Err(libc_errno::EPROTOTYPE);
        }
        if s.rx == 0 {
            // A datagram socket without a queue cannot receive. Reported as
            // refused rather than as a kernel error: from the sender's side it
            // is indistinguishable from nothing being there.
            return Err(libc_errno::ECONNREFUSED);
        }
        Ok((target, s.rx))
    }

    /// The receive queue of this socket's recorded default peer — where a
    /// `send(2)` (no destination) on a *connected* datagram socket goes.
    ///
    /// `EDESTADDRREQ` when there is no default peer, which is precisely what
    /// `send` on an unconnected datagram socket must report: the call is
    /// missing the address, not broken.
    pub fn dgram_default_dest(&self, id: u32) -> Result<u32, i32> {
        let s = self.socks.get(&id).ok_or(libc_errno::EBADF)?;
        let peer = s.peer.ok_or(libc_errno::EDESTADDRREQ)?;
        let t = self.socks.get(&peer).ok_or(libc_errno::ECONNREFUSED)?;
        if t.rx == 0 {
            return Err(libc_errno::ECONNREFUSED);
        }
        Ok(t.rx)
    }

    /// Give a datagram socket its receive queue.
    ///
    /// Both `rx` and `tx` are set to the same pipe id, and `tx` is **not** a
    /// send path for this type — every datagram send resolves its destination
    /// fresh. The duplication exists so the fd teardown path stays uniform:
    /// closing a `UnixSocket` fd does `pipe_close_read(rx)` and
    /// `pipe_close_write(tx)`, so a queue recorded only in `rx` would keep
    /// `write_count == 1` forever and the pipe would never be destroyed — a leak
    /// of one pipe per datagram socket, with nothing to point at it.
    pub fn attach_dgram_queue(&mut self, id: u32, queue: u32) {
        self.channels.entry(queue).or_default();
        if let Some(s) = self.socks.get_mut(&id) {
            s.rx = queue;
            s.tx = queue;
        }
    }

    /// `accept(2)`: claim the oldest queued connection.
    ///
    /// `EINVAL` if the socket is not listening, `EAGAIN` if the backlog is
    /// empty. `EAGAIN` rather than blocking here is deliberate — this module
    /// has no scheduler; the kernel decides whether to park.
    pub fn accept(&mut self, id: u32) -> Result<Pending, i32> {
        let s = self.socks.get_mut(&id).ok_or(libc_errno::EBADF)?;
        if s.state != SockState::Listening {
            return Err(libc_errno::EINVAL);
        }
        s.backlog.pop_front().ok_or(libc_errno::EAGAIN)
    }

    /// Pair two freshly created endpoints — `socketpair(2)`, and also the two
    /// halves of a completed `connect`/`accept`.
    ///
    /// `a_rx`/`a_tx` are pipe ids; the peer gets them swapped. Registers a
    /// [`Channel`] for each direction, which is what gives the pair real
    /// `SEQPACKET` framing instead of the byte-stream approximation the
    /// pipe-only implementation had.
    pub fn pair(&mut self, a: u32, b: u32, a_rx: u32, a_tx: u32) {
        self.channels.entry(a_rx).or_default();
        self.channels.entry(a_tx).or_default();
        // Both ends of a `socketpair` belong to the same process, so each one's
        // peer credentials are its own. Leaving `peer_creds` at its default made
        // `SO_PEERCRED` report **pid 0** on a socketpair — found by
        // `nettest-unix peercred` diffing against the Linux arm, which reports
        // the calling process. A daemon identifying its peer by pid would have
        // read 0 for everyone.
        let a_creds = self.socks.get(&a).map(|s| s.creds);
        let b_creds = self.socks.get(&b).map(|s| s.creds);
        if let Some(s) = self.socks.get_mut(&a) {
            s.rx = a_rx;
            s.tx = a_tx;
            s.peer = Some(b);
            s.state = SockState::Connected;
            if let Some(c) = b_creds {
                s.peer_creds = c;
            }
        }
        if let Some(s) = self.socks.get_mut(&b) {
            s.rx = a_tx;
            s.tx = a_rx;
            s.peer = Some(a);
            s.state = SockState::Connected;
            if let Some(c) = a_creds {
                s.peer_creds = c;
            }
        }
    }

    // ---- channels ----------------------------------------------------------

    #[must_use]
    pub fn channel(&self, pipe_id: u32) -> Option<&Channel> {
        self.channels.get(&pipe_id)
    }

    pub fn channel_mut(&mut self, pipe_id: u32) -> Option<&mut Channel> {
        self.channels.get_mut(&pipe_id)
    }

    /// Register a channel for a pipe that already exists (the kernel creates
    /// the pipes; this attaches the framing metadata to them).
    pub fn attach_channel(&mut self, pipe_id: u32) {
        self.channels.entry(pipe_id).or_default();
    }

    /// Forget a channel once its pipe is destroyed. Returns every `SCM_RIGHTS`
    /// descriptor still sitting in an unread record, which the caller **must**
    /// close: those are real references, and dropping the channel without
    /// releasing them is a silent fd leak that no probe can observe.
    pub fn detach_channel(&mut self, pipe_id: u32) -> Vec<u32> {
        let Some(ch) = self.channels.remove(&pipe_id) else {
            return Vec::new();
        };
        // `collect` on an all-empty iterator does not allocate, so the common
        // case (no ancillary data anywhere) costs nothing.
        ch.records.into_iter().flat_map(|r| r.anc_fds).collect()
    }

    /// Record a completed send of `bytes` on `pipe_id`.
    ///
    /// Called **after** the bytes reached the pipe, so a failed or short pipe
    /// write never leaves a boundary behind. `push_record` comes from
    /// [`plan_write`].
    ///
    /// # A plain stream write records nothing
    ///
    /// For `SOCK_STREAM` with no ancillary data and an empty queue this is a
    /// no-op — not even a `Record` with a byte count. That is deliberate, and it
    /// is what keeps the stream path allocation-free: the record queue is a
    /// `VecDeque`, so pushing the first entry heap-allocates, and a stream
    /// socket would pay that (plus per-write bookkeeping) for metadata nothing
    /// reads. Stream reads size themselves from the *pipe's* available bytes
    /// ([`plan_read`] ignores `front_record` for unframed types), so the queue
    /// has no job on a plain stream.
    ///
    /// Records still exist for a stream once ancillary data appears, because
    /// `SCM_RIGHTS` descriptors have to be anchored at their own point in the
    /// byte stream — hence the `back_mut` coalescing arm, which keeps later
    /// plain bytes behind fds that were sent before them.
    ///
    /// The consequence to know: on a plain stream channel `queued_bytes()` is 0
    /// while the pipe holds data. The bytes/boundaries invariant applies where
    /// boundaries exist, which is the framed types and ancillary-carrying
    /// streams.
    pub fn commit_write(&mut self, pipe_id: u32, bytes: usize, push_record: bool, anc_fds: Vec<u32>) {
        if !push_record && anc_fds.is_empty() {
            // Plain stream write: a counter bump, no allocation, no record.
            // Coalesce into the tail — the trailing record if one exists (so
            // later bytes stay behind descriptors already sent), otherwise
            // `pending_bytes`.
            let Some(ch) = self.channels.get_mut(&pipe_id) else { return };
            match ch.records.back_mut() {
                Some(last) if last.anc_fds.is_empty() => last.len += bytes,
                Some(_) => ch.records.push_back(Record { len: bytes, anc_fds: Vec::new() }),
                None => ch.pending_bytes += bytes,
            }
            return;
        }
        let ch = self.channels.entry(pipe_id).or_default();
        // Descriptors are arriving, so the bytes ahead of them need a record of
        // their own — otherwise a reader draining only those earlier bytes
        // would pop this record and receive the descriptors early.
        if ch.pending_bytes > 0 {
            let pending = core::mem::take(&mut ch.pending_bytes);
            ch.records.push_back(Record { len: pending, anc_fds: Vec::new() });
        }
        ch.records.push_back(Record { len: bytes, anc_fds });
    }

    /// The front record's length, for [`plan_read`].
    #[must_use]
    pub fn front_record(&self, pipe_id: u32) -> Option<usize> {
        self.channels.get(&pipe_id).and_then(|c| c.records.front()).map(|r| r.len)
    }

    /// Apply a completed read: consume `taken + discarded` bytes of framing and
    /// hand back any `SCM_RIGHTS` descriptors that came with the record.
    pub fn commit_read(&mut self, pipe_id: u32, consumed: usize, pop_record: bool) -> Vec<u32> {
        let Some(ch) = self.channels.get_mut(&pipe_id) else {
            return Vec::new();
        };
        if pop_record {
            return ch.records.pop_front().map(|r| r.anc_fds).unwrap_or_default();
        }
        // Stream: consume the un-recorded leading bytes first, then walk the
        // records. The order matters — those bytes sit *ahead* of every record,
        // so charging a read against the records first would pop a descriptor
        // record for bytes that had not been read yet.
        let mut left = consumed;
        let from_pending = left.min(ch.pending_bytes);
        ch.pending_bytes -= from_pending;
        left -= from_pending;
        let mut fds = Vec::new();
        while left > 0 {
            let Some(front) = ch.records.front_mut() else { break };
            if front.len > left {
                front.len -= left;
                break;
            }
            left -= front.len;
            if let Some(r) = ch.records.pop_front() {
                fds.extend(r.anc_fds);
            }
        }
        fds
    }
}
