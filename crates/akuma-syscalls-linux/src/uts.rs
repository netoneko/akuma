//! One field of `struct utsname`, and the rule `sethostname(2)` /
//! `setdomainname(2)` apply to the bytes handed to it.
//!
//! `utsname` is six 65-byte NUL-padded fields (`sysname`, `nodename`, `release`,
//! `version`, `machine`, `domainname`). Four are build facts and stay static; the
//! other two — `nodename` and `domainname` — are what userspace may write, so
//! they need a value that can change and a rule for what is a legal change. The
//! rule is Linux's `__NEW_UTS_LEN`: at most 64 bytes, so the 65th byte is always
//! the terminator. No NUL scanning — Linux stores exactly the `len` bytes it was
//! given, and so does this.

/// Bytes in one `utsname` field, terminator included.
pub const FIELD_LEN: usize = 65;
/// Longest name `sethostname`/`setdomainname` accept (`__NEW_UTS_LEN`).
pub const NAME_MAX: usize = FIELD_LEN - 1;

/// The name was longer than [`NAME_MAX`] — `EINVAL`.
#[derive(Debug, PartialEq, Eq)]
pub struct TooLong;

/// A NUL-padded `utsname` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field([u8; FIELD_LEN]);

impl Field {
    /// An all-zero field (the empty name).
    #[must_use]
    pub const fn empty() -> Self {
        Self([0; FIELD_LEN])
    }

    /// Replace the field with `name`, zero-padding the rest. On error the field
    /// is untouched.
    ///
    /// # Errors
    /// [`TooLong`] if `name` is longer than [`NAME_MAX`].
    pub fn set(&mut self, name: &[u8]) -> Result<(), TooLong> {
        if name.len() > NAME_MAX {
            return Err(TooLong);
        }
        self.0 = [0; FIELD_LEN];
        self.0[..name.len()].copy_from_slice(name);
        Ok(())
    }

    /// The 65 bytes exactly as they sit in a `utsname`.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; FIELD_LEN] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_pads_with_zeros_and_replaces_the_old_name() {
        let mut f = Field::empty();
        f.set(b"gentoo-ryzen").unwrap();
        assert_eq!(&f.as_bytes()[..12], b"gentoo-ryzen");
        assert!(f.as_bytes()[12..].iter().all(|&b| b == 0));
        f.set(b"x").unwrap();
        assert_eq!(f.as_bytes()[0], b'x');
        assert!(f.as_bytes()[1..].iter().all(|&b| b == 0), "the old tail must not survive");
    }

    #[test]
    fn sixty_four_bytes_fit_and_sixty_five_do_not() {
        let mut f = Field::empty();
        assert_eq!(f.set(&[b'a'; NAME_MAX]), Ok(()));
        assert_eq!(f.as_bytes()[NAME_MAX], 0, "the terminator byte is never written");
        let before = f;
        assert_eq!(f.set(&[b'a'; NAME_MAX + 1]), Err(TooLong));
        assert_eq!(f, before, "a refused name changes nothing");
    }

    #[test]
    fn the_empty_name_is_legal() {
        let mut f = Field::empty();
        f.set(b"abc").unwrap();
        f.set(b"").unwrap();
        assert_eq!(f, Field::empty());
    }

    #[test]
    fn embedded_nul_is_stored_as_given() {
        let mut f = Field::empty();
        f.set(b"a\0b").unwrap();
        assert_eq!(&f.as_bytes()[..3], b"a\0b");
    }
}
