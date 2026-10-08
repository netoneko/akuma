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

use crate::cdp::{find, num_field, str_field, Frames};

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
        // No GPU process at all. `--disable-gpu` alone still starts one for
        // SwiftShader/ANGLE, which fails here (no Vulkan surface extensions)
        // on every navigation; after a few failures the browser process
        // deliberately aborts ("GPU process isn't usable"), a SIGTRAP that
        // looked like slow starts, blank pages and wedged calls (2026-10-09,
        // ryzen). Software compositing also makes the screencast frames real.
        "--disable-gpu-compositing",
        "--disable-software-rasterizer",
        // Slimming for a headless kiosk with no profile worth syncing. Measured
        // 2026-10-09 (probe/flags_ab.py, 4 cold starts per arm): 3/4 survived
        // with the flags above alone, 4/4 and 4/4 with these; start-up time did
        // not change (10-14 s either way), so the stability gain is within
        // noise. They stay for the processes and background traffic they drop:
        // no crashpad handlers (their ptrace fails here anyway), no sync, no
        // component updater, no extensions.
        "--disable-background-networking",
        "--disable-sync",
        "--disable-extensions",
        "--disable-component-update",
        "--disable-default-apps",
        "--no-default-browser-check",
        "--disable-client-side-phishing-detection",
        "--disable-domain-reliability",
        "--disable-breakpad",
        "--disable-crash-reporter",
        "--disable-features=Translate,MediaRouter,OptimizationHints,BackForwardCache,AcceptCHFrame,InterestFeedContentSuggestions",
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

/// CDP sessions a client attached and has not detached. A client that dies
/// without detaching (a kill, or Ctrl-Q) would otherwise leave its screencast
/// running in Chromium for good, waiting on frame acks nobody sends, and the
/// next client's `Page.startScreencast` then never gets a reply (2026-10-09,
/// ryzen). The daemon owns the pipe, so it detaches them on the client's behalf.
#[derive(Default)]
struct Sessions {
    /// Ids of `Target.attachToTarget` requests still waiting for a reply.
    pending: Vec<u64>,
    live: Vec<String>,
    /// Ids for the daemon's own requests, far above any client's.
    next_id: u64,
}

impl Sessions {
    /// Look at a command on its way to Chromium.
    fn from_client(&mut self, m: &[u8]) {
        if find(m, b"\"Target.attachToTarget\"").is_some() {
            if let Some(id) = num_field(m, "id") {
                self.pending.push(id);
            }
        } else if find(m, b"\"Target.detachFromTarget\"").is_some() {
            if let Some(sid) = str_field(m, "sessionId") {
                self.live.retain(|s| s != sid);
            }
        }
    }

    /// Look at a message coming back from Chromium.
    fn from_chrome(&mut self, m: &[u8]) {
        if self.pending.is_empty() || !m.starts_with(b"{\"id\":") {
            return;
        }
        if let Some(id) = num_field(m, "id") {
            if let Some(i) = self.pending.iter().position(|&p| p == id) {
                self.pending.swap_remove(i);
                if let Some(sid) = str_field(m, "sessionId") {
                    self.live.push(sid.to_string());
                }
            }
        }
    }

    /// Detach everything the departed client left attached.
    fn detach_all(&mut self, to_chrome: &mut File) {
        self.pending.clear();
        for sid in self.live.drain(..) {
            self.next_id = self.next_id.max(1_000_000_000) + 1;
            let msg = format!(
                "{{\"id\":{},\"method\":\"Target.detachFromTarget\",\"params\":{{\"sessionId\":\"{sid}\"}}}}\0",
                self.next_id
            );
            eprintln!("[kami-daemon] detaching the departed client's session {sid}");
            let _ = to_chrome.write_all(msg.as_bytes());
        }
    }
}

