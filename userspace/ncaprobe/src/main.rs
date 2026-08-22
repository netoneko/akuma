//! ncaprobe — isolates Akuma-vs-Linux behaviour differences that only show up
//! on the real std/musl/pthreads runtime stack (the one tokio, Go and hyper
//! use). Build with `userspace/ncaprobe/build-musl.sh`; see its README.md.
//!
//! Every subcommand is designed to be run BOTH on Akuma and, unchanged, under
//! Docker on real Linux — the A/B is the whole point. Written for
//! `docs/archive/TOKIO_PIPE_EPOLL_HANG.md`.
//!
//! ```text
//! tokio [--workers N] [--tui]      end-to-end: does Command::output() complete?
//! eofedge                          does the EOF edge arrive after a partial drain?
//! ptyedge                          does a pty's 2nd EPOLLET edge arrive after an idle gap?
//! epoll [main|thread] [--late] [--zero]   raw pipe + spawn + epoll(ET) + pidfd
//! cross                            epoll_wait on one thread, epoll_ctl on another
//! fds                              open fds, which are epolls, and fd aliasing
//! waitid                           waitid(P_PIDFD, ...) — tokio's reaping call
//! timeoutleak [--fixed]            does a timed-out Command orphan its child? (nca bash.rs bug,
//!                                   fixed 2026-08-22 by adding .kill_on_drop(true); --fixed
//!                                   re-adds it here to show the contrast)
//! raw [main|thread|split]          tcsetattr(raw) + read(0), same/different thread
//! sleepbench                       what a short nanosleep actually costs
//! pollbench                        what a short epoll_wait timeout actually costs
//! termbench [--net]                stdout write-latency tail, +/- network load
//! pipebench [--epoll N]            pipe write+read cost
//! ```

use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const EPOLLIN: u32 = 0x1;
// The exact masks tokio used in the captured serial log
// (EPOLLET | EPOLLRDHUP | EPOLLOUT | EPOLLIN, and the same without EPOLLOUT).
const PIPE_MASK: u32 = 0x8000_2005;
const PIDFD_MASK: u32 = 0x8000_2001;

const SYS_PIDFD_OPEN: libc::c_long = 434;

fn tid() -> i64 {
    unsafe { libc::syscall(libc::SYS_gettid) as i64 }
}

fn ev_name(bits: u32) -> String {
    let mut v = Vec::new();
    for (b, n) in [
        (0x1u32, "IN"),
        (0x4, "OUT"),
        (0x8, "ERR"),
        (0x10, "HUP"),
        (0x2000, "RDHUP"),
    ] {
        if bits & b != 0 {
            v.push(n);
        }
    }
    if v.is_empty() {
        format!("0x{bits:x}")
    } else {
        format!("{} (0x{bits:x})", v.join("|"))
    }
}

// ---------------------------------------------------------------- probe A

fn probe_epoll(late: bool) {
    let t0 = Instant::now();
    let el = |t0: &Instant| format!("{:>6}ms", t0.elapsed().as_millis());

    println!("[{}] tid={} spawning /bin/busybox echo PROBE_OUT", el(&t0), tid());
    let child = match Command::new("/bin/busybox")
        .arg("echo")
        .arg("PROBE_OUT")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            println!("SPAWN FAILED: {e}");
            return;
        }
    };
    let pid = child.id();
    let out_fd = child.stdout.as_ref().unwrap().as_raw_fd();
    let err_fd = child.stderr.as_ref().unwrap().as_raw_fd();
    println!("[{}] child pid={pid} stdout_fd={out_fd} stderr_fd={err_fd}", el(&t0));

    if late {
        println!("[{}] --late: sleeping 800ms so the child exits BEFORE we register", el(&t0));
        std::thread::sleep(Duration::from_millis(800));
    }

    for fd in [out_fd, err_fd] {
        unsafe {
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }

    let pidfd = unsafe { libc::syscall(SYS_PIDFD_OPEN, pid as libc::c_int, 0) } as i32;
    println!("[{}] pidfd_open({pid}) = {pidfd}", el(&t0));

    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    println!("[{}] epoll_create1 = {epfd}", el(&t0));

    let add = |fd: i32, mask: u32, data: u64| {
        let mut ev = libc::epoll_event { events: mask, u64: data };
        let r = unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) };
        println!(
            "[{}] epoll_ctl(ADD, fd={fd}, 0x{mask:x}) = {r}{}",
            el(&t0),
            if r < 0 { format!(" errno={}", std::io::Error::last_os_error()) } else { String::new() }
        );
    };
    add(out_fd, PIPE_MASK, 1);
    add(err_fd, PIPE_MASK, 2);
    if pidfd >= 0 {
        add(pidfd, PIDFD_MASK, 3);
    }

    // mio polls with a zero timeout whenever the runtime already has work
    // queued. If the kernel's edge-trigger bookkeeping consumes the edge on a
    // scan that returns nothing, the following blocking wait never fires.
    if std::env::args().any(|a| a == "--zero") {
        std::thread::sleep(Duration::from_millis(300));
        let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 8];
        let n = unsafe { libc::epoll_wait(epfd, evs.as_mut_ptr(), 8, 0) };
        println!("[{}] --zero: pre-scan epoll_wait(timeout=0) -> {n}", el(&t0));
        for e in evs.iter().take(n.max(0) as usize) {
            println!("[{}]         data={} events={}", el(&t0), e.u64, ev_name(e.events));
        }
        println!("[{}] --zero: now doing the real blocking wait, WITHOUT reading anything first", el(&t0));
    }

    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut reaped = false;
    let mut collected = Vec::new();

    for round in 1..=12 {
        let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 8];
        let n = unsafe { libc::epoll_wait(epfd, evs.as_mut_ptr(), 8, 1000) };
        if n < 0 {
            println!("[{}] round {round}: epoll_wait ERR {}", el(&t0), std::io::Error::last_os_error());
            continue;
        }
        if n == 0 {
            println!(
                "[{}] round {round}: epoll_wait -> 0 (timeout)   stdout_eof={stdout_eof} stderr_eof={stderr_eof} reaped={reaped}",
                el(&t0)
            );
        }
        for e in evs.iter().take(n as usize) {
            let who = match e.u64 {
                1 => "stdout",
                2 => "stderr",
                3 => "pidfd",
                _ => "?",
            };
            println!("[{}] round {round}: READY {who} events={}", el(&t0), ev_name(e.events));

            if e.u64 == 1 || e.u64 == 2 {
                let fd = if e.u64 == 1 { out_fd } else { err_fd };
                loop {
                    let mut buf = [0u8; 512];
                    let r = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
                    if r > 0 {
                        collected.extend_from_slice(&buf[..r as usize]);
                        println!("[{}]           read({who}) = {r} bytes {:?}", el(&t0),
                            String::from_utf8_lossy(&buf[..r as usize]));
                    } else if r == 0 {
                        println!("[{}]           read({who}) = 0  EOF", el(&t0));
                        if e.u64 == 1 { stdout_eof = true } else { stderr_eof = true }
                        break;
                    } else {
                        let err = std::io::Error::last_os_error();
                        if err.kind() != std::io::ErrorKind::WouldBlock {
                            println!("[{}]           read({who}) err {err}", el(&t0));
                        }
                        break;
                    }
                }
            } else if e.u64 == 3 && !reaped {
                let mut status: libc::c_int = 0;
                let r = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
                println!("[{}]           waitpid(WNOHANG) = {r} status=0x{status:x}", el(&t0));
                if r > 0 {
                    reaped = true;
                }
            }
        }
        if stdout_eof && stderr_eof && reaped {
            println!("[{}] ALL DONE after {round} rounds — output {:?}", el(&t0),
                String::from_utf8_lossy(&collected));
            unsafe { libc::close(epfd) };
            return;
        }
    }

    println!(
        "[{}] GAVE UP: stdout_eof={stdout_eof} stderr_eof={stderr_eof} reaped={reaped}",
        el(&t0)
    );
    println!("--- what the state actually is, checked directly ---");
    let mut status: libc::c_int = 0;
    let r = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
    println!("  waitpid(WNOHANG) = {r} status=0x{status:x}");
    for (fd, who) in [(out_fd, "stdout"), (err_fd, "stderr")] {
        let mut buf = [0u8; 512];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        println!("  blind read({who}) = {n} {:?}", if n > 0 {
            String::from_utf8_lossy(&buf[..n as usize]).to_string()
        } else {
            String::from_utf8_lossy(b"").to_string()
        });
    }
    unsafe { libc::close(epfd) };
}

