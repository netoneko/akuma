//! Which code points each baked face really draws, versus the replacement box.
//!
//! The generator falls back to the replacement box for a code point a face has no
//! glyph for, so "the table has an entry" is not "the character is drawn". This
//! pins the ranges the console needs, and prints the rest as information
//! (`cargo test -p akuma-fbcon --test glyph_coverage -- --nocapture`).

use akuma_fbcon::font::{self, Font};

/// The replacement box: what a code point no table holds comes out as.
fn missing(font: &Font, cp: u32) -> bool {
    font.cell_cp(cp) == font.cell_cp(0x10_FFFF)
}

const GROUPS: &[(&str, u32, u32)] = &[
    ("ASCII", 0x20, 0x7E),
    ("Latin-1", 0xA0, 0xFF),
    ("Latin Ext-A", 0x100, 0x17F),
    ("punctuation 2010-2027", 0x2010, 0x2027),
    ("guillemets etc 2030-203A", 0x2030, 0x203A),
    ("euro", 0x20AC, 0x20AC),
    ("arrows", 0x2190, 0x21FF),
    ("geometric shapes", 0x25A0, 0x25FF),
    ("stars", 0x2605, 0x2606),
    ("suits", 0x2660, 0x2667),
    ("notes", 0x266A, 0x266B),
    ("check/cross", 0x2713, 0x2718),
    ("four-pointed stars", 0x2726, 0x2727),
];

#[test]
fn every_face_draws_all_of_ascii() {
    for f in [&font::IBM_PLEX_MONO, &font::SPLEEN] {
        for cp in 0x20..=0x7E {
            assert!(!missing(f, cp), "{} has no glyph for {cp:#x}", f.name());
        }
    }
}

#[test]
fn the_characters_a_tui_needs_are_drawn() {
    // What late.sh and most TUIs print outside ASCII: bullet, middle dot,
    // ellipsis, dashes, curly quotes, arrows, filled/hollow circle, check, a star.
    let needed = [
        0x00B7, 0x2022, 0x2026, 0x2013, 0x2014, 0x2018, 0x2019, 0x201C, 0x201D, 0x2190, 0x2191,
        0x2192, 0x2193, 0x25CF, 0x25CB, 0x25B6, 0x2713, 0x2726,
    ];
    for f in [&font::IBM_PLEX_MONO, &font::SPLEEN] {
        let gone: Vec<String> =
            needed.iter().filter(|&&cp| missing(f, cp)).map(|cp| format!("{cp:#x}")).collect();
        println!("{}: needed-but-missing {gone:?}", f.name());
    }
    // Plex Mono is the default face; hold it to the ones a shell prompt and a
    // chat client use every day.
    for cp in [0x00B7u32, 0x2022, 0x2026, 0x2013, 0x2014, 0x2018, 0x2019, 0x201C, 0x201D, 0x2192] {
        assert!(!missing(&font::IBM_PLEX_MONO, cp), "Plex Mono lacks {cp:#x}");
    }
    // Latin-1 accented letters: Polish/Czech names in a chat room.
    for cp in 0xC0u32..=0xFF {
        assert!(!missing(&font::IBM_PLEX_MONO, cp), "Plex Mono lacks Latin-1 {cp:#x}");
    }
}

#[test]
fn coverage_report() {
    for f in [&font::IBM_PLEX_MONO, &font::SPLEEN] {
        for &(name, a, z) in GROUPS {
            let n = (a..=z).filter(|&cp| missing(f, cp)).count();
            println!("{:14} {:26} {:3} of {:3} fall back to the box", f.name(), name, n, z - a + 1);
        }
    }
}

#[test]
fn a_glyph_beyond_ascii_is_not_blank_and_not_the_box() {
    let a_acute = font::IBM_PLEX_MONO.cell_cp(0xE1); // á
    assert!(a_acute.iter().any(|&c| c > 0), "á drew nothing");
    assert_ne!(a_acute, font::IBM_PLEX_MONO.cell_cp(0x10_FFFF));
    assert_ne!(a_acute, font::IBM_PLEX_MONO.cell_cp(u32::from(b'a')), "á must differ from a");
}
