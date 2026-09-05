//! HID report descriptors, and turning boot-keyboard reports into keystrokes.
//!
//! Two layers:
//!
//! * [`items`] / [`input_fields`] parse a raw HID report descriptor (the item
//!   stream a device answers `GET_DESCRIPTOR(REPORT)` with). This is enough to
//!   *recognise* a boot keyboard ([`is_boot_keyboard_report_descriptor`]) and
//!   to summarise any device's input reports.
//! * [`BootReport`] / [`BootKeyboardDecoder`] parse the fixed 8-byte report the
//!   device sends once it is in boot protocol (`SET_PROTOCOL(0)` on the
//!   interface), and emit ASCII on the key-down edge.
//!
//! For this target the driver will put the interface into boot protocol and
//! use the second layer — the report descriptor never has to be understood to
//! type. The first layer is here because the user asked for a parser for the
//! descriptors that were dumped, and because "is this actually a boot
//! keyboard" is a real check the driver wants before it trusts the 8-byte
//! layout.

use crate::keymap;

/// Item category (HID 1.11 §6.2.2.1, the `bType` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Main,
    Global,
    Local,
    Reserved,
}

/// `bTag` values for `Main` items.
pub mod main {
    pub const INPUT: u8 = 0x8;
    pub const OUTPUT: u8 = 0x9;
    pub const FEATURE: u8 = 0xB;
    pub const COLLECTION: u8 = 0xA;
    pub const END_COLLECTION: u8 = 0xC;
}

/// `bTag` values for `Global` items.
pub mod global {
    pub const USAGE_PAGE: u8 = 0x0;
    pub const LOGICAL_MINIMUM: u8 = 0x1;
    pub const LOGICAL_MAXIMUM: u8 = 0x2;
    pub const REPORT_SIZE: u8 = 0x7;
    pub const REPORT_ID: u8 = 0x8;
    pub const REPORT_COUNT: u8 = 0x9;
    pub const PUSH: u8 = 0xA;
    pub const POP: u8 = 0xB;
}

/// `bTag` values for `Local` items.
pub mod local {
    pub const USAGE: u8 = 0x0;
    pub const USAGE_MINIMUM: u8 = 0x1;
    pub const USAGE_MAXIMUM: u8 = 0x2;
}

/// The Generic Desktop usage page and the Keyboard usage within it — the two
/// items every keyboard report descriptor opens with.
pub const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
pub const USAGE_PAGE_KEYBOARD: u16 = 0x07;
pub const USAGE_GENERIC_DESKTOP_KEYBOARD: u32 = 0x06;

/// One parsed item, borrowing its data bytes from the descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item<'a> {
    pub kind: ItemKind,
    /// 0..=15. For a `Main` item, one of the [`main`] constants.
    pub tag: u8,
    /// 0, 1, 2 or 4 bytes for a short item; the payload for a long item.
    pub data: &'a [u8],
    pub long: bool,
}

impl Item<'_> {
    /// The data payload as an unsigned value, zero-extended (HID stores it
    /// little-endian, up to 4 bytes).
    #[must_use]
    pub fn value_u32(&self) -> u32 {
        let mut v = 0u32;
        for (i, &b) in self.data.iter().take(4).enumerate() {
            v |= u32::from(b) << (8 * i);
        }
        v
    }

    /// The data payload as a signed value, sign-extended from its byte width —
    /// `Logical Minimum` is routinely negative.
    #[must_use]
    pub fn value_i32(&self) -> i32 {
        let n = self.data.len().min(4);
        if n == 0 {
            return 0;
        }
        // Sign-extend by filling the unused high bytes with the sign bit before
        // reinterpreting — no `as` cast, so no wrap-around lint and no mistake.
        let negative = self.data[n - 1] & 0x80 != 0;
        let mut buf = if negative { [0xffu8; 4] } else { [0u8; 4] };
        buf[..n].copy_from_slice(&self.data[..n]);
        i32::from_le_bytes(buf)
    }
}

