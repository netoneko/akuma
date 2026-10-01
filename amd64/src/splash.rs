//! Quiet boot: the glowing Akuma mark on the television instead of a scrolling log.
//!
//! On the HP box the console *is* a television, and a boot that scrolls two
//! hundred lines past is neither readable nor pleasant to leave running. A quiet
//! boot hides the kernel's own messages (everything still goes to `dmesg`, which is
//! where it is read) and shows the mark, shifting colour, with the machine's
//! identity beneath it: the banner line, `uname -a`, the kernel version and an
//! uptime — drawn by `akuma_fbcon::splash`.
//!
//! # When it runs
//!
//! * **Starts** when the framebuffer console is created ([`begin`]) — before the
//!   first boot message, so none of them reach the screen.
//! * **Animates** from a daemon ([`spawn_daemon`]) once the scheduler and the timer
//!   exist. Until then it is a still frame; the USB root disk's bring-up spins
//!   with interrupts masked, and nothing can animate through that.
//! * **Ends** when the console shell spawns ([`end_for_console`]) — the screen is
//!   cleared and the shell has it — or when init is something other than `herd`
//!   ([`end_for_init`]), which will not spawn one.
//! * **Ends, and shows what happened,** if the boot crashes ([`crash`]: a panic, a
//!   fatal exception, `halt`) or takes longer than [`TIMEOUT_MS`]: quiet is lifted
//!   and the tail of the log is written to the screen. A boot that fails is never
//!   hidden behind a cheerful animation.
//!
//! # Switching it on and off
//!
//! On by default for a `no-tests` build (the build for a machine that is used, not
//! tested); `quiet` on the command line turns it on for any build; `fbverbose` turns
//! it off for any build — the escape hatch when the TV is the only way to watch a
//! hang.

use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use akuma_primitives::console::StackWriter;
use alloc::string::String;
use spinning_top::Spinlock;

use crate::{banner, serial};

