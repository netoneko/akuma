#![no_std]
#![forbid(unsafe_code)]
//! "Signed code only" as a middleware crate (B22). An image runs only if the policy can say *why* it may: its SHA-256 is on an
//! allowlist for its path, or a detached Ed25519 signature over its exact bytes verifies under a key that is allowed to vouch for its
//! path. Anything else is `EACCES`, and so is every other way of turning a file into code (a dynamically linked image, a file-backed
//! `PROT_EXEC` mapping, `mprotect(+X)`): signed code that can map unsigned code is not signed code only.
//!
//! **Why the verdict is `AllowFromImage`.** The kernel hands the policy its own copy of the whole file. Verifying that copy is
//! race-free; what is *not* is running the file afterwards, because the loader leaves whole-page segment pages as lazy records that
//! are read from the inode at fault time, and nothing stops the file being rewritten (or the path renamed) in between.
//! `AllowFromImage` makes the loader place every page from the bytes that were checked and look nothing up again.
//!
//! **Configuration** (boot parameters under `mw.signed.`; see [`Config`]). Both settings may repeat:
//!
//! | word | meaning |
//! |---|---|
//! | `allow=<64 hex SHA-256>:<scope>` | an image with exactly this hash may run from `scope` |
//! | `key=<64 hex Ed25519 public key>:<scope>[,<scope>...]` | this key may vouch (by a detached `<path>.sig`, 64 raw bytes) for images under these scopes |
//!
//! A scope is an exact absolute path, or a directory prefix ending in `/`. The path matched is the one the loader read (symlinks
//! resolved: `/bin/ls` arrives as `/bin/busybox`), so a hash for `/bin/busybox` covers its applets. **A malformed word makes the
//! policy refuse every image**, with the reason on the console: a typo in a trust list must not turn into "nothing is checked".
//!
//! **Cost.** One SHA-256 over the image on every exec (and a verify per matching key when the hash is not listed), single-threaded
//! in the exec path. No allocation here; the loader allocates the sidecar read, only when this table asks for one.
//!
//! **Not claimed.** A script run by a signed interpreter is data, not code. A dynamic library is unsigned code, so a dynamically
//! linked image is refused rather than half-checked. The signature covers bytes, not intent: a signed binary that is itself an
//! interpreter runs whatever it is given.

use akuma_primitives::safe_print;
use akuma_tokio_kernel_core::middleware::{ExecEvent, ExecKind, ExecVerdict, Params};
use sha2::{Digest, Sha256};

const EACCES: i32 = 13;

/// Allowlist entries a [`Config`] holds.
pub const MAX_ALLOW: usize = 16;
/// Keys a [`Config`] holds.
pub const MAX_KEYS: usize = 4;
/// Scopes one key may carry.
pub const MAX_SCOPES: usize = 4;

/// `allow=<hash>:<scope>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Allow<'a> {
    pub digest: [u8; 32],
    pub scope: &'a str,
}

/// `key=<public key>:<scope>,...`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key<'a> {
    pub public: [u8; 32],
    scopes: [&'a str; MAX_SCOPES],
    n: usize,
}

impl<'a> Key<'a> {
    /// The scopes this key may vouch under.
    #[must_use]
    pub fn scopes(&self) -> &[&'a str] {
        &self.scopes[..self.n]
    }
}

/// What the boot parameters said. Fixed size, no heap.
#[derive(Clone, Copy, Debug)]
pub struct Config<'a> {
    allow: [Option<Allow<'a>>; MAX_ALLOW],
    keys: [Option<Key<'a>>; MAX_KEYS],
    /// Words that did not parse (or did not fit). Non-zero: the policy refuses everything.
    pub malformed: u32,
}

impl<'a> Config<'a> {
    /// No entries: nothing may run.
    pub const EMPTY: Self = Self { allow: [None; MAX_ALLOW], keys: [None; MAX_KEYS], malformed: 0 };

    /// Read the `allow` and `key` words from a view already narrowed to `mw.signed.`.
    #[must_use]
    pub fn from_params(p: &Params<'a>) -> Self {
        let mut c = Self::EMPTY;
        let (mut na, mut nk) = (0, 0);
        p.each_value("allow", |w| match parse_allow(w) {
            Some(a) if na < MAX_ALLOW => {
                c.allow[na] = Some(a);
                na += 1;
            }
            _ => c.malformed += 1,
        });
        p.each_value("key", |w| match parse_key(w) {
            Some(k) if nk < MAX_KEYS => {
                c.keys[nk] = Some(k);
                nk += 1;
            }
            _ => c.malformed += 1,
        });
        c
    }

    /// The allowlist entries.
    pub fn allowed(&self) -> impl Iterator<Item = &Allow<'a>> {
        self.allow.iter().flatten()
    }

    /// The keys.
    pub fn keys(&self) -> impl Iterator<Item = &Key<'a>> {
        self.keys.iter().flatten()
    }
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = nib(b[2 * i])? << 4 | nib(b[2 * i + 1])?;
    }
    Some(out)
}

