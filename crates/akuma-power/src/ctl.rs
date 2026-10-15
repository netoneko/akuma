//! The `/dev/power` command lines: what the power daemon is allowed to ask
//! the kernel to do, decoded.
//!
//! The same split as every other control surface in the tree (`akuma-wifi`'s
//! command verbs, `akuma-kacho`'s policy decode): the *decision* — which lines
//! are well-formed, what each clamps to, what is refused — is pure and
//! host-tested here; the kernel side (`amd64/src/power.rs`) only applies what
//! this module accepted. One line is `key=value`, newline-terminated, in the
//! shape `wifi` and the heredoc-ish `/proc` knobs established.
//!
//! Refusals are values, not panics: the daemon is userspace, and a bad write
//! costs it an `EINVAL`, not the box.

/// One accepted command.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Ctl {
    /// `dim=<percent>` — the global console dim factor, clamped to `1..=100`.
    /// `0` is refused on purpose: this lever is the framebuffer console's
    /// colour scaling, which cannot truly blank a panel, and answering a
    /// `dim=0` with "success" would promise a watt the pixel maths does not
    /// deliver. A real blank is the backlight lever (P3), a different command.
    Dim(u8),
}

/// The result of decoding one line: accepted, or refused with the errno the
/// `write(2)` returns.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Parsed {
    Ok(Ctl),
    /// `EINVAL` — malformed, out of range, or a key this kernel does not know.
    Invalid,
}

/// Decode one command line (no trailing newline required; surrounding ASCII
/// whitespace tolerated).
#[must_use]
pub fn parse(line: &[u8]) -> Parsed {
    let line = trim(line);
    let Some((key, value)) = split_at_eq(line) else {
        return Parsed::Invalid;
    };
    match key {
        b"dim" => match parse_u8(value) {
            Some(pct) if (1..=100).contains(&pct) => Parsed::Ok(Ctl::Dim(pct)),
            _ => Parsed::Invalid,
        },
        _ => Parsed::Invalid,
    }
}

fn trim(line: &[u8]) -> &[u8] {
    let is_ws = |c: u8| c.is_ascii_whitespace();
    let start = line.iter().position(|c| !is_ws(*c)).unwrap_or(line.len());
    let end = line.iter().rposition(|c| !is_ws(*c)).map_or(start, |p| p + 1);
    &line[start..end]
}

fn split_at_eq(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let eq = line.iter().position(|c| *c == b'=')?;
    if eq == 0 {
        return None;
    }
    Some((&line[..eq], &line[eq + 1..]))
}

/// `0..=255` in decimal, digits only — no sign, no leading-plus, no overflow
/// wrapping a hostile `dim=99999999999` into something plausible.
fn parse_u8(value: &[u8]) -> Option<u8> {
    if value.is_empty() || value.len() > 3 || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut n: u32 = 0;
    for d in value {
        n = n * 10 + u32::from(d - b'0');
    }
    u8::try_from(n).ok()
}

#[cfg(test)]
mod tests {
    use super::{parse, Ctl, Parsed};

    fn dim(line: &str) -> Option<u8> {
        match parse(line.as_bytes()) {
            Parsed::Ok(Ctl::Dim(p)) => Some(p),
            _ => None,
        }
    }

    #[test]
    fn the_one_verb() {
        assert_eq!(dim("dim=40"), Some(40));
        assert_eq!(dim("dim=100\n"), Some(100));
        assert_eq!(dim(" dim=15 "), Some(15));
        assert_eq!(dim("dim=1"), Some(1));
    }

    #[test]
    fn out_of_range_and_zero_are_refused_not_clamped_into_existence() {
        assert_eq!(parse(b"dim=0"), Parsed::Invalid);
        assert_eq!(parse(b"dim=101"), Parsed::Invalid);
        assert_eq!(parse(b"dim=255"), Parsed::Invalid);
    }

    #[test]
    fn malformed_lines_are_einval() {
        for line in ["", "dim", "dim=", "=40", "dim =40", "dim=x", "dim=-40",
                     "dim=99999999999", "blank=1", "dim:40", "dim=+40"] {
            assert_eq!(parse(line.as_bytes()), Parsed::Invalid, "{line:?}");
        }
    }
}
