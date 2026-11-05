//! The boot command line, kept once and read-only, and the narrowed views middleware reads it through.
//!
//! **Write once.** [`capture`] takes the one buffer ([`akuma_primitives::TakeOnce`] hands out its `&'static mut` exactly once for the
//! life of the program), lets the machine fill it, and gives back a shared `&'static str`. The mutable borrow is consumed by that
//! conversion and the buffer cannot be taken again, so after `capture` returns nothing in the tree can change the line: not by
//! API, and not by a second capture (`Failure::Again`). That is a software guarantee; a kernel write through a raw pointer is
//! not stopped by it (making the page read-only in the page table is the stronger version, not done here).
//!
//! **Complete or absent.** A line that did not fit, or could not be read, is *not* published as a prefix: a cut-off
//! `mw.signed.key=ab12..` is still valid hex. [`failed`] says so, and a middleware that needs a value treats that as "refuse".
//!
//! **Narrowed views.** A middleware never sees the line. It is handed a [`Params`] that can only look at `token`s under a prefix
//! (`mw.nodl.log=1` is `log` to the view scoped to `mw.nodl.`). A view can narrow ([`Params::scoped`]) and has no way to widen, so
//! a crate cannot read `init=`, `epoch=` or another crate's keys. What a crate needs is described by the [`ParamSource`] trait, so
//! its parser takes the cmdline view in the kernel, a fixture in a host test, or a compiled-in table, and cannot tell.

use core::sync::atomic::{AtomicU8, Ordering::Relaxed};

use akuma_primitives::{OnceCopy, TakeOnce};

/// The most bytes of command line kept (Linux's x86 `COMMAND_LINE_SIZE` is also 2048). A longer line is a [`Failure::TooLong`].
pub const CAP: usize = 2048;

/// The prefix every middleware key lives under.
pub const MW: &str = "mw.";

static BUF: TakeOnce<[u8; CAP]> = TakeOnce::new([0; CAP]);
static LINE: OnceCopy<&'static str> = OnceCopy::new();
/// 0: not captured yet. 1: published. 2: capture failed (no line is published).
static STATE: AtomicU8 = AtomicU8::new(0);

/// Why a line was not published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The line is longer than [`CAP`] (or than the machine's own buffer).
    TooLong,
    /// The machine could not read it.
    Unreadable,
    /// It is not UTF-8.
    NotUtf8,
    /// [`capture`] was already called.
    Again,
}

/// Fill the buffer once, with `fill` (it returns how many bytes it wrote), and publish the result.
///
/// # Errors
/// Whatever `fill` reports; [`Failure::NotUtf8`] if the bytes are not UTF-8; [`Failure::Again`] on a second call. On any error
/// nothing is published and [`failed`] is true.
pub fn capture(fill: impl FnOnce(&mut [u8]) -> Result<usize, Failure>) -> Result<&'static str, Failure> {
    let Some(buf) = BUF.take() else {
        return Err(Failure::Again);
    };
    let n = match fill(&mut buf[..]) {
        Ok(n) => n.min(CAP),
        Err(e) => {
            STATE.store(2, Relaxed);
            return Err(e);
        }
    };
    // The `&'static mut` ends here: only the shared reborrow survives, and `BUF.take()` will never return it again.
    let shared: &'static [u8; CAP] = buf;
    if let Ok(s) = core::str::from_utf8(&shared[..n]) {
        LINE.set(s);
        STATE.store(1, Relaxed);
        Ok(s)
    } else {
        STATE.store(2, Relaxed);
        Err(Failure::NotUtf8)
    }
}

/// The captured line, or `""` before [`capture`] (and after a failed one).
#[must_use]
pub fn line() -> &'static str {
    LINE.get().unwrap_or("")
}

/// Whether [`capture`] was tried and did not publish a line.
#[must_use]
pub fn failed() -> bool {
    STATE.load(Relaxed) == 2
}

/// What a middleware reads its configuration through: `key` is relative to wherever the source is scoped.
pub trait ParamSource {
    /// The value of the first `key=value` word, if any.
    fn get(&self, key: &str) -> Option<&str>;
    /// Every `key=value` word with this key, in order, for a setting that may be repeated (`mw.signed.allow=` once per binary).
    fn for_each(&self, key: &str, f: &mut dyn FnMut(&str));
}

/// A read-only view of a command line, narrowed to the words under up to three prefixes in a row.
#[derive(Clone, Copy, Debug)]
pub struct Params<'a> {
    line: &'a str,
    prefix: [&'a str; 3],
    depth: usize,
}

impl<'a> Params<'a> {
    /// A view of `line` under `prefix` (use [`MW`] for a middleware's).
    #[must_use]
    pub const fn new(line: &'a str, prefix: &'a str) -> Self {
        Self { line, prefix: [prefix, "", ""], depth: 1 }
    }

