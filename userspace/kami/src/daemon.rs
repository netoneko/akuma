//! `kami --daemon`: owns Chromium so that it outlives any one session.
//!
//! Chromium's CDP pipe (`--remote-debugging-pipe`, fds 3/4) dies with the
//! process that holds it, so the daemon holds it and relays whole messages
//! between it and one client on a unix socket. A new client replaces the
//! old one; Chromium, its tabs and their state carry on across sessions.
//! The daemon exits when Chromium does (`kami --kill` asks it to).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use crate::cdp::Frames;

pub struct Config {
    pub sock: String,
    pub chromium: String,
    pub log: String,
    pub width: usize,
    pub height: usize,
    pub extra: Vec<String>,
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: pipe writes two fds into the array; on success we own both.
    // (Plain pipe + FD_CLOEXEC rather than pipe2 so host tests build on macOS.)
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both fds are freshly created and owned by nothing else.
    let ends = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in fds {
        // SAFETY: fd is open (owned by `ends`).
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(ends)
}

pub fn chromium_args(c: &Config) -> Vec<String> {
    let mut a: Vec<String> = [
        "--headless",
        "--no-sandbox",
        "--disable-gpu",
        "--remote-debugging-pipe",
        "--hide-scrollbars",
        "--mute-audio",
        "--no-first-run",
        "--disable-dev-shm-usage",
        "--user-data-dir=/tmp/kami-profile",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(format!("--window-size={},{}", c.width, c.height));
    a.extend(c.extra.iter().cloned());
    a.push("about:blank".into());
    a
}

fn log_file(path: &str) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

pub fn run(c: &Config) -> io::Result<()> {
    let (cmd_r, cmd_w) = pipe()?; // daemon -> chromium fd 3
    let (evt_r, evt_w) = pipe()?; // chromium fd 4 -> daemon
    let (a, b) = (cmd_r.as_raw_fd(), evt_w.as_raw_fd());
    let mut cmd = Command::new(&c.chromium);
    cmd.args(chromium_args(c))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file(&c.log)?);
    // SAFETY: only async-signal-safe calls (fcntl, dup2) between fork and
    // exec. Both ends are first moved above 10 so that placing one on 3
    // cannot close the other if it happened to be 3 or 4.
    unsafe {
        cmd.pre_exec(move || {
            let hi_a = libc::fcntl(a, libc::F_DUPFD, 10);
            let hi_b = libc::fcntl(b, libc::F_DUPFD, 10);
            if hi_a < 0 || hi_b < 0 || libc::dup2(hi_a, 3) < 0 || libc::dup2(hi_b, 4) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop((cmd_r, evt_w));
    let mut to_chrome = File::from(cmd_w);
    let mut from_chrome = File::from(evt_r);

    let _ = fs::remove_file(&c.sock);
    let listener = UnixListener::bind(&c.sock)?;
    eprintln!("[kami-daemon] chromium pid {} on {}", child.id(), c.sock);

    let mut chrome_msgs = Frames::default();
    let mut client_msgs = Frames::default();
    let mut client: Option<UnixStream> = None;

    loop {
        let mut fds = [
            libc::pollfd { fd: from_chrome.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd {
                fd: client.as_ref().map_or(-1, |s| s.as_raw_fd()),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: three pollfd entries; a negative fd is ignored by poll.
        if unsafe { libc::poll(fds.as_mut_ptr(), 3, 1000) } < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if let Ok(Some(status)) = child.try_wait() {
            eprintln!("[kami-daemon] chromium exited: {status}");
            break;
        }
        if fds[0].revents != 0 {
            if chrome_msgs.read_from(&mut from_chrome)? == 0 {
                eprintln!("[kami-daemon] chromium closed its pipe");
                break;
            }
            // Whole messages only, so a client that connects mid-stream never
            // sees half of one. With no client attached, they are dropped.
            while let Some(m) = chrome_msgs.next() {
                if let Some(s) = client.as_mut() {
                    if s.write_all(&m).and_then(|_| s.write_all(b"\0")).is_err() {
                        client = None;
                    }
                }
            }
        }
        if fds[1].revents != 0 {
            if let Ok((s, _)) = listener.accept() {
                if client.is_some() {
                    eprintln!("[kami-daemon] new client replaces the attached one");
                }
                client = Some(s);
                client_msgs.reset();
            }
        }
        if fds[2].revents != 0 {
            let s = client.as_mut().expect("polled fd belongs to the client");
            match client_msgs.read_from(s) {
                Ok(n) if n > 0 => {
                    // Forward whole commands only: a client dying mid-write
                    // must not leave half a message in Chromium's pipe.
                    while let Some(m) = client_msgs.next() {
                        to_chrome.write_all(&m)?;
                        to_chrome.write_all(b"\0")?;
                    }
                }
                _ => {
                    client = None;
                    client_msgs.reset();
                }
            }
        }
    }
    let _ = fs::remove_file(&c.sock);
    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

/// Start `kami --daemon` detached from this session (own session, no
/// controlling tty, stdio on /dev/null and the log), so closing the
/// terminal or ssh connection does not take Chromium with it.
pub fn spawn_detached(c: &Config) -> io::Result<()> {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::env::args().next().unwrap().into());
    let mut cmd = Command::new(exe);
    cmd.arg("--daemon")
        .args(["--sock", &c.sock, "--chromium", &c.chromium, "--log", &c.log])
        .args(["--size", &format!("{}x{}", c.width, c.height)]);
    for e in &c.extra {
        cmd.args(["--chrome-arg", e]);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(log_file(&c.log)?);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().map(|_| ())
}