/// Milliseconds between frames.
const TICK_MS: u64 = 50;
/// A splash this old means the console shell never came: show the log instead.
const TIMEOUT_MS: u64 = 90_000;
/// How much of the log tail a crash or timeout writes to the screen.
const REPLAY_BYTES: usize = 6_000;

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Frames drawn; time is `FRAMES * TICK_MS`, which needs no clock (the LAPIC timer
/// that `uptime_us` reads is not running for the first seconds of a boot).
static FRAMES: AtomicU64 = AtomicU64::new(0);
static PHASE: AtomicU8 = AtomicU8::new(0);
/// Set once the animation daemon exists. Before it, the status line is redrawn from
/// [`note_byte`] as each message completes; after it, the daemon redraws every frame.
static DAEMON: AtomicBool = AtomicBool::new(false);

/// The line being assembled from the log, and the last complete one.
struct Lines {
    cur: [u8; LINE_CAP],
    cur_len: usize,
    last: [u8; LINE_CAP],
    last_len: usize,
}
const LINE_CAP: usize = 72;
static LINES: Spinlock<Lines> =
    Spinlock::new(Lines { cur: [0; LINE_CAP], cur_len: 0, last: [0; LINE_CAP], last_len: 0 });
/// `/etc/console.conf`'s text, read by `run_init` while the splash was still up and
/// applied when it ends (applying it earlier would clear the screen under the splash).
static PENDING_CONF: Spinlock<Option<String>> = Spinlock::new(None);

/// What the status line says the machine is doing.
const PHASES: [&str; 3] = ["starting", "network", "starting services"];

/// Is the splash on screen?
#[must_use]
pub fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Say what boot is doing now (an index into [`PHASES`]); shown on the status line.
pub fn phase(n: u8) {
    PHASE.store(n.min(PHASES.len() as u8 - 1), Ordering::Relaxed);
}

/// Feed the splash a byte of the kernel's own output (called from `serial`, whatever
/// the quiet policy hides): the latest complete line becomes the splash's last status
/// line, so a hung boot still says where it stopped — the job the scrolling log did.
///
/// Never blocks and draws nothing from interrupt context it could deadlock in: a
/// contended lock just drops the byte.
pub fn note_byte(b: u8) {
    if !active() {
        return;
    }
    let Some(mut l) = LINES.try_lock() else { return };
    match b {
        b'\r' => {}
        b'\n' => {
            let line = l.cur;
            let n = l.cur_len;
            l.last[..n].copy_from_slice(&line[..n]);
            l.last_len = n;
            l.cur_len = 0;
            drop(l);
            // Until the daemon exists nothing else will redraw: do it now.
            if !DAEMON.load(Ordering::Acquire) {
                draw(FRAMES.load(Ordering::Relaxed));
            }
        }
        0x20..=0x7E => {
            if l.cur_len < LINE_CAP {
                let i = l.cur_len;
                l.cur[i] = b;
                l.cur_len += 1;
            }
        }
        _ => {}
    }
}

/// Start the quiet boot: hide the kernel's messages from the screen and draw the
/// first frame. Call once, right after the framebuffer console exists.
pub fn begin() {
    ACTIVE.store(true, Ordering::Release);
    serial::set_fb_quiet(true);
    draw(0);
}

/// Draw the frame for `frames` frames in.
fn draw(frames: u64) {
    let t_ms = frames * TICK_MS;
    let (title, uname, kernel, status);
    let mut a = StackWriter::<80>::new();
    let mut b = StackWriter::<96>::new();
    let mut c = StackWriter::<96>::new();
    let mut d = StackWriter::<80>::new();
    let build = akuma_syscalls_glue::version::BUILD_ID;
    let (sha, profile) = build.split_once('-').unwrap_or((build, ""));
    let _ = write!(a, "{}  {}{}", banner::VERSION_DESC, banner::RELEASE, banner::RELEASE_SUFFIX);
    let _ = write!(b, "Akuma akuma {} {} x86_64", banner::RELEASE, build);
    let _ = write!(c, "kernel  {}   commit {}   {}", banner::RELEASE, sha, profile);
    let phase = PHASES[usize::from(PHASE.load(Ordering::Relaxed))];
    let _ = write!(d, "up {}s - {}", t_ms / 1000, phase);
    // The latest line the kernel printed (trimmed), for the hang-diagnosis job.
    let mut e = StackWriter::<80>::new();
    if let Some(l) = LINES.try_lock() {
        let text = core::str::from_utf8(&l.last[..l.last_len]).unwrap_or("");
        let _ = write!(e, "> {}", text.trim());
    }
    title = a.as_str();
    uname = b.as_str();
    kernel = c.as_str();
    status = d.as_str();
    crate::multiboot2::fb_splash_frame(banner::ART, &[title, uname, kernel, status, e.as_str()], t_ms);
}

/// One animation step. Called by the daemon.
fn tick() {
    if !active() {
        return;
    }
    let n = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    if n * TICK_MS > TIMEOUT_MS {
        crash();
        return;
    }
    draw(n);
}

extern "C" fn splash_daemon() -> ! {
    loop {
        tick();
        let idle = !active();
        // Parked for a second once it has ended; for a frame while it runs. The
        // `timer_running` guard is the pump's: against a clock that does not move,
        // a deadline never arrives.
        if crate::lapic::timer_running() {
            let wait = if idle { 1_000_000 } else { TICK_MS * 1000 };
            crate::sched::block_until_deadline(crate::net::uptime_us() + wait);
        } else {
            crate::sched::yield_now();
        }
    }
}

/// Start the animation. Needs the scheduler and the timer (`boot::late_init`).
pub fn spawn_daemon() {
    if active() && crate::sched::spawn_daemon(splash_daemon).is_some() {
        DAEMON.store(true, Ordering::Release);
    }
}

/// The splash is over and the screen is the console's: clear it, apply the machine's
/// `/etc/console.conf` if `run_init` held it back, and tell the console its size.
///
/// Called as the console shell is spawned (the screen stays quiet — the shell is
/// what it is for), and by [`end_for_init`].
pub fn end_for_console() {
    if !ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    let conf = PENDING_CONF.lock().take();
    if let Some((rows, cols)) = crate::multiboot2::fb_splash_end(conf.as_deref(), true) {
        crate::console::set_size(rows, cols);
    }
}

/// `run_init` found an init that will not spawn a console shell (anything but
/// `herd`): there is nothing to wait for.
pub fn end_for_init() {
    end_for_console();
}

/// A crash, a halt or a timeout: stop the splash, lift the quiet, and write the
/// tail of the log to the screen so the failure is there to read.
///
/// Best effort and non-blocking — this can be called from a panic handler or a
/// fatal-exception dump that interrupted a console write, so it never waits on a
/// lock the interrupted code may hold.
pub fn crash() {
    if !ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    serial::set_fb_quiet(false);
    let _ = crate::multiboot2::fb_splash_end(None, false);
    serial::replay_klog_tail_to_fb(REPLAY_BYTES);
}

/// Hold `/etc/console.conf` until the splash ends. `true` if it was held (the
/// caller must not apply it now), `false` if there is no splash and it should be.
pub fn defer_config(text: &str) -> bool {
    if !active() {
        return false;
    }
    *PENDING_CONF.lock() = Some(String::from(text));
    true
}