// ---------------------------------------------------------------- probe E
// tokio's shape: one thread is already parked in epoll_wait when ANOTHER
// thread registers the child's fds into that same epoll.

fn probe_cross() {
    let t0 = Instant::now();
    let el = |t0: &Instant| format!("{:>6}ms", t0.elapsed().as_millis());

    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    println!("[{}] tid={} epoll_create1 = {epfd}", el(&t0), tid());

    let t1 = t0;
    let waiter = std::thread::spawn(move || {
        println!("[{}] waiter tid={} entering epoll_wait loop on epfd {epfd}", el(&t1), tid());
        for round in 1..=12 {
            let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 8];
            let n = unsafe { libc::epoll_wait(epfd, evs.as_mut_ptr(), 8, 1000) };
            if n <= 0 {
                println!("[{}] waiter round {round}: -> {n}", el(&t1));
                continue;
            }
            for e in evs.iter().take(n as usize) {
                println!("[{}] waiter round {round}: READY data={} events={}", el(&t1), e.u64, ev_name(e.events));
            }
            return true;
        }
        false
    });

    std::thread::sleep(Duration::from_millis(400));

    println!("[{}] registrar tid={} spawning child", el(&t0), tid());
    let mut child = Command::new("/bin/busybox")
        .arg("echo").arg("CROSS_OUT")
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().expect("spawn");
    let out_fd = child.stdout.as_ref().unwrap().as_raw_fd();
    unsafe {
        let fl = libc::fcntl(out_fd, libc::F_GETFL);
        libc::fcntl(out_fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    let mut ev = libc::epoll_event { events: PIPE_MASK, u64: 1 };
    let r = unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, out_fd, &mut ev) };
    println!("[{}] registrar epoll_ctl(ADD, fd={out_fd}) = {r}  (waiter is already parked)", el(&t0));

    let saw = waiter.join().unwrap_or(false);
    println!("[{}] waiter saw the event: {saw}", el(&t0));
    let _ = child.wait();
}

// ---------------------------------------------------------------- probe G
// The exact tokio read pattern: the child writes, stays alive briefly, then
// exits. We take the EPOLLIN edge, drain what's there, and go back to
// epoll_wait for the EOF edge — which is what read_to_end() waits for.

