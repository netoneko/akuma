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

/// Wireless: `/dev/wifi0` and the card that can be the network link.
///
/// The control device has its own backends (`wifisim`, the card). Probed in three steps because the card's firmware is a file: the device at
/// boot, the card after the root mount, the station daemon once the scheduler runs.
///
/// The RTL8169 is not in this class: it is the machine half's wired NIC (`net`), driven
/// through `akuma-net`'s own `ExternalDevice`. A wireless driver hands `net` its frame queues
/// ([`Wireless::link`]) and `net` makes them the interface, so the class is the edge between
/// a radio and the stack, not a general net-device registry.
pub struct Wireless {
    /// `/dev/wifi0`'s backend from the command line (`wifisim`), before the root mount.
    pub init: fn(&str),
    /// The card, after the root mount (its firmware is a file).
    pub card_init: fn(&str),
    /// The station daemon, once the scheduler runs.
    pub spawn: fn(),
    /// The card was kept up as a station and is the network link.
    pub is_link: fn() -> bool,
    /// The station's frame queues, which `net` makes the interface when [`Wireless::is_link`].
    pub link: &'static akuma_net::queued::FrameQueues,
    /// Stop the card's DMA before a warm reset.
    pub shutdown_for_reset: fn(),
    /// `/dev/wifi0` exists.
    pub present: fn() -> bool,
    /// `read(2)` on `/dev/wifi0`, per `(tgid, fd)` cursor.
    pub dev_read: fn(tgid: u32, fd: u32, buf: u64, len: usize) -> u64,
    /// `write(2)` on `/dev/wifi0`: one command line.
    pub dev_write: fn(buf: u64, len: usize) -> u64,
    /// A `/dev/wifi0` descriptor closed: drop its cursor.
    pub release: fn(tgid: u32, fd: u32),
}

static WIRELESS: Registered<&'static Wireless> = Registered::new("no wireless driver registered");

/// Install the wireless driver. Idempotent by `Registered`'s contract.
pub fn register_wireless(driver: &'static Wireless) {
    WIRELESS.register(driver);
}

pub fn wireless_init(cmdline: &str) {
    if let Some(d) = WIRELESS.get() {
        (d.init)(cmdline);
    }
}

pub fn wireless_card_init(cmdline: &str) {
    if let Some(d) = WIRELESS.get() {
        (d.card_init)(cmdline);
    }
}

pub fn wireless_spawn() {
    if let Some(d) = WIRELESS.get() {
        (d.spawn)();
    }
}

#[must_use]
pub fn wireless_is_link() -> bool {
    WIRELESS.get().is_some_and(|d| (d.is_link)())
}

/// The station's frame queues when the card is the link.
#[must_use]
pub fn wireless_link() -> Option<&'static akuma_net::queued::FrameQueues> {
    WIRELESS.get().filter(|d| (d.is_link)()).map(|d| d.link)
}

pub fn wireless_shutdown_for_reset() {
    if let Some(d) = WIRELESS.get() {
        (d.shutdown_for_reset)();
    }
}

#[must_use]
pub fn wireless_present() -> bool {
    WIRELESS.get().is_some_and(|d| (d.present)())
}

/// `ENODEV` without a driver; unreachable in practice, because `open` refuses the node
/// while [`wireless_present`] is false.
#[must_use]
pub fn wireless_dev_read(tgid: u32, fd: u32, buf: u64, len: usize) -> u64 {
    WIRELESS.get().map_or(crate::fd::errno::ENODEV, |d| (d.dev_read)(tgid, fd, buf, len))
}

/// As [`wireless_dev_read`].
#[must_use]
pub fn wireless_dev_write(buf: u64, len: usize) -> u64 {
    WIRELESS.get().map_or(crate::fd::errno::ENODEV, |d| (d.dev_write)(buf, len))
}

pub fn wireless_release(tgid: u32, fd: u32) {
    if let Some(d) = WIRELESS.get() {
        (d.release)(tgid, fd);
    }
}

