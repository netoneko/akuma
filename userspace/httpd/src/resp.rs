//! Allocation-free response construction.
//!
//! `httpd` used to build every response through `format!`: a `String` for the
//! status line and headers, a second one for the RFC1123 date, a `Vec<u8>` for
//! the file body, and an 8 KB `vec!` for the request buffer — five heap
//! round trips to serve a 23-byte file. None of that varies per request in a way
//! that needs the heap.
//!
//! What is here instead:
//!
//! - **[`ERROR_RESPONSES`]** — every error reply is a `const` byte string,
//!   complete with headers and body. Serving a 404 is now one `write_all` of a
//!   constant. There is no `Date` header on these; HTTP/1.0 does not require one
//!   and generating it was most of their cost.
//! - **[`FixedBuf`]** — a `core::fmt::Write` sink over a stack array, for the
//!   200 response line whose `Content-Type`/`Content-Length` genuinely vary.
//!   `write!` into it costs no allocation.
//! - **[`DateCache`]** — the RFC1123 date recomputed at most once per second
//!   instead of once per request. The old `format_time_rfc1123` walked a year at
//!   a time from 1970 and then ran a `format!`; at second granularity that work
//!   is being thrown away on all but the first request of each second.
//!
//! # What this is *not*
//!
//! It is not a fix for Akuma's round-trip latency, and the instrumentation in
//! `stats.rs` is what shows why: the `other` (compute) phase is where all of
//! this lands, and if `accept` dominates the request then removing every
//! allocation moves a number that was never the constraint. Measure with
//! `HTTPD_STATS=1` before and after rather than assuming.

use core::fmt::{self, Write};

/// A `core::fmt::Write` sink over a fixed stack buffer.
///
/// Writes past the end are dropped rather than panicking, and [`truncated`]
/// reports it — the same discipline as the kernel's `safe_print!`
/// (`akuma_primitives::console::StackWriter`): a formatting overflow on a
/// response path must degrade, never abort the server.
///
/// [`truncated`]: FixedBuf::truncated
pub struct FixedBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
    overflowed: bool,
}

impl<const N: usize> FixedBuf<N> {
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: [0u8; N], len: 0, overflowed: false }
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.overflowed
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.overflowed = false;
    }
}

impl<const N: usize> Default for FixedBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Write for FixedBuf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let b = s.as_bytes();
        let room = N - self.len;
        let take = b.len().min(room);
        self.buf[self.len..self.len + take].copy_from_slice(&b[..take]);
        self.len += take;
        if take < b.len() {
            self.overflowed = true;
        }
        // Always Ok: a truncated log/response line is recoverable, an Err here
        // would propagate out of `write!` and be unwrapped somewhere.
        Ok(())
    }
}

// ============================================================================
// Static error responses
// ============================================================================

/// Largest assembled error response. 256 covers every entry below with room to
/// spare; `build_error` asserts at compile time if one ever exceeds it.
const MAX_ERROR_RESPONSE: usize = 256;

/// A response assembled during const evaluation.
///
/// The array is fixed-size because const evaluation has no allocator; `len` is
/// how much of it is real, and [`ErrorResponse::as_bytes`] is what gets written
/// to the socket.
pub struct ErrorResponse {
    buf: [u8; MAX_ERROR_RESPONSE],
    len: usize,
}

impl ErrorResponse {
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8] {
        // `split_at` is const; slicing with a runtime-looking range is not.
        self.buf.split_at(self.len).0
    }
}

/// Append `src` to `dst` starting at `len`, returning the new length.
///
/// A `while` loop rather than `copy_from_slice` because the latter is not
/// callable in const context.
const fn push(dst: &mut [u8; MAX_ERROR_RESPONSE], mut len: usize, src: &[u8]) -> usize {
    let mut i = 0;
    while i < src.len() {
        assert!(len < MAX_ERROR_RESPONSE, "error response exceeds MAX_ERROR_RESPONSE");
        dst[len] = src[i];
        len += 1;
        i += 1;
    }
    len
}

/// Append `v` as decimal ASCII.
const fn push_u32(dst: &mut [u8; MAX_ERROR_RESPONSE], mut len: usize, v: u32) -> usize {
    if v >= 10 {
        len = push_u32(dst, len, v / 10);
    }
    assert!(len < MAX_ERROR_RESPONSE, "error response exceeds MAX_ERROR_RESPONSE");
    dst[len] = b'0' + (v % 10) as u8;
    len + 1
}