fn probe_eofedge() {
    let t0 = Instant::now();
    let el = |t0: &Instant| format!("{:>6}ms", t0.elapsed().as_millis());

    // writes immediately, then lives 1s longer, so the EOF transition happens
    // strictly AFTER we have already taken the "data available" edge.
    let mut child = Command::new("/bin/busybox")
        .arg("sh").arg("-c").arg("echo EARLY; sleep 1")
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().expect("spawn");
    let out_fd = child.stdout.as_ref().unwrap().as_raw_fd();
    unsafe {
        let fl = libc::fcntl(out_fd, libc::F_GETFL);
        libc::fcntl(out_fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    let mut ev = libc::epoll_event { events: PIPE_MASK, u64: 1 };
    unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, out_fd, &mut ev) };
    println!("[{}] child pid={} stdout fd={out_fd} registered EPOLLIN|EPOLLET", el(&t0), child.id());

    let mut got_eof = false;
    for round in 1..=8 {
        let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 4];
        let n = unsafe { libc::epoll_wait(epfd, evs.as_mut_ptr(), 4, 1000) };
        if n <= 0 {
            println!("[{}] round {round}: epoll_wait -> {n} (no edge)", el(&t0));
            continue;
        }
        println!("[{}] round {round}: READY events={}", el(&t0), ev_name(evs[0].events));
        // Drain exactly like read_to_end: read until EAGAIN or EOF.
        loop {
            let mut buf = [0u8; 512];
            let r = unsafe { libc::read(out_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if r > 0 {
                println!("[{}]         read = {r} {:?}", el(&t0), String::from_utf8_lossy(&buf[..r as usize]));
            } else if r == 0 {
                println!("[{}]         read = 0  EOF -- read_to_end can finish", el(&t0));
                got_eof = true;
                break;
            } else {
                println!("[{}]         read = EAGAIN -- back to epoll_wait for the EOF edge", el(&t0));
                break;
            }
        }
        if got_eof {
            break;
        }
    }
    if got_eof {
        println!("[{}] RESULT: got EOF — Command::output() would complete", el(&t0));
    } else {
        println!("[{}] RESULT: *** EOF edge NEVER delivered — this is the nca/tokio hang ***", el(&t0));
        let mut buf = [0u8; 64];
        let r = unsafe { libc::read(out_fd, buf.as_mut_ptr().cast(), buf.len()) };
        println!("          (a blind read right now returns {r} — the EOF was there all along)");
    }
    let _ = child.wait();
}

// ---------------------------------------------------------------- probe L
// nca's TUI reads keystrokes via crossterm's default (mio) backend, which is
// edge-triggered epoll (EPOLLET) on the pty fd — the exact same mechanism
// PipeRead had the missing epoll_on_fd_drained re-arm for (probe G/eofedge).
// This is the pty-shaped version of that test: drain an initial byte, go
// back to epoll_wait, then — well after the reader is idle again — have a
// companion writer deliver a SECOND byte to the master side (what sshd does
// when a keystroke arrives from the network) and time how long that second
// edge takes to arrive. Written for the nca input-freeze finding in
// docs/archive/TOKIO_PIPE_EPOLL_HANG.md ("New finding 2026-08-18").

fn open_pty_pair() -> Option<(i32, i32)> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if master < 0 {
        println!("posix_openpt failed: {}", std::io::Error::last_os_error());
        return None;
    }
    if unsafe { libc::grantpt(master) } != 0 {
        println!("grantpt failed: {}", std::io::Error::last_os_error());
        return None;
    }
    if unsafe { libc::unlockpt(master) } != 0 {
        println!("unlockpt failed: {}", std::io::Error::last_os_error());
        return None;
    }
    let name_ptr = unsafe { libc::ptsname(master) };
    if name_ptr.is_null() {
        println!("ptsname failed: {}", std::io::Error::last_os_error());
        return None;
    }
    let cname = unsafe { std::ffi::CStr::from_ptr(name_ptr) };
    println!("pty slave path: {}", cname.to_string_lossy());
    let slave = unsafe { libc::open(cname.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if slave < 0 {
        println!("open(slave) failed: {}", std::io::Error::last_os_error());
        return None;
    }
    Some((master, slave))
}

fn probe_ptyedge() {
    let t0 = Instant::now();
    let el = |t0: &Instant| format!("{:>7}ms", t0.elapsed().as_millis());

    let Some((master, slave)) = open_pty_pair() else {
        println!("RESULT: could not open a pty pair — probe inconclusive");
        return;
    };
    println!("[{}] master fd={master} slave fd={slave}", el(&t0));

    // A freshly opened pty defaults to canonical mode: a single byte with no
    // newline just sits in the line-discipline's edit buffer and is never
    // "readable" at all, which would make this probe fail on ANY kernel and
    // is not what nca hits (crossterm puts the real tty in raw mode). Put
    // this one in raw mode too so single bytes become readable immediately.
    unsafe {
        let mut raw: libc::termios = std::mem::zeroed();
        libc::tcgetattr(slave, &mut raw);
        libc::cfmakeraw(&mut raw);
        libc::tcsetattr(slave, libc::TCSANOW, &raw);
    }

    unsafe {
        let fl = libc::fcntl(slave, libc::F_GETFL);
        libc::fcntl(slave, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    // Read interest only (mio registers stdin-like sources for READABLE, not
    // WRITABLE) — PIPE_MASK's EPOLLOUT bit would make epoll_wait return
    // immediately on the pty's (almost always writable) slave fd regardless
    // of whether there's anything to read, defeating the test.
    const EPOLLIN_ET: u32 = 0x8000_0001; // EPOLLIN | EPOLLET
    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    let mut ev = libc::epoll_event { events: EPOLLIN_ET, u64: 1 };
    unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, slave, &mut ev) };
    println!("[{}] slave registered EPOLLIN|EPOLLET on epoll", el(&t0));

    // Writer: byte 'A' shortly after start (round 1 — the easy case), then
    // — only after the reader has drained and gone idle — byte 'B' after a
    // real gap (round 2 — the case that was hanging in nca).
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        let r1 = unsafe { libc::write(master, b"A".as_ptr().cast(), 1) };
        println!("[writer] write('A') -> {r1} err={:?}", std::io::Error::last_os_error());
        std::thread::sleep(Duration::from_millis(700));
        let r2 = unsafe { libc::write(master, b"B".as_ptr().cast(), 1) };
        println!("[writer] write('B') -> {r2} err={:?}", std::io::Error::last_os_error());
    });

    let drain = |t0: &Instant, fd: i32, label: &str| {
        loop {
            let mut buf = [0u8; 64];
            let r = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if r > 0 {
                println!(
                    "[{}]         {label} read = {r} {:?}",
                    el(t0),
                    String::from_utf8_lossy(&buf[..r as usize])
                );
            } else {
                println!("[{}]         {label} read = EAGAIN/err({r})", el(t0));
                break;
            }
        }
    };

    for (round, budget_ms, expect) in [(1, 2000, 'A'), (2, 3000, 'B')] {
        let wait_start = Instant::now();
        let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 4];
        let n = unsafe { libc::epoll_wait(epfd, evs.as_mut_ptr(), 4, budget_ms) };
        let waited = wait_start.elapsed();
        if n <= 0 {
            println!(
                "[{}] round {round}: epoll_wait(budget={budget_ms}ms) -> {n} after {waited:?} \
                 — *** edge for {expect:?} NEVER ARRIVED ***",
                el(&t0)
            );
            continue;
        }
        println!(
            "[{}] round {round}: READY events={} after {waited:?} (budget was {budget_ms}ms)",
            el(&t0),
            ev_name(evs[0].events)
        );
        drain(&t0, slave, &format!("round{round}"));
    }
    let _ = writer.join();
    println!("[{}] done", el(&t0));
}

// ---------------------------------------------------------------- probe M
// `ptyedge` above needs a real POSIX pty (`/dev/ptmx`), which does not exist
// on Akuma — `nca`'s actual stdin under sshd is a `FileDescriptor::Stdin`
// exec-channel, a different, Akuma-specific construct entirely (see
// `docs/archive/TOKIO_PIPE_EPOLL_HANG.md`, "New finding 2026-08-18"). This
// tests THAT code path directly: register fd 0 itself with EPOLLIN|EPOLLET,
// set it non-blocking, and time how long it takes to see a keystroke typed
// after an idle gap. Needs a real interactive session — run this, wait
// ~2s, then type one character.

