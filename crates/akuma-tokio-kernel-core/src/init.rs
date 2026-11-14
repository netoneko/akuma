//! The init program from the command line (`init=`, `initargs=`, `initenv=`), in fixed slots:
//! parsed before user mode exists, read again when init exits.

/// argv entries beyond `argv[0]`, and each one's byte budget.
pub const MAX_ARGV: usize = 16;
pub const ARG_LEN: usize = 96;
/// Environment entries (`initenv=`, comma-separated `KEY=value`, no comma or space in a value).
pub const MAX_ENV: usize = 8;

/// The init program. No heap.
#[derive(Clone, Copy)]
pub struct Prog {
    path: [u8; ARG_LEN],
    args: [[u8; ARG_LEN]; MAX_ARGV],
    n_args: usize,
    env: [[u8; ARG_LEN]; MAX_ENV],
    n_env: usize,
}

impl Default for Prog {
    fn default() -> Self {
        Self::new()
    }
}

impl Prog {
    #[must_use]
    pub const fn new() -> Self {
        Self { path: [0; ARG_LEN], args: [[0; ARG_LEN]; MAX_ARGV], n_args: 0, env: [[0; ARG_LEN]; MAX_ENV], n_env: 0 }
    }

    /// Fill from the boot command line. `initargs`/`initenv` are comma-separated
    /// (`initargs=sh,/etc/guest.sh`); entries past the slot count are dropped.
    pub fn parse(&mut self, cmdline: &str) {
        for token in cmdline.split_ascii_whitespace() {
            if let Some(v) = token.strip_prefix("init=") {
                store_slot(&mut self.path, v);
            } else if let Some(v) = token.strip_prefix("initargs=") {
                for a in v.split(',') {
                    if self.n_args < MAX_ARGV {
                        store_slot(&mut self.args[self.n_args], a);
                        self.n_args += 1;
                    }
                }
            } else if let Some(v) = token.strip_prefix("initenv=") {
                for a in v.split(',') {
                    if self.n_env < MAX_ENV {
                        store_slot(&mut self.env[self.n_env], a);
                        self.n_env += 1;
                    }
                }
            }
        }
    }

    /// argv[0], as the string it was stored as.
    #[must_use]
    pub fn path_str(&self) -> &str {
        str_until_nul(&self.path)
    }

    /// How many argv entries beyond `argv[0]`.
    #[must_use]
    pub fn n_args(&self) -> usize {
        self.n_args
    }

    /// One argv entry beyond `argv[0]`.
    #[must_use]
    pub fn arg_str(&self, i: usize) -> &str {
        str_until_nul(&self.args[i])
    }

    /// How many environment entries.
    #[must_use]
    pub fn n_env(&self) -> usize {
        self.n_env
    }

    /// One environment entry.
    #[must_use]
    pub fn env_str(&self, i: usize) -> &str {
        str_until_nul(&self.env[i])
    }
}

/// The `&str` a NUL-padded slot holds ("" when unset).
#[must_use]
pub fn str_until_nul(buf: &[u8]) -> &str {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..end]).unwrap_or("")
}

/// Store `s` NUL-terminated in `dst`, truncated to leave room for the NUL.
pub fn store_slot(dst: &mut [u8], s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&b[..n]);
    dst[n] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_init_args_env() {
        let mut p = Prog::new();
        p.parse("console=ttyS0 init=/bin/sh initargs=sh,/etc/guest.sh initenv=A=1,B=2");
        assert_eq!(p.path_str(), "/bin/sh");
        assert_eq!((p.n_args(), p.arg_str(0), p.arg_str(1)), (2, "sh", "/etc/guest.sh"));
        assert_eq!((p.n_env(), p.env_str(0), p.env_str(1)), (2, "A=1", "B=2"));
    }

    #[test]
    fn caps_and_truncates() {
        let mut p = Prog::new();
        let many = "initargs=a,a,a,a,a,a,a,a,a,a,a,a,a,a,a,a,a,a,a,a";
        p.parse(many);
        assert_eq!(p.n_args(), MAX_ARGV);
        let mut slot = [0u8; 4];
        store_slot(&mut slot, "abcdef");
        assert_eq!(str_until_nul(&slot), "abc");
    }
}
