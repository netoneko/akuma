//! Kernel half of AF_UNIX: the one table, and the syscall bodies.
//!
//! The decisions live in [`akuma_net::unix`], which is a pure state machine
//! with no kernel dependencies and 88 host tests. This module is the thin,
//! deliberately boring layer around it: it owns the single global
//! [`UnixTable`], copies addresses in and out of userspace, creates and wires
//! the kernel pipes that carry the bytes, and parks/wakes threads.
//!
//! Nothing here decides an errno on its own. If a rule needs asserting, it
//! belongs in `akuma_net::unix` where `cargo test` can reach it — that split is
//! the point of the whole design (`docs/archive/UNIX_SOCKET_IMPROVEMENTS.md`
//! §3.1), and code drifting back into this file is the way it erodes.
//!
//! # The BKL
//!
//! Every function here runs **under** the Big Kernel Lock, and every AF_UNIX
//! path in `net.rs` is handled *before* `NetBklGuard::new()` drops it. That is
//! not an oversight to be optimised later:
//!
//! - The pipe layer (`src/syscall/pipe.rs`) was never audited for the BKL-free
//!   window. The `no-bkl-network` carve-out is justified specifically by
//!   `akuma_net::socket::SOCKET_TABLE` and `smoltcp_net::NETWORK` carrying
//!   their own fine-grained locks; `PIPES` is not in that argument.
//! - [`UNIX_TABLE`] is a plain `Spinlock` with no `PreemptGuard`. Held across a
//!   context switch under real SMP it is an AB-BA wedge waiting to happen.
//!
//! Earning a BKL-free window here means giving both of those the
//! `PreemptGuard` treatment first. Until then, code added to this module must
//! stay on the BKL-held side of the guard, or it will work at `SMP=1` and
//! produce the `[BKL] stuck` class under load
//! (`docs/archive/BKL_VFS_CARVE_OUT.md` §8).
//!
//! # Allocations
//!
//! The budget per syscall, because "one more `Vec`" is how a socket path gets
//! slow without anyone noticing:
//!
//! | path | heap allocations |
//! |---|---|
//! | `send`/`recv`/`sendmsg`/`recvmsg` | **one** bounce buffer, via [`alloc_net_bounce`](super::net::alloc_net_bounce) |
//! | plain `SOCK_STREAM` write | none beyond that bounce — see `UnixTable::commit_write` |
//! | framed write | that bounce, plus one `VecDeque` growth amortised over the channel's life |
//! | `bind`/`connect` | one small `Vec` for the name (it is stored) |
//! | `accept` | one, and only if the caller passed a non-NULL address |
//! | `close`, `shutdown`, `listen`, `getsockname`, `getpeername`, `getsockopt` | none |
//! | readiness (`poll`/`select`/`epoll`) | none |
//!
//! The bounce is structural, not laziness: `copy_from_user` needs a kernel
//! destination, and copying user memory *inside* the `PIPES` lock (IRQs masked)
//! would fault on a lazily-mapped page with the lock held — the same shape as
//! the OOM-inside-the-pipe-lock wedge in `pipe.rs`'s `PIPE_CAPACITY` docs. The
//! byte count is identical to what the pre-table path allocated (`sys_write`
//! and `sys_read` each made one), and it is now *fallible*: those two used
//! `alloc::vec![0u8; n]`, which on a fragmented heap is `handle_alloc_error` →
//! `brk #1` → the whole kernel dies.
//!
//! Three allocations that were in the first version of this module and are not
//! here now, recorded so they do not come back: a second bounce inside
//! `unix_recv` (it reads straight into the caller's buffer, and drains a
//! truncated record's tail through a 256-byte stack buffer); a `Vec` in
//! [`read_sockaddr`] for a 110-byte struct; and a `Record` per plain stream
//! write.
//!
//! # Blocking
//!
//! `UNIX_TABLE` is **never** held across a block. Each blocking syscall is a
//! loop: take the lock, decide, drop the lock, park, retake. The pipe layer's
//! `pipe_check_set_reader`/`pipe_check_set_writer` close the TOCTOU window on
//! the data paths; [`accept_wait_slot`] does the same job for the accept queue.

use super::*;
use akuma_exec::process::FileDescriptor;
use akuma_exec::threading::{WakeHandle, wake_by_handle, wake_handle_for_thread};
use akuma_net::socket::libc_errno;
use akuma_net::unix::{
    self, ConnectOutcome, Pending, SOCKADDR_UN_LEN, SockAddrUn, SockState, SockType, Ucred,
    UnixName, UnixTable, plan_read, plan_write,
};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// The one AF_UNIX table.
///
/// A `Spinlock`, not a `PreemptGuard`-wrapped one — see the module docs' BKL
/// section for what that constrains.
static UNIX_TABLE: Spinlock<Option<UnixTable>> = Spinlock::new(None);

/// Threads parked in `accept(2)`, keyed by listener socket id.
///
/// Separate from the pipe pollers because a listener has no pipes: its
/// readiness is "the backlog is non-empty", which nothing in `pipe.rs` can
/// observe. Keyed by tid for dedup, holding the [`WakeHandle`] minted at
/// registration so an entry that outlives its thread wakes nobody rather than
/// the slot's next occupant — the same tid-recycling hazard `pipe.rs` documents
/// on its own poller map.
static ACCEPT_WAITERS: Spinlock<BTreeMap<u32, BTreeMap<usize, WakeHandle>>> =
    Spinlock::new(BTreeMap::new());

/// Run `f` with the table, creating it on first use.
///
/// Lazily initialised rather than a `const` `UnixTable::new()` because the
/// table holds `BTreeMap`s, which need the allocator — and this module can be
/// reached before anything guarantees the heap is up on every path.
fn with_table<R>(f: impl FnOnce(&mut UnixTable) -> R) -> R {
    crate::irq::with_irqs_disabled(|| {
        let mut guard = UNIX_TABLE.lock();
        let table = guard.get_or_insert_with(UnixTable::new);
        f(table)
    })
}

