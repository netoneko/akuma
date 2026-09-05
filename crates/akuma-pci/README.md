# akuma-pci

Pure PCI / PCIe configuration-space parsing — no port I/O, no MMIO, no
`unsafe`. `#![forbid(unsafe_code)]`, `#![no_std]`, zero dependencies.

## Why

The amd64 bare-metal target has no device enumeration: under a VMM every device
is announced (virtio-MMIO on the command line), and on real hardware nothing
is. `amd64/src/pci.rs` does the `0xCF8`/`0xCFC` port I/O and the write-1s BAR
size probe; this crate is everything that can be decided without touching the
bus:

* `Address::config_address` — the CONFIG_ADDRESS word
* `Header` — the type-0 header (vendor/device, class triple, `header_type`,
  subsystem ids, capability pointer), plus `is_xhci` / `is_ehci` / `is_ethernet`
  / `is_ahci` / `is_bridge`
* `decode_bars` / `bar_size` — BAR address decode (I/O, 32- and 64-bit memory,
  prefetchable) and the size arithmetic from a probe readback
* `capabilities` — the capability list walk, bounded against a pointer loop

`tests/hp_500_502nj.rs` runs it against the real config space of the reference
machine's xHCI (`00:14.0`), EHCI (`00:1a.0`) and Realtek NIC (`03:00.0`), read
from `/sys/bus/pci/devices/*/config`.
