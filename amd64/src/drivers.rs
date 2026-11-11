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

/// What a driver may use of the machine half.
pub mod services {
    /// The framebuffer console's pointer cell and `/dev/fb0`'s geometry (the touchpad
    /// maps motion onto the screen).
    pub mod display {
        pub use crate::fbdev::geometry;
        pub use crate::multiboot2::{fb_mouse, fb_set_pointer};
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
        pub use crate::polltime::{spin_us, ticks_ms, tsc};
    }
    pub mod uaccess {
        pub use crate::uaccess::{read_bytes, write_bytes};
    }
    pub mod lapic {
        pub use crate::lapic::tsc_uptime_us;
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
