//! The one-shot field table: one walk of a descriptor at probe time, then
//! O(n) lookups by usage for the lifetime of the device.

use crate::item::{HIO_CONST, HIO_RELATIVE, HIO_VARIABLE, HidLocation, ItemKind};
use crate::{Filter, HidParser};

/// Capacity of the one-shot table. A 5-finger precision-touchpad descriptor
///
/// expands to ~30 entries (per-finger tip/in-range/X/Y/id × 5, buttons,
/// contact count, features); 64 leaves headroom, and overflow is reported
/// through [`FieldTable::truncated`] rather than panicking.
pub const MAX_FIELDS: usize = 64;

/// One field of one report: where it sits and how to decode it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidField {
    pub kind: ItemKind,
    pub report_id: u8,
    /// `usage_page << 16 | code`.
    pub usage: u32,
    pub loc: HidLocation,
    /// `HIO_*` bits.
    pub flags: u32,
    pub logical_min: i32,
    pub logical_max: i32,
}

impl HidField {
    /// Is this a relative field (mouse-style delta)?
    #[must_use]
    pub fn is_relative(self) -> bool {
        self.flags & HIO_RELATIVE != 0
    }

    /// Is this an array field (not one-field-per-usage)?
    #[must_use]
    pub fn is_array(self) -> bool {
        self.flags & HIO_VARIABLE == 0
    }

    /// Decode this field out of a report payload — the report-ID byte must
    /// already be stripped when the reports are numbered, matching the
    /// positions this table records.
    #[must_use]
    pub fn read(&self, payload: &[u8]) -> i32 {
        crate::get_bits(payload, &self.loc)
    }

    /// The unsigned form of [`HidField::read`].
    #[must_use]
    pub fn read_unsigned(&self, payload: &[u8]) -> u32 {
        crate::get_ubits(payload, &self.loc)
    }
}

/// Every input and feature field of a descriptor, in stream order. Built
/// once (kernel side: into a `static`), then queried.
#[derive(Clone, Copy)]
pub struct FieldTable {
    fields: [HidField; MAX_FIELDS],
    len: usize,
    /// Set when the descriptor declares more fields than [`MAX_FIELDS`]
    /// could hold; the tail is dropped, the head is exact.
    pub truncated: bool,
}

impl FieldTable {
    /// Walk `desc` and collect every non-const `Input` and `Feature` field.
    #[must_use]
    pub fn collect(desc: &[u8]) -> Self {
        let mut t = Self {
            fields: [HidField {
                kind: ItemKind::Input,
                report_id: 0,
                usage: 0,
                loc: HidLocation::NONE,
                flags: 0,
                logical_min: 0,
                logical_max: 0,
            }; MAX_FIELDS],
            len: 0,
            truncated: false,
        };
        let mut p = HidParser::new(desc, Filter::All);
        while let Some(h) = p.next_item() {
            match h.kind {
                ItemKind::Input | ItemKind::Feature => {}
                _ => continue,
            }
            if h.flags & HIO_CONST != 0 {
                continue;
            }
            if t.len == MAX_FIELDS {
                t.truncated = true;
                break;
            }
            t.fields[t.len] = HidField {
                kind: h.kind,
                report_id: h.report_id,
                usage: h.usage,
                loc: h.loc,
                flags: h.flags,
                logical_min: h.logical_minimum,
                logical_max: h.logical_maximum,
            };
            t.len += 1;
        }
        t
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &HidField> {
        self.fields[..self.len].iter()
    }

    /// The `idx`-th field named `usage` in stream order — for the touchpad,
    /// finger *i*'s X is `nth(USAGE_X, i)` when each finger collection
    /// repeats the same usages.
    #[must_use]
    pub fn nth(&self, usage: u32, idx: usize) -> Option<&HidField> {
        let mut seen = 0;
        self.fields[..self.len].iter().find(|f| {
            if f.usage == usage {
                if seen == idx {
                    return true;
                }
                seen += 1;
            }
            false
        })
    }

    /// The only field named `usage` among `kind` items — `None` when absent
    /// or ambiguous (touchpads with per-finger collections are ambiguous for
    /// the finger usages; [`FieldTable::nth`] disambiguates).
    #[must_use]
    pub fn first_kind(&self, usage: u32, kind: ItemKind) -> Option<&HidField> {
        self.fields[..self.len]
            .iter()
            .find(|f| f.usage == usage && f.kind == kind)
    }
}