fn probe_stdinedge() {
    let t0 = Instant::now();
    let el = |t0: &Instant| format!("{:>7}ms", t0.elapsed().as_millis());

    unsafe {
        let fl = libc::fcntl(0, libc::F_GETFL);
        libc::fcntl(0, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    const EPOLLIN_ET: u32 = 0x8000_0001; // EPOLLIN | EPOLLET
    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    let mut ev = libc::epoll_event { events: EPOLLIN_ET, u64: 1 };
    let reg = unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, 0, &mut ev) };
    println!("[{}] fd 0 registered EPOLLIN|EPOLLET on epoll (epoll_ctl -> {reg})", el(&t0));
    println!("[{}] type ONE character now, then wait — no need to press Enter", el(&t0));

    for round in 1..=3 {
        let wait_start = Instant::now();
        let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 4];
        let n = unsafe { libc::epoll_wait(epfd, evs.as_mut_ptr(), 4, 15_000) };
        let waited = wait_start.elapsed();
        if n <= 0 {
            println!(
                "[{}] round {round}: epoll_wait(budget=15000ms) -> {n} after {waited:?} \
                 — *** edge never arrived ***",
                el(&t0)
            );
            continue;
        }
        println!(
            "[{}] round {round}: READY events={} after {waited:?}",
            el(&t0),
            ev_name(evs[0].events)
        );
        loop {
            let mut buf = [0u8; 64];
            let r = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if r > 0 {
                println!(
                    "[{}]         read = {r} {:?}",
                    el(&t0),
                    String::from_utf8_lossy(&buf[..r as usize])
                );
            } else {
                println!("[{}]         read = EAGAIN/err({r})", el(&t0));
                break;
            }
        }
        println!("[{}] type another character (or wait 15s to end)", el(&t0));
    }
    println!("[{}] done", el(&t0));
}

// ---------------------------------------------------------------- probe H
// Pipe read/write cost. `epoll_on_fd_drained` runs on every pipe read, so
// anything it does lands on sshd's bridge, every byte of a TUI's output and
// every busybox pipeline. This measures that per-read cost directly.
//
// `--epoll N` first registers the read end in N epoll instances, because the
// re-arm walks every instance in the table: the gap between N=0 and N=8 is the
// part that scales with how much else on the box is using epoll.

fn probe_pipebench() {
    const ITERS: usize = 20_000;
    let n_epolls: usize = std::env::args()
        .position(|a| a == "--epoll")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        println!("pipe() failed");
        return;
    }
    let (r, w) = (fds[0], fds[1]);
    unsafe {
        let fl = libc::fcntl(r, libc::F_GETFL);
        libc::fcntl(r, libc::F_SETFL, fl | libc::O_NONBLOCK);
    }

    let mut epfds = Vec::new();
    for _ in 0..n_epolls {
        let ep = unsafe { libc::epoll_create1(0) };
        let mut ev = libc::epoll_event { events: PIPE_MASK, u64: 1 };
        unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, r, &mut ev) };
        epfds.push(ep);
    }
    println!("pipe rw x{ITERS}, read end registered in {n_epolls} epoll instance(s)");

    let buf = [b'x'; 64];
    let mut rbuf = [0u8; 64];
    let t0 = Instant::now();
    for _ in 0..ITERS {
        unsafe {
            libc::write(w, buf.as_ptr().cast(), buf.len());
            libc::read(r, rbuf.as_mut_ptr().cast(), rbuf.len());
        }
    }
    let el = t0.elapsed();

    println!(
        "RESULT: {:.2} us/iter  ({} iters in {} ms, {:.0} iters/s)",
        el.as_secs_f64() * 1e6 / ITERS as f64,
        ITERS,
        el.as_millis(),
        ITERS as f64 / el.as_secs_f64()
    );
    for ep in epfds {
        unsafe { libc::close(ep) };
    }
    unsafe {
        libc::close(r);
        libc::close(w);
    }
}

// ---------------------------------------------------------------- probe I
// "Networking stutters the terminal." A TUI redraw is a burst of writes to
// stdout; stutter is the TAIL of that write latency, not the mean. Measure the
// distribution with the network idle, then again with a download running in
// another thread, and compare the tails.
//
// stdout here takes the same path nca's does: pipe -> sshd -> TCP. Results are
// buffered and printed at the end so the reporting does not perturb the thing
// being measured.

fn pctile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

fn probe_termbench() {
    const ITERS: usize = 1500;
    const CHUNK: usize = 1024;
    let with_net = std::env::args().any(|a| a == "--net");
    let host = std::env::args()
        .position(|a| a == "--host")
        .and_then(|i| std::env::args().nth(i + 1))
        .unwrap_or_else(|| "10.0.2.2:8899".to_string());

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut netthread = None;

    if with_net {
        let (stop_c, bytes_c, host_c) = (stop.clone(), bytes.clone(), host.clone());
        netthread = Some(std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            while !stop_c.load(std::sync::atomic::Ordering::Relaxed) {
                let Ok(mut s) = std::net::TcpStream::connect(&host_c) else {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                };
                let _ = s.write_all(
                    format!("GET /ncaprobe HTTP/1.0\r\nHost: {host_c}\r\n\r\n").as_bytes(),
                );
                let mut buf = [0u8; 16384];
                loop {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            bytes_c.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                            if stop_c.load(std::sync::atomic::Ordering::Relaxed) {
                                break;
                            }
                        }
                    }
                }
            }
        }));
        std::thread::sleep(Duration::from_millis(600)); // let traffic ramp
    }

    let chunk = vec![b'.'; CHUNK];
    let mut lat = Vec::with_capacity(ITERS);
    // warm up
    for _ in 0..50 {
        unsafe { libc::write(1, chunk.as_ptr().cast(), CHUNK) };
    }
    let t0 = Instant::now();
    for _ in 0..ITERS {
        let s = Instant::now();
        unsafe { libc::write(1, chunk.as_ptr().cast(), CHUNK) };
        lat.push(s.elapsed().as_micros() as u64);
    }
    let wall = t0.elapsed();

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(h) = netthread {
        let _ = h.join();
    }

    lat.sort_unstable();
    let total: u64 = lat.iter().sum();
    println!("\n\n=== termbench {} ===", if with_net { "WITH concurrent download" } else { "network idle" });
    println!("{ITERS} writes of {CHUNK}B to stdout in {} ms", wall.as_millis());
    if with_net {
        println!(
            "concurrent download moved {} KiB",
            bytes.load(std::sync::atomic::Ordering::Relaxed) / 1024
        );
    }
    println!(
        "write latency us:  p50={}  p90={}  p99={}  max={}  mean={}",
        pctile(&lat, 0.50),
        pctile(&lat, 0.90),
        pctile(&lat, 0.99),
        lat[lat.len() - 1],
        total / lat.len() as u64
    );
    let stalls: Vec<u64> = lat.iter().copied().filter(|&x| x > 10_000).collect();
    println!("writes over 10ms (visible stalls): {}", stalls.len());
    if !stalls.is_empty() {
        println!("  worst: {:?}", &stalls[stalls.len().saturating_sub(8)..]);
    }
}

