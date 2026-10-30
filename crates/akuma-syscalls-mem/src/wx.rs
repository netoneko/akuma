//! W^X without a JIT: the policy that no page is ever both writable and executable, and that a page
//! written since it was mapped can never be made executable.
//!
//! The rule is two checks, both pure:
//!
//! * **At the call:** a protection that asks for `PROT_WRITE | PROT_EXEC` together is refused
//!   ([`check_prot`]). `mmap` and `mprotect` both ask.
//! * **Per page:** a protection that asks for `PROT_EXEC` is refused on any page that has been written
//!   since it was mapped ([`check_page`]) - the JIT pattern (map RW, store code, flip to RX) is exactly
//!   this, and so is injected code. File-backed text a loader maps `PROT_EXEC` is never "written": the
//!   kernel fills it, the program does not.
//!
//! What counts as *written* is the caller's: the x86 PTE dirty bit (set by the CPU for a user store) plus
//! the kernel setting it on its own stores into user memory (`read(2)` into a buffer is a write). This crate
//! never sees a page table - the kernel hands it a `bool`.
//!
//! Both refusals answer `EACCES`, which is what OpenBSD's W^X and PaX's MPROTECT answer for the same
//! requests; Linux itself allows both, so this is deliberately *stricter than Linux*. Programs that need a
//! JIT (V8, a JVM, LuaJIT, `libffi` closures on some targets) cannot run under it; the kernel keeps a switch
//! (`wx=0` on the command line) rather than a per-program exemption.

use akuma_syscalls_linux::flags::prot;

const PROT_WRITE: u64 = prot::PROT_WRITE as u64;
const PROT_EXEC: u64 = prot::PROT_EXEC as u64;

/// The outcome of a W^X check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wx {
    /// Permitted.
    Allow,
    /// `PROT_WRITE` and `PROT_EXEC` asked for together.
    RefuseWriteExec,
    /// `PROT_EXEC` asked for on a page that has been written since it was mapped.
    RefuseWrittenPage,
}

impl Wx {
    /// The errno magnitude to answer, or `None` when the request is allowed.
    #[must_use]
    pub const fn errno(self) -> Option<u64> {
        match self {
            Self::Allow => None,
            Self::RefuseWriteExec | Self::RefuseWrittenPage => Some(EACCES),
        }
    }
}

/// `EACCES`.
pub const EACCES: u64 = 13;

/// Check the protection of a whole `mmap`/`mprotect` request: writable and executable together is refused.
#[must_use]
pub const fn check_prot(prot: u64) -> Wx {
    if prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0 { Wx::RefuseWriteExec } else { Wx::Allow }
}

/// Check one page of an `mprotect` that asks for `prot`: executable is refused if the page was written.
#[must_use]
pub const fn check_page(prot: u64, page_written: bool) -> Wx {
    if prot & PROT_EXEC != 0 && page_written { Wx::RefuseWrittenPage } else { Wx::Allow }
}

/// Check a whole range for `mprotect`: the call, then each page's written bit (stopping at the first
/// offender, so the caller changes nothing when this is not `Allow`).
#[must_use]
pub fn check_range(prot: u64, mut page_written: impl FnMut(u64) -> bool, pages: u64) -> Wx {
    let w = check_prot(prot);
    if w != Wx::Allow {
        return w;
    }
    if prot & PROT_EXEC == 0 {
        return Wx::Allow; // dropping or never gaining exec needs no per-page look
    }
    for i in 0..pages {
        let v = check_page(prot, page_written(i));
        if v != Wx::Allow {
            return v;
        }
    }
    Wx::Allow
}

#[cfg(test)]
mod tests {
    use super::*;
    const PROT_NONE: u64 = prot::PROT_NONE as u64;
    const PROT_READ: u64 = prot::PROT_READ as u64;

    #[test]
    fn write_plus_exec_is_refused_in_every_spelling() {
        assert_eq!(check_prot(PROT_WRITE | PROT_EXEC), Wx::RefuseWriteExec);
        assert_eq!(check_prot(PROT_READ | PROT_WRITE | PROT_EXEC), Wx::RefuseWriteExec);
        for ok in [PROT_NONE, PROT_READ, PROT_WRITE, PROT_READ | PROT_WRITE, PROT_EXEC, PROT_READ | PROT_EXEC] {
            assert_eq!(check_prot(ok), Wx::Allow, "{ok:#x}");
        }
    }

    #[test]
    fn exec_on_a_written_page_is_refused_and_only_exec() {
        assert_eq!(check_page(PROT_READ | PROT_EXEC, true), Wx::RefuseWrittenPage);
        assert_eq!(check_page(PROT_EXEC, true), Wx::RefuseWrittenPage);
        // Written pages stay usable as data.
        assert_eq!(check_page(PROT_READ, true), Wx::Allow);
        assert_eq!(check_page(PROT_READ | PROT_WRITE, true), Wx::Allow);
        // A never-written page may become executable.
        assert_eq!(check_page(PROT_READ | PROT_EXEC, false), Wx::Allow);
    }

    #[test]
    fn a_range_is_refused_if_any_one_page_was_written() {
        let written = |i: u64| i == 3;
        assert_eq!(check_range(PROT_READ | PROT_EXEC, written, 8), Wx::RefuseWrittenPage);
        assert_eq!(check_range(PROT_READ | PROT_EXEC, written, 3), Wx::Allow, "the written page is outside the range");
        assert_eq!(check_range(PROT_READ, written, 8), Wx::Allow, "no exec asked: nothing to refuse");
        assert_eq!(check_range(PROT_WRITE | PROT_EXEC, |_| false, 8), Wx::RefuseWriteExec, "the call itself is refused first");
    }

    #[test]
    fn the_range_walk_stops_at_the_first_offender() {
        let mut asked = 0;
        let v = check_range(PROT_EXEC, |i| { asked += 1; i == 2 }, 100);
        assert_eq!(v, Wx::RefuseWrittenPage);
        assert_eq!(asked, 3);
    }

    #[test]
    fn refusals_answer_eacces_and_allow_answers_nothing() {
        assert_eq!(Wx::Allow.errno(), None);
        assert_eq!(Wx::RefuseWriteExec.errno(), Some(13));
        assert_eq!(Wx::RefuseWrittenPage.errno(), Some(13));
    }
}
