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
//! boots on a 1024x768 monitor and a 4K television. An 8-pixel glyph is 3 mm on
//! one and 0.6 mm on the other. [`Console::auto_scale`] picks an integer
//! multiplier targeting a readable number of rows, so one small font serves
//! both and there is no second font table to keep in agreement.
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
