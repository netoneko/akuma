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
        // Few processes. Site isolation forks a renderer per cross-site iframe:
        // tumblr.com made ~40 in 100 s (2026-10-09, ryzen), which exhausted the
        // kernel's pipe cap (ENFILE, the zygote's CHECK) and cost every fork's
        // page-table copy besides. A kiosk reading pages has no use for the
        // isolation, so cap the renderers and fold the iframes into them.
        "--disable-site-isolation-trials",
        "--renderer-process-limit=4",
        "--disable-features=Translate,MediaRouter,OptimizationHints,BackForwardCache,AcceptCHFrame,InterestFeedContentSuggestions,IsolateOrigins,site-per-process",
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

/// Where Chromium keeps its profile, and so its `Singleton*` lock files.
const PROFILE: &str = "/tmp/kami-profile";
/// A crash loop gives up: more than this many restarts inside the window.
const MAX_RESTARTS: usize = 5;
const RESTART_WINDOW_MS: u64 = 60_000;

/// Restart policy: Chromium may be restarted [`MAX_RESTARTS`] times in a
/// sliding [`RESTART_WINDOW_MS`]; past that it is crash-looping and the daemon
/// stops, rather than burn the box respawning something that cannot start.
#[derive(Default)]
struct Restarts {
    at: Vec<u64>,
}

impl Restarts {
    fn allow(&mut self, now_ms: u64) -> bool {
        self.at.retain(|&t| now_ms.saturating_sub(t) < RESTART_WINDOW_MS);
        if self.at.len() >= MAX_RESTARTS {
            return false;
        }
        self.at.push(now_ms);
        true
    }
}

/// What a client can ask of the daemon itself (everything else is relayed to
/// Chromium). They travel as CDP-shaped requests so one socket carries both.
#[derive(Debug, PartialEq, Eq)]
enum Control {
    /// `Kami.hello`: who are you? (answer: pids, generation, restarts)
    Hello(u64),
    /// `Kami.restart`: Chromium is wedged, kill it and start another.
    Restart(u64),
    /// `Kami.shutdown`: stop Chromium and exit.
    Shutdown(u64),
}

fn control(m: &[u8]) -> Option<Control> {
    if !m.starts_with(b"{\"id\":") {
        return None;
    }
    let id = num_field(m, "id")?;
    match str_field(m, "method")? {
        "Kami.hello" => Some(Control::Hello(id)),
        "Kami.restart" => Some(Control::Restart(id)),
        "Kami.shutdown" => Some(Control::Shutdown(id)),
        _ => None,
    }
}

fn hello_reply(id: u64, daemon: u32, chromium: u32, generation: u32, restarts: usize, up_ms: u128) -> String {
    format!(
        "{{\"id\":{id},\"result\":{{\"daemon\":{daemon},\"chromium\":{chromium},\"generation\":{generation},\"restarts\":{restarts},\"upMs\":{up_ms}}}}}"
    )
}

/// Tell the client its Chromium is a new one: every session, tab and request
/// it had is gone, and it should attach again.
fn restarted_event(generation: u32, reason: &str) -> String {
    let reason: String = reason.chars().filter(|c| *c != '"' && *c != '\\' && !c.is_control()).collect();
    format!("{{\"method\":\"Kami.chromiumRestarted\",\"params\":{{\"generation\":{generation},\"reason\":\"{reason}\"}}}}")
}

struct Chrome {
    child: std::process::Child,
    to: File,
    from: File,
}

fn spawn_chromium(c: &Config) -> io::Result<Chrome> {
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
    // SAFETY: only async-signal-safe calls (setpgid, fcntl, dup2) between fork
    // and exec. Both ends are first moved above 10 so that placing one on 3
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
    let child = cmd.spawn()?;
    drop((cmd_r, evt_w));
    Ok(Chrome { child, to: File::from(cmd_w), from: File::from(evt_r) })
}

/// Is anything still alive in this process group?
fn group_alive(pgid: u32) -> bool {
    // SAFETY: signal 0 only checks that the group exists.
    unsafe { libc::kill(-(pgid as i32), 0) == 0 }
}