// ---------------------------------------------------------------- probe J
// Sleep granularity. sshd's bridge loop asks for sleep_ms(1) between polls, so
// whatever a 1 ms sleep ACTUALLY costs is the quantum every byte of terminal
// output is forwarded at. Ask for a range of short sleeps and report what came
// back.

fn probe_sleepbench() {
    println!("=== probe: nanosleep granularity ===");
    println!("requested -> actual (median of 40), us");
    for req_us in [500u64, 1_000, 2_000, 5_000, 10_000, 20_000] {
        let mut got = Vec::new();
        for _ in 0..40 {
            let ts = libc::timespec {
                tv_sec: (req_us / 1_000_000) as libc::time_t,
                tv_nsec: ((req_us % 1_000_000) * 1000) as _,
            };
            let t = Instant::now();
            unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) };
            got.push(t.elapsed().as_micros() as u64);
        }
        got.sort_unstable();
        let med = got[got.len() / 2];
        println!(
            "  {:>7} -> {:>7}   (min {:>7} max {:>7})  overshoot x{:.1}",
            req_us,
            med,
            got[0],
            got[got.len() - 1],
            med as f64 / req_us as f64
        );
    }
}

// ---------------------------------------------------------------- probe K
// Does a short poll TIMEOUT actually shorten the wait?
//
// `sys_epoll_pwait` caps each loop iteration's sleep at
// `effective_poll_interval_us` (10 ms normally, 1 ms for rump fds) and re-scans.
// That knob can only do something if the scheduler can actually deliver a wake
// sooner than the round-robin period. Poll an fd that never becomes ready and
// compare the requested timeout against the wall clock.
//
// If short timeouts come back at the same ~35 ms as long ones, then every
// poll-interval tuning knob in the kernel is inert and the round-robin pass is
// the only thing that matters.

fn probe_pollbench() {
    println!("=== probe: epoll_wait timeout accuracy (never-ready fd) ===");
    // An eventfd nobody ever writes: registered, watched, never ready.
    let efd = unsafe { libc::eventfd(0, 0) };
    let epfd = unsafe { libc::epoll_create1(0) };
    let mut ev = libc::epoll_event { events: EPOLLIN, u64: 1 };
    unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, efd, &mut ev) };

    println!("requested -> actual (median of 30), us");
    for req_ms in [1i32, 2, 5, 10, 50] {
        let mut got = Vec::new();
        for _ in 0..30 {
            let mut out = [libc::epoll_event { events: 0, u64: 0 }; 1];
            let t = Instant::now();
            let n = unsafe { libc::epoll_wait(epfd, out.as_mut_ptr(), 1, req_ms) };
            got.push(t.elapsed().as_micros() as u64);
            if n != 0 {
                println!("  (unexpected ready fd)");
            }
        }
        got.sort_unstable();
        let med = got[got.len() / 2];
        println!(
            "  {:>5} ms -> {:>7} us   (min {:>7} max {:>7})  overshoot x{:.1}",
            req_ms,
            med,
            got[0],
            got[got.len() - 1],
            med as f64 / (req_ms as f64 * 1000.0)
        );
    }
    unsafe {
        libc::close(epfd);
        libc::close(efd);
    }
}

// ---------------------------------------------------------------- probe F
// tokio's pidfd reaper reaps with waitid(P_PIDFD, ...), not wait4.

fn probe_waitid() {
    const P_PIDFD: libc::c_int = 3;
    const WEXITED: libc::c_int = 4;

    let mut child = Command::new("/bin/busybox")
        .arg("echo").arg("WAITID_OUT")
        .stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn()
        .or_else(|_| Command::new("/bin/echo").arg("WAITID_OUT")
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn())
        .expect("spawn");
    let pid = child.id();
    let pidfd = unsafe { libc::syscall(SYS_PIDFD_OPEN, pid as libc::c_int, 0) } as i32;
    println!("child pid={pid} pidfd={pidfd}");

    std::thread::sleep(Duration::from_millis(500));
    println!("(child has had 500ms to exit)");

    let mut info = [0u8; 128];
    let r = unsafe {
        libc::syscall(
            libc::SYS_waitid,
            P_PIDFD as libc::c_long,
            pidfd as libc::c_long,
            info.as_mut_ptr() as libc::c_long,
            (WEXITED | libc::WNOHANG) as libc::c_long,
            0 as libc::c_long,
        )
    };
    let err = std::io::Error::last_os_error();
    let si_signo = u32::from_ne_bytes(info[0..4].try_into().unwrap());
    let si_code = i32::from_ne_bytes(info[8..12].try_into().unwrap());
    let si_pid = u32::from_ne_bytes(info[16..20].try_into().unwrap());
    let si_status = i32::from_ne_bytes(info[24..28].try_into().unwrap());
    println!(
        "waitid(P_PIDFD, {pidfd}, WEXITED|WNOHANG) = {r}{}",
        if r < 0 { format!("  errno={err}") } else { String::new() }
    );
    println!("  siginfo: si_signo={si_signo} si_code={si_code} si_pid={si_pid} si_status={si_status}");
    if r == 0 && si_pid == 0 {
        println!("  *** returned success but siginfo is EMPTY — caller sees 'not exited yet' forever ***");
    }

    let mut status: libc::c_int = 0;
    let w = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
    println!("waitpid(WNOHANG) afterwards = {w} status=0x{status:x}");
    let _ = child.try_wait();
}

// ---------------------------------------------------------------- probe B

fn get_termios() -> Option<libc::termios> {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut t) } == 0 {
        Some(t)
    } else {
        println!("  tcgetattr(0) FAILED: {}", std::io::Error::last_os_error());
        None
    }
}

fn set_raw() {
    let Some(mut t) = get_termios() else { return };
    println!(
        "  [tid={}] before: lflag=0x{:x} iflag=0x{:x}",
        tid(),
        t.c_lflag,
        t.c_iflag
    );
    t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
    t.c_iflag &= !(libc::ICRNL | libc::IXON | libc::BRKINT | libc::INPCK | libc::ISTRIP);
    t.c_cc[libc::VMIN] = 1;
    t.c_cc[libc::VTIME] = 0;
    let r = unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
    println!("  [tid={}] tcsetattr(raw) = {r}", tid());
    if let Some(back) = get_termios() {
        println!(
            "  [tid={}] readback: lflag=0x{:x} iflag=0x{:x}  (ICANON={} ECHO={})",
            tid(),
            back.c_lflag,
            back.c_iflag,
            back.c_lflag & libc::ICANON != 0,
            back.c_lflag & libc::ECHO != 0
        );
    }
}