// ============================================================================
// fd helpers
// ============================================================================

/// The `(rx, tx, sock)` triple behind an fd, if it is a unix socket.
fn fd_parts(fd: u32) -> Option<(u32, u32, u32)> {
    let proc = akuma_exec::process::current_process_shared()?;
    match proc.get_fd(fd) {
        Some(FileDescriptor::UnixSocket { rx, tx, sock }) => Some((rx, tx, sock)),
        _ => None,
    }
}

/// The table id behind an fd.
///
/// `None` covers two different situations that must not be conflated by the
/// caller: the fd is not a unix socket at all (→ `ENOTSOCK`/`EBADF`), or it is
/// one of the pre-table pipe pairs with `sock == 0` (→ fall back to the raw
/// pipe behaviour, see `FileDescriptor::UnixSocket`'s docs). Callers that only
/// need "is there a table entry" use this; callers that must tell the two apart
/// use [`fd_parts`].
fn fd_sock(fd: u32) -> Option<u32> {
    match fd_parts(fd) {
        Some((_, _, sock)) if sock != 0 => Some(sock),
        _ => None,
    }
}

/// Credentials of the calling process, for [`Ucred`] capture at connect time.
///
/// **`uid`/`gid` are 0 for every process, because this kernel has no per-process
/// uid.** `getuid`/`geteuid` hardcode 0 (`src/syscall/mod.rs`), so there is no
/// truthful value to report. `pid` is real.
///
/// This is a stated limitation, not a stub to be quietly relied on: a daemon
/// that authorises clients by `SO_PEERCRED.uid` will see 0 for everyone and
/// therefore trust everyone. Anything security-relevant must not gate on the
/// uid until per-process credentials exist; the `pid` is usable for
/// identification. Left as a real capture path so that adding real uids is a
/// one-line change here rather than a redesign.
fn current_creds() -> Ucred {
    akuma_exec::process::current_process_shared().map_or_else(Ucred::default, |p| Ucred {
        pid: p.pid,
        uid: 0,
        gid: 0,
    })
}

/// Copy a `sockaddr_un` in from userspace and decode it.
///
/// `addrlen` is taken from the caller and used as the delimiter, which is what
/// makes abstract names work at all — but it is also a userspace-controlled
/// length, so it is clamped to `sizeof(sockaddr_un)` before anything reads it.
fn read_sockaddr(addr_ptr: u64, addrlen: usize) -> Result<UnixName, u64> {
    if addrlen < 2 {
        return Err(EINVAL);
    }
    let len = addrlen.min(SOCKADDR_UN_LEN);
    if !validate_user_ptr(addr_ptr, len) {
        return Err(EFAULT);
    }
    // Stack, not heap: `sockaddr_un` is 110 bytes and `addrlen` is already
    // clamped to it above, so there is nothing here the heap is needed for.
    // Every bind and every connect goes through this function.
    let mut raw = [0u8; SOCKADDR_UN_LEN];
    if copy_from_user(&mut raw[..len], addr_ptr).is_err() {
        return Err(EFAULT);
    }
    SockAddrUn::decode(&raw[..len]).map_err(neg_errno)
}

/// Write a name back out as `sockaddr_un` + `addrlen`, truncating to the
/// caller's buffer the way `getsockname(2)` specifies: the *untruncated*
/// length is what gets reported, so a caller with a short buffer can tell.
fn write_sockaddr(name: &UnixName, addr_ptr: u64, addrlen_ptr: u64) -> u64 {
    if addr_ptr == 0 || addrlen_ptr == 0 {
        return 0;
    }
    let mut cap: u32 = 0;
    if read_user_into(&mut cap, addrlen_ptr).is_err() {
        return EFAULT;
    }
    let sa = SockAddrUn::encode(name);
    let full = sa.len;
    let n = full.min(cap as usize);
    if n > 0 {
        if !validate_user_ptr(addr_ptr, n) {
            return EFAULT;
        }
        if copy_to_user(addr_ptr, &sa.as_slice()[..n]).is_err() {
            return EFAULT;
        }
    }
    // Linux reports the length the address *would* have needed.
    if write_user_val(addrlen_ptr, &(full as u32)).is_err() {
        return EFAULT;
    }
    0
}

// ============================================================================
// Teardown — the callbacks akuma-exec's fd table drives
// ============================================================================

/// Release one reference to a table entry; tear it down at zero.
///
/// Registered as `ExecRuntime::unix_sock_close`, so it runs from every path
/// that drops an fd: `close`, `dup2` overwrite, the `execve` cloexec sweep, and
/// process teardown.
///
/// The recursion into orphaned server endpoints is the part that matters. A
/// listener closed with N queued connections holds the only reference to N
/// server-side sockets; `UnixTable::close` hands them back precisely so they
/// can be closed here instead of leaking, and each of those may in turn have
/// its own state to release. Bounded because a server endpoint never has a
/// backlog of its own, so the recursion is one level deep.
pub fn unix_sock_close(sock: u32) {
    if sock == 0 {
        return;
    }
    let orphans = with_table(|t| t.close(sock));
    for orphan in orphans {
        // A queued-but-never-accepted server endpoint. Its client is parked or
        // will find the connection dead; drop the entry so neither the socket
        // nor its name outlives the listener.
        let more = with_table(|t| t.close(orphan));
        debug_assert!(more.is_empty(), "server endpoint had a backlog");
    }
    // Anything parked in accept() on this listener must be woken to discover
    // the listener is gone, or it sleeps forever on a queue nobody will fill.
    wake_accept_waiters(sock);
    crate::irq::with_irqs_disabled(|| {
        ACCEPT_WAITERS.lock().remove(&sock);
    });
}