/// Hypervisor: the AMD SVM host (`/dev/kvm0`, the `/dev/kvm` ioctl surface, the
/// `/dev/net/tun` tap behind its gateway).
///
/// Every node it serves is a char device, so the class is the char-device edge for those
/// three nodes: `fd`'s `open`/`read`/`ioctl` and `usermode`'s `write`/`writev`/`readv`/`close`
/// arms forward here, as do `fork` and `mmap`'s notes for the ioctl surface. The descriptor
/// kinds (`FileDescriptor::DevKvm`, ..) stay in `fd`: they are the fd table's vocabulary.
/// Without a driver none of the nodes opens (`ENOENT`, as on a CPU without SVM).
pub struct Hypervisor {
    /// The SVM probe and, by command line, the rehearsal guests (BSP, after `mem::init`).
    pub init: fn(&str),
    /// After the root mount and the LAPIC timer: the guests that read a kernel off disk.
    pub late: fn(&str),
    /// Once the host's network runs: a service guest (`kvmserve`).
    pub spawn_service: fn(&str),
    /// Per application processor: its host save area and `EFER.SVME`.
    pub ap_hook: fn(),
    /// `/dev/kvm0` opens (`kvmctl` armed the hypervisor).
    pub armed: fn() -> bool,
    /// `read(2)` on `/dev/kvm0`: the guest's console stream.
    pub ctl_read: fn(buf: u64, len: usize) -> u64,
    /// `write(2)` on `/dev/kvm0`: a VM spec, `stop` or `status`.
    pub ctl_write: fn(buf: u64, len: usize) -> u64,
    /// `/dev/kvm` opens (`kvmapi`/`kvmctl`).
    pub api_enabled: fn() -> bool,
    pub api_open: fn(flags: u32) -> u64,
    pub api_ioctl: fn(fd: u64, id: u32, req: u32, arg: u64) -> u64,
    /// A `fork` is about to copy the descriptors.
    pub api_note_fork: fn(),
    /// An `mmap` of a `/dev/kvm` descriptor, recorded.
    pub api_note_mmap: fn(fd: u64, addr: u64, len: u64, prot: u64, flags: u64, off: u64),
    /// Map a vCPU's run page; `ENODEV` for a descriptor that is not a vCPU.
    pub api_mmap_vcpu: fn(id: u32, addr: u64, len: u64, prot: u64, flags: u64) -> u64,
    pub tun_open: fn(flags: u32) -> u64,
    pub tun_release: fn(),
    pub tun_read: fn(buf: u64, len: usize) -> u64,
    pub tun_write: fn(buf: u64, len: usize) -> u64,
    pub tun_readv: fn(iov: u64, cnt: usize) -> u64,
    pub tun_writev: fn(iov: u64, cnt: usize) -> u64,
    pub tun_ioctl: fn(req: u32, arg: u64) -> u64,
    /// The gateway has a frame for the tap (the poll family's readiness).
    pub tun_has_frame: fn() -> bool,
}

static HYPERVISOR: Registered<&'static Hypervisor> = Registered::new("no hypervisor registered");

/// Install the hypervisor. Idempotent by `Registered`'s contract.
pub fn register_hypervisor(driver: &'static Hypervisor) {
    HYPERVISOR.register(driver);
}

#[must_use]
pub fn hypervisor_present() -> bool {
    HYPERVISOR.is_registered()
}

pub fn hypervisor_init(cmdline: &str) {
    if let Some(d) = HYPERVISOR.get() {
        (d.init)(cmdline);
    }
}

pub fn hypervisor_late(cmdline: &str) {
    if let Some(d) = HYPERVISOR.get() {
        (d.late)(cmdline);
    }
}

pub fn hypervisor_spawn_service(cmdline: &str) {
    if let Some(d) = HYPERVISOR.get() {
        (d.spawn_service)(cmdline);
    }
}

pub fn hypervisor_ap_hook() {
    if let Some(d) = HYPERVISOR.get() {
        (d.ap_hook)();
    }
}

#[must_use]
pub fn hypervisor_armed() -> bool {
    HYPERVISOR.get().is_some_and(|d| (d.armed)())
}

#[must_use]
pub fn hypervisor_api_enabled() -> bool {
    HYPERVISOR.get().is_some_and(|d| (d.api_enabled)())
}

pub fn hypervisor_note_fork() {
    if let Some(d) = HYPERVISOR.get() {
        (d.api_note_fork)();
    }
}

pub fn hypervisor_api_note_mmap(
    fd: u64,
    addr: akuma_primitives::addr::VirtAddr,
    len: u64,
    prot: u64,
    flags: u64,
    off: u64,
) {
    if let Some(d) = HYPERVISOR.get() {
        (d.api_note_mmap)(fd, addr.get() as u64, len, prot, flags, off);
    }
}

pub fn hypervisor_tun_release() {
    if let Some(d) = HYPERVISOR.get() {
        (d.tun_release)();
    }
}

#[must_use]
pub fn hypervisor_tun_has_frame() -> bool {
    HYPERVISOR.get().is_some_and(|d| (d.tun_has_frame)())
}

