//! USB standard descriptors.
//!
//! A device answers `GET_DESCRIPTOR(CONFIGURATION)` with one contiguous blob:
//! the configuration descriptor followed, in order, by every interface,
//! endpoint and class-specific descriptor it contains, each a
//! `[bLength, bDescriptorType, ...]` record. [`descriptors`] walks that blob;
//! [`find_boot_keyboard`] walks it looking for the one interface this target
//! cares about.
//!
//! The walker is deliberately forgiving: an unknown `bDescriptorType` is
//! yielded, not rejected (class descriptors like HID's `0x21` sit between the
//! interface and its endpoints), and a record whose `bLength` runs past the
//! buffer ends iteration rather than panicking. It stops at the first
//! zero-length record — a malformed device that would otherwise loop forever.

use crate::raw;

/// `bDescriptorType` values (USB 2.0 Table 9-5, plus the HID class's own).
pub mod descriptor_type {
    pub const DEVICE: u8 = 1;
    pub const CONFIGURATION: u8 = 2;
    pub const STRING: u8 = 3;
    pub const INTERFACE: u8 = 4;
    pub const ENDPOINT: u8 = 5;
    pub const INTERFACE_ASSOCIATION: u8 = 11;
    /// HID class descriptor, follows the interface it belongs to.
    pub const HID: u8 = 0x21;
    /// HID report descriptor — never in the config blob; fetched separately.
    pub const HID_REPORT: u8 = 0x22;
}

/// USB device class codes relevant here (`bInterfaceClass`).
pub mod class {
    pub const HID: u8 = 0x03;
    pub const HUB: u8 = 0x09;
}

/// HID subclass / protocol for the boot keyboard (HID 1.11 §4.2, §4.3).
pub mod hid_boot {
    /// `bInterfaceSubClass`: this interface supports the boot protocol.
    pub const SUBCLASS: u8 = 0x01;
    /// `bInterfaceProtocol` under the boot subclass.
    pub const PROTOCOL_KEYBOARD: u8 = 0x01;
    pub const PROTOCOL_MOUSE: u8 = 0x02;
}

/// One `[bLength, bDescriptorType, ...]` record, borrowing the blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawDescriptor<'a> {
    pub descriptor_type: u8,
    /// The whole record, `bytes[0] == bLength == bytes.len()`.
    pub bytes: &'a [u8],
}

/// Iterator over the descriptor records in a configuration blob.
#[derive(Debug, Clone)]
pub struct Descriptors<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for Descriptors<'a> {
    type Item = RawDescriptor<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.buf.get(self.pos..)?;
        if rest.len() < 2 {
            return None;
        }
        let len = rest[0] as usize;
        // A record must hold at least its own header and must not claim more
        // bytes than remain. `len == 0` would never advance `pos`.
        if len < 2 || len > rest.len() {
            return None;
        }
        let record = &rest[..len];
        self.pos += len;
        Some(RawDescriptor { descriptor_type: record[1], bytes: record })
    }
}

/// Walk the descriptor records in `buf`.
///
/// `buf` may be a bare configuration blob or the sysfs-style "device
/// descriptor then every configuration" dump — a leading device descriptor is
/// just another record and is skipped by callers that do not want it.
#[must_use]
pub fn descriptors(buf: &[u8]) -> Descriptors<'_> {
    Descriptors { buf, pos: 0 }
}

/// Device descriptor (USB 2.0 Table 9-8), 18 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub bcd_usb: u16,
    pub class: u8,
    pub sub_class: u8,
    pub protocol: u8,
    /// `bMaxPacketSize0` — the control endpoint's max packet size. 8/16/32/64.
    pub max_packet_size0: u8,
    pub vendor: u16,
    pub product: u16,
    pub bcd_device: u16,
    pub i_manufacturer: u8,
    pub i_product: u8,
    pub i_serial_number: u8,
    pub num_configurations: u8,
}

impl DeviceDescriptor {
    /// Parse from a record whose `bDescriptorType` is `DEVICE`. Accepts a
    /// longer buffer (the sysfs dump) and reads only the first 18 bytes.
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if raw::u8(b, 0)? < 18 || raw::u8(b, 1)? != descriptor_type::DEVICE {
            return None;
        }
        Some(Self {
            bcd_usb: raw::u16(b, 2)?,
            class: raw::u8(b, 4)?,
            sub_class: raw::u8(b, 5)?,
            protocol: raw::u8(b, 6)?,
            max_packet_size0: raw::u8(b, 7)?,
            vendor: raw::u16(b, 8)?,
            product: raw::u16(b, 10)?,
            bcd_device: raw::u16(b, 12)?,
            i_manufacturer: raw::u8(b, 14)?,
            i_product: raw::u8(b, 15)?,
            i_serial_number: raw::u8(b, 16)?,
            num_configurations: raw::u8(b, 17)?,
        })
    }
}

