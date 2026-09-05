//! The sign-on banner: Akuma's cat, then the version line.
//!
//! Printed once, right before `run_init` hands the machine to `sshd` — so on the
//! HP box, whose console is a television, the last thing on screen before the
//! login service comes up says what is running. The cat is the signature; the
//! version is the fact.

use crate::serial;

/// `uname -r` — the kernel release. Shared with `usermode::UTSNAME` so the
/// banner and `uname(2)` cannot disagree.
pub const RELEASE: &str = "0.1.0-amd64";

/// `uname -v` — the longer description, same source as above.
pub const VERSION_DESC: &str = "Akuma/amd64 (x86_64 bring-up)";

/// Akuma's cat. A demon has horns; a bring-up kernel has a cat with horns.
/// Deliberately inside 40 columns so it does not wrap on the narrowest grid
/// `akuma-fbcon` will fall back to (72 columns — see its `MIN_COLS`).
const CAT: &str = r"       /\_/\        /\_/\
      ( o.o )      ( -.- )   akuma
       > ^ <  ~~~~~ > ^ <
";

/// Print [`CAT`] then the version line. `run_init` calls this on both boot
/// paths just before the init program starts.
pub fn print() {
    serial::puts("\n");
    serial::puts(CAT);
    serial::puts("  ");
    serial::puts(VERSION_DESC);
    serial::puts("  ");
    serial::puts(RELEASE);
    serial::puts("\n\n");
}
