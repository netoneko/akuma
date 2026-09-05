//! A text console for a machine whose only output is a framebuffer.
//!
//! On a board with a UART, kernel bring-up prints to `0x3F8` and this crate is
//! unnecessary. The amd64 target's reference machine has **no UART at all** —
//! no port on the back, no header, nothing at the legacy addresses — so the
//! only way a bare-metal boot can say anything is to draw it.
//!
//! What the firmware hands over (through GRUB's multiboot2 framebuffer tag, or
//! a UEFI GOP query) is a **linear array of pixels**: an address, a pitch, a
//! width and height, a depth, and where each colour channel sits inside a
//! pixel. No text mode, no font, no acceleration, no scrolling. Everything
//! above that is this crate.
//!
//! # The three decisions that matter
//!
//! **Never read from video memory.** A framebuffer is mapped uncached or
//! write-combining; reads are one to two orders of magnitude slower than
//! writes. The obvious way to scroll — copy the visible pixels up one line — is
//! therefore the one way you must not do it. [`Console`] keeps the text in a
//! character grid in ordinary RAM and re-draws from that, so video memory is
//! write-only.
//!
//! **Scale the font from the resolution, do not pick a size.** The same kernel
//! boots on a 1024x768 monitor and a 4K television. A 24-pixel glyph is 9 mm on
//! one and 1.8 mm on the other. [`Console::auto_scale`] picks an integer
//! multiplier targeting a readable number of rows, so one font table serves
//! every screen and there is no per-resolution set to keep in agreement.
//!
//! **Anti-alias the glyphs.** [`font`] stores a coverage value per pixel rather
//! than a bit, and [`Rgb::blend`] mixes the edge pixels. That is what lets the
//! default font be [`font::JETBRAINS_MONO`] — an outline face drawn for screens
//! — instead of a bitmap font drawn for a fixed cell. A 1-bit rasterization of
//! such a face has visibly uneven stems, which is the failure that makes
//! outline fonts look wrong on a console and gets blamed on the font.
//!
//! **Leave a margin.** Televisions overscan: they crop a few percent off every
//! edge and show the rest, a habit inherited from analogue broadcast that HDMI
//! never fully shook. Text drawn at `x = 0` can simply not be on the screen,
//! and the failure looks exactly like "the kernel printed nothing".
//! [`Console::auto_margin`] insets by about 4 %.
//!
//! `#![forbid(unsafe_code)]`: the crate never touches the framebuffer itself. It
//! writes through a [`Surface`], and the one unsafe thing in the system — a
//! `write_volatile` to a physical address the firmware named — lives in the
//! consumer that implements it.

#![no_std]
#![forbid(unsafe_code)]

pub mod console;
pub mod font;
pub mod format;

pub use console::Console;
pub use format::PixelFormat;

/// A colour, before it is packed into whatever the hardware's pixel looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rgb {
    /// Red channel, full range regardless of the hardware's depth.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl Rgb {
    /// A colour from its three channels.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Black.
    pub const BLACK: Self = Self::new(0, 0, 0);
    /// White.
    pub const WHITE: Self = Self::new(0xEE, 0xEE, 0xEE);
    /// The colour ordinary kernel output is drawn in.
    pub const TEXT: Self = Self::new(0xC8, 0xD0, 0xD8);
    /// Something went well.
    pub const GOOD: Self = Self::new(0x50, 0xD0, 0x60);
    /// Something needs looking at.
    pub const WARN: Self = Self::new(0xE0, 0xC0, 0x40);
    /// Something failed.
    pub const BAD: Self = Self::new(0xE0, 0x50, 0x50);
    /// Headings and framing.
    pub const ACCENT: Self = Self::new(0x60, 0xA0, 0xE0);

    /// `self` mixed toward `fg`, where `coverage` 0 is all `self` and 255 all
    /// `fg`.
    ///
    /// This is what makes an outline font readable at a console's size. A glyph
    /// edge covers part of a pixel, and drawing that pixel as either wholly ink
    /// or wholly background is what gives a 1-bit rasterization its uneven
    /// stems — the same vertical stroke lands on a pixel boundary in `l` and
    /// half across one in `d`, so one comes out a pixel wide and the other two.
    ///
    /// Integer arithmetic, rounded rather than truncated: truncating leaves
    /// full coverage one short of `fg`, so a solid glyph interior comes out a
    /// shade off the colour that was asked for.
    #[must_use]
    pub const fn blend(self, fg: Self, coverage: u8) -> Self {
        const fn mix(bg: u8, fg: u8, coverage: u8) -> u8 {
            let (bg, fg, c) = (bg as u32, fg as u32, coverage as u32);
            ((fg * c + bg * (255 - c) + 127) / 255) as u8
        }
        Self {
            r: mix(self.r, fg.r, coverage),
            g: mix(self.g, fg.g, coverage),
            b: mix(self.b, fg.b, coverage),
        }
    }
}

/// Somewhere pixels can be written.
///
/// The implementor owns the mapping and the pixel format. Coordinates are in
/// pixels from the top-left of the visible area, and an implementation **must
/// ignore** a coordinate outside its own bounds rather than trusting the
/// caller: a console that scribbles past the end of the framebuffer on a
/// mis-parsed pitch is how a bring-up turns into a triple fault.
pub trait Surface {
    /// Visible width in pixels.
    fn width(&self) -> usize;
    /// Visible height in pixels.
    fn height(&self) -> usize;
    /// Write one pixel. Out-of-bounds coordinates are ignored.
    fn put(&mut self, x: usize, y: usize, color: Rgb);

    /// Fill a rectangle. Clipped to the surface.
    ///
    /// Provided so an implementation can override it with something that writes
    /// whole rows at a time; the default is correct but pixel at a time.
    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, color: Rgb) {
        for dy in 0..h {
            for dx in 0..w {
                self.put(x + dx, y + dy, color);
            }
        }
    }
}