/// Iterator over the items in a raw HID report descriptor.
#[derive(Debug, Clone)]
pub struct Items<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for Items<'a> {
    type Item = Item<'a>;

    fn next(&mut self) -> Option<Item<'a>> {
        let rest = self.buf.get(self.pos..)?;
        let &prefix = rest.first()?;

        // Long item: 0xFE, bDataSize, bLongItemTag, then the data.
        if prefix == 0xFE {
            let size = *rest.get(1)? as usize;
            let tag = *rest.get(2)?;
            let data = rest.get(3..3 + size)?;
            self.pos += 3 + size;
            return Some(Item { kind: ItemKind::Reserved, tag, data, long: true });
        }

        let size = match prefix & 0b11 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let kind = match (prefix >> 2) & 0b11 {
            0 => ItemKind::Main,
            1 => ItemKind::Global,
            2 => ItemKind::Local,
            _ => ItemKind::Reserved,
        };
        let tag = prefix >> 4;
        let data = rest.get(1..1 + size)?;
        self.pos += 1 + size;
        Some(Item { kind, tag, data, long: false })
    }
}

/// Parse a raw HID report descriptor into its item stream.
#[must_use]
pub fn items(descriptor: &[u8]) -> Items<'_> {
    Items { buf: descriptor, pos: 0 }
}

/// A summary of one `Input` main item — one run of `report_count` fields of
/// `report_size` bits each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReportField {
    /// 0 when the descriptor uses no report IDs (the boot keyboard case).
    pub report_id: u8,
    pub usage_page: u16,
    pub usage_minimum: u32,
    pub usage_maximum: u32,
    pub logical_minimum: i32,
    pub logical_maximum: i32,
    pub report_size: u8,
    pub report_count: u8,
    /// `Constant` (padding) rather than `Data`.
    pub constant: bool,
    /// `Variable` (one field per control) rather than `Array` (a list of
    /// pressed usages — how the 6 key slots are encoded).
    pub variable: bool,
    pub relative: bool,
}

impl ReportField {
    #[must_use]
    pub fn total_bits(&self) -> u32 {
        u32::from(self.report_size) * u32::from(self.report_count)
    }
}

/// Global/local parser state carried across items.
#[derive(Default, Clone, Copy)]
struct GlobalState {
    usage_page: u16,
    logical_minimum: i32,
    logical_maximum: i32,
    report_size: u8,
    report_count: u8,
    report_id: u8,
}

/// Walk the descriptor and fill `out` with one [`ReportField`] per `Input`
/// item, returning how many were written (capped at `out.len()`).
///
/// `Collection`/`End Collection` nesting is ignored — it does not affect the
/// bit layout — and `Push`/`Pop` are not implemented (no keyboard descriptor
/// in the wild uses them; a device that does gets a best-effort parse).
#[must_use]
pub fn input_fields(descriptor: &[u8], out: &mut [ReportField]) -> usize {
    let mut g = GlobalState::default();
    let (mut usage_min, mut usage_max): (u32, u32) = (0, 0);
    let mut n = 0;

    for item in items(descriptor) {
        match item.kind {
            ItemKind::Global => match item.tag {
                global::USAGE_PAGE => g.usage_page = item.value_u32() as u16,
                global::LOGICAL_MINIMUM => g.logical_minimum = item.value_i32(),
                global::LOGICAL_MAXIMUM => g.logical_maximum = item.value_i32(),
                global::REPORT_SIZE => g.report_size = item.value_u32() as u8,
                global::REPORT_COUNT => g.report_count = item.value_u32() as u8,
                global::REPORT_ID => g.report_id = item.value_u32() as u8,
                _ => {}
            },
            ItemKind::Local => match item.tag {
                local::USAGE | local::USAGE_MINIMUM => usage_min = item.value_u32(),
                local::USAGE_MAXIMUM => usage_max = item.value_u32(),
                _ => {}
            },
            ItemKind::Main => {
                if item.tag == main::INPUT
                    && let Some(slot) = out.get_mut(n)
                {
                    let flags = item.value_u32();
                    *slot = ReportField {
                        report_id: g.report_id,
                        usage_page: g.usage_page,
                        usage_minimum: usage_min,
                        usage_maximum: usage_max.max(usage_min),
                        logical_minimum: g.logical_minimum,
                        logical_maximum: g.logical_maximum,
                        report_size: g.report_size,
                        report_count: g.report_count,
                        constant: flags & (1 << 0) != 0,
                        variable: flags & (1 << 1) != 0,
                        relative: flags & (1 << 2) != 0,
                    };
                    n += 1;
                }
                // Local items are consumed by the Main item that follows them.
                usage_min = 0;
                usage_max = 0;
            }
            ItemKind::Reserved => {}
        }
    }
    n
}