fn read_one(label: &str) {
    println!("  [tid={}] {label}: reading 1 byte from fd 0 — press ESC now", tid());
    let mut buf = [0u8; 8];
    let n = std::io::stdin().read(&mut buf).unwrap_or(-1isize as usize);
    if n == usize::MAX {
        println!("  [tid={}] read FAILED: {}", tid(), std::io::Error::last_os_error());
        return;
    }
    println!(
        "  [tid={}] read {n} byte(s): {:02x?}  {}",
        tid(),
        &buf[..n],
        if n > 0 && buf[0] == 0x1b { "<<< ESC ARRIVED" } else { "<<< not ESC" }
    );
}

fn probe_raw(mode: &str) {
    println!("isatty(0) = {}", unsafe { libc::isatty(0) });
    println!("main thread tid = {}", tid());
    let saved = get_termios();

    match mode {
        "main" => {
            set_raw();
            read_one("main");
        }
        "thread" => {
            std::thread::spawn(|| {
                set_raw();
                read_one("thread");
            })
            .join()
            .ok();
        }
        "split" => {
            println!("  setting raw on MAIN thread:");
            set_raw();
            println!("  reading on a SPAWNED thread:");
            std::thread::spawn(|| {
                if let Some(t) = get_termios() {
                    println!(
                        "  [tid={}] sees lflag=0x{:x} (ICANON={} ECHO={})",
                        tid(),
                        t.c_lflag,
                        t.c_lflag & libc::ICANON != 0,
                        t.c_lflag & libc::ECHO != 0
                    );
                }
                read_one("split");
            })
            .join()
            .ok();
        }
        other => println!("unknown raw mode {other}"),
    }

    if let Some(s) = saved {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &s) };
        println!("restored");
    }
}

// ---------------------------------------------------------------- probe N
// Reproduces the cargo->rustc "Bad address (os error 14)" spawn failure
// (docs/archive/NCA_MISSING_SYSCALLS.md §1) at the plain std::process::Command
// level: the *exact* heavy rustc invocation cargo uses to build proc-macro2's
// build script on this guest, piped stdout/stderr (matching cargo's JSON
// diagnostics capture), looped many times. The existing investigation found
// this racy (~8 successes before the first big-compile spawn failed) and a
// lighter synthetic mimic (piped stdio + big argv, no real heavy rustc
// invocation) passed 40/40 — this probe spawns the real thing instead, to
// see whether the failure needs the actual weight of a real compile to
// trigger, and to catch a raw_os_error on the Rust side per iteration.

/// One spawn+wait of the exact heavy rustc invocation cargo uses for
/// proc-macro2's build script, with cargo's exact injected environment.
/// `label` must be unique across concurrent callers (out-dir and `-C
/// metadata` both key off it).
fn bigspawn_one(label: &str) -> (bool, String) {
    let args: [&str; 13] = [
        "--crate-name", "build_script_build", "--edition=2021",
        "/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/proc-macro2-1.0.106/build.rs",
        "--error-format=json",
        "--json=diagnostic-rendered-ansi,artifacts,future-incompat",
        "--crate-type", "bin", "--emit=dep-info,link",
        "-C", "embed-bitcode=no", "-C", "debug-assertions=off",
    ];
    let out_dir = format!("/tmp/rustc_bigspawn_{label}");
    let _ = std::fs::create_dir_all(&out_dir);
    let t0 = Instant::now();
    let mut cmd = Command::new("/usr/local/bin/rustc");
    // NOT env_clear()'d: cargo adds these on top of its own inherited
    // environment (PATH etc still needed to find the linker), it doesn't
    // replace it.
    cmd.env("CARGO", "/usr/local/bin/cargo");
    cmd.env("CARGO_CRATE_NAME", "build_script_build");
    cmd.env(
        "CARGO_MANIFEST_DIR",
        "/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/proc-macro2-1.0.106",
    );
    cmd.env(
        "CARGO_MANIFEST_PATH",
        "/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/proc-macro2-1.0.106/Cargo.toml",
    );
    cmd.env(
        "CARGO_PKG_AUTHORS",
        "David Tolnay <dtolnay@gmail.com>:Alex Crichton <alex@alexcrichton.com>",
    );
    cmd.env(
        "CARGO_PKG_DESCRIPTION",
        "A substitute implementation of the compiler's `proc_macro` API to decouple token-based libraries from the procedural macro use case.",
    );
    cmd.env("CARGO_PKG_HOMEPAGE", "");
    cmd.env("CARGO_PKG_LICENSE", "MIT OR Apache-2.0");
    cmd.env("CARGO_PKG_LICENSE_FILE", "");
    cmd.env("CARGO_PKG_NAME", "proc-macro2");
    cmd.env("CARGO_PKG_README", "README.md");
    cmd.env(
        "CARGO_PKG_REPOSITORY",
        "https://github.com/dtolnay/proc-macro2",
    );
    cmd.env("CARGO_PKG_RUST_VERSION", "1.68");
    cmd.env("CARGO_PKG_VERSION", "1.0.106");
    cmd.env("CARGO_PKG_VERSION_MAJOR", "1");
    cmd.env("CARGO_PKG_VERSION_MINOR", "0");
    cmd.env("CARGO_PKG_VERSION_PATCH", "106");
    cmd.env("CARGO_PKG_VERSION_PRE", "");
    cmd.env("LD_LIBRARY_PATH", "");
    cmd.args(args)
        .arg("--cfg")
        .arg("feature=\"default\"")
        .arg("--cfg")
        .arg("feature=\"proc-macro\"")
        .arg("--check-cfg")
        .arg("cfg(docsrs,test)")
        .arg("--check-cfg")
        .arg("cfg(feature, values(\"default\", \"nightly\", \"proc-macro\", \"span-locations\"))")
        .arg("-C")
        .arg(format!("metadata=bigspawn{label}"))
        .arg("--out-dir")
        .arg(&out_dir)
        .arg("-C")
        .arg("strip=symbols")
        .arg("--cap-lints")
        .arg("allow")
        .current_dir("/tmp/native-cli-ai")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let result = match cmd.output() {
        Ok(out) if out.status.success() => (
            true,
            format!("[{label}] OK in {}ms", t0.elapsed().as_millis()),
        ),
        Ok(out) => {
            let full_path = format!("/tmp/bigspawn_fail_{label}.stderr");
            let _ = std::fs::write(&full_path, &out.stderr);
            let stderr_head: String =
                String::from_utf8_lossy(&out.stderr).chars().take(300).collect();
            (
                false,
                format!(
                    "[{label}] rustc exited {:?} in {}ms full_stderr={full_path} stderr={stderr_head:?}",
                    out.status.code(),
                    t0.elapsed().as_millis()
                ),
            )
        }
        Err(e) => (
            false,
            format!(
                "[{label}] *** SPAWN FAILED after {}ms: {e} (raw_os_error={:?}) ***",
                t0.elapsed().as_millis(),
                e.raw_os_error()
            ),
        ),
    };
    let _ = std::fs::remove_dir_all(&out_dir);
    result
}

