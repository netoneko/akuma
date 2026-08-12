//! `forkprobe` — does `fork()` actually work from a `no_std` libakuma binary?
//!
//! `userspace/sshd`'s process-per-session model rests on one assumption that
//! nothing in this tree had ever exercised: that a libakuma binary (not musl,
//! no libc `fork()` fixups) can `clone(SIGCHLD)`, keep running on both sides,
//! and have the child use a socket fd the parent `accept()`ed. `elftest` issues
//! a raw `clone()` but only `CLONE_VFORK|CLONE_VM` immediately followed by
//! `execve` — a child that never runs libakuma code in its own address space.
//! This probe is the missing evidence.
//!
//! Four tests, each printing `forkprobe: <name> PASS|FAIL`, and a final
//! `forkprobe: ALL PASS` / `forkprobe: FAILURES=<n>` line for a log grep:
//!
//! 1. `basic`    — both sides return, child's exit code reaches the parent.
//! 2. `cow`      — heap writes after the fork stay private to each side.
//! 3. `sockfd`   — an `accept()`ed connection works from a forked child, and
//!                 keeps working in the parent's other child after the first
//!                 side closes it (the refcount contract in
//!                 `crates/akuma-net/src/socket.rs::remove_socket`).
//! 4. `many`     — 24 concurrent children (sshd's default session cap) all make
//!                 progress and all get reaped.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libakuma::socket_const::{AF_INET, SOCK_STREAM};
use libakuma::{
    accept, bind, close, connect, exit, fork, listen, println, recv, send, set_nonblocking, sleep_ms,
    socket, wait_any, waitpid_status, ForkResult, SocketAddrV4,
};

/// Port for the `sockfd` test. Well clear of sshd (2222) and httpd (80/8080).
const PROBE_PORT: u16 = 19_222;

/// How long any single wait-for-child loop will spin before calling it a hang.
const TIMEOUT_MS: u64 = 20_000;

/// Matches `sshd`'s default `max_sessions` — this is the concurrency level the
/// real server will actually reach, so it is the one worth proving.
const MANY_CHILDREN: usize = 24;

#[unsafe(no_mangle)]
pub extern "C" fn main() {
    println("forkprobe: starting");

    let mut failures = 0usize;
    if !test_basic() {
        failures += 1;
    }
    if !test_cow() {
        failures += 1;
    }
    if !test_sockfd() {
        failures += 1;
    }
    if !test_many() {
        failures += 1;
    }

    if failures == 0 {
        println("forkprobe: ALL PASS");
        exit(0);
    } else {
        println(&format!("forkprobe: FAILURES={}", failures));
        exit(1);
    }
}

// ============================================================================
// 1. basic — both sides return, exit code propagates
// ============================================================================

fn test_basic() -> bool {
    // 42 is arbitrary but must survive `encode_wait_status`'s high-byte
    // encoding intact, so a wrong shift shows up as a wrong number rather than
    // as a zero that a missing-child bug would also produce.
    const CHILD_EXIT: i32 = 42;

    let child_pid = match fork() {
        Ok(ForkResult::Child) => {
            // Anything this child prints goes to the *parent's* I/O channel —
            // fork keeps `parent.channel` deliberately (see step 8 of
            // `fork_process`), which is exactly why sshd's children can talk on
            // the same console.
            println("forkprobe:   [child] alive in my own address space");
            exit(CHILD_EXIT);
        }
        Ok(ForkResult::Parent(pid)) => pid,
        Err(e) => {
            println(&format!("forkprobe: basic FAIL (fork errno {})", e));
            return false;
        }
    };

    println(&format!("forkprobe:   [parent] forked child pid={}", child_pid));

    match reap(child_pid) {
        Some(code) if code == CHILD_EXIT => {
            println("forkprobe: basic PASS");
            true
        }
        Some(code) => {
            println(&format!(
                "forkprobe: basic FAIL (exit code {}, expected {})",
                code, CHILD_EXIT
            ));
            false
        }
        None => {
            println("forkprobe: basic FAIL (child never exited)");
            false
        }
    }
}

