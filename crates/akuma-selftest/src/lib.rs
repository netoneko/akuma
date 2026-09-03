#![no_std]
// A harness that could itself be the broken thing is not a harness.
#![forbid(unsafe_code)]
//! Boot self-test harness: a pass/fail tally, and got-vs-want reporting.
//!
//! # Why this exists
//!
//! `src/tests.rs` and `src/process_tests.rs` are 36k lines of boot self-tests
//! written as `fn test_*() -> bool` with ad-hoc printing at each site, and the
//! `amd64/` target had started reproducing the same shape — seven smoke tests
//! each hand-rolling `if ok { "[OK]" } else { "[FAIL]" }`. Neither side had a
//! tally, which meant a failing check printed `[FAIL]` and the boot carried on
//! and finished as if nothing had happened.
//!
//! # Design
//!
//! * **No dependencies, no allocation, no `unsafe`.** This runs when the thing
//!   being tested may be the allocator. Numbers are formatted into stack
//!   buffers.
//! * **Output is a `fn(&str)` the caller supplies.** The harness must not know
//!   whether it is talking to a PL011, a 16550 or a host `print!`. That is also
//!   what makes it host-testable: the tests below capture into a buffer.
//! * **Got-vs-want is the default, not an extra.** [`Suite::check`] takes a
//!   bool and can only say "no". [`Suite::check_eq`] prints both values on
//!   failure, which is the difference between "the exit status was wrong" and
//!   "the exit status was 0x37, which is the message length".

/// Widest rendering of a `u64` across both bases.
///
/// Hex needs 18 (`0x` + 16 digits); **decimal needs 20** — `u64::MAX` is
/// 18446744073709551615. Sized for hex alone at first, which made `dec` panic
/// with "index out of bounds: the len is 18 but the index is 18" on the largest
/// input. In a kernel that is a panic raised *by the diagnostic path*, which is
/// the one place a panic is least useful; the host test is what caught it.
const NUM_BUF: usize = 20;

/// Format `v` as hex into `buf`, returning the populated slice.
fn hex(v: u64, buf: &mut [u8; NUM_BUF]) -> &str {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    buf[0] = b'0';
    buf[1] = b'x';
    // Trim leading zeros, but always keep one digit.
    let mut started = false;
    let mut n = 2;
    for shift in (0..16).rev() {
        let nibble = (v >> (shift * 4)) & 0xF;
        if nibble != 0 || started || shift == 0 {
            started = true;
            buf[n] = DIGITS[nibble as usize];
            n += 1;
        }
    }
    core::str::from_utf8(&buf[..n]).unwrap_or("0x?")
}

/// Format `v` as decimal into `buf`, returning the populated slice.
fn dec(v: u64, buf: &mut [u8; NUM_BUF]) -> &str {
    if v == 0 {
        buf[0] = b'0';
        return core::str::from_utf8(&buf[..1]).unwrap_or("0");
    }
    let mut tmp = [0u8; NUM_BUF];
    let mut n = 0;
    let mut x = v;
    while x > 0 {
        tmp[n] = b'0' + (x % 10) as u8;
        x /= 10;
        n += 1;
    }
    for i in 0..n {
        buf[i] = tmp[n - 1 - i];
    }
    core::str::from_utf8(&buf[..n]).unwrap_or("?")
}

/// A named group of checks with a pass/fail tally.
pub struct Suite {
    name: &'static str,
    passed: u32,
    failed: u32,
    emit: fn(&str),
}

impl Suite {
    /// Start a suite. `emit` writes a string wherever this platform's console is.
    #[must_use]
    pub fn new(name: &'static str, emit: fn(&str)) -> Self {
        Self {
            name,
            passed: 0,
            failed: 0,
            emit,
        }
    }

    fn put(&self, s: &str) {
        (self.emit)(s);
    }

    /// Record a check. Returns `ok`, so a caller can bail out on failure.
    pub fn check(&mut self, what: &str, ok: bool) -> bool {
        self.put("  ");
        self.put(what);
        if ok {
            self.passed += 1;
            self.put("   [OK]\n");
        } else {
            self.failed += 1;
            self.put("   [FAIL]\n");
        }
        ok
    }