/// A scope is an exact absolute path or a directory prefix (`/usr/bin/`), in clean form.
fn valid_scope(s: &str) -> bool {
    s.starts_with('/') && s.len() > 1 && clean(s.strip_suffix('/').unwrap_or(s))
}

/// An absolute path with no empty, `.` or `..` segment and no trailing slash: the only form the policy will match against.
#[must_use]
pub fn clean(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    !rest.is_empty() && rest.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

fn parse_allow(w: &str) -> Option<Allow<'_>> {
    let (hash, scope) = w.split_once(':')?;
    valid_scope(scope).then_some(())?;
    Some(Allow { digest: hex32(hash)?, scope })
}

fn parse_key(w: &str) -> Option<Key<'_>> {
    let (public, scopes) = w.split_once(':')?;
    let mut k = Key { public: hex32(public)?, scopes: [""; MAX_SCOPES], n: 0 };
    for s in scopes.split(',') {
        valid_scope(s).then_some(())?;
        *k.scopes.get_mut(k.n)? = s;
        k.n += 1;
    }
    Some(k)
}

/// Whether `scope` covers `path`: an exact match, or `path` strictly inside a directory scope.
#[must_use]
pub fn scope_covers(scope: &str, path: &str) -> bool {
    if scope.ends_with('/') { path.len() > scope.len() && path.starts_with(scope) } else { path == scope }
}

/// Why an image may run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// Its SHA-256 is on the allowlist for this path.
    Hash,
    /// A detached signature verified under a key scoped to this path.
    Signature,
}

/// Why an image may not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The configuration had a word that did not parse: everything is refused.
    Malformed,
    /// The path is not absolute and clean (`..`, `//`, relative).
    BadPath,
    /// A dynamically linked image: its interpreter and libraries are not covered.
    Dynamic,
    /// Not on the allowlist for this path, and no key vouched for it.
    NotListed,
}

/// The decision for one image, with no I/O and no logging: see [`exec_policy`] for the event wrapper.
///
/// `sidecar` is the contents of `<path>.sig` when there is one: 64 raw bytes, an Ed25519 signature over the whole of `image`.
pub fn decide(cfg: &Config<'_>, path: &str, interp: Option<&str>, image: &[u8], sidecar: Option<&[u8]>) -> Result<Via, Reason> {
    if cfg.malformed != 0 {
        return Err(Reason::Malformed);
    }
    if !clean(path) {
        return Err(Reason::BadPath);
    }
    if interp.is_some() {
        return Err(Reason::Dynamic);
    }
    let digest: [u8; 32] = Sha256::digest(image).into();
    if cfg.allowed().any(|a| a.digest == digest && scope_covers(a.scope, path)) {
        return Ok(Via::Hash);
    }
    if let Some(sig) = sidecar.and_then(|s| <&[u8; akuma_ed25519::SIGNATURE_LEN]>::try_from(s).ok()) {
        let signed = cfg.keys().any(|k| k.scopes().iter().any(|s| scope_covers(s, path)) && akuma_ed25519::verify(&k.public, image, sig));
        if signed {
            return Ok(Via::Signature);
        }
    }
    Err(Reason::NotListed)
}