/// Take one reference. Registered as `ExecRuntime::unix_sock_clone_ref`.
pub fn unix_sock_clone_ref(sock: u32) {
    if sock == 0 {
        return;
    }
    with_table(|t| t.clone_ref(sock));
}

/// Forget a channel and close any `SCM_RIGHTS` descriptors still in flight on
/// it. Called when the pipe carrying the channel's bytes is destroyed.
///
/// The returned descriptors are the leak this exists to prevent: they are real
/// references held by unread records, and dropping the channel without
/// releasing them is a silent fd leak that nothing in userspace can observe.
/// Ancillary data is not implemented yet, so the list is always empty today —
/// the call site exists so that adding `SCM_RIGHTS` cannot forget it.
pub fn unix_channel_detach(pipe_id: u32) {
    let in_flight = with_table(|t| t.detach_channel(pipe_id));
    debug_assert!(
        in_flight.is_empty(),
        "SCM_RIGHTS landed without wiring its teardown"
    );
}

// ============================================================================
// accept(2) waiters
// ============================================================================

/// Register the current thread as waiting for a connection on `listener`,
/// **atomically with** re-checking the backlog.
///
/// Returns `true` if the caller should NOT block (a connection is already
/// queued, or the listener is gone). Same shape and same reason as
/// `pipe_check_set_reader`: registering and then checking leaves a window in
/// which a `connect` lands with no waiter recorded, and the thread then sleeps
/// through its own wake-up.
fn accept_wait_slot(listener: u32, tid: usize) -> bool {
    crate::irq::with_irqs_disabled(|| {
        let mut guard = UNIX_TABLE.lock();
        let table = guard.get_or_insert_with(UnixTable::new);
        match table.get(listener) {
            None => return true, // listener gone → don't block
            Some(s) if s.accept_ready() => return true,
            Some(s) if s.state != SockState::Listening => return true,
            Some(_) => {}
        }
        ACCEPT_WAITERS
            .lock()
            .entry(listener)
            .or_default()
            .insert(tid, wake_handle_for_thread(tid));
        false
    })
}

/// Wake everything parked in `accept` on this listener.
fn wake_accept_waiters(listener: u32) {
    let handles: Vec<WakeHandle> = crate::irq::with_irqs_disabled(|| {
        let mut map = ACCEPT_WAITERS.lock();
        map.get_mut(&listener)
            .map(|w| core::mem::take(w).into_values().collect())
            .unwrap_or_default()
    });
    for h in handles {
        wake_by_handle(h);
    }
}

// ============================================================================
// socket(2) / socketpair(2)
// ============================================================================

/// Allocate a table entry and install it on a fresh fd. `socket(AF_UNIX, …)`.
///
/// Returns the fd, or a negated errno. Note there are no pipes yet: an
/// unconnected unix socket has nothing to read or write, and `rx`/`tx` stay 0
/// until `connect`/`accept` wires them.
pub fn sys_socket_unix(sock_type: i32, cloexec: bool, nonblock: bool) -> u64 {
    let Some(ty) = SockType::from_raw(sock_type & 0xFF) else {
        return EPROTOTYPE;
    };
    let Some(proc) = akuma_exec::process::current_process_shared() else {
        return ESRCH;
    };
    let creds = current_creds();
    let sock = with_table(|t| t.alloc(ty, creds));
    // A datagram socket gets its receive queue right away, not at `bind`.
    // Anyone who learns its name can send to it, and it must also be able to
    // receive a *reply* while still unbound — which is what a `syslog(3)`-style
    // client does. Creating the queue at bind time would silently drop those.
    let (rx, tx) = if ty == SockType::Dgram {
        let q = super::pipe::pipe_create();
        with_table(|t| t.attach_dgram_queue(sock, q));
        (q, q)
    } else {
        (0, 0)
    };
    let fd = proc.alloc_fd(FileDescriptor::UnixSocket { rx, tx, sock });
    if cloexec {
        proc.set_cloexec(fd);
    }
    if nonblock {
        proc.set_nonblock(fd);
    }
    if crate::config::SYSCALL_DEBUG_NET_ENABLED {
        crate::safe_print!(96, "[unix] socket(type={}) = fd {}\n", ty.to_raw(), fd);
    }
    u64::from(fd)
}

/// Allocate the two table entries backing a `socketpair`, wire them to the
/// pipes, and hand back the ids.
///
/// Split out from `sys_socketpair` so that syscall keeps its rollback path in
/// one place, and so a pair gets real `SOCK_SEQPACKET` framing: before the
/// table existed, `socketpair(AF_UNIX, SOCK_SEQPACKET, 0)` produced a byte
/// stream that silently merged messages.
pub fn socketpair_alloc(sock_type: i32, px: u32, py: u32) -> Option<(u32, u32)> {
    let ty = SockType::from_raw(sock_type & 0xFF)?;
    let creds = current_creds();
    Some(with_table(|t| {
        let a = t.alloc(ty, creds);
        let b = t.alloc(ty, creds);
        // Endpoint a reads px and writes py; the peer is mirrored.
        t.pair(a, b, px, py);
        (a, b)
    }))
}

/// Roll back a failed `socketpair`.
pub fn socketpair_rollback(a: u32, b: u32) {
    with_table(|t| {
        t.close(a);
        t.close(b);
    });
}

// ============================================================================
// bind / listen
// ============================================================================