/// Configuration descriptor (USB 2.0 Table 9-10), the first 9 bytes of a blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigurationDescriptor {
    /// `wTotalLength` — the size of this whole blob, header included.
    pub total_length: u16,
    pub num_interfaces: u8,
    pub configuration_value: u8,
    pub i_configuration: u8,
    pub attributes: u8,
    /// `bMaxPower` already doubled: the value in milliamps, not 2 mA units.
    pub max_power_ma: u16,
}

impl ConfigurationDescriptor {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if raw::u8(b, 0)? < 9 || raw::u8(b, 1)? != descriptor_type::CONFIGURATION {
            return None;
        }
        Some(Self {
            total_length: raw::u16(b, 2)?,
            num_interfaces: raw::u8(b, 4)?,
            configuration_value: raw::u8(b, 5)?,
            i_configuration: raw::u8(b, 6)?,
            attributes: raw::u8(b, 7)?,
            max_power_ma: u16::from(raw::u8(b, 8)?) * 2,
        })
    }

    #[must_use]
    pub fn self_powered(&self) -> bool {
        self.attributes & (1 << 6) != 0
    }

    #[must_use]
    pub fn remote_wakeup(&self) -> bool {
        self.attributes & (1 << 5) != 0
    }
}

/// Interface descriptor (USB 2.0 Table 9-12), 9 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterfaceDescriptor {
    pub interface_number: u8,
    pub alternate_setting: u8,
    pub num_endpoints: u8,
    pub class: u8,
    pub sub_class: u8,
    pub protocol: u8,
    pub i_interface: u8,
}

impl InterfaceDescriptor {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if raw::u8(b, 0)? < 9 || raw::u8(b, 1)? != descriptor_type::INTERFACE {
            return None;
        }
        Some(Self {
            interface_number: raw::u8(b, 2)?,
            alternate_setting: raw::u8(b, 3)?,
            num_endpoints: raw::u8(b, 4)?,
            class: raw::u8(b, 5)?,
            sub_class: raw::u8(b, 6)?,
            protocol: raw::u8(b, 7)?,
            i_interface: raw::u8(b, 8)?,
        })
    }

    /// Is this the HID boot keyboard interface? (`class 3, subclass 1,
    /// protocol 1`).
    #[must_use]
    pub fn is_boot_keyboard(&self) -> bool {
        self.class == class::HID
            && self.sub_class == hid_boot::SUBCLASS
            && self.protocol == hid_boot::PROTOCOL_KEYBOARD
    }
}

/// Transfer type of an endpoint (`bmAttributes` bits 1:0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferType {
    Control,
    Isochronous,
    Bulk,
    Interrupt,
}

impl TransferType {
    #[must_use]
    pub fn from_attributes(attrs: u8) -> Self {
        match attrs & 0b11 {
            0 => Self::Control,
            1 => Self::Isochronous,
            2 => Self::Bulk,
            _ => Self::Interrupt,
        }
    }
}

/// Endpoint descriptor (USB 2.0 Table 9-13), 7 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointDescriptor {
    /// `bEndpointAddress`: bits 3:0 number, bit 7 direction (1 = IN).
    pub address: u8,
    pub attributes: u8,
    /// `wMaxPacketSize` bits 10:0 — the packet size. Bits 12:11 (high-bandwidth
    /// multiplier, high-speed only) are kept out.
    pub max_packet_size: u16,
    /// `bInterval` — raw. For a full/low-speed interrupt endpoint this is the
    /// polling period in frames (1..255); see [`crate::ehci::interrupt_period_frames`].
    pub interval: u8,
}

impl EndpointDescriptor {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if raw::u8(b, 0)? < 7 || raw::u8(b, 1)? != descriptor_type::ENDPOINT {
            return None;
        }
        Some(Self {
            address: raw::u8(b, 2)?,
            attributes: raw::u8(b, 3)?,
            max_packet_size: raw::u16(b, 4)? & 0x07ff,
            interval: raw::u8(b, 6)?,
        })
    }

    #[must_use]
    pub fn number(&self) -> u8 {
        self.address & 0x0f
    }

    /// Direction: `true` for IN (device-to-host).
    #[must_use]
    pub fn direction_in(&self) -> bool {
        self.address & 0x80 != 0
    }

    #[must_use]
    pub fn transfer_type(&self) -> TransferType {
        TransferType::from_attributes(self.attributes)
    }
}

