//! The machine half's two edges with its device drivers (`crates/akuma-drivers-amd64`).
//!
//! **Registration, one call per class.** A driver crate cannot be a dependency of this one
//! (the drivers call into the kernel, so that would be a cycle), so each class of device the
//! boot paths probe is a [`Registered`] table of function pointers. The bin's `before_main`
//! hook (`akuma_kernel::entry!`'s argument) runs `akuma_drivers_amd64::register()`, which
//! fills a slot for every driver its features compiled in. The boot paths call the
//! forwarding functions below; a class nobody registered is a no-op, which is exactly what a
//! build without that driver must be. So turning a driver's feature off removes its code and
//! its probe and leaves no `cfg` here.
//!
//! **Services, one re-export per item.** [`services`] is everything a driver may reach in the
//! machine half. Like `crate::testing`, its list is the whole interface.

use akuma_primitives::Registered;
use akuma_ryzen_amd64::MachineDescription;

/// Sound: one controller, probed once PCI has been scanned.
#[derive(Clone, Copy)]
pub struct Sound {
    /// Find the controller, bring it and its codec up and register `/dev/dsp`'s backend.
    /// `selftest` (the `hdatest` command-line flag) also plays a test tone.
    pub init: fn(selftest: bool),
}

static SOUND: Registered<Sound> = Registered::new("no sound driver registered");

/// Install the sound driver. Idempotent by `Registered`'s contract: a second call is ignored.
pub fn register_sound(driver: Sound) {
    SOUND.register(driver);
}

/// Probe for a sound controller. Nothing when no driver is compiled in.
pub fn sound_init(selftest: bool) {
    if let Some(d) = SOUND.get() {
        (d.init)(selftest);
    }
}

/// Pointer: `/dev/input/mouse0` and the console's mouse reports. Found by a table scan on the
/// multiboot2 path and brought up just before `init`.
#[derive(Clone, Copy)]
pub struct Pointer {
    /// Decide whether the machine has the device (ACPI tables, `touchpad=`). No bring-up.
    pub discover: fn(&MachineDescription, &str),
    /// Bring up what [`Pointer::discover`] found.
    pub init: fn(),
    /// Drain the device into its queues; the console pump calls this every pass.
    pub poll: fn(),
    /// The device came up: `/dev/input/mouse0` exists.
    pub present: fn() -> bool,
    /// A `/dev/input/mouse0` read would not block.
    pub has_events: fn() -> bool,
    /// `read(2)` on `/dev/input/mouse0`: whole records, or `EAGAIN`.
    pub dev_read: fn(buf: u64, len: usize) -> u64,
    /// `write(2)` on `/dev/input/mouse0`: control lines.
    pub dev_write: fn(buf: u64, len: usize) -> u64,
    /// Mouse reports for a console program that turned reporting on, typed in like keys.
    pub console_bridge: fn(out: &mut [u8]) -> usize,
}

static POINTER: Registered<Pointer> = Registered::new("no pointer driver registered");

/// Install the pointer driver. Idempotent by `Registered`'s contract.
pub fn register_pointer(driver: Pointer) {
    POINTER.register(driver);
}

pub fn pointer_discover(machine: &MachineDescription, cmdline: &str) {
    if let Some(d) = POINTER.get() {
        (d.discover)(machine, cmdline);
    }
}

pub fn pointer_init() {
    if let Some(d) = POINTER.get() {
        (d.init)();
    }
}

pub fn pointer_poll() {
    if let Some(d) = POINTER.get() {
        (d.poll)();
    }
}

#[must_use]
pub fn pointer_present() -> bool {
    POINTER.get().is_some_and(|d| (d.present)())
}

#[must_use]
pub fn pointer_has_events() -> bool {
    POINTER.get().is_some_and(|d| (d.has_events)())
}

/// `ENODEV` without a driver; unreachable in practice, because `open` refuses the node
/// while [`pointer_present`] is false.
#[must_use]
pub fn pointer_dev_read(buf: u64, len: usize) -> u64 {
    POINTER.get().map_or(crate::fd::errno::ENODEV, |d| (d.dev_read)(buf, len))
}

/// As [`pointer_dev_read`].
#[must_use]
pub fn pointer_dev_write(buf: u64, len: usize) -> u64 {
    POINTER.get().map_or(crate::fd::errno::ENODEV, |d| (d.dev_write)(buf, len))
}

#[must_use]
pub fn pointer_console_bridge(out: &mut [u8]) -> usize {
    POINTER.get().map_or(0, |d| (d.console_bridge)(out))
}

