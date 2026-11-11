//! The amd64 kernel's device drivers.
//!
//! Each module is the *wiring* half of a driver: the host-tested crate (`akuma-hda`, ...)
//! decides what to say to the device, and the module here maps its registers, owns its DMA
//! memory and hands the machine half (`akuma-kernel-amd64`) what it registers. The machine
//! half never names a driver. It probes through one registration slot per class
//! (`akuma_kernel_amd64::drivers`), and [`register`], the bin's `before_main` hook, fills a
//! slot for every driver this build's features compiled in. Turning a feature off removes the
//! driver's code **and** its probe; the class then reads as absent, the same as a machine
//! without the device.
//!
//! What a driver may use of the machine half is `akuma_kernel_amd64::drivers::services`, one
//! re-export per item.
//!
//! Moved out of `amd64/src` one driver per commit from 2026-11-11
//! (`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`). Kernel-only: on any other target the crate is
//! empty.

#![no_std]
#![cfg(all(target_arch = "x86_64", target_os = "none"))]
#![forbid(unsafe_code)]

#[cfg(feature = "hda")]
mod hda;
#[cfg(feature = "touchpad")]
mod i2c;
#[cfg(feature = "touchpad")]
mod touchpad;

/// Install every driver this build compiled in. The bin's `before_main`, before either entry
/// point probes anything.
pub fn register() {
    #[cfg(feature = "hda")]
    akuma_kernel_amd64::drivers::register_sound(akuma_kernel_amd64::drivers::Sound {
        init: hda::init,
    });
    #[cfg(feature = "touchpad")]
    akuma_kernel_amd64::drivers::register_pointer(akuma_kernel_amd64::drivers::Pointer {
        discover: touchpad::discover,
        init: touchpad::init,
        poll: touchpad::poll,
        present: touchpad::present,
        has_events: touchpad::has_events,
        dev_read: touchpad::dev_read,
        dev_write: touchpad::dev_write,
        console_bridge: touchpad::console_bridge,
    });
}