// ============================================================================
// 2. cow — post-fork heap writes are private
// ============================================================================

fn test_cow() -> bool {
    // A heap allocation, so this exercises the mmap-backed allocator regions
    // (libakuma's global allocator is mmap-only — see
    // `docs/archive/LIBAKUMA_AUDIT.md` item 13 for the brk arm that used to
    // sit behind this). Those are the regions CoW fork has historically
    // gotten wrong — see `docs/archive/` on lost mmap region extents.
    let mut owned: Vec<String> = Vec::new();
    for i in 0..64 {
        owned.push(format!("parent-value-{}", i));
    }

    let child_pid = match fork() {
        Ok(ForkResult::Child) => {
            // Scribble over every string. If CoW is broken and the pages are
            // genuinely shared, the parent's check below sees these writes.
            for (i, s) in owned.iter_mut().enumerate() {
                *s = format!("child-scribble-{}", i);
            }
            // Force more heap traffic so the allocator has to fault in new
            // pages in the child's own address space, not just break CoW on
            // existing ones.
            let mut extra: Vec<String> = Vec::new();
            for i in 0..256 {
                extra.push(format!("child-extra-{}", i));
            }
            if extra.len() == 256 && owned[0] == "child-scribble-0" {
                exit(0);
            }
            exit(1);
        }
        Ok(ForkResult::Parent(pid)) => pid,
        Err(e) => {
            println(&format!("forkprobe: cow FAIL (fork errno {})", e));
            return false;
        }
    };

    let child_ok = reap(child_pid) == Some(0);
    if !child_ok {
        println("forkprobe: cow FAIL (child could not write its own copy)");
        return false;
    }

    let intact = owned
        .iter()
        .enumerate()
        .all(|(i, s)| *s == format!("parent-value-{}", i));

    if intact {
        println("forkprobe: cow PASS");
        true
    } else {
        println("forkprobe: cow FAIL (child's writes leaked into the parent)");
        false
    }
}

// ============================================================================
// 3. sockfd — an accepted connection, handed to a child by fork
// ============================================================================

