//! Main-item kinds and the bit-position vocabulary shared by the parser and
//! the field table.

/// Which report half an item belongs to. Numeric values match the C's
/// `enum hid_kind` ordering (not that anything depends on that).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Input,
    Output,
    Collection,
    Feature,
    EndCollection,
}

/// A field's position inside its report payload. `pos` counts **bits** from
///
/// the start of the payload (the report-ID byte excluded); `count` is the
/// field count for array items, 1 for every per-usage VARIABLE item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidLocation {
    pub pos: u32,
    pub size: u32,
    pub count: u32,
}

impl HidLocation {
    /// Field absent — `size == 0`, which every read treats as "no data",
    /// exactly like the C's zeroed `hid_location`.
    pub const NONE: Self = Self {
        pos: 0,
        size: 0,
        count: 0,
    };

    /// Is this a real field?
    #[must_use]
    pub fn is_some(self) -> bool {
        self.size != 0
    }
}

/// Main item flag: constant (not data).
pub const HIO_CONST: u32 = 0x001;
/// Main item flag: variable (one field per usage), as opposed to array.
pub const HIO_VARIABLE: u32 = 0x002;
/// Main item flag: relative coordinates (a mouse delta, not an position).
pub const HIO_RELATIVE: u32 = 0x004;
/// Main item flag: wraparound value.
pub const HIO_WRAP: u32 = 0x008;
/// Main item flag: nonlinear value.
pub const HIO_NONLINEAR: u32 = 0x010;
/// Main item flag: no preferred state.
pub const HIO_NOPREF: u32 = 0x020;
/// Main item flag: null state exists.
pub const HIO_NULLSTATE: u32 = 0x040;
/// Main item flag: volatile (feature/output items).
pub const HIO_VOLATILE: u32 = 0x080;
/// Main item flag: buffered bytes.
pub const HIO_BUFFEREDBYTE: u32 = 0x100;
