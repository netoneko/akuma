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

/// Fastest and slowest interval between frames. The actual one adapts to what the
/// last frame cost ([`tick_ms`]): on an uncached framebuffer a frame was ~43 ms
/// (71 MB/s), so a fixed 50 ms was an 86% duty cycle that slowed the very boot it
/// decorates. With the framebuffer write-combining (`multiboot2::map_wc`) frames are
/// cheap and the interval falls to [`MIN_TICK_MS`]; if that mapping ever fails the
/// splash backs off by itself instead of eating the boot.
const MIN_TICK_MS: u64 = 50;
const MAX_TICK_MS: u64 = 150;
/// A splash this old means the console shell never came: show the log instead.
const TIMEOUT_MS: u64 = 90_000;
/// How much of the log tail a crash or timeout writes to the screen.
const REPLAY_BYTES: usize = 6_000;

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The TSC when the splash began, and when the last frame was drawn. Time is TSC
/// based — not a frame count, and not `uptime_us` (the LAPIC timer behind it is not
/// running for the first seconds of a boot) — so the colour keeps moving however the
/// frames get scheduled.
static START_TSC: AtomicU64 = AtomicU64::new(0);
static LAST_TSC: AtomicU64 = AtomicU64::new(0);
/// A frame is being drawn: a second drawer (the daemon, a boot wait loop, a log line)
/// skips rather than queue behind it.
static DRAWING: AtomicBool = AtomicBool::new(false);
/// Frames drawn, and the TSC cycles they took in total and at worst, for the one line
/// `dmesg` gets when the splash ends — the measurement behind any framebuffer-speed work.
static FRAMES: AtomicU64 = AtomicU64::new(0);
static DRAW_CYCLES: AtomicU64 = AtomicU64::new(0);
static DRAW_MAX_CYCLES: AtomicU64 = AtomicU64::new(0);
/// What the most recent frame cost, in TSC cycles; 0 before the first.
static LAST_COST: AtomicU64 = AtomicU64::new(0);

/// Interval until the next frame: three times the last frame's cost (a duty cycle of
/// at most a quarter), clamped to `MIN_TICK_MS..=MAX_TICK_MS`.
fn tick_ms() -> u64 {
    let per_ms = (tsc_hz() / 1000).max(1);
    (LAST_COST.load(Ordering::Relaxed) / per_ms * 3).clamp(MIN_TICK_MS, MAX_TICK_MS)
}

/// The TSC rate; before the LAPIC calibration has run, a guess (a frame's *rate* is
/// all this affects, and the guess is the same order as any machine this runs on).
fn tsc_hz() -> u64 {
    match crate::lapic::tsc_hz() {
        0 => 3_000_000_000,
        hz => hz,
    }
}

fn rdtsc() -> u64 {
    // SAFETY: RDTSC is unprivileged and present on every x86_64.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Milliseconds since the splash began.
pub fn elapsed_ms() -> u64 {
    rdtsc().wrapping_sub(START_TSC.load(Ordering::Relaxed)) / (tsc_hz() / 1000).max(1)
}
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
    let n = n.min(PHASES.len() as u8 - 1);
    PHASE.store(n, Ordering::Relaxed);
    // When each phase began, for the `[boot]` line (`elapsed_ms() + 1` so 0 means unset).
    PHASE_MS[usize::from(n)].store(elapsed_ms() + 1, Ordering::Relaxed);
}

/// Milliseconds (+1) from the splash's start to the start of each [`PHASES`] entry.
static PHASE_MS: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

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
                frame();
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
    PHASE_MS[0].store(1, Ordering::Relaxed);
    let now = rdtsc();
    START_TSC.store(now, Ordering::Relaxed);
    LAST_TSC.store(now, Ordering::Relaxed);
    ACTIVE.store(true, Ordering::Release);
    serial::set_fb_quiet(true);
    frame();
}

/// Draw one frame now, unless one is already being drawn.
fn frame() {
    if DRAWING.swap(true, Ordering::Acquire) {
        return;
    }
    let t0 = rdtsc();
    LAST_TSC.store(t0, Ordering::Relaxed);
    FRAMES.fetch_add(1, Ordering::Relaxed);
    draw(elapsed_ms());
    let took = rdtsc().wrapping_sub(t0);
    DRAW_CYCLES.fetch_add(took, Ordering::Relaxed);
    DRAW_MAX_CYCLES.fetch_max(took, Ordering::Relaxed);
    LAST_COST.store(took, Ordering::Relaxed);
    DRAWING.store(false, Ordering::Release);
}

/// Draw a frame if one is due. **Cheap when it is not** (an atomic load, a `rdtsc`
/// and a compare), so the long synchronous waits of a boot — the xHCI bring-up, the
/// DHCP settle — call it from their own loops: a daemon cannot animate through them
/// (the boot task never yields there, and a second task touching the network stack
/// at that point is what `net::settle_for_dhcp` warns about), but the boot task can
/// draw a frame itself every [`tick_ms`].
pub fn pulse() {
    if !active() {
        return;
    }
    let due = tsc_hz() / 1000 * tick_ms();
    if rdtsc().wrapping_sub(LAST_TSC.load(Ordering::Relaxed)) >= due {
        if elapsed_ms() > TIMEOUT_MS {
            crash();
            return;
        }
        frame();
    }
}

/// Draw the frame `t_ms` into the splash.
fn draw(t_ms: u64) {
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

extern "C" fn splash_daemon() -> ! {
    loop {
        pulse();
        let idle = !active();
        // Parked for a second once it has ended; for a frame while it runs. The
        // `timer_running` guard is the pump's: against a clock that does not move,
        // a deadline never arrives.
        if crate::lapic::timer_running() {
            let wait = if idle { 1_000_000 } else { tick_ms() * 1000 };
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
    // For `dmesg` only: did the animation actually run? (A boot that spends its time in
    // waits that never call `pulse` shows a still frame, and this says so.)
    // How long the machine took to come up, from the moment the console exists (so after
    // firmware and GRUB, which this cannot see) to the shell's prompt, and where it went.
    let mut b = StackWriter::<160>::new();
    let _ = write!(b, "[boot] shell after {} ms (", elapsed_ms());
    for (i, name) in PHASES.iter().enumerate() {
        let at = PHASE_MS[i].load(Ordering::Relaxed);
        if at > 0 {
            let _ = write!(b, "{}{} +{}", if i == 0 { "" } else { ", " }, name, at - 1);
        }
    }
    let _ = write!(b, ")\n");
    serial::klog_only(b.as_str());
    let mut w = StackWriter::<96>::new();
    let frames = FRAMES.load(Ordering::Relaxed).max(1);
    let per_ms = (tsc_hz() / 1000).max(1);
    let _ = write!(
        w,
        "[splash] ended after {} ms: {} frames, draw avg {} us max {} us\n",
        elapsed_ms(),
        frames,
        DRAW_CYCLES.load(Ordering::Relaxed) / frames * 1000 / per_ms,
        DRAW_MAX_CYCLES.load(Ordering::Relaxed) * 1000 / per_ms,
    );
    serial::klog_only(w.as_str());
    // The banner goes on the screen the shell is about to have, before its prompt:
    // the quiet boot hid the one `run_init` prints, and a machine that boots to a
    // prompt should still say what it is.
    banner::print_visible();
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