/// The shape sshd will use: bind/listen in the parent, `accept()` in the
/// parent, then `fork()` and let the child own the conversation.
///
/// A second child is forked onto the *same* accepted fd after the first one is
/// done with it. That is the part worth proving: `remove_socket` refcounts, so
/// the first child's `exit()` (which closes its whole fd table) must not tear
/// the smoltcp handle out from under the still-open parent copy. If it does,
/// child two reads from a dead — or worse, recycled — socket.
fn test_sockfd() -> bool {
    let listener = socket(AF_INET, SOCK_STREAM, 0);
    if listener < 0 {
        println(&format!("forkprobe: sockfd FAIL (socket errno {})", listener));
        return false;
    }

    let bind_addr = SocketAddrV4::new([0, 0, 0, 0], PROBE_PORT);
    if bind(listener, &bind_addr) < 0 {
        println("forkprobe: sockfd FAIL (bind)");
        close(listener);
        return false;
    }
    if listen(listener, 8) < 0 {
        println("forkprobe: sockfd FAIL (listen)");
        close(listener);
        return false;
    }

    // The client is itself a forked child — it needs no special fd handling,
    // it just dials the loopback address (`LoopbackAwareDevice` in
    // crates/akuma-net/src/smoltcp_net.rs intercepts 127.x traffic).
    let client_pid = match fork() {
        Ok(ForkResult::Child) => {
            close(listener); // a client has no business holding the listener
            exit(run_client());
        }
        Ok(ForkResult::Parent(pid)) => pid,
        Err(e) => {
            println(&format!("forkprobe: sockfd FAIL (client fork errno {})", e));
            close(listener);
            return false;
        }
    };

    // Non-blocking accept + bounded spin, so a client that never connects
    // fails this test instead of wedging the whole probe.
    set_nonblocking(listener, true);
    let mut conn = -1;
    let mut waited = 0;
    while waited < TIMEOUT_MS {
        let fd = accept(listener);
        if fd >= 0 {
            conn = fd;
            break;
        }
        sleep_ms(5);
        waited += 5;
    }

    if conn < 0 {
        println("forkprobe: sockfd FAIL (never accepted a connection)");
        close(listener);
        let _ = reap(client_pid);
        return false;
    }
    println(&format!("forkprobe:   [parent] accepted, conn fd={}", conn));

    // The accepted fd inherits the listener's non-blocking flag in some paths;
    // the children want plain blocking semantics.
    set_nonblocking(conn, false);

    // --- child one: read PING, answer PONG ---
    let handler_pid = match fork() {
        Ok(ForkResult::Child) => {
            close(listener);
            exit(run_handler(conn, b"PING", b"PONG"));
        }
        Ok(ForkResult::Parent(pid)) => pid,
        Err(e) => {
            println(&format!("forkprobe: sockfd FAIL (handler fork errno {})", e));
            close(conn);
            close(listener);
            let _ = reap(client_pid);
            return false;
        }
    };

    let handler_ok = reap(handler_pid) == Some(0);

    // --- child two: same fd, after child one exited and closed its copy ---
    let second_pid = match fork() {
        Ok(ForkResult::Child) => {
            close(listener);
            exit(run_handler(conn, b"PING2", b"PONG2"));
        }
        Ok(ForkResult::Parent(pid)) => pid,
        Err(e) => {
            println(&format!("forkprobe: sockfd FAIL (second fork errno {})", e));
            close(conn);
            close(listener);
            let _ = reap(client_pid);
            return false;
        }
    };

    let second_ok = reap(second_pid) == Some(0);
    let client_ok = reap(client_pid) == Some(0);

    close(conn);
    close(listener);

    if handler_ok && second_ok && client_ok {
        println("forkprobe: sockfd PASS");
        true
    } else {
        println(&format!(
            "forkprobe: sockfd FAIL (handler={} second={} client={})",
            handler_ok, second_ok, client_ok
        ));
        false
    }
}

/// Dial the probe port, do two request/response round trips, exit 0 on success.
fn run_client() -> i32 {
    let addr = SocketAddrV4::new([127, 0, 0, 1], PROBE_PORT);

    let fd = socket(AF_INET, SOCK_STREAM, 0);
    if fd < 0 {
        println("forkprobe:   [client] socket failed");
        return 1;
    }

    // The parent may not have reached `listen()`/`accept()` yet — retry rather
    // than racing it.
    let mut waited = 0;
    let mut connected = false;
    while waited < TIMEOUT_MS {
        if connect(fd, &addr) >= 0 {
            connected = true;
            break;
        }
        sleep_ms(20);
        waited += 20;
    }
    if !connected {
        println("forkprobe:   [client] connect timed out");
        close(fd);
        return 1;
    }

    let ok = round_trip(fd, b"PING", b"PONG") && round_trip(fd, b"PING2", b"PONG2");
    close(fd);
    if ok {
        0
    } else {
        1
    }
}

fn round_trip(fd: i32, req: &[u8], expect: &[u8]) -> bool {
    if send(fd, req, 0) != req.len() as isize {
        println("forkprobe:   [client] send failed");
        return false;
    }
    let mut buf = [0u8; 64];
    match read_exact(fd, &mut buf[..expect.len()]) {
        true if &buf[..expect.len()] == expect => true,
        true => {
            println("forkprobe:   [client] wrong reply");
            false
        }
        false => {
            println("forkprobe:   [client] reply timed out");
            false
        }
    }
}

/// Read `req`, reply `resp`. Runs in a forked child on an fd it inherited.
fn run_handler(conn: i32, req: &[u8], resp: &[u8]) -> i32 {
    let mut buf = [0u8; 64];
    if !read_exact(conn, &mut buf[..req.len()]) {
        println("forkprobe:   [handler] read timed out on inherited fd");
        return 1;
    }
    if &buf[..req.len()] != req {
        println("forkprobe:   [handler] inherited fd delivered the wrong bytes");
        return 1;
    }
    if send(conn, resp, 0) != resp.len() as isize {
        println("forkprobe:   [handler] write on inherited fd failed");
        return 1;
    }
    0
}

