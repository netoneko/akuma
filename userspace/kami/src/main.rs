//! kami (紙, paper) — a headless Chromium on the Linux framebuffer.
//!
//! A background daemon owns Chromium (see `daemon.rs`); each `kami` run is a
//! session that attaches to its tab, starts a PNG screencast at
//! `screen / scale` pixels and blits each frame `scale`x onto `/dev/fb0`. The
//! scale is chosen from the screen (at least a 1280x720 page) unless `--scale`.
//! Keys typed on the tty go back as CDP input events. Quitting detaches and
//! leaves Chromium and the page running for the next session.
//!
//!   kami [--scale N] [--fb PATH|none] [--chromium PATH] [--sock PATH] [--log PATH]
//!        [--frames N] [--seconds S] [--chrome-arg ARG]... [URL]
//!   kami --kill          stop the daemon and its Chromium
//!   kami tui [--page-fonts] [URL]   the page's layout painted into the terminal
//!                        (built with `--features tui`; see `tui/`)
//!
//! With no URL a fresh tab opens [`HOME`]; an existing tab stays where it is.
//!
//! Keys are modal, like vim (see `nav.rs`): in Normal mode `j`/`k`/`d`/`u`/
//! `gg`/`G` scroll, `H`/`L` go back/forward, `f` labels every clickable thing
//! with letters to type, `i` starts typing into the page (Esc stops). Arrows /
//! PgUp / PgDn / Home / End / Enter / Tab / Esc always pass through. Ctrl-R
//! reloads, Ctrl-C or Ctrl-Q detaches.

mod bar;
mod cdp;
mod daemon;
mod display;
mod fb;
mod machine;
mod nav;
mod png;

use std::io;
use std::time::Instant;

use display::Display;
use cdp::Cdp;

/// What a fresh tab opens when no URL is given. A reattach with no URL keeps
/// whatever the tab is showing.
const HOME: &str = "https://www.tumblr.com/";

struct Args {
    url: Option<String>,
    scale: usize,
    fb: String,
    chromium: String,
    sock: String,
    log: String,
    frames: Option<u64>,
    seconds: Option<f64>,
    extra: Vec<String>,
    daemon: bool,
    kill: bool,
    size: Option<(usize, usize)>,
    /// Poll `Page.captureScreenshot` every N ms instead of trusting the screencast.
    poll: Option<u64>,
    /// `kami tui`: paint the page's layout into the terminal instead of pixels.
    tui: bool,
    /// `kami tui --page-fonts`: keep the page's own fonts (no `cells.js`).
    page_fonts: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: kami [--scale N] [--fb PATH] [--chromium PATH] [--sock PATH] [--log PATH]\n\
         \x20           [--frames N] [--seconds S] [--poll MS] [--chrome-arg ARG]... [URL]\n\
         \x20      kami --kill\n\
         \x20      kami tui [--page-fonts] [URL]"
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut a = Args {
        url: None,
        scale: 0, // 0 = pick from the screen (display::auto_scale)
        fb: "/dev/fb0".into(),
        chromium: "chromium".into(),
        sock: "/tmp/kami.sock".into(),
        log: "/tmp/kami.log".into(),
        frames: None,
        seconds: None,
        extra: Vec::new(),
        daemon: false,
        kill: false,
        size: None,
        poll: None,
        tui: false,
        page_fonts: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--scale" => a.scale = val().parse().unwrap_or_else(|_| usage()),
            "--fb" => a.fb = val(),
            "--chromium" => a.chromium = val(),
            "--sock" => a.sock = val(),
            "--log" => a.log = val(),
            "--frames" => a.frames = Some(val().parse().unwrap_or_else(|_| usage())),
            "--seconds" => a.seconds = Some(val().parse().unwrap_or_else(|_| usage())),
            "--poll" => a.poll = Some(val().parse().unwrap_or_else(|_| usage())),
            "--chrome-arg" => a.extra.push(val()),
            "--page-fonts" => a.page_fonts = true,
            "--daemon" => a.daemon = true,
            "--kill" => a.kill = true,
            "--size" => {
                let v = val();
                let (w, h) = v.split_once('x').unwrap_or_else(|| usage());
                a.size = Some((w.parse().unwrap_or_else(|_| usage()), h.parse().unwrap_or_else(|_| usage())));
            }
            "-h" | "--help" => usage(),
            _ if arg.starts_with("--") => usage(),
            "tui" if !a.tui && a.url.is_none() => a.tui = true,
            _ => a.url = Some(arg),
        }
    }
    a
}

