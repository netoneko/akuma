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
//! epoll [main|thread] [--late] [--zero]   raw pipe + spawn + epoll(ET) + pidfd
//! cross                            epoll_wait on one thread, epoll_ctl on another
//! fds                              open fds, which are epolls, and fd aliasing
//! waitid                           waitid(P_PIDFD, ...) — tokio's reaping call
//! raw [main|thread|split]          tcsetattr(raw) + read(0), same/different thread
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
        Some("pipebench") => {
            println!("=== probe: pipe read/write cost ===");
            probe_pipebench();
        }
        Some("eofedge") => {
            println!("=== probe: EOF edge after a partial drain ===");
            probe_eofedge();
        }
        Some("waitid") => {
            println!("=== probe: waitid(P_PIDFD) — tokio's reaping call ===");
            probe_waitid();
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
            println!("       ncaprobe tokio|eofedge|cross|fds|waitid|pipebench [--epoll N]");
            println!("       ncaprobe raw [main|thread|split]");
        }
    }
}
