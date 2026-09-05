# akuma-usb

Pure USB parsing for the amd64 bare-metal target — no controller, no DMA, no
`unsafe`. `#![forbid(unsafe_code)]`, `#![no_std]`, zero dependencies, host-tested
against bytes measured on the reference machine (HP 500-502nj) on 2026-09-05.

## Why

The HP box has a USB keyboard (ROCCAT Vulcan AIMO) and no PS/2 controller, so
`amd64/src/kbd.rs` finds nothing and there is no way to type at a bare-metal
shell. A USB keyboard driver is mostly logic a host can check without hardware:

| module | what it parses | for |
|---|---|---|
| `descriptor` | USB standard descriptor hierarchy; `find_boot_keyboard` | picking the interface/endpoint to open |
| `hid` | HID report-descriptor item stream; `is_boot_keyboard_report_descriptor` | confirming the boot layout before trusting it |
| `hid::BootKeyboardDecoder` | the 8-byte boot-keyboard report | turning key-down edges into bytes |
| `keymap` | HID usage code → ASCII (shift / ctrl / caps) | the USB counterpart of `kbd.rs`'s scancode tables |
| `ehci` | EHCI capability/operational registers, `USBLEGSUP` handoff, `PORTSC`, and the split-transaction queue-head + qTD dwords | bringing the controller up and running one interrupt transfer |

The controller-touching half (MMIO reads/writes, DMA allocation, threading the
queue head into the periodic frame list) is left to the kernel; this crate is
the layout and the bit math under it.

## The hardware fact that shapes the driver

Measured on the box: the keyboard enumerates **full speed on EHCI** `00:1a.0`,
behind the Intel Integrated Rate Matching Hub (single-TT). The xHCI's
`XUSB2PRM` (USB-2.0 port routing mask) reads `0x00000000` — **no USB-2.0 port on
this board can be routed to xHCI** — so the keyboard cannot be moved off EHCI.
The driver is therefore an EHCI driver doing transaction-translator *split*
transactions, which is why `ehci` is the controller module here and there is no
`xhci` one.

## Tests

```
cargo test -p akuma-usb --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

`tests/roccat_vulcan.rs` — the keyboard's real descriptors (`lsusb -v`,
`usbhid-dump`) and a typed-input trace through the decoder.
`tests/ehci_hp_500_502nj.rs` — EHCI `00:1a.0`'s live register block and the
hand-computed split-transaction queue head for the ROCCAT's interrupt endpoint.