/// The exec policy over `cfg`: one `match` over every way the kernel introduces code. The kernel's table calls this from a
/// function of its own that supplies `cfg` (an [`ExecPolicy`](akuma_tokio_kernel_core::middleware::ExecPolicy) is a bare `fn`).
#[must_use]
pub fn exec_policy(cfg: &Config<'_>, ev: &ExecEvent) -> ExecVerdict {
    match ev.kind {
        ExecKind::Image { path, interp, image, sidecar } => match decide(cfg, path, interp, image, sidecar) {
            Ok(via) => {
                safe_print!(160, "[signed] pid {} may run {} ({:?})\n", ev.pid, path, via);
                ExecVerdict::AllowFromImage
            }
            Err(why) => {
                safe_print!(160, "[signed] pid {} refused {} ({:?})\n", ev.pid, path, why);
                ExecVerdict::Deny(EACCES)
            }
        },
        ExecKind::Map { inode: None } => ExecVerdict::Allow,
        ExecKind::Map { inode: Some(_) } => {
            safe_print!(96, "[signed] pid {} refused a file-backed PROT_EXEC mmap\n", ev.pid);
            ExecVerdict::Deny(EACCES)
        }
        ExecKind::Protect => {
            safe_print!(96, "[signed] pid {} refused mprotect(PROT_EXEC)\n", ev.pid);
            ExecVerdict::Deny(EACCES)
        }
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use akuma_tokio_kernel_core::params::MW;

    use super::*;

    // sha256("abc"), FIPS 180-2.
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    // RFC 8032 §7.1 TEST 2: this key signed the one byte 0x72.
    const PK2: &str = "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c";
    const SIG2: [u8; 64] = [
        0x92, 0xa0, 0x09, 0xa9, 0xf0, 0xd4, 0xca, 0xb8, 0x72, 0x0e, 0x82, 0x0b, 0x5f, 0x64, 0x25, 0x40, 0xa2, 0xb2, 0x7b, 0x54, 0x16, 0x50, 0x3f, 0x8f, 0xb3, 0x76,
        0x22, 0x23, 0xeb, 0xdb, 0x69, 0xda, 0x08, 0x5a, 0xc1, 0xe4, 0x3e, 0x15, 0x99, 0x6e, 0x45, 0x8f, 0x36, 0x13, 0xd0, 0xf1, 0x1d, 0x8c, 0x38, 0x7b, 0x2e, 0xae,
        0xb4, 0x30, 0x2a, 0xee, 0xb0, 0x0d, 0x29, 0x16, 0x12, 0xbb, 0x0c, 0x00,
    ];

    fn cfg(line: &str) -> Config<'_> {
        Config::from_params(&Params::new(line, MW).scoped("signed."))
    }

    #[test]
    fn a_listed_hash_runs_from_its_path_and_only_there() {
        let line = std::format!("mw.signed.allow={ABC}:/bin/busybox");
        let c = cfg(&line);
        assert_eq!(decide(&c, "/bin/busybox", None, b"abc", None), Ok(Via::Hash));
        assert_eq!(decide(&c, "/tmp/busybox", None, b"abc", None), Err(Reason::NotListed), "right bytes, wrong place");
        assert_eq!(decide(&c, "/bin/busybox", None, b"abd", None), Err(Reason::NotListed), "right place, other bytes");
    }

    #[test]
    fn a_directory_scope_covers_what_is_inside_it() {
        let line = std::format!("mw.signed.allow={ABC}:/usr/bin/");
        let c = cfg(&line);
        assert_eq!(decide(&c, "/usr/bin/kot", None, b"abc", None), Ok(Via::Hash));
        assert_eq!(decide(&c, "/usr/bin/", None, b"abc", None), Err(Reason::BadPath), "the directory itself");
        assert_eq!(decide(&c, "/usr/binx/kot", None, b"abc", None), Err(Reason::NotListed), "a sibling with the same prefix");
    }

    #[test]
    fn a_detached_signature_verifies_under_a_key_scoped_to_the_path() {
        let line = std::format!("mw.signed.key={PK2}:/usr/bin/,/opt/x");
        let c = cfg(&line);
        assert_eq!(decide(&c, "/usr/bin/tool", None, &[0x72], Some(&SIG2)), Ok(Via::Signature));
        assert_eq!(decide(&c, "/opt/x", None, &[0x72], Some(&SIG2)), Ok(Via::Signature));
        assert_eq!(decide(&c, "/bin/tool", None, &[0x72], Some(&SIG2)), Err(Reason::NotListed), "valid signature, key not scoped here");
        assert_eq!(decide(&c, "/usr/bin/tool", None, &[0x73], Some(&SIG2)), Err(Reason::NotListed), "signed bytes changed");
        assert_eq!(decide(&c, "/usr/bin/tool", None, &[0x72], None), Err(Reason::NotListed), "no sidecar");
        assert_eq!(decide(&c, "/usr/bin/tool", None, &[0x72], Some(&SIG2[..63])), Err(Reason::NotListed), "short sidecar");
        let mut bad = SIG2;
        bad[10] ^= 1;
        assert_eq!(decide(&c, "/usr/bin/tool", None, &[0x72], Some(&bad)), Err(Reason::NotListed), "one flipped bit");
    }

    #[test]
    fn nothing_is_configured_so_nothing_runs() {
        assert_eq!(decide(&cfg(""), "/bin/sh", None, b"abc", None), Err(Reason::NotListed));
    }

    #[test]
    fn a_malformed_word_refuses_everything() {
        for bad in [
            "mw.signed.allow=zz:/bin/x",                                           // not hex
            "mw.signed.allow=ba7816bf:/bin/x",                                     // short
            "mw.signed.allow=ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", // no scope
            "mw.signed.allow=ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad:bin/x", // relative scope
            "mw.signed.allow=ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad:/bin/../x", // dotdot scope
        ] {
            let c = cfg(bad);
            assert_ne!(c.malformed, 0, "{bad}");
            assert_eq!(decide(&c, "/bin/x", None, b"abc", None), Err(Reason::Malformed));
        }
        // One bad word spoils a good list: a trust list is all or nothing.
        let line = std::format!("mw.signed.allow={ABC}:/bin/x mw.signed.allow=oops");
        assert_eq!(decide(&cfg(&line), "/bin/x", None, b"abc", None), Err(Reason::Malformed));
    }

    #[test]
    fn more_entries_than_fit_is_malformed_not_silently_dropped() {
        let mut line = std::string::String::new();
        for i in 0..=MAX_ALLOW {
            write!(line, "mw.signed.allow={ABC}:/bin/x{i} ").unwrap();
        }
        assert_ne!(cfg(&line).malformed, 0);
    }

    #[test]
    fn a_path_that_is_not_clean_is_refused_before_anything_is_hashed() {
        let line = std::format!("mw.signed.allow={ABC}:/bin/");
        let c = cfg(&line);
        for p in ["bin/x", "/bin//x", "/bin/../bin/x", "/bin/./x", "/bin/x/", "/", ""] {
            assert_eq!(decide(&c, p, None, b"abc", None), Err(Reason::BadPath), "{p:?}");
        }
    }

    #[test]
    fn a_dynamic_image_is_refused_even_when_its_bytes_are_listed() {
        let line = std::format!("mw.signed.allow={ABC}:/bin/x");
        assert_eq!(decide(&cfg(&line), "/bin/x", Some("/lib/ld.so"), b"abc", None), Err(Reason::Dynamic));
    }

    #[test]
    fn the_policy_allows_from_the_image_and_refuses_the_other_ways_code_arrives() {
        let line = std::format!("mw.signed.allow={ABC}:/bin/x");
        let c = cfg(&line);
        let ev = |kind| exec_policy(&c, &ExecEvent { pid: 7, kind });
        assert_eq!(ev(ExecKind::Image { path: "/bin/x", interp: None, image: b"abc", sidecar: None }), ExecVerdict::AllowFromImage);
        assert_eq!(ev(ExecKind::Image { path: "/bin/x", interp: None, image: b"abd", sidecar: None }), ExecVerdict::Deny(EACCES));
        assert_eq!(ev(ExecKind::Map { inode: Some(3) }), ExecVerdict::Deny(EACCES));
        assert_eq!(ev(ExecKind::Map { inode: None }), ExecVerdict::Allow);
        assert_eq!(ev(ExecKind::Protect), ExecVerdict::Deny(EACCES));
    }
}