/// `bind(2)` on an AF_UNIX socket.
///
/// A pathname bind has to touch the filesystem as well as the name table, and
/// the order is deliberate: **the name table claim comes first**. If the node
/// were created first and the table claim then failed with `EADDRINUSE`, the
/// failed `bind` would have left a file behind that nothing owns — the exact
/// stale node that makes the *next* daemon start look like a live conflict.
pub fn sys_bind(fd: u32, addr_ptr: u64, addrlen: usize) -> u64 {
    let Some(sock) = fd_sock(fd) else {
        return if fd_parts(fd).is_some() { EOPNOTSUPP } else { ENOTSOCK };
    };
    let name = match read_sockaddr(addr_ptr, addrlen) {
        Ok(n) => n,
        Err(e) => return e,
    };
    // A filesystem name whose node exists but whose socket does not is stale:
    // a daemon died without unlinking. Linux reports EADDRINUSE and expects the
    // daemon to unlink first, so a client sees a node it cannot connect to
    // rather than silently talking to the wrong process.
    if let Err(e) = with_table(|t| t.bind(sock, name.clone())) {
        return neg_errno(e);
    }
    if let Some(path) = name.path_bytes()
        && let Ok(p) = core::str::from_utf8(path)
    {
        create_socket_node(p);
    }
    if let Some(path) = name.path_bytes() {
        crate::safe_print!(
            160,
            "[unix] bind(fd={}) path len={}\n",
            fd,
            path.len()
        );
    } else {
        crate::safe_print!(96, "[unix] bind(fd={}) abstract\n", fd);
    }
    0
}

/// Create the filesystem presence for a pathname bind: a real `S_IFSOCK` node.
///
/// The type bits are the point. An earlier version created an ordinary empty
/// file, and `nettest-unix path` — diffing against its Linux control arm —
/// reported `mode=0o100644 S_ISSOCK=false`: a client that checks `S_ISSOCK`
/// before connecting, which is the normal thing to do, would refuse to talk to
/// a socket that was working perfectly.
///
/// A failure here is logged and **not** propagated. The name is already claimed
/// in the table at this point, and `connect` resolves against that table rather
/// than against the inode, so the socket works either way — a rootfs that
/// cannot make the node (read-only, or a filesystem without the type) should
/// not turn a working `bind` into a failure. What it does cost is the userspace
/// conventions: `stat`, `unlink` and `ls` will not see the path.
fn create_socket_node(path: &str) {
    if let Err(e) = crate::vfs::create_socket_node(path) {
        let _ = e;
        crate::safe_print!(
            112,
            "[unix] bind: S_IFSOCK node create failed (name is still bound)\n"
        );
    }
}

/// `listen(2)`.
pub fn sys_listen(fd: u32, backlog: i32) -> u64 {
    let Some(sock) = fd_sock(fd) else {
        return if fd_parts(fd).is_some() { EOPNOTSUPP } else { ENOTSOCK };
    };
    match with_table(|t| t.listen(sock, backlog)) {
        Ok(()) => {
            crate::safe_print!(96, "[unix] listen(fd={}, backlog={})\n", fd, backlog);
            0
        }
        Err(e) => neg_errno(e),
    }
}

// ============================================================================
// connect / accept
// ============================================================================

/// `connect(2)`.
///
/// On success the client's endpoint is fully usable **before** the server calls
/// `accept`: the two pipes are created here and both endpoints wired, so a
/// client may write its request immediately and have it buffered. Linux behaves
/// this way and clients rely on it — deferring the wiring to `accept` would
/// lose every byte written in between.
pub fn sys_connect(fd: u32, addr_ptr: u64, addrlen: usize) -> u64 {
    let Some(sock) = fd_sock(fd) else {
        return if fd_parts(fd).is_some() { EOPNOTSUPP } else { ENOTSOCK };
    };
    let name = match read_sockaddr(addr_ptr, addrlen) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let creds = current_creds();
    let nonblock = super::net::fd_is_nonblock(fd);

    loop {
        let outcome = with_table(|t| t.connect(sock, &name, creds));
        match outcome {
            Ok(ConnectOutcome::DgramPeerSet { .. }) => {
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    crate::safe_print!(96, "[unix] connect(fd={}) dgram peer set\n", fd);
                }
                return 0;
            }
            Ok(ConnectOutcome::Queued { listener, server_sock }) => {
                // Create the channel now so the client can write before the
                // server accepts. `px` carries client -> server, `py` the
                // reverse.
                let px = super::pipe::pipe_create();
                let py = super::pipe::pipe_create();
                with_table(|t| {
                    t.attach_channel(px);
                    t.attach_channel(py);
                    if let Some(s) = t.get_mut(sock) {
                        s.rx = py;
                        s.tx = px;
                    }
                    if let Some(s) = t.get_mut(server_sock) {
                        s.rx = px;
                        s.tx = py;
                    }
                });
                // The fd was installed by `socket()` with no pipes; now that
                // `connect` has wired them, update the descriptor in place.
                // `update_fd` rather than `set_fd` so a concurrent close on a
                // shared fd table cannot be raced into re-creating the entry.
                if let Some(proc) = akuma_exec::process::current_process_shared() {
                    proc.update_fd(fd, |e| {
                        if let FileDescriptor::UnixSocket { rx, tx, .. } = e {
                            *rx = py;
                            *tx = px;
                        }
                    });
                }
                wake_accept_waiters(listener);
                if crate::config::SYSCALL_DEBUG_NET_ENABLED {
                    crate::safe_print!(96, "[unix] connect(fd={}) queued\n", fd);
                }
                return 0;
            }
            // A full backlog is transient: a blocking client waits for the
            // server to catch up rather than failing a live service.
            Err(e) if e == libc_errno::EAGAIN && !nonblock => {
                akuma_exec::threading::schedule_blocking(10_000);
            }
            Err(e) => return neg_errno(e),
        }
    }
}

