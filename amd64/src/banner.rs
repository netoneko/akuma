//! The sign-on banner: Akuma's mark, then the version line.
//!
//! Printed once, right before `run_init` hands the machine to `sshd` — so on the
//! HP box, whose console is a television, the last thing on screen before the
//! login service comes up says what is running. The art is the signature; the
//! version is the fact.

use crate::serial;

/// `uname -r` — the kernel release, plus this target's suffix.
///
/// The doc here claimed it was "shared with `usermode::UTSNAME` so the banner
/// and `uname(2)` cannot disagree". There is no `usermode::UTSNAME` — `uname`
/// folded into `akuma-syscalls-glue` at C1 step 3 — and the two did disagree:
/// this literal said `0.1.0-amd64` while `uname -r` said `0.1.0` only by
/// coincidence, both of them wrong (the glue crate's package version, not the
/// kernel's). Now it *is* shared, so [`print`] writes this const and then the
/// suffix rather than one string.
pub const RELEASE: &str = akuma_syscalls_glue::version::RELEASE;

/// What this target appends to [`RELEASE`], so a banner on the HP box's
/// television says which of the two kernels is up.
pub const RELEASE_SUFFIX: &str = "-amd64";

/// `uname -v` — the longer description, same source as above.
pub const VERSION_DESC: &str = "Akuma/amd64";

/// Akuma's mark, the 40-column cut — a local copy of `src/akuma_40.txt`, the
/// same art `userspace/sshd` prints on an interactive login (its own
/// `akuma_40.txt`). Kept here rather than reaching across the source tree, the
/// way sshd's copy is.
pub const ART: &str = include_str!("akuma_40.txt");

/// Print [`ART`] then the version line. `run_init` calls this on both boot
/// paths just before the init program starts.
pub fn print() {
    serial::puts("\n");
    for line in ART.lines() {
        serial::puts(line);
        serial::puts("\n");
    }
    serial::puts("\n  ");
    serial::puts(VERSION_DESC);
    serial::puts("  ");
    serial::puts(RELEASE);
    serial::puts(RELEASE_SUFFIX);
    serial::puts("\n\n");
}

/// [`print`] for a console that is being kept quiet: the mark **in colour** plus the
/// `uname -a` line, written straight to the framebuffer console (not through
/// `serial`, so the thirty-odd kilobytes of colour escapes stay out of `dmesg`).
///
/// A quiet boot (`splash`) hides `run_init`'s [`print`]; the splash ends just before
/// the shell starts, and this puts the banner on the cleared screen above its prompt.
/// The colours are the splash's wave frozen at the moment the boot finished, so each
/// boot's cat is a slightly different one. The version line is not repeated: `uname`
/// carries it.
pub fn print_visible() {
    use akuma_fbcon::splash::art_color;

    fn put(s: &str) {
        for b in s.bytes() {
            if b == b'\n' {
                crate::multiboot2::mirror_byte(b'\r');
            }
            crate::multiboot2::mirror_byte(b);
        }
    }

    let t_ms = crate::splash::elapsed_ms();
    put("\n");
    for (y, line) in ART.lines().enumerate() {
        let mut last = None;
        for (x, ch) in line.bytes().enumerate() {
            if ch != b' ' {
                let c = art_color(ch, x, y, t_ms);
                // Only when the colour changes: runs of one character share it.
                if last != Some((c.r, c.g, c.b)) {
                    let mut w = akuma_primitives::console::StackWriter::<24>::new();
                    let _ = core::fmt::Write::write_fmt(&mut w, format_args!("\x1b[38;2;{};{};{}m", c.r, c.g, c.b));
                    put(w.as_str());
                    last = Some((c.r, c.g, c.b));
                }
            }
            crate::multiboot2::mirror_byte(ch);
        }
        put("\x1b[0m\n");
    }
    put("\n  Akuma akuma ");
    put(RELEASE);
    put(" ");
    put(akuma_syscalls_glue::version::BUILD_ID);
    put(" x86_64\n\n");
}