fn b64_decode(src: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    out.clear();
    out.reserve(src.len() / 4 * 3);
    let src = match src.iter().position(|&c| c == b'=') {
        Some(p) => &src[..p],
        None => src,
    };
    for chunk in src.chunks(4) {
        let mut n = 0u32;
        for &c in chunk {
            n = n << 6 | val(c).ok_or("bad base64")?;
        }
        match chunk.len() {
            4 => out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]),
            3 => out.extend_from_slice(&[(n >> 10) as u8, (n >> 2) as u8]),
            2 => out.push((n >> 4) as u8),
            _ => return Err("bad base64 length".into()),
        }
    }
    Ok(())
}

/// Every input kami sees goes to a log (`KAMI_INPUT_LOG`, default
/// `/tmp/kami-input.log`, appended): the tty's mode before and after raw mode,
/// each raw chunk read, what it decoded to and what that did. When keys "do
/// nothing" this says whether they reached kami at all (2026-10-08, ryzen:
/// Enter and Ctrl-Q seemed not to arrive).
static INPUT_LOG: std::sync::OnceLock<std::sync::Mutex<std::fs::File>> = std::sync::OnceLock::new();
static LOG_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn input_log_open() {
    let path = std::env::var_os("KAMI_INPUT_LOG").unwrap_or_else(|| "/tmp/kami-input.log".into());
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = INPUT_LOG.set(std::sync::Mutex::new(f));
    }
    let _ = LOG_START.set(Instant::now());
}

macro_rules! ilog {
    ($($arg:tt)*) => {
        if let Some(m) = $crate::INPUT_LOG.get() {
            if let Ok(mut f) = m.lock() {
                use std::io::Write;
                let t = $crate::LOG_START.get().map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
                let _ = writeln!(f, "[{t:9.3}] {}", format_args!($($arg)*));
            }
        }
    };
}

#[cfg(feature = "tui")]
mod tui;

/// `kami tui` has the terminal in its alternate screen with the cursor hidden.
/// The input pump's emergency quit exits from another thread, so it must put
/// the terminal back itself.
static TUI_SCREEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
const TUI_LEAVE: &str = "\x1b[?1049l\x1b[?25h";

fn log_termios(what: &str, t: &libc::termios) {
    ilog!(
        "termios {what}: iflag={:#o} oflag={:#o} lflag={:#o} cflag={:#o} vmin={} vtime={}",
        t.c_iflag, t.c_oflag, t.c_lflag, t.c_cflag, t.c_cc[libc::VMIN], t.c_cc[libc::VTIME]
    );
}

/// Raw mode on stdin for as long as this lives, if stdin is a tty.
struct RawTty(Option<libc::termios>);

impl RawTty {
    fn enter() -> RawTty {
        // SAFETY: termios is plain data; tcgetattr fills it or fails.
        unsafe {
            if libc::isatty(0) == 0 {
                return RawTty(None);
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) < 0 {
                return RawTty(None);
            }
            let saved = t;
            log_termios("before", &saved);
            libc::cfmakeraw(&mut t);
            let rc = libc::tcsetattr(0, libc::TCSANOW, &t);
            let mut back: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut back) == 0 {
                ilog!("tcsetattr rc={rc}; the tty now reads back as:");
                log_termios("after", &back);
            }
            RawTty(Some(saved))
        }
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            // SAFETY: restoring the attributes read in `enter`.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
    }
}