/// `accept(2)` / `accept4(2)`.
///
/// Blocking is a check-register-park loop with the table lock dropped across
/// the park ([`accept_wait_slot`]); holding it would wedge every other core's
/// AF_UNIX traffic for the duration of an idle server's wait.
pub fn sys_accept(fd: u32, addr_ptr: u64, addrlen_ptr: u64, flags: u32) -> u64 {
    let Some(listener) = fd_sock(fd) else {
        return if fd_parts(fd).is_some() { EOPNOTSUPP } else { ENOTSOCK };
    };
    let nonblock_fd = super::net::fd_is_nonblock(fd);
    // accept4's SOCK_NONBLOCK/SOCK_CLOEXEC apply to the NEW fd, and are
    // independent of the listener's own O_NONBLOCK — which governs whether
    // this call blocks. Conflating them is a classic accept4 bug: a
    // non-blocking listener would hand out blocking connections, or a blocking
    // listener would refuse to wait.
    let new_cloexec = flags & 0x8_0000 != 0;
    let new_nonblock = flags & 0x800 != 0;

    let pending: Pending = loop {
        match with_table(|t| t.accept(listener)) {
            Ok(p) => break p,
            Err(e) if e == libc_errno::EAGAIN => {
                if nonblock_fd {
                    return EAGAIN;
                }
                let tid = akuma_exec::threading::current_thread_id();
                if !accept_wait_slot(listener, tid) {
                    akuma_exec::threading::schedule_blocking(u64::MAX);
                }
                // The listener may have been closed while we were parked.
                if with_table(|t| t.get(listener).is_none()) {
                    return EINVAL;
                }
            }
            Err(e) => return neg_errno(e),
        }
    };

    let Some(proc) = akuma_exec::process::current_process_shared() else {
        return ESRCH;
    };
    // The peer name is only cloned when the caller actually asked for an
    // address. Most servers pass NULL, and cloning a `UnixName` heap-allocates.
    let want_addr = addr_ptr != 0 && addrlen_ptr != 0;
    let (rx, tx, peer_name) = with_table(|t| {
        t.get(pending.server_sock)
            .map_or((0, 0, UnixName::Unnamed), |s| {
                let name = if want_addr { s.peer_name.clone() } else { UnixName::Unnamed };
                (s.rx, s.tx, name)
            })
    });
    let new_fd = proc.alloc_fd(FileDescriptor::UnixSocket {
        rx,
        tx,
        sock: pending.server_sock,
    });
    if new_cloexec {
        proc.set_cloexec(new_fd);
    }
    if new_nonblock {
        proc.set_nonblock(new_fd);
    }
    if want_addr {
        let r = write_sockaddr(&peer_name, addr_ptr, addrlen_ptr);
        if r != 0 {
            // The fd is already installed; tearing it down here would race a
            // concurrent dup. Report the fault and leave the connection —
            // Linux also keeps the accepted fd on an addr copy-out failure.
            crate::safe_print!(96, "[unix] accept: addr copyout failed\n");
        }
    }
    if crate::config::SYSCALL_DEBUG_NET_ENABLED {
        crate::safe_print!(96, "[unix] accept(fd={}) = fd {}\n", fd, new_fd);
    }
    u64::from(new_fd)
}

// ============================================================================
// getsockname / getpeername
// ============================================================================

/// `getsockname(2)`. An unbound socket — including every `socketpair`
/// endpoint — reports `addrlen == 2` and no path, which is what Linux reports
/// and what the pre-table code answered `EBADF` to.
pub fn sys_getsockname(fd: u32, addr_ptr: u64, addrlen_ptr: u64) -> u64 {
    let Some((_, _, sock)) = fd_parts(fd) else {
        return ENOTSOCK;
    };
    let name = if sock == 0 {
        UnixName::Unnamed
    } else {
        with_table(|t| t.get(sock).map_or(UnixName::Unnamed, |s| s.name.clone()))
    };
    write_sockaddr(&name, addr_ptr, addrlen_ptr)
}

/// `getpeername(2)`. `ENOTCONN` for a socket with no peer — a distinction a
/// client uses to tell "not connected yet" from "connected to an unnamed peer".
pub fn sys_getpeername(fd: u32, addr_ptr: u64, addrlen_ptr: u64) -> u64 {
    let Some((_, _, sock)) = fd_parts(fd) else {
        return ENOTSOCK;
    };
    if sock == 0 {
        // A pre-table socketpair endpoint: connected, peer unnamed.
        return write_sockaddr(&UnixName::Unnamed, addr_ptr, addrlen_ptr);
    }
    let Some((state, name)) =
        with_table(|t| t.get(sock).map(|s| (s.state, s.peer_name.clone())))
    else {
        return EBADF;
    };
    if state != SockState::Connected {
        return ENOTCONN;
    }
    write_sockaddr(&name, addr_ptr, addrlen_ptr)
}

// ============================================================================
// shutdown / getsockopt
// ============================================================================

/// `shutdown(2)` on a unix socket.
///
/// Was a `return 0` stub for every non-AF_INET fd, which made `SHUT_WR` a lie:
/// the peer never saw EOF, so a protocol that ends a request by half-closing
/// hung until something else timed it out. That cost 5 s per nginx request on
/// the AF_INET side before the same stub was fixed there
/// (`akuma_net::socket::socket_shutdown`).
///
/// `SHUT_WR` closes this endpoint's write end of the `tx` pipe, which is what
/// actually delivers the EOF; the recorded [`unix::Shutdown`] state is what
/// makes the *local* side report it consistently through `read` and the
/// readiness syscalls.
pub fn sys_shutdown(fd: u32, how: i32) -> u64 {
    let Some((_, tx, sock)) = fd_parts(fd) else {
        // Non-socket fds keep the old permissive success: nothing in the tree
        // half-closes a pipe, and failing them now would be a new error where
        // there has never been one.
        return 0;
    };
    let mut state = unix::Shutdown::default();
    if let Err(e) = state.apply(how) {
        return neg_errno(e);
    }
    if sock != 0 {
        let applied = with_table(|t| {
            t.get_mut(sock).map(|s| {
                let _ = s.shutdown.apply(how);
                s.shutdown
            })
        });
        if applied.is_none() {
            return EBADF;
        }
    }
    // The half-close only becomes visible to the peer when the pipe's write end
    // goes away — the state above is bookkeeping, this is the wire effect.
    if state.wr && tx != 0 {
        super::pipe::pipe_close_write(tx);
    }
    0
}