/// What a display driver's [`Display::init`] brought up, for the `font:`/`fb:` lines of the
/// multiboot2 banner.
pub struct ConsoleUp {
    pub fb: akuma_multiboot2::Framebuffer,
    pub font: &'static str,
    pub fw: usize,
    pub fh: usize,
    pub fcols: usize,
    pub frows: usize,
    pub fscale: usize,
}

/// Display: the framebuffer console GRUB's linear framebuffer becomes (and, through it,
/// `/dev/fb0`, the boot splash and the console's pointer cell).
///
/// Registered as a `&'static` table rather than by value because [`mirror_byte`] runs for
/// every byte the kernel prints. Without a driver every entry reads as "no framebuffer
/// console": the console is the serial port, the splash and the colour band draw nothing,
/// `/dev/fb0` is never registered, and what the console would have drawn goes to the
/// `fbtrace` stand-in (`multiboot2::fb_trace_byte`) exactly as on a machine without a
/// framebuffer.
pub struct Display {
    /// Bring the console up on the firmware framebuffer, or say why there is none.
    pub init: fn(&akuma_multiboot2::BootInfo<'_>) -> Result<ConsoleUp, &'static str>,
    /// Draw into a heap copy from now on; whether that took.
    pub enable_double_buffer: fn() -> bool,
    /// Draw one byte; `false` if there is no console to draw it on.
    pub mirror_byte: fn(u8) -> bool,
    /// Stop drawing (a program owns `/dev/fb0`).
    pub mute: fn(),
    /// Resume drawing and repaint the grid.
    pub unmute_and_repaint: fn(),
    /// Backlight-style dimming, percent.
    pub set_dim: fn(u8),
    /// Draw again whatever muted it: the crash path.
    pub force_unmute: fn(),
    /// The console pump's quiet lap: draw the cursor; a changed `(rows, cols)` once.
    pub cursor_idle: fn() -> Option<(u16, u16)>,
    /// The console's `(rows, cols)`.
    pub grid: fn() -> Option<(u16, u16)>,
    /// Apply a `/etc/fbcon.conf` text; the new `(rows, cols)`.
    pub apply_config: fn(&str) -> Option<(u16, u16)>,
    /// One splash frame: the mark, the info lines, `t_ms` into the boot.
    pub splash_frame: fn(&str, &[&str], u64),
    /// End the splash, optionally applying a config; the new `(rows, cols)`.
    pub splash_end: fn(Option<&str>, bool) -> Option<(u16, u16)>,
    /// Bytes the terminal answers a query with (`CSI 6 n`, ...), typed in like keys.
    pub take_reply: fn(&mut [u8]) -> usize,
    /// What the program on the console asked of the pointer, and the grid to place it on.
    pub mouse: fn() -> Option<(u16, bool, [u32; 6])>,
    /// Where to draw the pointer cell, or hide it.
    pub set_pointer: fn(Option<(usize, usize)>),
    /// `TIOCSWINSZ`: a smaller view of the grid; the `(rows, cols)` that took.
    pub set_view: fn(u16, u16) -> Option<(u16, u16)>,
    /// The `[fb]` summary line.
    pub summary: fn(),
    /// Give this application processor the write-combining PAT entry the BSP set.
    pub pat_init_ap: fn(),
    /// [`crate::multiboot2`]'s `cycle_forever` colour band, step `i`.
    pub band: fn(usize),
    /// The splash's colour for an art character, as `(r, g, b)`.
    pub art_color: fn(u8, usize, usize, u64) -> (u8, u8, u8),
}

static DISPLAY: Registered<&'static Display> = Registered::new("no display driver registered");

/// Install the display driver. Idempotent by `Registered`'s contract.
pub fn register_display(driver: &'static Display) {
    DISPLAY.register(driver);
}

/// [`Display::init`]; `Err` without a driver, which the multiboot2 banner reports as
/// `fb: none, headless (..)`.
///
/// # Errors
/// Why there is no framebuffer console.
pub fn display_init(info: &akuma_multiboot2::BootInfo<'_>) -> Result<ConsoleUp, &'static str> {
    DISPLAY.get().map_or(Err("no display driver in this build (`fbcon` off)"), |d| (d.init)(info))
}

#[must_use]
pub fn fb_enable_double_buffer() -> bool {
    DISPLAY.get().is_some_and(|d| (d.enable_double_buffer)())
}

/// One byte to the framebuffer console; without one, to the `fbtrace` stand-in.
pub fn mirror_byte(byte: u8) {
    if !DISPLAY.get().is_some_and(|d| (d.mirror_byte)(byte)) {
        crate::multiboot2::fb_trace_byte(byte);
    }
}

pub fn fb_mute() {
    if let Some(d) = DISPLAY.get() {
        (d.mute)();
    }
}

pub fn fb_unmute_and_repaint() {
    if let Some(d) = DISPLAY.get() {
        (d.unmute_and_repaint)();
    }
}

pub fn fb_set_dim(pct: u8) {
    if let Some(d) = DISPLAY.get() {
        (d.set_dim)(pct);
    }
}

pub fn fb_force_unmute() {
    if let Some(d) = DISPLAY.get() {
        (d.force_unmute)();
    }
}

#[must_use]
pub fn cursor_idle() -> Option<(u16, u16)> {
    DISPLAY.get().and_then(|d| (d.cursor_idle)())
}

#[must_use]
pub fn fb_grid() -> Option<(u16, u16)> {
    DISPLAY.get().and_then(|d| (d.grid)())
}

pub fn fb_apply_config(text: &str) -> Option<(u16, u16)> {
    DISPLAY.get().and_then(|d| (d.apply_config)(text))
}

pub fn fb_splash_frame(art: &str, info: &[&str], t_ms: u64) {
    if let Some(d) = DISPLAY.get() {
        (d.splash_frame)(art, info, t_ms);
    }
}

pub fn fb_splash_end(conf: Option<&str>, blocking: bool) -> Option<(u16, u16)> {
    DISPLAY.get().and_then(|d| (d.splash_end)(conf, blocking))
}

#[must_use]
pub fn fb_take_reply(out: &mut [u8]) -> usize {
    DISPLAY.get().map_or(0, |d| (d.take_reply)(out))
}

#[must_use]
pub fn fb_mouse() -> Option<(u16, bool, [u32; 6])> {
    DISPLAY.get().and_then(|d| (d.mouse)())
}

pub fn fb_set_pointer(cell: Option<(usize, usize)>) {
    if let Some(d) = DISPLAY.get() {
        (d.set_pointer)(cell);
    }
}

pub fn fb_set_view(cols: u16, rows: u16) -> Option<(u16, u16)> {
    DISPLAY.get().and_then(|d| (d.set_view)(cols, rows))
}

pub fn fb_summary() {
    if let Some(d) = DISPLAY.get() {
        (d.summary)();
    }
}

pub fn pat_init_ap() {
    if let Some(d) = DISPLAY.get() {
        (d.pat_init_ap)();
    }
}

pub fn display_band(i: usize) {
    if let Some(d) = DISPLAY.get() {
        (d.band)(i);
    }
}

#[must_use]
pub fn display_art_color(ch: u8, x: usize, y: usize, t_ms: u64) -> Option<(u8, u8, u8)> {
    DISPLAY.get().map(|d| (d.art_color)(ch, x, y, t_ms))
}

/// What a driver may use of the machine half.
pub mod services {
    /// The EGA text line, the output of last resort before any console exists.
    pub mod boot {
        pub use crate::multiboot2::ega_text;
    }
    /// `/dev/fb0`'s registration (the display driver offers its framebuffer there).
    pub mod fbdev {
        pub use crate::fbdev::register;
    }
    /// The framebuffer console's pointer cell and `/dev/fb0`'s geometry (the touchpad
    /// maps motion onto the screen).
    pub mod display {
        pub use crate::drivers::{fb_mouse, fb_set_pointer};
        pub use crate::fbdev::geometry;
    }
    pub mod errno {
        pub use crate::fd::errno::{EAGAIN, EFAULT, EINVAL, ENODEV};
    }
    pub mod input {
        pub use crate::input::note_input;
    }
    pub mod machine {
        pub use crate::machine::Physmap;
    }
    pub mod net {
        pub use crate::net::uptime_us;
    }
    pub mod paging {
        pub use crate::paging::map_device_window;
    }
    pub mod polltime {
        pub use crate::polltime::{spin_us, ticks_ms, tsc, tsc_hz};
    }
    pub mod uaccess {
        pub use crate::uaccess::{read_bytes, write_bytes};
    }
    pub mod lapic {
        pub use crate::lapic::{tsc_hz, tsc_uptime_us};
    }
    pub mod pci {
        pub use crate::pci::{
            config_space, enable, find_class, for_each, map_bar_window, probe_bar_size,
            read_u16_config, write_u16_config,
        };
    }
    pub mod sched {
        pub use crate::sched::yield_now;
    }
    pub mod serial {
        pub use crate::serial::{put_dec, put_hex, put_hexn, puts};
    }
    pub mod usermode {
        pub use crate::usermode::current_process;
    }
}