    /// Record a check, printing both values when they differ.
    ///
    /// The reason this is the primary API rather than a convenience: a bare
    /// bool can only report that something was wrong. When an amd64 process
    /// exited with `0x37` instead of `0x0b`, the *value* was the diagnosis —
    /// 0x37 is 55, the length of the message it had just written, which named
    /// the bug immediately.
    pub fn check_eq(&mut self, what: &str, got: u64, want: u64) -> bool {
        let ok = got == want;
        self.put("  ");
        self.put(what);
        if ok {
            self.passed += 1;
            self.put("   [OK]\n");
        } else {
            self.failed += 1;
            let mut g = [0u8; NUM_BUF];
            let mut w = [0u8; NUM_BUF];
            self.put("   [FAIL] got ");
            self.put(hex(got, &mut g));
            self.put(" want ");
            self.put(hex(want, &mut w));
            self.put("\n");
        }
        ok
    }

    /// Note something without scoring it — a measurement, not an assertion.
    pub fn note(&mut self, what: &str, value: u64) {
        let mut b = [0u8; NUM_BUF];
        self.put("  ");
        self.put(what);
        self.put(" ");
        self.put(dec(value, &mut b));
        self.put("\n");
    }

    /// Checks recorded so far.
    #[must_use]
    pub const fn passed(&self) -> u32 {
        self.passed
    }

    /// Failures recorded so far.
    #[must_use]
    pub const fn failed(&self) -> u32 {
        self.failed
    }

    /// Print the tally. Returns true if everything passed.
    ///
    /// `#[must_use]`: the whole reason this crate exists is that a `[FAIL]`
    /// used to print and the boot then carried on and announced success. A
    /// caller that drops this verdict has reintroduced exactly that.
    #[must_use]
    pub fn report(&self) -> bool {
        let mut p = [0u8; NUM_BUF];
        let mut f = [0u8; NUM_BUF];
        self.put("\n");
        self.put(self.name);
        self.put(": ");
        self.put(dec(u64::from(self.passed), &mut p));
        self.put(" passed, ");
        self.put(dec(u64::from(self.failed), &mut f));
        self.put(self.if_failed(" FAILED\n", " failed\n"));
        self.failed == 0
    }

    const fn if_failed(&self, yes: &'static str, no: &'static str) -> &'static str {
        if self.failed == 0 { no } else { yes }
    }
}

/// `std` for the host tests only: the harness itself is `no_std`, but capturing
/// its output to assert on needs a growable buffer.
#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;
    use std::sync::Mutex;

    // A `fn(&str)` cannot capture, so the tests route through a static buffer.
    // That constraint is deliberate on the harness side rather than an
    // inconvenience: a capturing closure would need a fat pointer, and this has
    // to work before there is a heap.
    static OUT: Mutex<String> = Mutex::new(String::new());

    fn emit(s: &str) {
        OUT.lock().unwrap().push_str(s);
    }
    fn taken() -> String {
        let mut g = OUT.lock().unwrap();
        let s = g.clone();
        g.clear();
        s
    }

    /// One lock for the shared buffer, since `cargo test` runs tests in threads
    /// and they would otherwise interleave into each other's output.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn counts_and_reports() {
        let _g = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = taken();
        let mut s = Suite::new("suite", emit);
        assert!(s.check("a", true));
        assert!(!s.check("b", false));
        assert_eq!((s.passed(), s.failed()), (1, 1));
        assert!(!s.report());
        let out = taken();
        assert!(out.contains("1 passed, 1 FAILED"), "{out}");
    }

    #[test]
    fn all_passing_reports_lowercase_failed() {
        let _g = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = taken();
        let mut s = Suite::new("suite", emit);
        s.check("a", true);
        assert!(s.report());
        let out = taken();
        assert!(out.contains("1 passed, 0 failed"), "{out}");
        assert!(!out.contains("FAILED"), "{out}");
    }

    /// The property that named the Stage H bug: a mismatch prints both values.
    #[test]
    fn check_eq_prints_got_and_want() {
        let _g = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = taken();
        let mut s = Suite::new("suite", emit);
        assert!(!s.check_eq("exit status", 0x37, 0x0b));
        let out = taken();
        assert!(out.contains("got 0x37"), "{out}");
        assert!(out.contains("want 0xb"), "{out}");
    }

    #[test]
    fn hex_and_dec_edges() {
        let mut b = [0u8; NUM_BUF];
        assert_eq!(hex(0, &mut b), "0x0");
        assert_eq!(hex(0xdead_beef, &mut b), "0xdeadbeef");
        assert_eq!(hex(u64::MAX, &mut b), "0xffffffffffffffff");
        assert_eq!(dec(0, &mut b), "0");
        assert_eq!(dec(u64::MAX, &mut b), "18446744073709551615");
    }
}