/// Does this report descriptor describe a standard HID boot keyboard?
///
/// The check is structural, not byte-exact: an 8-bit x 8 modifier field over
/// the keyboard usage page with usages `0xE0..=0xE7`, an 8-bit array of at
/// least 6 key slots on the same page, and no report IDs. A device that
/// advertises `bInterfaceSubClass = 1` will honour `SET_PROTOCOL(Boot)`
/// regardless; this is the belt-and-braces confirmation before the driver
/// trusts the fixed layout.
#[must_use]
pub fn is_boot_keyboard_report_descriptor(descriptor: &[u8]) -> bool {
    let mut fields = [ReportField::default(); 16];
    let n = input_fields(descriptor, &mut fields);
    let fields = &fields[..n];

    let has_modifier_field = fields.iter().any(|f| {
        f.usage_page == USAGE_PAGE_KEYBOARD
            && f.variable
            && f.report_size == 1
            && f.report_count == 8
            && f.usage_minimum == 0xE0
            && f.usage_maximum == 0xE7
    });
    let has_key_array = fields.iter().any(|f| {
        f.usage_page == USAGE_PAGE_KEYBOARD
            && !f.variable
            && !f.constant
            && f.report_size == 8
            && f.report_count >= 6
    });
    let no_report_ids = fields.iter().all(|f| f.report_id == 0);

    has_modifier_field && has_key_array && no_report_ids
}

/// The fixed 8-byte HID boot-keyboard input report (HID 1.11 App. B.1):
/// `[modifiers, reserved, key0, key1, key2, key3, key4, key5]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootReport {
    pub modifiers: u8,
    pub keys: [u8; 6],
}

impl BootReport {
    pub const LEN: usize = 8;

    /// Parse the first 8 bytes. A boot keyboard whose endpoint has a larger
    /// `wMaxPacketSize` (the ROCCAT's is 64) still puts the boot layout in
    /// bytes 0..8, so a longer buffer is fine.
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < Self::LEN {
            return None;
        }
        Some(Self {
            modifiers: b[0],
            keys: [b[2], b[3], b[4], b[5], b[6], b[7]],
        })
    }

    #[must_use]
    pub fn shift(&self) -> bool {
        self.modifiers & keymap::MOD_SHIFT != 0
    }

    #[must_use]
    pub fn ctrl(&self) -> bool {
        self.modifiers & keymap::MOD_CTRL != 0
    }

    /// The keyboard reports `ErrorRollOver` in every slot when more keys are
    /// held than it can encode — the report carries no usable key data.
    #[must_use]
    pub fn rolled_over(&self) -> bool {
        self.keys[0] == keymap::USAGE_ERROR_ROLL_OVER
    }
}

/// Turns a stream of boot reports into a stream of typed bytes.
///
/// Emits on the key-down edge only: a key present in this report but not the
/// last one. A held key does not repeat — key repeat, if ever wanted, is a
/// timer layered on top, the same division of labour `kbd.rs` leaves to the
/// PS/2 keyboard's own hardware repeat. Caps Lock is tracked here because the
/// host, not the keyboard, owns that state under the boot protocol.
#[derive(Debug, Clone)]
pub struct BootKeyboardDecoder {
    prev_keys: [u8; 6],
    caps: bool,
}

impl Default for BootKeyboardDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl BootKeyboardDecoder {
    #[must_use]
    pub const fn new() -> Self {
        Self { prev_keys: [0; 6], caps: false }
    }

    #[must_use]
    pub fn caps_lock(&self) -> bool {
        self.caps
    }

    /// Feed one report; `emit` is called, in key-slot order, with the ASCII
    /// byte of each newly-pressed key that maps to one.
    pub fn feed(&mut self, report: &BootReport, mut emit: impl FnMut(u8)) {
        if report.rolled_over() {
            // Can't tell which key is new; drop this report's keys but leave
            // `prev_keys` so the eventual release still resolves.
            return;
        }
        let shift = report.shift();
        let ctrl = report.ctrl();
        for &k in &report.keys {
            if k == 0 || k == keymap::USAGE_ERROR_ROLL_OVER {
                continue;
            }
            if self.prev_keys.contains(&k) {
                continue; // still held from last report
            }
            if k == keymap::USAGE_CAPS_LOCK {
                self.caps = !self.caps;
                continue;
            }
            if let Some(c) = keymap::usage_to_ascii(k, shift, ctrl, self.caps) {
                emit(c);
            }
        }
        self.prev_keys = report.keys;
    }
}