/// The `SO_TYPE` answer for a unix fd.
///
/// `sys_getsockopt` previously answered a hardcoded `1` (`SOCK_STREAM`) for
/// every non-AF_INET fd, so a `SOCK_SEQPACKET` or `SOCK_DGRAM` socket
/// misreported its own type — and a client that picks its framing off the
/// answer then reads the stream wrong.
pub fn sock_type_of(fd: u32) -> Option<i32> {
    let sock = fd_sock(fd)?;
    with_table(|t| t.get(sock).map(|s| s.ty.to_raw()))
}

/// The `SO_PEERCRED` answer: the peer's pid/uid/gid as captured at connect
/// time. See [`Ucred`] for why connect-time and not send-time.
pub fn peer_cred_of(fd: u32) -> Option<Ucred> {
    let sock = fd_sock(fd)?;
    with_table(|t| t.get(sock).map(|s| s.peer_creds))
}

// ============================================================================
// Readiness
// ============================================================================

/// Whether an `accept` on this fd would succeed right now, and register the
/// caller for a wake-up if not.
///
/// This is the one genuinely new readiness predicate AF_UNIX adds, and it has
/// to report identically through `poll`, `select` and `epoll`. The AF_INET side
/// of that same contract is what the `_exceptfds_ptr` bug violated — `poll`
/// said `CONNECTED` and `select` said `HARDFAIL` for one socket at one moment
/// (`docs/runbooks/cargo-cannot-reach-crates-io.md` §3) — which is why the
/// probe checks all four readiness syscalls rather than one.
pub fn listener_ready(fd: u32, tid: Option<usize>) -> Option<bool> {
    let sock = fd_sock(fd)?;
    let ready = with_table(|t| {
        let s = t.get(sock)?;
        if s.state != SockState::Listening {
            return None;
        }
        Some(s.accept_ready())
    })?;
    if !ready && let Some(tid) = tid {
        crate::irq::with_irqs_disabled(|| {
            ACCEPT_WAITERS
                .lock()
                .entry(sock)
                .or_default()
                .insert(tid, wake_handle_for_thread(tid));
        });
    }
    Some(ready)
}

// ============================================================================
// Framed send/receive
// ============================================================================

/// Send `data` on a unix socket, preserving message boundaries for the framed
/// types.
///
/// The ordering here is the bytes/boundaries sync rule from
/// [`akuma_net::unix`]: [`plan_write`] decides, the pipe takes the bytes, and
/// only then is the boundary recorded. Recording first and writing second would
/// leave a boundary behind for bytes a failed `pipe_write` never accepted, and
/// every subsequent record on that channel would be wrong by the shortfall.
pub fn unix_send(fd: u32, data: &[u8], dontwait: bool) -> u64 {
    let Some((_, tx, sock)) = fd_parts(fd) else {
        return ENOTSOCK;
    };
    if tx == 0 {
        return ENOTCONN;
    }
    // No table entry (a pre-table rump sysproxy channel): raw pipe write, the
    // behaviour that path has always had.
    let Some(sock) = (sock != 0).then_some(sock) else {
        return pipe_write_bytes(tx, data, dontwait || super::net::fd_is_nonblock(fd));
    };

    let (ty, wr_shut) = match with_table(|t| t.get(sock).map(|s| (s.ty, s.shutdown.wr))) {
        Some(v) => v,
        None => return EBADF,
    };
    // A datagram socket's `tx` is its OWN receive queue, not a send path (see
    // `UnixTable::attach_dgram_queue`) — writing through it here would deliver
    // the caller's message back to itself. Route to the recorded peer instead.
    if ty == SockType::Dgram {
        return unix_sendto(fd, data, None, dontwait);
    }
    if wr_shut {
        // Linux also raises SIGPIPE here; `pipe_write` does that for the
        // broken-pipe case, and a local SHUT_WR is the caller's own doing, so
        // the errno alone is the honest answer.
        return EPIPE;
    }
    let nonblock = dontwait || super::net::fd_is_nonblock(fd);

    loop {
        let room = super::pipe::PIPE_CAPACITY
            .saturating_sub(super::pipe::pipe_bytes_available(tx));
        let sndbuf = with_table(|t| t.channel(tx).map_or(unix::DEFAULT_SNDBUF, |c| c.sndbuf));
        match plan_write(ty, data.len(), room, sndbuf) {
            Ok(plan) => {
                if plan.bytes == 0 && !plan.push_record {
                    return 0;
                }
                let written = match super::pipe::pipe_write(tx, &data[..plan.bytes]) {
                    Ok(n) => n,
                    Err(e) => return neg_errno(e),
                };
                if ty.is_framed() && written != plan.bytes {
                    // Must not happen: `plan_write` only returns Ok for a
                    // framed type when the whole message fits, and the pipe is
                    // only drained (never shrunk) by a concurrent reader. If it
                    // ever does, the channel's boundaries no longer match the
                    // pipe's bytes, so say so loudly rather than record a lie.
                    crate::safe_print!(
                        128,
                        "[unix] FRAMING DESYNC tx={} want={} wrote={}\n",
                        tx,
                        plan.bytes,
                        written
                    );
                }
                with_table(|t| t.commit_write(tx, written, plan.push_record, Vec::new()));
                return written as u64;
            }
            Err(e) if e == libc_errno::EAGAIN => {
                if nonblock {
                    return EAGAIN;
                }
                let tid = akuma_exec::threading::current_thread_id();
                if !super::pipe::pipe_check_set_writer(tx, tid) {
                    akuma_exec::threading::schedule_blocking(u64::MAX);
                }
                if !super::pipe::pipe_can_write(tx) && super::pipe::pipe_hup(tx) {
                    return EPIPE;
                }
            }
            Err(e) => return neg_errno(e),
        }
    }
}

