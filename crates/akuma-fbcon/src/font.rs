//! The console font: **Spleen**, by Frederic Cambus.
//!
//! ```text
//! Copyright (c) 2018-2026, Frederic Cambus
//! All rights reserved.
//! SPDX-License-Identifier: BSD-2-Clause
//! ```
//!
//! Full text in `vendor/spleen/LICENSE`. Spleen is a monospaced bitmap font
//! designed for consoles and shipped in OpenBSD base; the 8x16 size is used
//! here and the table is generated from the upstream BDF by `build.rs`, so the
//! submodule stays the source of truth and updating the font is a submodule
//! bump rather than a regenerated blob in a diff.
//!
//! One byte per row, most significant bit leftmost, sixteen rows per glyph, in
//! code-point order from `0x20` through `0x7E`. A byte outside that range
//! renders as [`REPLACEMENT`] rather than as whatever follows the table — a
//! console that silently draws garbage for a stray byte is worse than one that
//! draws a visible box.
//!
//! Larger sizes exist upstream (12x24, 16x32, 32x64) and would render sharper
//! than this one scaled up on a 4K display. Adding one is a change to `build.rs`
//! and to how [`crate::Console`] indexes a row, since anything wider than eight
//! pixels needs more than one byte per row.

include!(concat!(env!("OUT_DIR"), "/spleen.rs"));

/// Drawn for any byte outside `FIRST..=LAST`: a hollow box.
pub const REPLACEMENT: [u8; HEIGHT] = [
    0b0000_0000, 0b0000_0000, 0b0111_1110, 0b0100_0010,
    0b0100_0010, 0b0100_0010, 0b0100_0010, 0b0100_0010,
    0b0100_0010, 0b0100_0010, 0b0100_0010, 0b0111_1110,
    0b0000_0000, 0b0000_0000, 0b0000_0000, 0b0000_0000,
];

/// The rows of `byte`'s glyph.
#[must_use]
pub const fn glyph(byte: u8) -> &'static [u8; HEIGHT] {
    if byte < FIRST || byte > LAST {
        return &REPLACEMENT;
    }
    &GLYPHS[(byte - FIRST) as usize]
}
