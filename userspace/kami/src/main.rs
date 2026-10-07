//! kami (紙, paper) — a headless Chromium on the Linux framebuffer.
//!
//! A background daemon owns Chromium (see `daemon.rs`); each `kami` run is a
//! session that attaches to its tab, starts a PNG screencast at
//! `screen / scale` pixels and blits each frame `scale`x onto `/dev/fb0`.
//! Keys typed on the tty go back as CDP input events. Quitting detaches and
//! leaves Chromium and the page running for the next session.
//!
//!   kami [--scale N] [--fb PATH] [--chromium PATH] [--sock PATH] [--log PATH]
//!        [--frames N] [--seconds S] [--chrome-arg ARG]... [URL]
//!   kami --kill          stop the daemon and its Chromium
//!
//! Keys: arrows / PgUp / PgDn / Home / End scroll, Enter / Backspace / Tab /
//! Esc pass through, other text is typed, Ctrl-R reloads, Ctrl-C or Ctrl-Q
//! detaches.

mod cdp;
mod daemon;
mod fb;
mod png;

use std::io;
use std::time::{Duration, Instant};

use cdp::{escape, event_session, first_page, method, num_field, str_field, Cdp};

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
}

fn usage() -> ! {
    eprintln!(
        "usage: kami [--scale N] [--fb PATH] [--chromium PATH] [--sock PATH] [--log PATH]\n\
         \x20           [--frames N] [--seconds S] [--chrome-arg ARG]... [URL]\n\
         \x20      kami --kill"
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut a = Args {
        url: None,
        scale: 2,
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
            "--chrome-arg" => a.extra.push(val()),
            "--daemon" => a.daemon = true,
            "--kill" => a.kill = true,
            "--size" => {
                let v = val();
                let (w, h) = v.split_once('x').unwrap_or_else(|| usage());
                a.size = Some((w.parse().unwrap_or_else(|_| usage()), h.parse().unwrap_or_else(|_| usage())));
            }
            "-h" | "--help" => usage(),
            _ if arg.starts_with("--") => usage(),
            _ => a.url = Some(arg),
        }
    }
    if a.scale == 0 {
        usage();
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
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
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

fn send_key(c: &mut Cdp, key: &str, code: u32) -> io::Result<()> {
    let text = match key {
        "Enter" => ",\"text\":\"\\r\"",
        _ => "",
    };
    let kind = if text.is_empty() { "rawKeyDown" } else { "keyDown" };
    c.send(
        "Input.dispatchKeyEvent",
        &format!("{{\"type\":\"{kind}\",\"key\":\"{key}\",\"code\":\"{key}\",\"windowsVirtualKeyCode\":{code}{text}}}"),
        true,
    )?;
    c.send(
        "Input.dispatchKeyEvent",
        &format!("{{\"type\":\"keyUp\",\"key\":\"{key}\",\"code\":\"{key}\",\"windowsVirtualKeyCode\":{code}}}"),
        true,
    )?;
    Ok(())
}

fn main() {
    let args = parse_args();
    let r = if args.daemon {
        let (width, height) = args.size.unwrap_or((1920, 1080));
        daemon::run(&daemon_config(&args, width, height))
    } else if args.kill {
        kill(&args)
    } else {
        session(&args)
    };
    if let Err(e) = r {
        eprintln!("[kami] {e}");
        std::process::exit(1);
    }
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
    c.call("Browser.close", "{}", false)?;
    eprintln!("[kami] chromium closed");
    Ok(())
}

/// Connect to the daemon, starting it first if there is none.
fn connect(args: &Args, width: usize, height: usize) -> io::Result<Cdp> {
    if let Ok(c) = Cdp::connect(&args.sock) {
        eprintln!("[kami] attached to the running chromium");
        return Ok(c);
    }
    daemon::spawn_detached(&daemon_config(args, width, height))?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(c) = Cdp::connect(&args.sock) {
            eprintln!("[kami] started chromium in {} ms", t0.elapsed().as_millis());
            return Ok(c);
        }
    }
    Err(io::Error::other(format!("daemon did not come up; see {}", args.log)))
}

fn session(args: &Args) -> io::Result<()> {
    let mut fb = fb::Fb::open(&args.fb)?;
    let (w, h) = (fb.width / args.scale, fb.height / args.scale);
    let mut c = connect(args, w, h)?;

    // Reuse the existing tab if there is one; that is the point of the daemon.
    let targets = c.call("Target.getTargets", "{}", false)?;
    let (target, fresh) = match first_page(&targets) {
        Some(t) => (t, false),
        None => {
            let url = escape(args.url.as_deref().unwrap_or("about:blank"));
            let r = c.call("Target.createTarget", &format!("{{\"url\":\"{url}\"}}"), false)?;
            (str_field(&r, "targetId").ok_or(io::Error::other("no targetId"))?.to_string(), true)
        }
    };
    let r = c.call(
        "Target.attachToTarget",
        &format!("{{\"targetId\":\"{target}\",\"flatten\":true}}"),
        false,
    )?;
    let session = str_field(&r, "sessionId").ok_or(io::Error::other("no sessionId"))?.to_string();
    c.set_session(&session);
    // `--window-size` includes the (invisible) window frame in new headless
    // mode, so pin the page viewport to exactly what the screen shows.
    c.call(
        "Emulation.setDeviceMetricsOverride",
        &format!("{{\"width\":{w},\"height\":{h},\"deviceScaleFactor\":1,\"mobile\":false}}"),
        true,
    )?;
    c.call("Page.enable", "{}", true)?;
    if let (false, Some(url)) = (fresh, &args.url) {
        c.call("Page.navigate", &format!("{{\"url\":\"{}\"}}", escape(url)), true)?;
        // The navigation may swap the renderer; a screencast started before
        // the new page commits fails with "Not attached to an active page".
        c.wait_event("Page.frameNavigated", Duration::from_secs(15))?;
    }
    let cast = format!("{{\"format\":\"png\",\"everyNthFrame\":1,\"maxWidth\":{w},\"maxHeight\":{h}}}");
    let mut tries = 0;
    while let Err(e) = c.call("Page.startScreencast", &cast, true) {
        tries += 1;
        if tries == 50 || !e.to_string().contains("Not attached to an active page") {
            return Err(e);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("[kami] screencast {w}x{h} x{} on target {target}", args.scale);

    let tty = RawTty::enter();
    let started = Instant::now();
    let mut png = png::Decoder::default();
    let mut bytes = Vec::new();
    let (mut frames, mut dec_t, mut blit_t, mut size_t) = (0u64, Duration::ZERO, Duration::ZERO, 0usize);
    let mut window = Instant::now();
    let mut stdin_open = tty.0.is_some();
    let mut msgs: Vec<Vec<u8>> = std::mem::take(&mut c.queued);

    'outer: loop {
        for m in msgs.drain(..) {
            if method(&m) != Some("Page.screencastFrame") || event_session(&m) != Some(&session) {
                continue;
            }
            let ack = num_field(&m, "sessionId");
            let data = str_field(&m, "data").unwrap_or("");
            let td = Instant::now();
            let ok = b64_decode(data.as_bytes(), &mut bytes).and_then(|_| png.decode(&bytes));
            let tb = Instant::now();
            match ok {
                Ok(()) => fb.blit(&png.pixels, png.width, png.height, png.channels, args.scale),
                Err(e) => eprintln!("[kami] frame dropped: {e}\r"),
            }
            dec_t += tb - td;
            blit_t += tb.elapsed();
            size_t += bytes.len();
            if let Some(id) = ack {
                c.send("Page.screencastFrameAck", &format!("{{\"sessionId\":{id}}}"), true)?;
            }
            frames += 1;
            if frames % 30 == 0 || frames == 1 || args.frames == Some(frames) {
                let n = if frames == 1 { 1 } else { 30.min(frames) } as u32;
                eprintln!(
                    "[kami] frame {frames} {}x{}: {:.1} fps, png {} KB, decode {:.1} ms, blit {:.1} ms\r",
                    png.width,
                    png.height,
                    n as f64 / window.elapsed().as_secs_f64(),
                    size_t / n as usize / 1024,
                    dec_t.as_secs_f64() * 1e3 / n as f64,
                    blit_t.as_secs_f64() * 1e3 / n as f64,
                );
                (dec_t, blit_t, size_t, window) = (Duration::ZERO, Duration::ZERO, 0, Instant::now());
            }
            if args.frames == Some(frames) {
                break 'outer;
            }
        }
        if args.seconds.is_some_and(|s| started.elapsed().as_secs_f64() >= s) {
            break;
        }

        let mut fds = [
            libc::pollfd { fd: c.fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: 0, events: if stdin_open { libc::POLLIN } else { 0 }, revents: 0 },
        ];
        // SAFETY: two valid pollfd entries.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, 250) } < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if fds[0].revents != 0 {
            if !c.fill()? {
                eprintln!("[kami] daemon went away\r");
                return Ok(());
            }
            while let Some(m) = c.next() {
                msgs.push(m);
            }
        }
        if fds[1].revents != 0 {
            let mut buf = [0u8; 256];
            // SAFETY: reading into a stack buffer of the stated size.
            let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                stdin_open = false;
                continue;
            }
            for k in decode_keys(&buf[..n as usize]) {
                match k {
                    Input::Quit => break 'outer,
                    Input::Reload => {
                        c.send("Page.reload", "{}", true)?;
                    }
                    Input::Key(key, code) => send_key(&mut c, key, code)?,
                    Input::Text(t) => {
                        c.send("Input.insertText", &format!("{{\"text\":\"{}\"}}", escape(&t)), true)?;
                    }
                }
            }
        }
    }

    drop(tty);
    // Detach, leaving Chromium and the page for the next session.
    let _ = c.call("Page.stopScreencast", "{}", true);
    let _ = c.call("Target.detachFromTarget", &format!("{{\"sessionId\":\"{session}\"}}"), false);
    eprintln!("[kami] detached after {frames} frames; chromium keeps running (kami --kill stops it)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(first_page(t).as_deref(), Some("P1"));
        assert_eq!(first_page(br#"{"id":1,"result":{"targetInfos":[]}}"#), None);
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
