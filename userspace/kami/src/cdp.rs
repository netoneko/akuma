//! Chrome DevTools Protocol framing and the client end of the daemon socket.
//!
//! On the wire every CDP message is one JSON object terminated by NUL — that
//! is Chromium's `--remote-debugging-pipe` format, and the daemon relays it
//! over its unix socket unchanged.
//!
//! JSON is handled by scanning for the handful of fields this tool needs.
//! That is sound for what Chromium emits here: replies start `{"id":N`,
//! events start `{"method":"`, and the values read (ids, session/target ids,
//! base64 frame data) contain no escapes.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

/// Splits a byte stream into NUL-terminated messages.
#[derive(Default)]
pub struct Frames {
    buf: Vec<u8>,
    start: usize,
}

impl Frames {
    /// One read from `r`. Returns the byte count; 0 is EOF.
    pub fn read_from(&mut self, r: &mut impl Read) -> io::Result<usize> {
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > (8 << 20) {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        let old = self.buf.len();
        self.buf.resize(old + (1 << 20), 0);
        let n = r.read(&mut self.buf[old..]);
        self.buf.truncate(old + *n.as_ref().unwrap_or(&0));
        n
    }

    /// The next complete message, without its NUL.
    pub fn next(&mut self) -> Option<Vec<u8>> {
        let nul = self.buf[self.start..].iter().position(|&b| b == 0)?;
        let msg = self.buf[self.start..self.start + nul].to_vec();
        self.start += nul + 1;
        Some(msg)
    }

    pub fn reset(&mut self) {
        self.buf.clear();
        self.start = 0;
    }
}

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The string value of the first `"key":"..."` in `msg`.
pub fn str_field<'a>(msg: &'a [u8], key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":\"");
    let at = find(msg, pat.as_bytes())? + pat.len();
    let end = at + msg[at..].iter().position(|&b| b == b'"')?;
    std::str::from_utf8(&msg[at..end]).ok()
}

/// The numeric value of the first `"key":<digits>` in `msg`, skipping
/// occurrences of the same key with a string value.
pub fn num_field(msg: &[u8], key: &str) -> Option<u64> {
    let pat = format!("\"{key}\":");
    let mut from = 0;
    while let Some(i) = find(&msg[from..], pat.as_bytes()) {
        let at = from + i + pat.len();
        let digits = msg[at..].iter().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0 {
            return std::str::from_utf8(&msg[at..at + digits]).ok()?.parse().ok();
        }
        from = at;
    }
    None
}

pub fn method(msg: &[u8]) -> Option<&str> {
    if msg.starts_with(b"{\"method\":\"") {
        str_field(msg, "method")
    } else {
        None
    }
}

/// The session an event belongs to: the top-level string `sessionId`, which
/// Chromium serialises after `params`, so take the last occurrence.
pub fn event_session(msg: &[u8]) -> Option<&str> {
    let pat = b"\"sessionId\":\"";
    let mut at = None;
    let mut from = 0;
    while let Some(i) = find(&msg[from..], pat) {
        at = Some(from + i + pat.len());
        from = from + i + pat.len();
    }
    let at = at?;
    let end = at + msg[at..].iter().position(|&b| b == b'"')?;
    std::str::from_utf8(&msg[at..end]).ok()
}

/// The first target in a `Target.getTargets` reply whose type is "page".
pub fn first_page(reply: &[u8]) -> Option<String> {
    let pat = b"{\"targetId\":\"";
    let mut rest = reply;
    while let Some(i) = find(rest, pat) {
        rest = &rest[i + pat.len()..];
        let end = rest.iter().position(|&b| b == b'"')?;
        let next = find(rest, pat).unwrap_or(rest.len());
        if find(&rest[..next], b"\"type\":\"page\"").is_some() {
            return std::str::from_utf8(&rest[..end]).ok().map(str::to_string);
        }
    }
    None
}

pub struct Cdp {
    stream: UnixStream,
    frames: Frames,
    next_id: u64,
    session: Option<String>,
    /// Events that arrived while `call` waited for its reply.
    pub queued: Vec<Vec<u8>>,
}

impl Cdp {
    pub fn connect(path: &str) -> io::Result<Cdp> {
        Ok(Cdp {
            stream: UnixStream::connect(path)?,
            frames: Frames::default(),
            next_id: 0,
            session: None,
            queued: Vec::new(),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Send a command; `params` is a JSON object literal. Returns its id.
    pub fn send(&mut self, method: &str, params: &str, to_session: bool) -> io::Result<u64> {
        self.next_id += 1;
        let mut msg = format!("{{\"id\":{},\"method\":\"{method}\",\"params\":{params}", self.next_id);
        if let (true, Some(s)) = (to_session, &self.session) {
            msg.push_str(&format!(",\"sessionId\":\"{s}\""));
        }
        msg.push_str("}\0");
        self.stream.write_all(msg.as_bytes())
            .map(|_| self.next_id)
    }

    /// Write one complete message the caller has already framed (id, session
    /// and all); the NUL terminator is added here.
    pub fn send_raw(&mut self, msg: &str) -> io::Result<()> {
        self.stream.write_all(msg.as_bytes())?;
        self.stream.write_all(b"\0")
    }

    /// One read from the socket. `Ok(false)` at EOF.
    pub fn fill(&mut self) -> io::Result<bool> {
        Ok(self.frames.read_from(&mut self.stream)? > 0)
    }

    pub fn next(&mut self) -> Option<Vec<u8>> {
        self.frames.next()
    }

    /// Send a command and block for its reply; events seen meanwhile are queued.
    pub fn call(&mut self, method: &str, params: &str, to_session: bool) -> io::Result<Vec<u8>> {
        let id = self.send(method, params, to_session)?;
        let head = format!("{{\"id\":{id},");
        let err = format!("{head}\"error\"");
        loop {
            while let Some(m) = self.next() {
                if m.starts_with(err.as_bytes()) {
                    return Err(io::Error::other(format!("{method}: {}", String::from_utf8_lossy(&m))));
                }
                if m.starts_with(head.as_bytes()) {
                    return Ok(m);
                }
                if m.starts_with(b"{\"method\":") {
                    self.queued.push(m);
                }
            }
            if !self.fill()? {
                return Err(io::Error::other("daemon closed the connection"));
            }
        }
    }
}