/// The rest answer `ENOENT` (an open) or `ENODEV` without a driver. Only the opens are
/// reachable then: no descriptor of these kinds can exist without one.
macro_rules! hypervisor_forward {
    ($($(#[$m:meta])* fn $name:ident => $field:ident ($($a:ident: $t:ty),*) -> u64 or $none:ident;)*) => {$(
        $(#[$m])*
        #[must_use]
        pub fn $name($($a: $t),*) -> u64 {
            HYPERVISOR.get().map_or(crate::fd::errno::$none, |d| (d.$field)($($a),*))
        }
    )*};
}

hypervisor_forward! {
    fn hypervisor_ctl_read => ctl_read(buf: u64, len: usize) -> u64 or ENODEV;
    fn hypervisor_ctl_write => ctl_write(buf: u64, len: usize) -> u64 or ENODEV;
    fn hypervisor_api_open => api_open(flags: u32) -> u64 or ENOENT;
    fn hypervisor_api_ioctl => api_ioctl(fd: u64, id: u32, req: u32, arg: u64) -> u64 or ENODEV;
    fn hypervisor_api_mmap_vcpu => api_mmap_vcpu(id: u32, addr: u64, len: u64, prot: u64, flags: u64)
        -> u64 or ENODEV;
    fn hypervisor_tun_open => tun_open(flags: u32) -> u64 or ENOENT;
    fn hypervisor_tun_read => tun_read(buf: u64, len: usize) -> u64 or ENODEV;
    fn hypervisor_tun_write => tun_write(buf: u64, len: usize) -> u64 or ENODEV;
    fn hypervisor_tun_readv => tun_readv(iov: u64, cnt: usize) -> u64 or ENODEV;
    fn hypervisor_tun_writev => tun_writev(iov: u64, cnt: usize) -> u64 or ENODEV;
    fn hypervisor_tun_ioctl => tun_ioctl(req: u32, arg: u64) -> u64 or ENODEV;
}

/// What a driver may use of the machine half.
pub mod services {
    /// The VFS's view of driver-owned device nodes.
    pub mod vfs {
        pub use akuma_vfs_glue::set_wifi_present;
    }
    /// The EGA text line, the output of last resort before any console exists.
    pub mod boot {
        pub use crate::multiboot2::{ega_text, framebuffer_window};
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
    /// Reading a firmware file off the root filesystem.
    pub mod fs {
        pub use crate::fs::{metadata, read_at, resolve_symlinks};
    }
    pub mod errno {
        pub use crate::fd::errno::{
            E2BIG, EAGAIN, EBADF, EBUSY, EEXIST, EFAULT, EINTR, EINVAL, EIO, EMFILE, ENODEV,
            ENOENT, ENOMEM, ENOTTY, EOPNOTSUPP, ESRCH,
        };
    }
    /// The fd table's view a driver needs: `O_CLOEXEC` and an eventfd's identity.
    pub mod fd {
        pub use crate::fd::eventfd_id_of;
        pub mod open_flags {
            pub use crate::fd::open_flags::O_CLOEXEC;
        }
    }
    /// User mappings of physical frames (the hypervisor's guest RAM and run pages).
    pub mod mm {
        pub use crate::mm::{mmap_frames, remap_user_to_phys};
    }
    /// A guest disk on an NVMe partition.
    pub mod nvme {
        pub use crate::nvme::{
            close_guest_partition, flush, guest_read, guest_write, open_guest_partition,
        };
    }
    pub mod phys {
        pub use crate::phys::{FreshFrame, KERNEL_VMA};
    }
    /// Cores: which one, how many, waking and starting them, the BKL window.
    pub mod smp {
        pub use crate::smp::{
            bkl_drop_window, cpu_index, ensure_started, is_deferred, kick_core, online_cpus,
            MAX_CPUS,
        };
    }
    pub mod input {
        pub use crate::input::note_input;
    }
    pub mod machine {
        pub use crate::machine::Physmap;
    }
    pub mod net {
        pub use crate::net::{rng_fill_checked, uptime_us, wake_netpoll};
    }
    pub mod paging {
        pub use crate::paging::map_device_window;
    }
    pub mod polltime {
        pub use crate::polltime::{spin_us, ticks_ms, tsc, tsc_hz};
    }
    pub mod uaccess {
        pub use crate::uaccess::{read_bytes, read_val, write_bytes, write_val};
    }
    pub mod lapic {
        pub use crate::lapic::{arm_oneshot_in_us, tsc_hz, tsc_uptime_us, US_PER_TICK_TARGET};
    }
    pub mod pci {
        pub use crate::pci::{
            config_space, enable, enable_bridges_above, enable_full, find_class, for_each,
            map_bar_window, probe_bar_size, quiesce,
            read_u16_config, write_u16_config,
        };
    }
    pub mod sched {
        pub use crate::sched::{
            affinity_of, allow_tick, block_until_deadline, fpu_restore_live, fpu_save_live,
            halted_tsc, repin_current, reserve_cores, spawn_daemon, spawn_daemon_on, unpin_current,
            wake, yield_now, FxArea,
        };
    }
    pub mod serial {
        pub use crate::serial::{put_dec, put_hex, put_hexn, puts};
    }
    pub mod usermode {
        pub use crate::usermode::current_process;
    }
}
