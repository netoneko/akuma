//! Reading `/proc/boxes` — the kernel's list of live boxes.
//!
//! The file is a header line followed by one CSV row per box:
//!
//! ```text
//! ID,NAME,ROOT,CREATOR,PRIMARY
//! 12345,web,/var/lib/box/containers/web,3,17
//! ```
//!
//! Only box 0 (the host) can read it; inside a box it does not exist. Parsing
//! is separate from reading so the lookups `box use`, `box show`, `box close`
//! and `box ps` all share one interpretation of the table.

use alloc::string::String;
use alloc::vec::Vec;

/// One row of `/proc/boxes`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BoxEntry {
    pub id: u64,
    pub name: String,
    pub root: String,
    pub creator: String,
    pub primary: String,
}

/// Digits only, ignoring anything else — the ids the kernel writes are plain
/// decimal, and a row that somehow is not stays readable rather than
/// disappearing from `box ps`.
fn digits_to_u64(s: &str) -> u64 {
    let mut n = 0u64;
    for b in s.as_bytes() {
        if b.is_ascii_digit() {
            n = n.wrapping_mul(10).wrapping_add(u64::from(*b - b'0'));
        }
    }
    n
}

/// Parse the whole table, skipping the header line.
pub fn parse(content: &str) -> Vec<BoxEntry> {
    content
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut parts = line.split(',');
            BoxEntry {
                id: digits_to_u64(parts.next().unwrap_or("")),
                name: String::from(parts.next().unwrap_or("")),
                root: String::from(parts.next().unwrap_or("")),
                creator: String::from(parts.next().unwrap_or("")),
                primary: String::from(parts.next().unwrap_or("-")),
            }
        })
        .collect()
}

/// A box id written literally: `0x1f4` or `500`. `None` for anything else,
/// including a name — which is the caller's cue to look the table up.
pub fn parse_id(target: &str) -> Option<u64> {
    if let Some(hex) = target.strip_prefix("0x") {
        if hex.is_empty() {
            return None;
        }
        let mut id = 0u64;
        for b in hex.as_bytes() {
            let digit = match *b {
                b'0'..=b'9' => *b - b'0',
                b'a'..=b'f' => *b - b'a' + 10,
                b'A'..=b'F' => *b - b'A' + 10,
                _ => return None,
            };
            id = (id << 4) | u64::from(digit);
        }
        return Some(id);
    }

    if target.is_empty() || !target.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(digits_to_u64(target))
}

/// Resolve what the user typed — an id or a name — against the table.
///
/// An id is taken literally without consulting the table, so `box close 500`
/// works even for a box that has already lost its row.
pub fn resolve(target: &str, content: &str) -> Option<u64> {
    parse_id(target).or_else(|| {
        parse(content)
            .into_iter()
            .find(|e| e.name == target)
            .map(|e| e.id)
    })
}

/// The row for a box id, if it is still listed.
pub fn find(content: &str, id: u64) -> Option<BoxEntry> {
    parse(content).into_iter().find(|e| e.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "ID,NAME,ROOT,CREATOR,PRIMARY\n\
        12345,web,/var/lib/box/containers/web,3,17\n\
        678,db,/var/lib/box/containers/db,3,21\n";

    #[test]
    fn parses_rows_and_skips_the_header() {
        let entries = parse(TABLE);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            BoxEntry {
                id: 12345,
                name: String::from("web"),
                root: String::from("/var/lib/box/containers/web"),
                creator: String::from("3"),
                primary: String::from("17"),
            }
        );
        assert_eq!(entries[1].name, "db");
    }

    #[test]
    fn an_empty_table_has_no_rows() {
        assert!(parse("ID,NAME,ROOT,CREATOR,PRIMARY\n").is_empty());
        // A short read that caught only part of the header is not a box.
        assert!(parse("").is_empty());
        assert!(parse("ID,NAME,ROOT,CREATOR,PRIMARY").is_empty());
    }

    #[test]
    fn a_truncated_row_keeps_the_fields_it_has() {
        let entries = parse("ID,NAME,ROOT,CREATOR,PRIMARY\n7,partial\n");
        assert_eq!(entries[0].id, 7);
        assert_eq!(entries[0].name, "partial");
        assert_eq!(entries[0].root, "");
        assert_eq!(entries[0].primary, "-");
    }

    #[test]
    fn parses_decimal_and_hex_ids() {
        assert_eq!(parse_id("500"), Some(500));
        assert_eq!(parse_id("0x1f4"), Some(500));
        assert_eq!(parse_id("0X1F4"), None); // only the lowercase prefix
        assert_eq!(parse_id("0x1F4"), Some(500));
        assert_eq!(parse_id("0"), Some(0));
    }

    #[test]
    fn a_name_is_not_an_id() {
        assert_eq!(parse_id("web"), None);
        assert_eq!(parse_id("web2"), None);
        assert_eq!(parse_id("2web"), None);
        assert_eq!(parse_id(""), None);
        assert_eq!(parse_id("0x"), None);
        assert_eq!(parse_id("0xzz"), None);
    }

    #[test]
    fn resolves_names_through_the_table() {
        assert_eq!(resolve("web", TABLE), Some(12345));
        assert_eq!(resolve("db", TABLE), Some(678));
        assert_eq!(resolve("nope", TABLE), None);
    }

    #[test]
    fn resolves_ids_without_the_table() {
        // A box that has already been closed can still be named by id.
        assert_eq!(resolve("999", ""), Some(999));
        assert_eq!(resolve("0x3e7", ""), Some(999));
    }

    #[test]
    fn a_numeric_name_resolves_as_an_id_not_a_name() {
        // Documented consequence of ids winning: naming a box `12345` when a
        // different box already has that id makes the name unreachable.
        let table = "ID,NAME,ROOT,CREATOR,PRIMARY\n42,12345,/,3,9\n";
        assert_eq!(resolve("12345", table), Some(12345));
    }

    #[test]
    fn finds_a_row_by_id() {
        assert_eq!(find(TABLE, 678).unwrap().name, "db");
        assert_eq!(find(TABLE, 678).unwrap().root, "/var/lib/box/containers/db");
        assert_eq!(find(TABLE, 1), None);
    }
}
