//! Packing a colour into the hardware's pixel.
//!
//! Firmware does not agree on pixel layout. The same `bpp = 32` is `XRGB` on
//! one machine and `XBGR` on the next, and 16-bit modes split the channels
//! unevenly (5-6-5) because green gets the spare bit. Multiboot2 and UEFI GOP
//! both describe this the same way — a **position and a size, in bits, per
//! channel** — so that is what this takes.
//!
//! Reading those fields rather than assuming a layout is the difference between
//! a boot that prints white text and one that prints blue text, or nothing.

use crate::Rgb;

/// Where each colour channel lives inside one pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    /// Bits per pixel; 15, 16, 24 and 32 are the ones that occur.
    pub bpp: u8,
    /// Bit position of the least significant bit of the red channel.
    pub red_pos: u8,
    /// Width of the red channel in bits.
    pub red_size: u8,
    /// Bit position of the green channel.
    pub green_pos: u8,
    /// Width of the green channel.
    pub green_size: u8,
    /// Bit position of the blue channel.
    pub blue_pos: u8,
    /// Width of the blue channel.
    pub blue_size: u8,
}

impl PixelFormat {
    /// 32 bits per pixel, blue in the low byte — what x86 firmware almost
    /// always reports, and what GOP calls `PixelBlueGreenRedReserved8BitPerColor`.
    pub const XRGB8888: Self = Self {
        bpp: 32,
        red_pos: 16,
        red_size: 8,
        green_pos: 8,
        green_size: 8,
        blue_pos: 0,
        blue_size: 8,
    };

    /// 32 bits per pixel with red and blue the other way round.
    pub const XBGR8888: Self = Self {
        bpp: 32,
        red_pos: 0,
        red_size: 8,
        green_pos: 8,
        green_size: 8,
        blue_pos: 16,
        blue_size: 8,
    };

    /// 16 bits per pixel, 5-6-5.
    pub const RGB565: Self = Self {
        bpp: 16,
        red_pos: 11,
        red_size: 5,
        green_pos: 5,
        green_size: 6,
        blue_pos: 0,
        blue_size: 5,
    };

    /// Bytes one pixel occupies, rounded up.
    #[must_use]
    pub const fn bytes_per_pixel(&self) -> usize {
        (self.bpp as usize).div_ceil(8)
    }

    /// Pack a colour into a pixel.
    ///
    /// Channels narrower than eight bits are taken from the **high** end of the
    /// input, not the low: 5-bit red from `0xFF` must come out all-ones, and
    /// truncating the low bits is what makes white stay white. Taking the low
    /// bits instead turns full brightness into whatever the bottom five bits
    /// happen to be — a bug that shows as a wrongly-coloured but otherwise
    /// perfect display, which is a slow thing to notice.
    #[must_use]
    pub const fn encode(&self, c: Rgb) -> u32 {
        let r = shrink(c.r, self.red_size) << self.red_pos;
        let g = shrink(c.g, self.green_size) << self.green_pos;
        let b = shrink(c.b, self.blue_size) << self.blue_pos;
        r | g | b
    }

    /// Whether this format describes anything drawable.
    ///
    /// A zeroed structure — what a missing or mis-parsed firmware tag leaves
    /// behind — is not: every colour would encode to black, and the console
    /// would appear to do nothing at all.
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        self.bpp >= 8
            && self.bpp <= 32
            && self.red_size > 0
            && self.green_size > 0
            && self.blue_size > 0
            && (self.red_pos as u32 + self.red_size as u32) <= self.bpp as u32
            && (self.green_pos as u32 + self.green_size as u32) <= self.bpp as u32
            && (self.blue_pos as u32 + self.blue_size as u32) <= self.bpp as u32
    }
}

/// Take the top `size` bits of an eight-bit channel.
const fn shrink(value: u8, size: u8) -> u32 {
    if size == 0 {
        return 0;
    }
    if size >= 8 {
        return value as u32;
    }
    (value >> (8 - size)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_bit_channels_pass_through() {
        let f = PixelFormat::XRGB8888;
        assert_eq!(f.encode(Rgb::new(0x12, 0x34, 0x56)), 0x0012_3456);
        assert_eq!(f.encode(Rgb::BLACK), 0);
        assert_eq!(f.encode(Rgb::new(0xFF, 0xFF, 0xFF)), 0x00FF_FFFF);
    }

    /// The channel order is the whole reason this type exists.
    #[test]
    fn red_and_blue_swap_with_the_format() {
        let red = Rgb::new(0xFF, 0, 0);
        assert_eq!(PixelFormat::XRGB8888.encode(red), 0x00FF_0000);
        assert_eq!(PixelFormat::XBGR8888.encode(red), 0x0000_00FF);
    }

    /// White must survive a narrow channel. Taking the low bits instead of the
    /// high ones passes every test that only checks black.
    #[test]
    fn full_brightness_stays_full_in_a_narrow_channel() {
        let f = PixelFormat::RGB565;
        assert_eq!(f.encode(Rgb::new(0xFF, 0xFF, 0xFF)), 0xFFFF);
        assert_eq!(f.encode(Rgb::new(0xFF, 0, 0)), 0xF800);
        assert_eq!(f.encode(Rgb::new(0, 0xFF, 0)), 0x07E0);
        assert_eq!(f.encode(Rgb::new(0, 0, 0xFF)), 0x001F);
    }

    #[test]
    fn a_channel_never_escapes_its_field() {
        let f = PixelFormat::RGB565;
        for v in 0..=255u8 {
            let px = f.encode(Rgb::new(v, v, v));
            assert_eq!(px & !0xFFFF, 0, "value {v} overflowed 16 bits");
        }
    }

    #[test]
    fn bytes_per_pixel_rounds_up() {
        assert_eq!(PixelFormat::XRGB8888.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::RGB565.bytes_per_pixel(), 2);
        let rgb24 = PixelFormat { bpp: 24, ..PixelFormat::XRGB8888 };
        assert_eq!(rgb24.bytes_per_pixel(), 3);
    }

    /// A zeroed format is what a missing firmware tag leaves behind, and it
    /// draws a perfectly black screen. Catching it early turns "nothing
    /// happened" into a message.
    #[test]
    fn a_zeroed_format_is_rejected() {
        let zero = PixelFormat {
            bpp: 0, red_pos: 0, red_size: 0, green_pos: 0,
            green_size: 0, blue_pos: 0, blue_size: 0,
        };
        assert!(!zero.is_usable());
        assert!(PixelFormat::XRGB8888.is_usable());
        assert!(PixelFormat::RGB565.is_usable());

        // A channel that runs off the end of the pixel is equally unusable.
        let overrun = PixelFormat { red_pos: 28, ..PixelFormat::XRGB8888 };
        assert!(!overrun.is_usable());
    }
}
