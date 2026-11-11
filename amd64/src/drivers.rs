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

/// What a driver may use of the machine half.
pub mod services {
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
        pub use crate::serial::{put_dec, put_hexn, puts};
    }
    pub mod usermode {
        pub use crate::usermode::current_process;
    }
}