    /// The view middleware gets: the captured line, under `mw.`.
    #[must_use]
    pub fn middleware() -> Params<'static> {
        Params::new(line(), MW)
    }

    /// Narrow further: `scoped("nodl.")` of a view under `mw.` is the view under `mw.nodl.`. There is no inverse.
    ///
    /// # Panics
    /// On a fourth level: three is more than any crate nests.
    #[must_use]
    pub fn scoped(&self, ns: &'a str) -> Self {
        assert!(self.depth < self.prefix.len(), "params: scoped too deep");
        let mut p = *self;
        p.prefix[p.depth] = ns;
        p.depth += 1;
        p
    }

    /// [`ParamSource::get`], but the value borrows from the line, not from this view: for a view over the captured (`'static`)
    /// line that is a `&'static str` a hook can keep.
    #[must_use]
    pub fn value(&self, key: &str) -> Option<&'a str> {
        let line: &'a str = self.line;
        line.split_ascii_whitespace().find_map(|w| self.rest(w)?.strip_prefix(key)?.strip_prefix('='))
    }

    /// [`ParamSource::for_each`], but each value borrows from the line, not from this view (see [`Self::value`]).
    pub fn each_value(&self, key: &str, mut f: impl FnMut(&'a str)) {
        let line: &'a str = self.line;
        for w in line.split_ascii_whitespace() {
            if let Some(v) = self.rest(w).and_then(|r| r.strip_prefix(key)).and_then(|r| r.strip_prefix('=')) {
                f(v);
            }
        }
    }

    fn rest<'t>(&self, mut word: &'t str) -> Option<&'t str> {
        for seg in &self.prefix[..self.depth] {
            word = word.strip_prefix(seg)?;
        }
        Some(word)
    }
}

impl ParamSource for Params<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.value(key)
    }
    fn for_each(&self, key: &str, f: &mut dyn FnMut(&str)) {
        self.each_value(key, f);
    }
}

/// A fixed table of `(key, value)`, for a host test or a compiled-in default. A newtype, because a bare slice's own `get` would
/// shadow the trait's in a method call.
#[derive(Clone, Copy, Debug)]
pub struct Table<'a>(pub &'a [(&'a str, &'a str)]);

impl ParamSource for Table<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }
    fn for_each(&self, key: &str, f: &mut dyn FnMut(&str)) {
        for (_, v) in self.0.iter().filter(|(k, _)| *k == key) {
            f(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "init=/bin/herd smp=4 mw.nodl.log=0 mw.signed.key=ab12 mw.nodlx.log=9 xmw.nodl.log=7";

    #[test]
    fn a_view_sees_only_its_namespace() {
        let mw = Params::new(LINE, MW);
        let nodl = mw.scoped("nodl.");
        assert_eq!(nodl.get("log"), Some("0"));
        assert_eq!(nodl.get("key"), None, "another crate's key");
        assert_eq!(mw.scoped("signed.").get("key"), Some("ab12"));
        // The prefix ends at its dot: `nodlx` is not `nodl`, and `xmw.` is not `mw.`.
        assert_eq!(mw.scoped("nodlx.").get("log"), Some("9"));
        // The middleware view cannot reach the kernel's own words.
        assert_eq!(mw.get("init"), None);
        assert_eq!(mw.get("smp"), None);
    }

    #[test]
    fn a_key_is_whole_not_a_prefix() {
        let p = Params::new("mw.a.keyring=1 mw.a.key=2", MW).scoped("a.");
        assert_eq!(p.get("key"), Some("2"));
    }

    #[test]
    fn a_repeated_key_is_visited_in_order_and_only_under_its_namespace() {
        let p = Params::new("mw.s.allow=a mw.t.allow=x mw.s.allow=b mw.s.allowed=c mw.s.allow=c", MW).scoped("s.");
        let mut seen = [""; 4];
        let mut n = 0;
        p.each_value("allow", |v| {
            seen[n] = v;
            n += 1;
        });
        assert_eq!((n, &seen[..n]), (3, &["a", "b", "c"][..]));
        let t = Table(&[("k", "1"), ("j", "2"), ("k", "3")]);
        let mut m = 0;
        let mut count = 0;
        t.for_each("k", &mut |_| count += 1);
        assert_eq!(count, 2);
        p.for_each("allow", &mut |_| m += 1);
        assert_eq!(m, 3);
    }

    #[test]
    fn a_table_is_a_source_too() {
        let t = Table(&[("log", "1")]);
        assert_eq!(t.get("log"), Some("1"));
        assert_eq!(t.get("key"), None);
    }

    #[test]
    #[should_panic(expected = "scoped too deep")]
    fn a_fourth_level_is_refused() {
        let _ = Params::new("", MW).scoped("a.").scoped("b.").scoped("c.");
    }

    /// One test owns the statics (they are per-process).
    #[test]
    fn capture_publishes_once_and_never_a_prefix() {
        assert_eq!(line(), "");
        assert!(!failed());
        let got = capture(|buf| {
            let l = b"init=/x mw.k.v=1";
            buf[..l.len()].copy_from_slice(l);
            Ok(l.len())
        });
        assert_eq!(got, Ok("init=/x mw.k.v=1"));
        assert_eq!(line(), "init=/x mw.k.v=1");
        assert_eq!(Params::middleware().scoped("k.").get("v"), Some("1"));
        // A second capture cannot replace it.
        assert_eq!(capture(|_| Ok(0)), Err(Failure::Again));
        assert_eq!(line(), "init=/x mw.k.v=1");
        assert!(!failed());
    }
}
