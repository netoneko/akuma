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
//! Moved out of `crates/akuma-kernel-amd64/src` one driver per commit from 2026-11-11
//! (`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`). Kernel-only: on any other target the crate is
//! empty.

#![no_std]
#![cfg(all(target_arch = "x86_64", target_os = "none"))]
// The one driver that needs `unsafe` (the hypervisor, which enters guests) is its own crate,
// `akuma-kvm-amd64`, behind the `kvm` feature.
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(feature = "fbcon")]
mod fbcon;

#[cfg(feature = "hda")]
mod hda;
#[cfg(feature = "i8042")]
mod kbd;
#[cfg(feature = "nvme")]
mod nvme;
#[cfg(feature = "xhci")]
mod xhci;
#[cfg(feature = "touchpad")]
mod i2c;
#[cfg(feature = "touchpad")]
mod touchpad;
#[cfg(feature = "wifi")]
mod rtw89;
#[cfg(feature = "wifi")]
mod rtw89_sta;
#[cfg(feature = "wifi")]
mod wifi;

/// Install every driver this build compiled in. The bin's `before_main`, before either entry
/// point probes anything.
pub fn register() {
    #[cfg(feature = "fbcon")]
    akuma_kernel_amd64::drivers::register_display(&fbcon::DISPLAY);
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
    #[cfg(feature = "wifi")]
    akuma_kernel_amd64::drivers::register_wireless(&wifi::WIRELESS);
    #[cfg(feature = "xhci")]
    akuma_kernel_amd64::drivers::register_usb(akuma_kernel_amd64::drivers::Usb {
        init: xhci::init,
        is_initialized: xhci::is_initialized,
        shutdown: xhci::shutdown,
        read_bytes: xhci::read_bytes,
        write_bytes: xhci::write_bytes,
        flush: xhci::flush,
        init_keyboard: xhci::init_keyboard,
        keyboard_getb: xhci::keyboard_getb,
        keyboard_has_byte: xhci::keyboard_has_byte,
        #[cfg(not(feature = "no-tests"))]
        smoke_test: Some(xhci::smoke_test),
        #[cfg(feature = "no-tests")]
        smoke_test: None,
    });
    #[cfg(feature = "nvme")]
    akuma_kernel_amd64::drivers::register_nvme(akuma_kernel_amd64::drivers::Nvme {
        init: nvme::init,
        open_partition: nvme::open_partition,
        read_bytes: nvme::read_bytes,
        write_bytes: nvme::write_bytes,
        flush: nvme::flush,
        shutdown: nvme::shutdown,
        open_guest_partition: nvme::open_guest_partition,
        close_guest_partition: nvme::close_guest_partition,
        guest_read: nvme::guest_read,
        guest_write: nvme::guest_write,
    });
    #[cfg(feature = "i8042")]
    akuma_kernel_amd64::drivers::register_keyboard(akuma_kernel_amd64::drivers::Keyboard {
        init: kbd::init,
        getb: kbd::getb,
        has_byte: kbd::has_byte,
    });
    #[cfg(feature = "kvm")]
    akuma_kernel_amd64::drivers::register_hypervisor(&akuma_kvm_amd64::HYPERVISOR);
}