fn probe_bigspawn(iterations: usize) {
    let mut ok = 0usize;
    let mut fail = 0usize;
    for i in 0..iterations {
        let (pass, line) = bigspawn_one(&format!("{i:08x}"));
        println!("{line}");
        if pass {
            ok += 1;
        } else {
            fail += 1;
        }
    }
    println!("RESULT: {ok} ok, {fail} failed out of {iterations}");
}

/// Same spawn, but from `threads` OS threads concurrently, `iters_per_thread`
/// rounds each — cargo's own execution is multi-threaded (job-scheduling pool
/// + jobserver), unlike the sequential `bigspawn` above, which never
/// reproduced the failure in 80 combined iterations. This tests whether
/// concurrent spawning is the missing ingredient.
fn probe_bigspawn_threads(threads: usize, iters_per_thread: usize) {
    let results: std::sync::Arc<std::sync::Mutex<Vec<(bool, String)>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for t in 0..threads {
        let results = results.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..iters_per_thread {
                let (pass, line) = bigspawn_one(&format!("t{t}_{i:08x}"));
                println!("{line}");
                results.lock().unwrap().push((pass, line));
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    let results = results.lock().unwrap();
    let ok = results.iter().filter(|(pass, _)| *pass).count();
    let fail = results.len() - ok;
    println!(
        "RESULT: {ok} ok, {fail} failed out of {} ({threads} threads x {iters_per_thread})",
        results.len()
    );
}

// ---------------------------------------------------------------- probe C
// Exactly what nca's BashTool does (crates/core/src/tools/bash.rs:43-67),
// on a multi-thread runtime like nca's.

fn probe_tokio(blocking_tui: bool, worker_threads: usize) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()
        .expect("runtime");

    println!("runtime: {worker_threads} worker thread(s), available_parallelism={:?}",
        std::thread::available_parallelism());

    rt.block_on(async move {
        // nca holds a spawn_blocking task for its whole TUI lifetime. Model
        // that: it occupies a blocking-pool thread and never returns.
        if blocking_tui {
            println!("occupying a spawn_blocking thread (like nca's TUI) ...");
            tokio::task::spawn_blocking(|| loop {
                std::thread::sleep(Duration::from_millis(66));
            });
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        for (i, command) in ["echo TOKIO_OUT", "pwd", "hostname"].iter().enumerate() {
            let t0 = Instant::now();
            println!("--- call {i}: sh -lc {command:?}");
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-lc")
                .arg(command)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            match tokio::time::timeout(Duration::from_secs(10), cmd.output()).await {
                Ok(Ok(out)) => println!(
                    "    OK in {}ms status={:?} stdout={:?} stderr={:?}",
                    t0.elapsed().as_millis(),
                    out.status.code(),
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ),
                Ok(Err(e)) => println!("    SPAWN ERR after {}ms: {e}", t0.elapsed().as_millis()),
                Err(_) => println!("    *** TIMED OUT after {}ms — this is the nca bug ***",
                    t0.elapsed().as_millis()),
            }
        }
    });
}

// ---------------------------------------------------------------- probe C2
// nca's `bash.rs` `execute_bash` tool, verbatim: `tokio::time::timeout(secs,
// cmd.output())`, and — as shipped, before this investigation's fix — no
// `.kill_on_drop(true)` on the `Command`. Per tokio's own documented default,
// dropping a `Child` without that flag does NOT signal the process; it keeps
// running, orphaned. `--fixed` adds the flag back to show the contrast.
//
// Call A holds an flock well past its own timeout, simulating cargo holding
// its own target-dir build lock across a command nca gave up on. Call B then
// wants the same lock — real production shape: "cargo build" timed out,
// then a later "cargo build" in the same workspace contends with the first,
// STILL-RUNNING one for no reason its own log shows.

async fn timeoutleak_run_one(label: &str, command: &str, timeout_secs: u64, fixed: bool) -> Option<i32> {
    let t0 = Instant::now();
    println!("--- {label}: sh -lc {command:?}  (timeout={timeout_secs}s)");
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-lc").arg(command).stdout(Stdio::piped()).stderr(Stdio::piped());
    if fixed {
        cmd.kill_on_drop(true);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            println!("    SPAWN FAILED: {e}");
            return None;
        }
    };
    let pid = child.id().map(|p| p as i32);
    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(Ok(status)) => {
            println!("    completed in {}ms status={:?}", t0.elapsed().as_millis(), status.code());
            None
        }
        Ok(Err(e)) => {
            println!("    WAIT ERR after {}ms: {e}", t0.elapsed().as_millis());
            None
        }
        Err(_) => {
            println!(
                "    *** TIMED OUT after {}ms (this is what nca reports to the model; pid={pid:?}) ***",
                t0.elapsed().as_millis()
            );
            pid
        }
    }
    // `child` (and, if `fixed`, its kill-on-drop) is dropped HERE, exactly
    // where nca's `match timeout(...).await { Err(_) => ... }` drops it.
}

fn probe_timeoutleak(fixed: bool) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");

    println!(
        "=== probe: nca bash.rs's timeout pattern {} ===",
        if fixed { "WITH .kill_on_drop(true) (the fix)" } else { "AS SHIPPED (no kill_on_drop)" }
    );

    rt.block_on(async move {
        let lockfile = "/tmp/ncaprobe_timeoutleak.lock";
        let _ = std::fs::remove_file(lockfile);

        let orphan_pid = timeoutleak_run_one(
            "call A (holds an flock for 8s)",
            &format!("flock {lockfile} -c 'sleep 8'"),
            2,
            fixed,
        )
        .await;

        if let Some(pid) = orphan_pid {
            std::thread::sleep(Duration::from_millis(300));
            let alive = unsafe { libc::kill(pid, 0) == 0 };
            println!(
                "    orphan check: pid={pid} still alive={alive}  {}",
                if alive {
                    "*** LEAKED — still running after nca reported it timed out ***"
                } else {
                    "correctly killed"
                }
            );
        }

        // Independent of whether A leaked: does B's OWN timeout still fire at
        // its OWN deadline? This isolates "the orphan causes contention" from
        // "the timeout mechanism itself is broken" — they are different bugs
        // and this probe is built to tell them apart.
        let b_pid = timeoutleak_run_one(
            "call B (wants the SAME lock, right after A)",
            &format!("exec 9>{lockfile}; flock 9; echo GOT_LOCK"),
            3,
            fixed,
        )
        .await;
        if let Some(pid) = b_pid {
            let alive = unsafe { libc::kill(pid, 0) == 0 };
            println!(
                "    B also timed out (pid={pid} alive={alive}) — contention from A's orphan, \
                 but B's OWN timeout still fired correctly: the leak, not the timer, is the bug"
            );
        } else {
            println!("    call B completed within its own timeout");
        }

        let _ = std::fs::remove_file(lockfile);
    });
}

