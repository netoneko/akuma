//! Which program a kernel runs as init: `init=` and `initargs=` on the boot command line.
//!
//! One reader for every Akuma kernel, so `init=/bin/sshd initargs=-d` means the same thing on
//! AArch64, on amd64's PVH and multiboot2 boots, and under Firecracker. Each kernel keeps its own
//! **default** (what it runs when the line names nothing), because those differ on purpose: AArch64
//! has always started `herd daemon`, a GRUB boot gives the console `/bin/sh`, and a PVH boot with
//! no `init=` runs nothing and halts after its verdict.
//!
//! `initargs=` is **comma-separated**: the line splits on whitespace, so one word cannot hold a
//! space, and `initargs=uname,-a` is `busybox uname -a`. Empty elements are dropped. `argv[0]` is
//! always the path; the arguments follow it.

/// What to run as init: a path and its comma-separated arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Init<'a> {
    /// The program, as the command line spelled it (a `/bin -> usr/bin` link is the loader's to follow).
    pub path: &'a str,
    /// `initargs=`'s value, unsplit. [`Init::args`] splits it.
    pub args: &'a str,
}

impl<'a> Init<'a> {
    /// A fixed init, for a kernel's default.
    #[must_use]
    pub const fn new(path: &'a str, args: &'a str) -> Self {
        Self { path, args }
    }

    /// What `line` names, if it names a non-empty `init=`. The first `init=` and the first
    /// `initargs=` win, as for every other word on the line. An `initargs=` with no `init=` is
    /// ignored: it says how to run a program the line did not choose.
    #[must_use]
    pub fn from_line(line: &'a str) -> Option<Self> {
        let path = word(line, "init=").filter(|p| !p.is_empty())?;
        Some(Self { path, args: word(line, "initargs=").unwrap_or("") })
    }

    /// [`Init::from_line`], or `default` when the line names no init.
    #[must_use]
    pub fn from_line_or(line: &'a str, default: Self) -> Self {
        Self::from_line(line).unwrap_or(default)
    }

    /// The arguments after `argv[0]`, in order, empty elements dropped.
    pub fn args(&self) -> impl Iterator<Item = &'a str> + 'a {
        self.args.split(',').filter(|a| !a.is_empty())
    }
}

/// The value of the first whitespace-separated word that starts with `key` (which includes the `=`).
fn word<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_ascii_whitespace().find_map(|w| w.strip_prefix(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_and_args() {
        let i = Init::from_line("console=ttyS0 init=/bin/busybox initargs=uname,-a smp=2").unwrap();
        assert_eq!(i.path, "/bin/busybox");
        assert_eq!(i.args().collect::<alloc_free::Vec4>().as_slice(), ["uname", "-a"]);
    }

    #[test]
    fn no_init_word_is_none_and_the_default_wins() {
        assert_eq!(Init::from_line("smp=4 initargs=x"), None);
        let d = Init::new("/bin/herd", "daemon");
        assert_eq!(Init::from_line_or("smp=4", d), d);
        assert_eq!(Init::from_line_or("", d), d);
    }

    #[test]
    fn an_empty_init_is_no_init() {
        assert_eq!(Init::from_line("init= initargs=a"), None);
    }

    #[test]
    fn a_longer_key_is_not_init() {
        // `initargs=` and `xinit=` must not be read as `init=`.
        assert_eq!(Init::from_line("initargs=a xinit=/bin/sh"), None);
    }

    #[test]
    fn first_word_wins_and_empty_args_drop() {
        let i = Init::from_line("init=/a init=/b initargs=,x,,y, initargs=z").unwrap();
        assert_eq!(i.path, "/a");
        assert_eq!(i.args().collect::<alloc_free::Vec4>().as_slice(), ["x", "y"]);
        assert_eq!(Init::new("/bin/sshd", "").args().count(), 0);
    }

    /// A four-slot collector, so the tests need no `alloc`.
    mod alloc_free {
        #[derive(Default)]
        pub struct Vec4<'a>([&'a str; 4], usize);
        impl<'a> Vec4<'a> {
            pub fn as_slice(&self) -> &[&'a str] {
                &self.0[..self.1]
            }
        }
        impl<'a> FromIterator<&'a str> for Vec4<'a> {
            fn from_iter<I: IntoIterator<Item = &'a str>>(it: I) -> Self {
                let mut v = Self::default();
                for s in it {
                    v.0[v.1] = s;
                    v.1 += 1;
                }
                v
            }
        }
    }
}