/// Take a Chromium's whole process group down, gently: SIGTERM, up to 3 s for
/// it to empty (its helpers exit on their own once the browser is gone), then
/// SIGKILL whatever is left. A single SIGKILL to ~100 threads at once was
/// followed by a kernel hang on the ryzen box twice on 2026-10-09 (a stuck BKL
/// hold in `klog`: `[BKL] stuck: ... tag=501`); whether the gentler order
/// avoids it is not established.
fn kill_group(pgid: u32) {
    let sig = |s: i32| {
        // SAFETY: signalling the group the child made for itself in pre_exec; a
        // group that is already gone just makes this fail.
        unsafe { libc::kill(-(pgid as i32), s) };
    };
    sig(libc::SIGTERM);
    for _ in 0..30 {
        if !group_alive(pgid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    sig(libc::SIGKILL);
}

fn pid_file(c: &Config) -> String {
    format!("{}.pid", c.sock)
}

/// The previous daemon is dead (nothing answered on the socket), but a crash
/// or a kill can have left its Chromium tree and the profile's lock files
/// behind; either makes the next Chromium hand its work to a ghost or refuse
/// to start. The pid file names the old group.
fn reap_previous(c: &Config) {
    if let Ok(text) = fs::read_to_string(pid_file(c)) {
        if let Some(pgid) = text.split_whitespace().nth(1).and_then(|p| p.parse::<u32>().ok()) {
            if pgid > 1 {
                eprintln!("[kami-daemon] reaping the previous Chromium group {pgid}");
                kill_group(pgid);
            }
        }
    }
    for f in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = fs::remove_file(format!("{PROFILE}/{f}"));
    }
}

pub fn run(c: &Config) -> io::Result<()> {
    // One daemon per socket: if something answers, it is serving.
    if UnixStream::connect(&c.sock).is_ok() {
        eprintln!("[kami-daemon] another daemon already serves {}; exiting", c.sock);
        return Ok(());
    }
    reap_previous(c);
    let t0 = std::time::Instant::now();
    let ms = |t: std::time::Instant| t.elapsed().as_millis();
    let mut first_msg = true;
    let _ = fs::remove_file(&c.sock);
    let listener = UnixListener::bind(&c.sock)?;
    let mut chrome = spawn_chromium(c)?;
    let mut gen: u32 = 1;
    let write_pid = |chrome: &Chrome| {
        let _ = fs::write(pid_file(c), format!("{} {}\n", std::process::id(), chrome.child.id()));
    };
    write_pid(&chrome);
    eprintln!("[kami-daemon] +{} ms: chromium spawned", ms(t0));
    eprintln!("[kami-daemon] +{} ms: chromium pid {} on {}", ms(t0), chrome.child.id(), c.sock);

    let mut chrome_msgs = Frames::default();
    let mut client_msgs = Frames::default();
    let mut client: Option<UnixStream> = None;
    let mut sessions = Sessions::default();
    let mut restarts = Restarts::default();
    let mut quit = false;

    while !quit {
        let mut fds = [
            libc::pollfd { fd: chrome.from.as_raw_fd(), events: libc::POLLIN, revents: 0 },
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
        let mut restart: Option<String> = None;
        if let Ok(Some(status)) = chrome.child.try_wait() {
            restart = Some(format!("chromium exited: {status}"));
        } else if fds[0].revents != 0 {
            match chrome_msgs.read_from(&mut chrome.from) {
                Ok(0) | Err(_) => restart = Some("chromium closed its pipe".into()),
                Ok(_) => {
                    // Whole messages only, so a client that connects
                    // mid-stream never sees half of one. With no client
                    // attached, they are dropped.
                    while let Some(m) = chrome_msgs.next() {
                        sessions.from_chrome(&m);
                        if first_msg {
                            first_msg = false;
                            eprintln!("[kami-daemon] +{} ms: first message from chromium ({} B)", ms(t0), m.len());
                        }
                        if let Some(s) = client.as_mut() {
                            if s.write_all(&m).and_then(|_| s.write_all(b"\0")).is_err() {
                                client = None;
                            }
                        }
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
                sessions.detach_all(&mut chrome.to);
                eprintln!("[kami-daemon] +{} ms: client attached", ms(t0));
                client = Some(s);
                client_msgs.reset();
            }
        }
        // `fds[2]` was polled before this pass touched `client`: the Chromium
        // branch drops it on a failed write (the client had just exited) and
        // the accept branch replaces it. Handle the revents only if the fd
        // polled is still the attached client's, else they describe a socket
        // that is gone and the next poll reports the real one (2026-10-09: an
        // `expect` here panicked the daemon at the end of a page_try run).
        let polled_is_client = client.as_ref().is_some_and(|s| s.as_raw_fd() == fds[2].fd);
        if fds[2].revents != 0 && restart.is_none() && polled_is_client {
            let s = client.as_mut().expect("checked just above");
            match client_msgs.read_from(s) {
                Ok(n) if n > 0 => {
                    // Forward whole commands only: a client dying mid-write
                    // must not leave half a message in Chromium's pipe.
                    while let Some(m) = client_msgs.next() {
                        let reply = |client: &mut Option<UnixStream>, text: String| {
                            if let Some(s) = client.as_mut() {
                                if s.write_all(text.as_bytes()).and_then(|_| s.write_all(b"\0")).is_err() {
                                    *client = None;
                                }
                            }
                        };
                        match control(&m) {
                            Some(Control::Hello(id)) => {
                                let r = hello_reply(id, std::process::id(), chrome.child.id(), gen, restarts.at.len(), ms(t0));
                                reply(&mut client, r);
                            }
                            Some(Control::Restart(id)) => {
                                reply(&mut client, format!("{{\"id\":{id},\"result\":{{}}}}"));
                                restart = Some("the client asked for a restart".into());
                            }
                            Some(Control::Shutdown(id)) => {
                                reply(&mut client, format!("{{\"id\":{id},\"result\":{{}}}}"));
                                quit = true;
                            }
                            None => {
                                sessions.from_client(&m);
                                if chrome.to.write_all(&m).and_then(|_| chrome.to.write_all(b"\0")).is_err() {
                                    restart = Some("writing to chromium failed".into());
                                }
                            }
                        }
                    }
                }
                _ => {
                    client = None;
                    client_msgs.reset();
                    sessions.detach_all(&mut chrome.to);
                }
            }
        }
        if quit {
            break;
        }
        if let Some(reason) = restart {
            eprintln!("[kami-daemon] +{} ms: {reason}", ms(t0));
            kill_group(chrome.child.id());
            let _ = chrome.child.wait();
            if !restarts.allow(t0.elapsed().as_millis() as u64) {
                eprintln!("[kami-daemon] chromium restarted {MAX_RESTARTS} times in a minute; giving up");
                break;
            }
            chrome = spawn_chromium(c)?;
            gen += 1;
            write_pid(&chrome);
            sessions = Sessions::default();
            chrome_msgs.reset();
            first_msg = true;
            eprintln!("[kami-daemon] +{} ms: chromium restarted as generation {gen}, pid {}", ms(t0), chrome.child.id());
            if let Some(s) = client.as_mut() {
                let ev = restarted_event(gen, &reason);
                if s.write_all(ev.as_bytes()).and_then(|_| s.write_all(b"\0")).is_err() {
                    client = None;
                }
            }
        }
    }
    let _ = fs::remove_file(&c.sock);
    let _ = fs::remove_file(pid_file(c));
    kill_group(chrome.child.id());
    let _ = chrome.child.kill();
    let _ = chrome.child.wait();
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

    #[test]
    fn control_messages_are_recognised_and_everything_else_is_relayed() {
        assert_eq!(control(br#"{"id":4,"method":"Kami.hello","params":{}}"#), Some(Control::Hello(4)));
        assert_eq!(control(br#"{"id":5,"method":"Kami.restart","params":{}}"#), Some(Control::Restart(5)));
        assert_eq!(control(br#"{"id":6,"method":"Kami.shutdown","params":{}}"#), Some(Control::Shutdown(6)));
        assert_eq!(control(br#"{"id":7,"method":"Page.enable","params":{},"sessionId":"S"}"#), None);
        assert_eq!(control(br#"{"method":"Kami.hello"}"#), None, "no id, not a request");
    }

    #[test]
    fn a_crash_loop_is_given_up_on_but_a_slow_trickle_is_not() {
        let mut r = Restarts::default();
        for i in 0..MAX_RESTARTS as u64 {
            assert!(r.allow(i * 1_000), "restart {i} is within the allowance");
        }
        assert!(!r.allow(6_000), "the sixth inside a minute is a crash loop");
        // A minute after the first, the window has moved on.
        assert!(r.allow(RESTART_WINDOW_MS + 500));
    }

    #[test]
    fn replies_and_events_are_well_formed() {
        let h = hello_reply(9, 100, 200, 3, 2, 4500);
        assert_eq!(num_field(h.as_bytes(), "id"), Some(9));
        assert_eq!(num_field(h.as_bytes(), "generation"), Some(3));
        assert_eq!(num_field(h.as_bytes(), "chromium"), Some(200));
        let e = restarted_event(2, "chromium exited: signal: 11 (SIGSEGV)\n\"quoted\"");
        assert!(e.starts_with("{\"method\":\"Kami.chromiumRestarted\""));
        assert_eq!(num_field(e.as_bytes(), "generation"), Some(2));
        assert!(!e[1..e.len() - 1].contains('\n'), "no raw control characters in the JSON");
    }
}