/// Send a datagram to an explicit destination name — `sendto(2)` on a
/// `SOCK_DGRAM` socket.
///
/// This is the path `/dev/log` uses, and the reason it is separate from
/// [`unix_send`]: a datagram socket has no `tx` peer to write through. The
/// destination is resolved per call, so an unconnected socket can address a
/// different name every time, and a receiver that restarts is picked up by the
/// next send rather than leaving the sender wired to a dead endpoint.
pub fn unix_sendto(fd: u32, data: &[u8], dest: Option<&UnixName>, dontwait: bool) -> u64 {
    let Some((_, _, sock)) = fd_parts(fd) else {
        return ENOTSOCK;
    };
    // `sock == 0` is a pre-table pipe pair — box 0's rump sysproxy channel,
    // which reaches this function through `sendto`/`sendmsg` on its fd 3.
    // `unix_send` has the raw-pipe fallback for it. Returning an error here
    // instead would break the rump handshake, and the symptom would be "box 0's
    // rump stack never comes up", several layers away from anything that looks
    // like socket code.
    if sock == 0 {
        return unix_send(fd, data, dontwait);
    }
    let Some(ty) = with_table(|t| t.get(sock).map(|s| s.ty)) else {
        return EBADF;
    };
    if ty != SockType::Dgram {
        // A connection-oriented socket ignores any destination and sends to its
        // peer, which is what Linux does for a connected socket.
        return unix_send(fd, data, dontwait);
    }
    // Resolve the queue on every call. A destination that goes away between two
    // sends must fail the second one, not the first.
    let queue = match dest {
        Some(name) => match with_table(|t| t.resolve_dgram_dest(name)) {
            Ok((_, q)) => q,
            Err(e) => return neg_errno(e),
        },
        None => match with_table(|t| t.dgram_default_dest(sock)) {
            Ok(q) => q,
            Err(e) => return neg_errno(e),
        },
    };
    let nonblock = dontwait || super::net::fd_is_nonblock(fd);
    deliver_datagram(queue, data, nonblock)
}

/// Put one whole datagram on `queue`, all-or-nothing.
///
/// A datagram is never split: [`plan_write`] refuses a partial one because
/// there is no way to record "two thirds of a message", and the next boundary
/// would then be wrong by the shortfall for the rest of the channel's life.
fn deliver_datagram(queue: u32, data: &[u8], nonblock: bool) -> u64 {
    loop {
        let room = super::pipe::PIPE_CAPACITY
            .saturating_sub(super::pipe::pipe_bytes_available(queue));
        let sndbuf = with_table(|t| t.channel(queue).map_or(unix::DEFAULT_SNDBUF, |c| c.sndbuf));
        match plan_write(SockType::Dgram, data.len(), room, sndbuf) {
            Ok(plan) => {
                // A zero-length datagram writes nothing but is still a real,
                // deliverable message — the record is what carries it.
                let written = if plan.bytes == 0 {
                    0
                } else {
                    match super::pipe::pipe_write(queue, &data[..plan.bytes]) {
                        Ok(n) => n,
                        Err(e) => return neg_errno(e),
                    }
                };
                if written != plan.bytes {
                    crate::safe_print!(
                        128,
                        "[unix] DGRAM DESYNC queue={} want={} wrote={}\n",
                        queue,
                        plan.bytes,
                        written
                    );
                }
                with_table(|t| t.commit_write(queue, written, true, Vec::new()));
                return plan.bytes as u64;
            }
            Err(e) if e == libc_errno::EAGAIN => {
                if nonblock {
                    return EAGAIN;
                }
                let tid = akuma_exec::threading::current_thread_id();
                if !super::pipe::pipe_check_set_writer(queue, tid) {
                    akuma_exec::threading::schedule_blocking(u64::MAX);
                }
            }
            Err(e) => return neg_errno(e),
        }
    }
}

/// Decode a `sendto` destination address, if the caller supplied one.
///
/// `Ok(None)` means "no destination given", which for a connected socket is
/// normal and for an unconnected datagram socket becomes `EDESTADDRREQ` further
/// down — a distinction the caller must not collapse.
pub fn read_dest(addr_ptr: u64, addrlen: usize) -> Result<Option<UnixName>, u64> {
    if addr_ptr == 0 || addrlen == 0 {
        return Ok(None);
    }
    read_sockaddr(addr_ptr, addrlen).map(Some)
}

/// Write to a pipe honouring `O_NONBLOCK`, for the no-table-entry path.
fn pipe_write_bytes(tx: u32, data: &[u8], nonblock: bool) -> u64 {
    if data.is_empty() {
        return 0;
    }
    loop {
        match super::pipe::pipe_write(tx, data) {
            Ok(0) => {
                if nonblock {
                    return EAGAIN;
                }
                let tid = akuma_exec::threading::current_thread_id();
                if !super::pipe::pipe_check_set_writer(tx, tid) {
                    akuma_exec::threading::schedule_blocking(u64::MAX);
                }
            }
            Ok(n) => return n as u64,
            Err(e) => return neg_errno(e),
        }
    }
}