/// Forward tty input to a pipe the session polls, and quit from here on
/// Ctrl-C / Ctrl-Q. The session loop blocks inside CDP calls whenever Chromium
/// is slow or wedged (a blank page, a crashed network service), and while it
/// does it reads no keys: without this thread there is then no way out.
/// Exiting skips the screencast detach; the daemon drops a vanished client.
fn spawn_input_pump(saved: libc::termios) -> io::Result<i32> {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` has room for the two descriptors pipe() writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let (r, w) = (fds[0], fds[1]);
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        loop {
            // SAFETY: reading into a stack buffer of the stated size.
            let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                ilog!("tty read returned {n} ({}); input closed", io::Error::last_os_error());
                // SAFETY: closing our own write end; the reader sees EOF.
                unsafe { libc::close(w) };
                return;
            }
            ilog!("tty bytes [{}]", buf[..n as usize].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
            if buf[..n as usize].iter().any(|&b| b == 0x03 || b == 0x11) {
                ilog!("quit key");
                // SAFETY: restoring the attributes read before raw mode.
                unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
                if TUI_SCREEN.load(std::sync::atomic::Ordering::Relaxed) {
                    use std::io::Write;
                    let mut out = io::stdout();
                    let _ = out.write_all(TUI_LEAVE.as_bytes());
                    let _ = out.flush();
                }
                eprintln!("\r\n[kami] quit; chromium keeps running (kami --kill stops it)");
                std::process::exit(0);
            }
            // SAFETY: writing `n` bytes from the buffer just filled.
            unsafe { libc::write(w, buf.as_ptr() as *const _, n as usize) };
        }
    });
    Ok(r)
}

#[derive(Debug)]
enum Input {
    Key(&'static str, u32),
    Text(String),
    Reload,
    Quit,
}

/// Decode a chunk of tty bytes into input actions.
fn decode_keys(bytes: &[u8]) -> Vec<Input> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut text = String::new();
    let flush = |text: &mut String, out: &mut Vec<Input>| {
        if !text.is_empty() {
            out.push(Input::Text(std::mem::take(text)));
        }
    };
    while i < bytes.len() {
        let b = bytes[i];
        let key = match b {
            0x03 | 0x11 => Some(Input::Quit),
            0x12 => Some(Input::Reload),
            b'\r' | b'\n' => Some(Input::Key("Enter", 13)),
            0x7f | 0x08 => Some(Input::Key("Backspace", 8)),
            b'\t' => Some(Input::Key("Tab", 9)),
            0x1b => {
                let seq = &bytes[i + 1..];
                let (k, used) = match seq {
                    [b'[', b'A', ..] => (("ArrowUp", 38), 2),
                    [b'[', b'B', ..] => (("ArrowDown", 40), 2),
                    [b'[', b'C', ..] => (("ArrowRight", 39), 2),
                    [b'[', b'D', ..] => (("ArrowLeft", 37), 2),
                    [b'[', b'H', ..] => (("Home", 36), 2),
                    [b'[', b'F', ..] => (("End", 35), 2),
                    [b'[', b'5', b'~', ..] => (("PageUp", 33), 3),
                    [b'[', b'6', b'~', ..] => (("PageDown", 34), 3),
                    [b'[', b'1', b'~', ..] => (("Home", 36), 3),
                    [b'[', b'4', b'~', ..] => (("End", 35), 3),
                    [b'[', b'3', b'~', ..] => (("Delete", 46), 3),
                    _ => (("Escape", 27), 0),
                };
                i += used;
                Some(Input::Key(k.0, k.1))
            }
            _ => None,
        };
        match key {
            Some(k) => {
                flush(&mut text, &mut out);
                out.push(k);
                i += 1;
            }
            None => {
                // Printable UTF-8: take the whole sequence starting here.
                let len = match b {
                    0xf0..=0xff => 4,
                    0xe0..=0xef => 3,
                    0xc0..=0xdf => 2,
                    _ => 1,
                };
                let end = (i + len).min(bytes.len());
                if b >= 0x20 {
                    text.push_str(&String::from_utf8_lossy(&bytes[i..end]));
                }
                i = end;
            }
        }
    }
    flush(&mut text, &mut out);
    out
}

fn main() {
    let args = parse_args();
    let r = if args.daemon {
        let (width, height) = args.size.unwrap_or((1920, 1080));
        daemon::run(&daemon_config(&args, width, height))
    } else if args.kill {
        kill(&args)
    } else if args.tui {
        tui_session(&args)
    } else {
        session(&args)
    };
    if let Err(e) = r {
        eprintln!("[kami] {e}");
        std::process::exit(1);
    }
}

#[cfg(feature = "tui")]
fn tui_session(args: &Args) -> io::Result<()> {
    tui::run(args)
}

#[cfg(not(feature = "tui"))]
fn tui_session(_: &Args) -> io::Result<()> {
    Err(io::Error::other("this kami was built without `kami tui` (cargo build --features tui)"))
}

fn daemon_config(args: &Args, width: usize, height: usize) -> daemon::Config {
    daemon::Config {
        sock: args.sock.clone(),
        chromium: args.chromium.clone(),
        log: args.log.clone(),
        width,
        height,
        extra: args.extra.clone(),
    }
}

fn kill(args: &Args) -> io::Result<()> {
    let mut c = Cdp::connect(&args.sock)?;
    // The daemon stops Chromium's whole process group and exits. An older
    // daemon does not know the request (Chromium answers with an error), so
    // fall back to asking Chromium to close.
    if c.call("Kami.shutdown", "{}", false).is_err() {
        c.call("Browser.close", "{}", false)?;
    }
    let _ = std::fs::remove_file(format!("{}.target", args.sock));
    eprintln!("[kami] chromium closed");
    Ok(())
}

/// Decode one base64 PNG and put it on the display. Returns `(ok, empty)`:
/// an empty frame (fully transparent, as Akuma's screencast can send) is
/// reported but not shown, so it cannot paint the screen white.
fn present(fb: &mut dyn Display, png: &mut png::Decoder, bytes: &mut Vec<u8>, b64: &str, scale: usize) -> (bool, bool) {
    let td = Instant::now();
    if let Err(e) = b64_decode(b64.as_bytes(), bytes).and_then(|_| png.decode(bytes)) {
        ilog!("frame dropped: {e}");
        return (false, false);
    }
    let tb = Instant::now();
    let empty = png.channels == 4 && (0..png.pixels.len() / 4).step_by(997).all(|i| png.pixels[i * 4 + 3] == 0);
    if !empty {
        fb.blit(&png.pixels, png.width, png.height, png.channels, scale);
        // Debug: KAMI_DUMP=<path> keeps the latest shown frame's PNG.
        if let Some(path) = std::env::var_os("KAMI_DUMP") {
            let _ = std::fs::write(path, &*bytes);
        }
    }
    ilog!(
        "presented {}x{} ({} KB){}: decode {} ms, blit {} ms",
        png.width,
        png.height,
        bytes.len() / 1024,
        if empty { ", EMPTY" } else { "" },
        (tb - td).as_millis(),
        tb.elapsed().as_millis()
    );
    (true, empty)
}

/// The tab an earlier kami pinned, if any.
fn pinned_target(args: &Args) -> Option<String> {
    std::fs::read_to_string(format!("{}.target", args.sock)).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// The I/O shell around [`machine::Machine`]: poll the daemon socket and the
/// tty, turn what arrives into events, and do what the machine answers with.
/// It holds no session state of its own and never waits on Chromium.
fn session(args: &Args) -> io::Result<()> {
    use machine::{Effect, Event};

    input_log_open();
    ilog!("---- session start, pid {} ----", std::process::id());
    let mut fb: Box<dyn Display> = if args.fb == "none" {
        let (w, h) = args.size.unwrap_or((1920, 1176));
        Box::new(display::Null { size: (w, h), last: String::new() })
    } else {
        Box::new(fb::Fb::open(&args.fb)?)
    };
    let scale = if args.scale == 0 { display::auto_scale(fb.screen_size()) } else { args.scale };
    let view = fb.page_size(scale);
    ilog!("startup: display opened (screen {:?}, scale {scale}, page {}x{})", fb.screen_size(), view.0, view.1);
    let mut m = machine::Machine::new(machine::Config {
        url: args.url.clone(),
        home: HOME.into(),
        view,
        poll_ms: args.poll,
        max_frames: args.frames,
        seconds: args.seconds,
        pinned: pinned_target(args),
        output: machine::Output::Pixels,
        cell_fonts: false,
    });

    let tty = RawTty::enter();
    let mut stdin_open = tty.0.is_some();
    let input_fd = match tty.0 {
        Some(saved) => spawn_input_pump(saved)?,
        None => 0,
    };
    let t0 = Instant::now();
    let mut c: Option<Cdp> = None;
    let mut png = png::Decoder::default();
    let mut bytes = Vec::new();

    loop {
        let mut queue = gather(&mut c, input_fd, &mut stdin_open, t0)?;
        while let Some(ev) = queue.pop_front() {
            for eff in m.handle(ev) {
                let Some(eff) = common_effect(eff, &mut c, &mut queue, args, view) else { continue };
                match eff {
                    Effect::Present { b64, source } => {
                        let (ok, empty) = present(&mut *fb, &mut png, &mut bytes, &b64, scale);
                        queue.push_back(Event::Presented { source, ok, empty });
                    }
                    Effect::Status(t) => fb.status(&t),
                    Effect::Done(err) => {
                        ilog!("session done: {err:?}");
                        drop(tty);
                        eprintln!("[kami] detached; chromium keeps running (kami --kill stops it)");
                        return match err {
                            None => Ok(()),
                            Some(e) => Err(io::Error::other(e)),
                        };
                    }
                    // Layout mode only (`kami tui`).
                    Effect::Layout { .. } | Effect::Scroll(_) => {}
                    Effect::TryConnect { .. } | Effect::Send(_) | Effect::Pin(_) | Effect::Log(_) => {}
                }
            }
        }
    }
}

/// One turn of a session loop's input side: wait up to 100 ms for the daemon
/// socket or the tty, and turn what arrived into events, the clock first.
fn gather(
    c: &mut Option<Cdp>,
    input_fd: i32,
    stdin_open: &mut bool,
    t0: Instant,
) -> io::Result<std::collections::VecDeque<machine::Event>> {
    use machine::Event;
    let mut queue = std::collections::VecDeque::new();
    let mut fds = [
        libc::pollfd { fd: c.as_ref().map_or(-1, |c| c.fd()), events: libc::POLLIN, revents: 0 },
        // A negative fd is skipped by poll; `events: 0` is not enough, since
        // POLLHUP is always reported and a closed input would spin the loop.
        libc::pollfd { fd: if *stdin_open { input_fd } else { -1 }, events: libc::POLLIN, revents: 0 },
    ];
    // SAFETY: two valid pollfd entries.
    if unsafe { libc::poll(fds.as_mut_ptr(), 2, 100) } < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(queue);
        }
        return Err(e);
    }
    queue.push_back(Event::Tick(t0.elapsed().as_millis() as u64));
    if fds[0].revents != 0 {
        if let Some(cc) = c.as_mut() {
            match cc.fill() {
                Ok(true) => {
                    while let Some(msg) = cc.next() {
                        queue.push_back(Event::Cdp(msg));
                    }
                }
                _ => {
                    *c = None;
                    queue.push_back(Event::DaemonGone);
                }
            }
        }
    }
    if fds[1].revents != 0 {
        let mut buf = [0u8; 256];
        // SAFETY: reading into a stack buffer of the stated size.
        let n = unsafe { libc::read(input_fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n <= 0 {
            *stdin_open = false;
            queue.push_back(Event::InputClosed);
        } else {
            ilog!("tty chunk [{}]", buf[..n as usize].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
            for k in decode_keys(&buf[..n as usize]) {
                queue.push_back(Event::Key(k));
            }
        }
    }
    Ok(queue)
}

/// Perform the effects every front end performs the same way (the daemon
/// connection, CDP writes, the tab pin, the log); hand back the rest.
/// `view` is the page viewport, for a daemon started from here.
fn common_effect(
    eff: machine::Effect,
    c: &mut Option<Cdp>,
    queue: &mut std::collections::VecDeque<machine::Event>,
    args: &Args,
    view: (usize, usize),
) -> Option<machine::Effect> {
    use machine::{Effect, Event};
    match eff {
        Effect::TryConnect { spawn } => {
            if spawn {
                if let Err(e) = daemon::spawn_detached(&daemon_config(args, view.0, view.1)) {
                    ilog!("could not spawn the daemon: {e}");
                }
            }
            match Cdp::connect(&args.sock) {
                Ok(x) => {
                    *c = Some(x);
                    queue.push_back(Event::Connected);
                }
                Err(_) => queue.push_back(Event::ConnectFailed),
            }
        }
        Effect::Send(msg) => {
            if let Some(cc) = c.as_mut() {
                if let Err(e) = cc.send_raw(&msg) {
                    ilog!("send failed: {e}");
                    *c = None;
                    queue.push_back(Event::DaemonGone);
                }
            }
        }
        Effect::Pin(target) => {
            let _ = std::fs::write(format!("{}.target", args.sock), target);
        }
        Effect::Log(t) => ilog!("{t}"),
        other => return Some(other),
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::{event_session, first_page_with, method, num_field, str_field};

    #[test]
    fn base64_round_trip() {
        let mut out = Vec::new();
        b64_decode(b"aGVsbG8gd29ybGQ=", &mut out).unwrap();
        assert_eq!(out, b"hello world");
        b64_decode(b"YWI=", &mut out).unwrap();
        assert_eq!(out, b"ab");
        b64_decode(b"YWJj", &mut out).unwrap();
        assert_eq!(out, b"abc");
    }

    #[test]
    fn fields() {
        let m = br#"{"method":"Page.screencastFrame","params":{"data":"QUJD","metadata":{},"sessionId":7},"sessionId":"ABCD"}"#;
        assert_eq!(method(m), Some("Page.screencastFrame"));
        assert_eq!(str_field(m, "data"), Some("QUJD"));
        assert_eq!(num_field(m, "sessionId"), Some(7));
        assert_eq!(str_field(m, "sessionId"), Some("ABCD"));
        assert_eq!(method(br#"{"id":3,"result":{}}"#), None);
        assert_eq!(event_session(m), Some("ABCD"));
        let t = br#"{"id":1,"result":{"targetInfos":[{"targetId":"B1","type":"browser"},{"targetId":"P1","type":"page","url":"about:blank"}]}}"#;
        assert_eq!(first_page_with(t, None).as_deref(), Some("P1"));
        assert_eq!(first_page_with(br#"{"id":1,"result":{"targetInfos":[]}}"#, None), None);
    }

    #[test]
    fn keys() {
        let k = decode_keys(b"ab\x1b[B\r\x1b[6~\x03");
        assert!(matches!(&k[0], Input::Text(t) if t == "ab"));
        assert!(matches!(k[1], Input::Key("ArrowDown", 40)));
        assert!(matches!(k[2], Input::Key("Enter", 13)));
        assert!(matches!(k[3], Input::Key("PageDown", 34)));
        assert!(matches!(k[4], Input::Quit));
        assert!(matches!(decode_keys(b"\x1b")[0], Input::Key("Escape", 27)));
    }
}