pub fn run(c: &Config) -> io::Result<()> {
    let t0 = std::time::Instant::now();
    let mut first_msg = true;
    let (cmd_r, cmd_w) = pipe()?; // daemon -> chromium fd 3
    let (evt_r, evt_w) = pipe()?; // chromium fd 4 -> daemon
    let (a, b) = (cmd_r.as_raw_fd(), evt_w.as_raw_fd());
    let mut cmd = Command::new(&c.chromium);
    cmd.args(chromium_args(c))
        // There is no D-Bus on this system: Chromium would try the system bus
        // at startup and keep failing NameHasOwner calls on it, a few dozen
        // log lines per session. An address that cannot parse makes it give
        // up at once.
        .env("DBUS_SESSION_BUS_ADDRESS", "disabled:")
        .env("DBUS_SYSTEM_BUS_ADDRESS", "disabled:")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file(&c.log)?);
    // SAFETY: only async-signal-safe calls (fcntl, dup2) between fork and
    // exec. Both ends are first moved above 10 so that placing one on 3
    // cannot close the other if it happened to be 3 or 4.
    unsafe {
        cmd.pre_exec(move || {
            // Own process group, so the daemon can take the whole tree down
            // (zygotes, renderers, crashpad handlers) when the browser dies:
            // they otherwise outlive it as orphans.
            libc::setpgid(0, 0);
            let hi_a = libc::fcntl(a, libc::F_DUPFD, 10);
            let hi_b = libc::fcntl(b, libc::F_DUPFD, 10);
            if hi_a < 0 || hi_b < 0 || libc::dup2(hi_a, 3) < 0 || libc::dup2(hi_b, 4) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    eprintln!("[kami-daemon] +{} ms: chromium spawned", t0.elapsed().as_millis());
    drop((cmd_r, evt_w));
    let mut to_chrome = File::from(cmd_w);
    let mut from_chrome = File::from(evt_r);

    let _ = fs::remove_file(&c.sock);
    let listener = UnixListener::bind(&c.sock)?;
    eprintln!("[kami-daemon] +{} ms: chromium pid {} on {}", t0.elapsed().as_millis(), child.id(), c.sock);

    let mut chrome_msgs = Frames::default();
    let mut client_msgs = Frames::default();
    let mut client: Option<UnixStream> = None;
    let mut sessions = Sessions::default();

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
                sessions.from_chrome(&m);
                if first_msg {
                    first_msg = false;
                    eprintln!("[kami-daemon] +{} ms: first message from chromium ({} B)", t0.elapsed().as_millis(), m.len());
                }
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
                // Whoever was attached is gone as far as its sessions go.
                sessions.detach_all(&mut to_chrome);
                eprintln!("[kami-daemon] +{} ms: client attached", t0.elapsed().as_millis());
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
                        sessions.from_client(&m);
                        to_chrome.write_all(&m)?;
                        to_chrome.write_all(b"\0")?;
                    }
                }
                _ => {
                    client = None;
                    client_msgs.reset();
                    sessions.detach_all(&mut to_chrome);
                }
            }
        }
    }
    let _ = fs::remove_file(&c.sock);
    // SAFETY: signalling the group the child made for itself in pre_exec; a
    // pid that is already gone just makes this fail.
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn out_file(name: &str) -> (File, std::path::PathBuf) {
        let p = std::env::temp_dir().join(format!("kami-daemon-test-{name}-{}", std::process::id()));
        (File::create(&p).unwrap(), p)
    }

    #[test]
    fn departed_clients_sessions_are_detached() {
        let mut s = Sessions::default();
        s.from_client(br#"{"id":7,"method":"Target.attachToTarget","params":{"targetId":"T","flatten":true}}"#);
        s.from_client(br#"{"id":8,"method":"Page.enable","params":{}}"#);
        s.from_chrome(br#"{"id":8,"result":{}}"#); // not an attach: ignored
        s.from_chrome(br#"{"id":7,"result":{"sessionId":"SESS1"}}"#);
        assert_eq!(s.live, ["SESS1"]);
        let (mut f, path) = out_file("a");
        s.detach_all(&mut f);
        let sent = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(sent.contains(r#""method":"Target.detachFromTarget""#) && sent.contains("SESS1"));
        assert!(sent.ends_with('\0'));
        assert!(s.live.is_empty());
    }

    #[test]
    fn a_session_the_client_detached_itself_is_left_alone() {
        let mut s = Sessions::default();
        s.from_client(br#"{"id":1,"method":"Target.attachToTarget","params":{}}"#);
        s.from_chrome(br#"{"id":1,"result":{"sessionId":"S"}}"#);
        s.from_client(br#"{"id":2,"method":"Target.detachFromTarget","params":{"sessionId":"S"}}"#);
        let (mut f, path) = out_file("b");
        s.detach_all(&mut f);
        let sent = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(sent.is_empty());
    }
}