/// Fill `buf` completely, tolerating short reads, giving up after [`TIMEOUT_MS`].
fn read_exact(fd: i32, buf: &mut [u8]) -> bool {
    let mut got = 0;
    let mut waited = 0;
    while got < buf.len() && waited < TIMEOUT_MS {
        let n = recv(fd, &mut buf[got..], 0);
        if n > 0 {
            got += n as usize;
        } else if n == 0 {
            return false; // peer closed
        } else {
            sleep_ms(5);
            waited += 5;
        }
    }
    got == buf.len()
}

// ============================================================================
// 4. many — 24 concurrent children, sshd's default cap
// ============================================================================

/// Forks [`MANY_CHILDREN`] at once and reaps them with `wait_any()` — the same
/// anonymous-children pattern sshd's accept loop uses. Each child does a little
/// heap work before exiting so they overlap in time rather than exiting in
/// fork order.
fn test_many() -> bool {
    let mut pids: Vec<u32> = Vec::new();

    for i in 0..MANY_CHILDREN {
        match fork() {
            Ok(ForkResult::Child) => {
                // Stagger, so the parent is still forking while early children
                // run — the point is concurrency, not a serial queue.
                sleep_ms((i as u64 % 7) * 3);
                let mut sum = 0usize;
                for k in 0..2000 {
                    let s = format!("{}-{}", i, k);
                    sum += s.len();
                }
                // Exit code encodes "I really ran": nonzero work, capped into a
                // byte so the wait status can carry it.
                exit(if sum > 0 { 0 } else { 1 });
            }
            Ok(ForkResult::Parent(pid)) => pids.push(pid),
            Err(e) => {
                println(&format!(
                    "forkprobe: many FAIL (fork #{} errno {}) — {} already live",
                    i,
                    e,
                    pids.len()
                ));
                // Still reap what we made, so a partial failure does not leave
                // zombies behind for the next test.
                drain(&mut pids);
                return false;
            }
        }
    }

    println(&format!("forkprobe:   [parent] {} children live", pids.len()));

    let reaped = drain(&mut pids);
    if reaped == MANY_CHILDREN {
        println("forkprobe: many PASS");
        true
    } else {
        println(&format!(
            "forkprobe: many FAIL (reaped {} of {})",
            reaped, MANY_CHILDREN
        ));
        false
    }
}

/// Reap every pid in `pids` via `wait_any()`, returning how many exited 0.
fn drain(pids: &mut Vec<u32>) -> usize {
    let expected = pids.len();
    let mut clean = 0;
    let mut waited = 0;

    while clean < expected && waited < TIMEOUT_MS {
        match wait_any() {
            Some(st) if st.exit_code() == 0 && !st.signaled() => clean += 1,
            Some(st) => {
                println(&format!(
                    "forkprobe:   [parent] child {} ended badly (raw=0x{:x})",
                    st.pid, st.raw
                ));
                // Counted as reaped-but-failed: the loop must still terminate.
                clean += 1;
                return clean.saturating_sub(1);
            }
            None => {
                sleep_ms(5);
                waited += 5;
            }
        }
    }

    pids.clear();
    clean
}

// ============================================================================
// Shared helpers
// ============================================================================

/// Poll one known child to exit, returning its exit code (`None` on timeout).
fn reap(pid: u32) -> Option<i32> {
    let mut waited = 0;
    while waited < TIMEOUT_MS {
        if let Some(st) = waitpid_status(pid) {
            if st.signaled() {
                println(&format!(
                    "forkprobe:   [parent] child {} died from signal {:?}",
                    pid,
                    st.term_signal()
                ));
                return None;
            }
            return Some(st.exit_code());
        }
        sleep_ms(5);
        waited += 5;
    }
    None
}
