//! Finding one `OperationRegion` declaration in AML bytes.
//!
//! `OperationRegion (NAME, Space, Offset, Length)` encodes as
//! `5B 80 <NameString> <Space:u8> <Offset:TermArg> <Length:TermArg>`
//! (ACPI 6.x §20.2.5.2). For a region in `SystemMemory` (space 0) the offset is
//! a physical address, written as an integer constant: `0A`/`0B`/`0C`/`0E` for a
//! byte/word/dword/qword, or `00`/`01` for the constants zero and one.
//!
//! This is a *scan*, not a parse: it looks for the two-byte opcode and checks
//! what follows. Anything that does not fit — an offset that is a computed
//! expression, a name that is not ours — is skipped, never guessed at. A false
//! positive needs `5B 80`, a name match **and** a valid encoding, and even then
//! the kernel only reads the window, so the worst outcome is implausible data,
//! which [`crate::ec::Reading::validate`] rejects.

/// `ExtOpPrefix`, then `OpRegionOp`.
const OP_REGION: [u8; 2] = [0x5B, 0x80];
/// Address space id for physical memory.
pub const SPACE_SYSTEM_MEMORY: u8 = 0;

/// What an `OperationRegion` declares.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Region {
    pub space: u8,
    pub offset: u64,
    pub len: u64,
}

/// Longest declaration this recognises, in bytes from `5B`.
///
/// Opcode (2), a multi-segment name (up to 1 prefix, 2 and 4×4 here), space (1)
/// and two 9-byte integers. A caller scanning in chunks overlaps by this much so
/// a declaration straddling a chunk edge is still seen whole.
pub const MAX_DECL_LEN: usize = 48;

/// Parse an AML integer constant at the start of `b`: `(value, bytes used)`.
fn int(b: &[u8]) -> Option<(u64, usize)> {
    match *b.first()? {
        0x00 => Some((0, 1)),
        0x01 => Some((1, 1)),
        0x0A => Some((u64::from(*b.get(1)?), 2)),
        0x0B => Some((u64::from(u16::from_le_bytes(b.get(1..3)?.try_into().ok()?)), 3)),
        0x0C => Some((u64::from(u32::from_le_bytes(b.get(1..5)?.try_into().ok()?)), 5)),
        0x0E => Some((u64::from_le_bytes(b.get(1..9)?.try_into().ok()?), 9)),
        _ => None,
    }
}

/// Parse a NameString at the start of `b`, returning its **last** 4-byte
/// segment and the bytes it occupies. Handles root/parent prefixes
/// (`\`, `^`), the dual (`2E`) and multi (`2F`) forms, and the null name.
fn name_string(b: &[u8]) -> Option<([u8; 4], usize)> {
    let mut i = 0;
    while matches!(*b.get(i)?, 0x5C | 0x5E) {
        i += 1;
    }
    let segs = match *b.get(i)? {
        0x2E => {
            i += 1;
            2
        }
        0x2F => {
            let n = usize::from(*b.get(i + 1)?);
            i += 2;
            n
        }
        0x00 => return None, // null name: nothing to match
        _ => 1,
    };
    if segs == 0 {
        return None;
    }
    let last = i + 4 * (segs - 1);
    let seg: [u8; 4] = b.get(last..last + 4)?.try_into().ok()?;
    Some((seg, last + 4))
}

/// The first `OperationRegion` in `buf` whose (last-segment) name is `name` and
/// whose space is `SystemMemory`.
#[must_use]
pub fn find_region(buf: &[u8], name: &[u8; 4]) -> Option<Region> {
    let mut at = 0;
    while at + 2 <= buf.len() {
        let rel = buf[at..].windows(2).position(|w| w == OP_REGION)?;
        let start = at + rel;
        at = start + 1;
        let body = &buf[start + 2..];
        let Some((seg, used)) = name_string(body) else { continue };
        if seg != *name {
            continue;
        }
        let Some(&space) = body.get(used) else { continue };
        if space != SPACE_SYSTEM_MEMORY {
            continue;
        }
        let rest = &body[used + 1..];
        let Some((offset, n)) = int(rest) else { continue };
        let Some((len, _)) = int(&rest[n..]) else { continue };
        return Some(Region { space, offset, len });
    }
    None
}