/// Assemble one complete error response — status line, headers, and body —
/// entirely at compile time.
///
/// `Content-Length` is **computed** from the body this same function builds,
/// rather than written as a literal beside it. That distinction matters: a
/// hand-written length silently desynchronises the first time anyone edits the
/// body, and the failure mode is a client hanging on bytes that never arrive.
const fn build_error(code: u32, msg: &str) -> ErrorResponse {
    let msg = msg.as_bytes();

    // Pass 1: the body, so its length is known before the headers are written.
    let mut body = [0u8; MAX_ERROR_RESPONSE];
    let mut blen = push(&mut body, 0, b"<!DOCTYPE html>\n<html><head><title>");
    blen = push_u32(&mut body, blen, code);
    blen = push(&mut body, blen, b" ");
    blen = push(&mut body, blen, msg);
    blen = push(&mut body, blen, b"</title></head>\n<body><h1>");
    blen = push_u32(&mut body, blen, code);
    blen = push(&mut body, blen, b" ");
    blen = push(&mut body, blen, msg);
    blen = push(&mut body, blen, b"</h1></body></html>\n");

    // Pass 2: the whole response.
    let mut buf = [0u8; MAX_ERROR_RESPONSE];
    let mut len = push(&mut buf, 0, b"HTTP/1.0 ");
    len = push_u32(&mut buf, len, code);
    len = push(&mut buf, len, b" ");
    len = push(&mut buf, len, msg);
    len = push(&mut buf, len, b"\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: ");
    len = push_u32(&mut buf, len, blen as u32);
    len = push(&mut buf, len, b"\r\nConnection: close\r\n\r\n");

    let mut i = 0;
    while i < blen {
        assert!(len < MAX_ERROR_RESPONSE, "error response exceeds MAX_ERROR_RESPONSE");
        buf[len] = body[i];
        len += 1;
        i += 1;
    }

    ErrorResponse { buf, len }
}

/// Every error reply this server can produce, fully assembled at compile time.
///
/// Serving one is a single `write_all` of a constant: no `format!`, no `String`,
/// no `Date` header (HTTP/1.0 does not require one, and generating it was most
/// of the old path's cost).
pub const ERROR_RESPONSES: &[(u16, &ErrorResponse)] = &[
    (400, &BAD_REQUEST),
    (403, &FORBIDDEN),
    (404, &NOT_FOUND),
    (405, &METHOD_NOT_ALLOWED),
    (500, &INTERNAL_ERROR),
    (504, &GATEWAY_TIMEOUT),
];

const BAD_REQUEST: ErrorResponse = build_error(400, "Bad Request");
const FORBIDDEN: ErrorResponse = build_error(403, "Forbidden");
const NOT_FOUND: ErrorResponse = build_error(404, "Not Found");
const METHOD_NOT_ALLOWED: ErrorResponse = build_error(405, "Method Not Allowed");
const INTERNAL_ERROR: ErrorResponse = build_error(500, "Internal Server Error");
const GATEWAY_TIMEOUT: ErrorResponse = build_error(504, "Gateway Timeout");

/// The complete, ready-to-write response for `code`, falling back to 500 for a
/// code that is not in the table. Never allocates and never fails.
#[must_use]
pub fn error_response(code: u16) -> &'static [u8] {
    let mut i = 0;
    while i < ERROR_RESPONSES.len() {
        if ERROR_RESPONSES[i].0 == code {
            return ERROR_RESPONSES[i].1.as_bytes();
        }
        i += 1;
    }
    INTERNAL_ERROR.as_bytes()
}

// ============================================================================
// Date header
// ============================================================================

/// The RFC1123 `Date` value, recomputed at most once per second.
///
/// Serving N requests in one second used to run the 1970-forward year walk and
/// a `format!` N times for a string that was identical every time.
pub struct DateCache {
    secs: u64,
    buf: FixedBuf<40>,
}

impl DateCache {
    #[must_use]
    pub const fn new() -> Self {
        Self { secs: u64::MAX, buf: FixedBuf::new() }
    }

    /// The date string for `now_us`, formatting only if the second changed.
    pub fn get(&mut self, now_us: u64) -> &[u8] {
        let secs = now_us / 1_000_000;
        if secs != self.secs {
            self.secs = secs;
            self.buf.clear();
            write_rfc1123(&mut self.buf, secs);
        }
        self.buf.as_bytes()
    }
}

impl Default for DateCache {
    fn default() -> Self {
        Self::new()
    }
}

const WDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun",
    "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

const fn is_leap_year(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Format `secs` (Unix epoch seconds) as RFC1123 into `w`.
///
/// Pulled out of `main.rs`'s `format_time_rfc1123` unchanged in arithmetic, but
/// writing into a sink instead of returning a `String`. Kept public so the host
/// test below can exercise it without a socket.
pub fn write_rfc1123<W: Write>(w: &mut W, secs: u64) {
    let mut days = secs / 86400;
    let secs_today = secs % 86400;
    let (hour, minute, second) = (secs_today / 3600, (secs_today % 3600) / 60, secs_today % 60);
    let wday = WDAYS[((days + 4) % 7) as usize];

    let mut year = 1970u64;
    loop {
        let year_days = if is_leap_year(year) { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }

    let lengths: [u64; 12] = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 0;
    for (i, &n) in lengths.iter().enumerate() {
        if days < n {
            month = i;
            break;
        }
        days -= n;
    }

    let _ = write!(
        w,
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        wday, days + 1, MONTHS[month], year, hour, minute, second
    );
}