/// Receive into `buf`, honouring message boundaries for the framed types.
///
/// Returns `(bytes, truncated)`; `truncated` is what sets `MSG_TRUNC` in a
/// `recvmsg` reply. A negated errno is returned in the first element with the
/// high bit set, so callers check `(n as i64) < 0` as usual.
pub fn unix_recv(fd: u32, buf: &mut [u8], dontwait: bool, peek: bool) -> (u64, bool) {
    let Some((rx, _, sock)) = fd_parts(fd) else {
        return (ENOTSOCK, false);
    };
    if rx == 0 {
        return (ENOTCONN, false);
    }
    let Some(sock) = (sock != 0).then_some(sock) else {
        return (pipe_read_bytes(rx, buf, dontwait || super::net::fd_is_nonblock(fd)), false);
    };

    let (ty, rd_shut) = match with_table(|t| t.get(sock).map(|s| (s.ty, s.shutdown.rd))) {
        Some(v) => v,
        None => return (EBADF, false),
    };
    // `SHUT_RD` does **not** discard what has already arrived.
    //
    // Verified against Linux with `nettest-unix shutdown`: after
    // `shutdown(SHUT_RD)` a socket with 6 buffered bytes still returns those 6
    // bytes, and only then reads as EOF. An earlier version of this function
    // returned 0 immediately, which silently destroyed data the peer had
    // already successfully sent — the caller saw a clean EOF and never learned
    // that a complete message had been thrown away. That divergence is exactly
    // what the probe's Linux control arm exists to find, and it found it.
    //
    // So `SHUT_RD` only changes what happens when the queue is *empty*: EOF
    // instead of a block.
    let nonblock = dontwait || super::net::fd_is_nonblock(fd);

    loop {
        let avail = super::pipe::pipe_bytes_available(rx);
        let front = with_table(|t| t.front_record(rx));
        // A framed socket keys entirely off the record queue: `avail > 0` with
        // no record would mean the boundaries desynced, and reading in that
        // state is how a desync turns into corrupt data rather than a stall.
        let has_work = if ty.is_framed() { front.is_some() } else { avail > 0 };
        if has_work {
            let plan = plan_read(ty, front, avail, buf.len(), peek);
            // Straight into the caller's buffer. An intermediate `Vec` here
            // would be a SECOND heap allocation per receive on top of the
            // bounce `net.rs` already made, on the hottest path this module
            // has (rustc's spawn handshake runs through it on every link).
            let (mut consumed, _eof) = if plan.take > 0 {
                super::pipe::pipe_read(rx, &mut buf[..plan.take])
            } else {
                (0, false)
            };
            let taken = consumed;
            // Drop the tail of a truncated record. A framed record is consumed
            // WHOLE (`plan_read`'s contract), and the tail must actually leave
            // the pipe or the next record boundary is off by that many bytes
            // and the channel never resynchronises. Drained through a small
            // fixed stack buffer so discarding a 64 KiB datagram costs no
            // allocation at all.
            if plan.discard > 0 {
                let mut sink = [0u8; 256];
                let mut left = plan.discard;
                while left > 0 {
                    let want = left.min(sink.len());
                    let (d, _) = super::pipe::pipe_read(rx, &mut sink[..want]);
                    if d == 0 {
                        break;
                    }
                    consumed += d;
                    left -= d;
                }
            }
            if consumed > 0 {
                super::poll::epoll_on_fd_drained(fd);
            }
            // A zero-length datagram moves no bytes but must still be consumed,
            // or the receiver re-reads the same empty record forever.
            with_table(|t| t.commit_read(rx, consumed, plan.consume_record));
            return (taken as u64, plan.truncated);
        }
        // Nothing queued. EOF when the peer's write end is truly gone, or when
        // this end has been shut down for reading — "drained" and "at EOF" are
        // different answers otherwise, and folding them together is what made a
        // tokio client park forever on the AF_INET side
        // (docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md).
        if rd_shut || super::pipe::pipe_hup(rx) {
            return (0, false);
        }
        if nonblock {
            return (EAGAIN, false);
        }
        let tid = akuma_exec::threading::current_thread_id();
        if !super::pipe::pipe_check_set_reader(rx, tid) {
            akuma_exec::threading::schedule_blocking(u64::MAX);
        }
    }
}

/// Read from a pipe honouring `O_NONBLOCK`, for the no-table-entry path.
fn pipe_read_bytes(rx: u32, buf: &mut [u8], nonblock: bool) -> u64 {
    if buf.is_empty() {
        return 0;
    }
    loop {
        let (n, eof) = super::pipe::pipe_read(rx, buf);
        if n > 0 {
            super::poll::epoll_on_fd_drained(0);
            return n as u64;
        }
        if eof {
            return 0;
        }
        if nonblock {
            return EAGAIN;
        }
        let tid = akuma_exec::threading::current_thread_id();
        if !super::pipe::pipe_check_set_reader(rx, tid) {
            akuma_exec::threading::schedule_blocking(u64::MAX);
        }
    }
}

// ============================================================================
// Introspection
// ============================================================================

/// `(live sockets, bound names)` — the boot self-tests' leak check.
///
/// Exposed because every accumulating leak class in this design is invisible
/// from userspace and shows up only as a drift in these two numbers: a name left
/// behind by a closed listener (which makes every later `bind` fail
/// `EADDRINUSE`, so the service can never restart), and a server endpoint queued
/// but never accepted.
///
/// `kernel_tests`-gated because `test_unix_table_returns_to_baseline` is its
/// only caller, and `extreme-size` builds with `no-tests` against a 4.0 MB
/// floor — an ungated accessor there is dead code the linker still has to carry.
#[cfg(kernel_tests)]
#[must_use]
pub fn table_stats() -> (usize, usize) {
    with_table(|t| (t.len(), t.name_count()))
}