/// HID class descriptor (HID 1.11 §6.2.1), the record with type `0x21` that
/// follows a HID interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HidDescriptor {
    pub bcd_hid: u16,
    pub country_code: u8,
    /// `wDescriptorLength` of the first subordinate descriptor — for a keyboard
    /// this is the report descriptor's length, the size to ask for with
    /// `GET_DESCRIPTOR(REPORT)`.
    pub report_descriptor_length: u16,
}

impl HidDescriptor {
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if raw::u8(b, 0)? < 9 || raw::u8(b, 1)? != descriptor_type::HID {
            return None;
        }
        // b[5] = bNumDescriptors, then pairs of (bDescriptorType, wLength).
        // The first pair (b[6..9]) is mandatory and is the report descriptor.
        if raw::u8(b, 6)? != descriptor_type::HID_REPORT {
            return None;
        }
        Some(Self {
            bcd_hid: raw::u16(b, 2)?,
            country_code: raw::u8(b, 4)?,
            report_descriptor_length: raw::u16(b, 7)?,
        })
    }
}

/// Everything the driver needs to open the keyboard's interrupt pipe, found by
/// walking a configuration blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootKeyboard {
    /// `SET_CONFIGURATION` argument.
    pub configuration_value: u8,
    /// `SET_INTERFACE` / the interface to `SET_PROTOCOL(Boot)` on.
    pub interface_number: u8,
    pub alternate_setting: u8,
    /// `bEndpointAddress` of the interrupt IN endpoint (e.g. `0x81`).
    pub endpoint_address: u8,
    pub endpoint_max_packet_size: u16,
    /// `bInterval` from the endpoint descriptor, unmodified.
    pub endpoint_interval: u8,
    /// Length to request with `GET_DESCRIPTOR(REPORT)`, or `None` if the HID
    /// class descriptor was missing (rare; the boot protocol still works).
    pub report_descriptor_length: Option<u16>,
}

/// Scan a configuration blob for the first HID boot-keyboard interface and its
/// interrupt IN endpoint.
///
/// The blob may include a leading device descriptor and multiple configuration
/// descriptors; the first matching interface wins and its `configuration_value`
/// is reported so the caller knows which `SET_CONFIGURATION` to issue.
#[must_use]
pub fn find_boot_keyboard(blob: &[u8]) -> Option<BootKeyboard> {
    let mut current_config: u8 = 0;
    let mut pending: Option<BootKeyboard> = None;

    for d in descriptors(blob) {
        match d.descriptor_type {
            descriptor_type::CONFIGURATION => {
                if let Some(c) = ConfigurationDescriptor::parse(d.bytes) {
                    current_config = c.configuration_value;
                }
            }
            descriptor_type::INTERFACE => {
                if let Some(i) = InterfaceDescriptor::parse(d.bytes) {
                    // Each new interface record replaces any half-built match;
                    // a boot keyboard with no interrupt IN endpoint is not one.
                    pending = i.is_boot_keyboard().then_some(BootKeyboard {
                        configuration_value: current_config,
                        interface_number: i.interface_number,
                        alternate_setting: i.alternate_setting,
                        endpoint_address: 0,
                        endpoint_max_packet_size: 0,
                        endpoint_interval: 0,
                        report_descriptor_length: None,
                    });
                }
            }
            descriptor_type::HID => {
                if let (Some(k), Some(h)) = (pending.as_mut(), HidDescriptor::parse(d.bytes)) {
                    k.report_descriptor_length = Some(h.report_descriptor_length);
                }
            }
            descriptor_type::ENDPOINT => {
                if let (Some(k), Some(e)) = (pending.as_mut(), EndpointDescriptor::parse(d.bytes))
                    && e.direction_in()
                    && e.transfer_type() == TransferType::Interrupt
                {
                    k.endpoint_address = e.address;
                    k.endpoint_max_packet_size = e.max_packet_size;
                    k.endpoint_interval = e.interval;
                    return Some(*k);
                }
            }
            _ => {}
        }
    }
    // A match that reached here never found its endpoint.
    None
}