// ---------------------------------------------------------------- probe D
// Which fds are open, and which of them are epoll instances?
// akuma's sys_epoll_ctl returns EBADF for a non-epoll fd and ENOENT for an
// epoll fd that doesn't hold the target — a perfect discriminator.

fn scan_fds(label: &str) {
    let mut line = format!("[{label}] tid={} fds:", tid());
    for fd in 0..24 {
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            continue;
        }
        let r = unsafe { libc::epoll_ctl(fd, libc::EPOLL_CTL_DEL, 999, std::ptr::null_mut()) };
        let kind = if r == 0 {
            "EPOLL?"
        } else {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ENOENT) => "EPOLL",
                Some(libc::EBADF) => "-",
                Some(libc::EINVAL) => "-(einval)",
                other => {
                    line.push_str(&format!(" {fd}:err{other:?}"));
                    continue;
                }
            }
        };
        line.push_str(&format!(" {fd}:{kind}"));
    }
    println!("{line}");
}

/// Are epoll fds `a` and `b` the same kernel instance? Register a target on
/// `a`, then try to remove it via `b`. 0 => same instance, ENOENT => distinct.
fn alias_test(a: i32, b: i32) {
    let target = unsafe { libc::eventfd(0, 0) };
    if target < 0 {
        println!("alias_test: eventfd failed");
        return;
    }
    let mut ev = libc::epoll_event { events: EPOLLIN, u64: 0xABCD };
    let r1 = unsafe { libc::epoll_ctl(a, libc::EPOLL_CTL_ADD, target, &mut ev) };
    let r2 = unsafe { libc::epoll_ctl(b, libc::EPOLL_CTL_DEL, target, std::ptr::null_mut()) };
    let e2 = std::io::Error::last_os_error();
    println!(
        "alias_test: ADD via fd {a} -> {r1};  DEL via fd {b} -> {r2}{}   ==> {}",
        if r2 < 0 { format!(" ({e2})") } else { String::new() },
        if r2 == 0 { "*** SAME kernel epoll instance ***" } else { "distinct instances (correct)" }
    );
    unsafe {
        libc::epoll_ctl(a, libc::EPOLL_CTL_DEL, target, std::ptr::null_mut());
        libc::close(target);
    }
}

fn probe_fds() {
    scan_fds("before runtime");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    scan_fds("after runtime (main thread)");
    alias_test(3, 5);
    // Control: two epolls we create ourselves must be distinct.
    let x = unsafe { libc::epoll_create1(0) };
    let y = unsafe { libc::epoll_create1(0) };
    println!("control: fresh epolls {x} and {y}");
    alias_test(x, y);
    unsafe {
        libc::close(x);
        libc::close(y);
    }
    rt.block_on(async {
        scan_fds("inside block_on");
        tokio::spawn(async { scan_fds("inside tokio::spawn task") })
            .await
            .ok();
        tokio::task::spawn_blocking(|| scan_fds("inside spawn_blocking"))
            .await
            .ok();
    });
    std::thread::spawn(|| scan_fds("plain std::thread")).join().ok();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("pollbench") => {
            probe_pollbench();
        }
        Some("sleepbench") => {
            probe_sleepbench();
        }
        Some("termbench") => {
            probe_termbench();
        }
        Some("pipebench") => {
            println!("=== probe: pipe read/write cost ===");
            probe_pipebench();
        }
        Some("eofedge") => {
            println!("=== probe: EOF edge after a partial drain ===");
            probe_eofedge();
        }
        Some("ptyedge") => {
            println!("=== probe: pty EPOLLET edge for a second, later byte ===");
            probe_ptyedge();
        }
        Some("stdinedge") => {
            println!("=== probe: fd 0 (real Stdin exec-channel) EPOLLET edge ===");
            probe_stdinedge();
        }
        Some("bigspawn") => {
            let iterations = args
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(50usize);
            println!("=== probe: real rustc spawn x{iterations} (cargo EFAULT repro) ===");
            probe_bigspawn(iterations);
        }
        Some("bigspawn-threads") => {
            let threads = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4usize);
            let iters = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10usize);
            println!(
                "=== probe: real rustc spawn from {threads} concurrent threads x{iters} each ==="
            );
            probe_bigspawn_threads(threads, iters);
        }
        Some("waitid") => {
            println!("=== probe: waitid(P_PIDFD) — tokio's reaping call ===");
            probe_waitid();
        }
        Some("timeoutleak") => {
            let fixed = args.iter().any(|a| a == "--fixed");
            probe_timeoutleak(fixed);
        }
        Some("cross") => {
            println!("=== probe: epoll_wait on one thread, epoll_ctl(ADD) on another ===");
            probe_cross();
        }
        Some("fds") => {
            println!("=== probe: fd table / epoll instances ===");
            probe_fds();
        }
        Some("tokio") => {
            let blocking_tui = args.iter().any(|a| a == "--tui");
            let workers = args
                .iter()
                .position(|a| a == "--workers")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            println!("=== probe: tokio Command::output(){} ===",
                if blocking_tui { " with a TUI-like spawn_blocking hog" } else { "" });
            probe_tokio(blocking_tui, workers);
        }
        Some("epoll") => {
            let late = args.iter().any(|a| a == "--late");
            let on_thread = args.get(2).map(String::as_str) == Some("thread");
            println!("=== probe: epoll ({}){} ===",
                if on_thread { "on spawned thread" } else { "on main thread" },
                if late { " --late" } else { "" });
            if on_thread {
                std::thread::spawn(move || probe_epoll(late)).join().ok();
            } else {
                probe_epoll(late);
            }
        }
        Some("raw") => {
            let mode = args.get(2).map(String::as_str).unwrap_or("main");
            println!("=== probe: raw ({mode}) ===");
            probe_raw(mode);
        }
        _ => {
            println!("usage: ncaprobe epoll [main|thread] [--late] [--zero]");
            println!("       ncaprobe tokio|eofedge|ptyedge|cross|fds|waitid|pipebench [--epoll N]");
            println!("       ncaprobe raw [main|thread|split]");
        }
    }
}
