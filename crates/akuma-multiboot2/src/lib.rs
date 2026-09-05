//! The information block GRUB hands a multiboot2 kernel.
//!
//! A `u32` total size, a `u32` of padding, then a run of tags. Every tag is
//! `{u32 type, u32 size}` followed by its body, and **the next tag begins at
//! the next 8-byte boundary** — the size does not include that padding, so a
//! walker that advances by `size` alone desynchronises on the first
//! odd-length tag and then reads noise as tag headers.
//!
//! # Why this is a crate and not twenty lines in the kernel
//!
//! It was twenty lines in the kernel, and it had a one-byte bug: the framebuffer
//! tag's `reserved` field is a `u16`, not a `u8`, so the colour fields begin at
//! tag offset **32** and not 31. Read one byte early, `blue_size` comes out as
//! zero, the pixel format fails its own validity check, and the kernel — whose
//! only console *is* that framebuffer — has no way to say so. The machine
//! showed a black screen and halted. The cost of finding that was a reboot
//! cycle on hardware in another room.
//!
//! Every offset in this file is now pinned by a test against a byte-for-byte
//! synthetic block built to the specification, which is a thing that can be run
//! in a fifth of a second on a laptop.
//!
//! `#![forbid(unsafe_code)]`: this takes a byte slice. Turning GRUB's physical
//! pointer into one is the caller's problem, and the caller's single `unsafe`.

#![no_std]
#![forbid(unsafe_code)]

/// What GRUB leaves in `%eax`. A block reached without this is not ours.
pub const BOOTLOADER_MAGIC: u32 = 0x36D7_6289;

/// Tag types this crate understands. Others are skipped.
pub mod tag {
    /// End of the tag list.
    pub const END: u32 = 0;
    /// Kernel command line, NUL-terminated.
    pub const CMDLINE: u32 = 1;
    /// Boot loader name, NUL-terminated.
    pub const LOADER_NAME: u32 = 2;
    /// A loaded module.
    pub const MODULE: u32 = 3;
    /// Basic upper/lower memory, in KiB.
    pub const BASIC_MEMINFO: u32 = 4;
    /// The full memory map.
    pub const MEMORY_MAP: u32 = 6;
    /// Framebuffer geometry and pixel layout.
    pub const FRAMEBUFFER: u32 = 8;
    /// A copy of the ACPI 1.0 RSDP.
    pub const ACPI_OLD: u32 = 14;
    /// A copy of the ACPI 2.0+ RSDP.
    pub const ACPI_NEW: u32 = 15;
    /// Physical address the image was loaded at.
    pub const LOAD_BASE_ADDR: u32 = 21;
}

/// How the framebuffer's pixels are organised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramebufferKind {
    /// Palette-indexed. This crate reports it but cannot describe the palette.
    Indexed,
    /// Direct colour, with the channel positions in [`Framebuffer::format`].
    Rgb,
    /// Not a framebuffer at all: an EGA text buffer at `0xB8000`, which is what
    /// GRUB reports when it could not set a graphics mode.
    EgaText,
    /// Something this crate does not know.
    Unknown(u8),
}

impl FramebufferKind {
    const fn from_raw(raw: u8) -> Self {
        match raw {
            0 => FramebufferKind::Indexed,
            1 => FramebufferKind::Rgb,
            2 => FramebufferKind::EgaText,
            other => FramebufferKind::Unknown(other),
        }
    }
}

/// Where each colour channel sits inside a pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChannelLayout {
    /// Bit position of the red channel.
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

impl ChannelLayout {
    /// Whether all three channels have a non-zero width.
    ///
    /// A layout failing this is the signature of a misread tag, not of exotic
    /// hardware: no real framebuffer has a zero-bit channel.
    #[must_use]
    pub const fn is_plausible(&self) -> bool {
        self.red_size > 0 && self.green_size > 0 && self.blue_size > 0
    }

    /// The layout `bpp` implies when the tag's own is unusable.
    ///
    /// A guess, and deliberately one: on a machine whose only console is this
    /// framebuffer, drawing in possibly-wrong colours beats drawing nothing.
    /// 32bpp is `XRGB8888` and 16bpp is `RGB565` on every x86 firmware anyone
    /// has met.
    #[must_use]
    pub const fn assumed_for(bpp: u8) -> Option<Self> {
        match bpp {
            32 | 24 => Some(ChannelLayout {
                red_pos: 16,
                red_size: 8,
                green_pos: 8,
                green_size: 8,
                blue_pos: 0,
                blue_size: 8,
            }),
            16 => Some(ChannelLayout {
                red_pos: 11,
                red_size: 5,
                green_pos: 5,
                green_size: 6,
                blue_pos: 0,
                blue_size: 5,
            }),
            _ => None,
        }
    }
}

/// The framebuffer tag, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framebuffer {
    /// Physical address of the first pixel.
    pub addr: u64,
    /// Bytes between the starts of consecutive rows. **Not** `width * bpp/8`.
    pub pitch: u32,
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Bits per pixel.
    pub bpp: u8,
    /// What kind of framebuffer this is.
    pub kind: FramebufferKind,
    /// Channel positions, as reported.
    pub format: ChannelLayout,
    /// Whether [`Framebuffer::format`] came from the tag or was assumed from
    /// `bpp` because the tag's own fields did not make sense.
    pub format_assumed: bool,
}

impl Framebuffer {
    /// Whether this describes something that can be drawn into.
    #[must_use]
    pub const fn is_drawable(&self) -> bool {
        matches!(self.kind, FramebufferKind::Rgb)
            && self.addr != 0
            && self.width > 0
            && self.height > 0
            && self.pitch > 0
            && self.bpp >= 8
            && self.format.is_plausible()
    }

    /// Total bytes the framebuffer occupies.
    #[must_use]
    pub const fn size_bytes(&self) -> u64 {
        self.pitch as u64 * self.height as u64
    }
}

/// A file the boot loader placed in memory alongside the kernel.
///
/// This is how a kernel with no storage driver gets a root filesystem: GRUB
/// reads the image off whatever it booted from — a disk it already knows how to
/// read — and leaves it in RAM. What arrives is a physical range and a string.
///
/// **Nothing in the memory map marks these frames as taken.** The loader
/// reports them as ordinary available memory, so a kernel that seeds its
/// physical allocator from the map alone will hand out the pages holding its own
/// root filesystem. Reserving [`Module::start`]..[`Module::end`] is the
/// caller's job and is not optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Module<'a> {
    /// Physical address of the first byte.
    pub start: u32,
    /// Physical address one past the last byte.
    pub end: u32,
    /// The string GRUB was given for it in `module2 <file> <string>`.
    pub string: &'a str,
}

impl Module<'_> {
    /// Length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        (self.end - self.start) as usize
    }

    /// Whether the module is empty, which is never useful and usually means a
    /// truncated tag.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// One entry of the memory map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRegion {
    /// Physical base.
    pub base: u64,
    /// Length in bytes.
    pub length: u64,
    /// Region type; 1 is ordinary usable RAM.
    pub kind: u32,
}

impl MemoryRegion {
    /// Whether this is memory the kernel may allocate from.
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        self.kind == 1
    }
}

/// Walks the memory-map tag's entries.
///
/// The tag carries its own `entry_size`, and this honours it rather than
/// assuming 24 bytes: the specification allows a longer entry, and a parser
/// that steps by its own idea of the size walks off into the middle of records.
pub struct MemoryRegions<'a> {
    body: &'a [u8],
    entry_size: usize,
    offset: usize,
}

impl Iterator for MemoryRegions<'_> {
    type Item = MemoryRegion;

    fn next(&mut self) -> Option<MemoryRegion> {
        let e = self.body.get(self.offset..self.offset + self.entry_size)?;
        self.offset += self.entry_size;
        Some(MemoryRegion {
            base: u64::from_le_bytes(e.get(0..8)?.try_into().ok()?),
            length: u64::from_le_bytes(e.get(8..16)?.try_into().ok()?),
            kind: u32::from_le_bytes(e.get(16..20)?.try_into().ok()?),
        })
    }
}

/// One tag: its type and its body, the body being everything after the
/// `{type, size}` header.
#[derive(Debug, Clone, Copy)]
pub struct Tag<'a> {
    /// The tag type; see [`tag`].
    pub typ: u32,
    /// The tag's payload.
    pub body: &'a [u8],
}

/// Walks the tag list.
pub struct Tags<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Tags<'a> {
    type Item = Tag<'a>;

    fn next(&mut self) -> Option<Tag<'a>> {
        if self.rest.len() < 8 {
            return None;
        }
        let typ = u32::from_le_bytes(self.rest[0..4].try_into().ok()?);
        let size = u32::from_le_bytes(self.rest[4..8].try_into().ok()?) as usize;
        if typ == tag::END || size < 8 || size > self.rest.len() {
            return None;
        }
        let body = &self.rest[8..size];
        // Tags are 8-byte aligned; `size` does not include the padding.
        let step = size.next_multiple_of(8).min(self.rest.len());
        self.rest = &self.rest[step..];
        Some(Tag { typ, body })
    }
}

/// The whole information block.
#[derive(Debug, Clone, Copy)]
pub struct BootInfo<'a> {
    bytes: &'a [u8],
}

impl<'a> BootInfo<'a> {
    /// Wrap a block. `None` if it is too short to hold even its own header.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 8 {
            return None;
        }
        Some(BootInfo { bytes })
    }

    /// The size the block claims for itself.
    #[must_use]
    pub fn total_size(&self) -> u32 {
        u32::from_le_bytes(self.bytes[0..4].try_into().unwrap_or([0; 4]))
    }

    /// Every tag, in order.
    #[must_use]
    pub fn tags(&self) -> Tags<'a> {
        Tags { rest: &self.bytes[8..] }
    }

    /// The first tag of a given type.
    #[must_use]
    pub fn tag(&self, typ: u32) -> Option<Tag<'a>> {
        self.tags().find(|t| t.typ == typ)
    }

    /// A NUL-terminated string tag, or `""`.
    ///
    /// Invalid UTF-8 yields `""` rather than a panic: this text came from a
    /// boot loader, and a kernel that dies formatting its own diagnostics has
    /// no way left to explain itself.
    #[must_use]
    pub fn string(&self, typ: u32) -> &'a str {
        let Some(t) = self.tag(typ) else { return "" };
        let end = t.body.iter().position(|b| *b == 0).unwrap_or(t.body.len());
        core::str::from_utf8(&t.body[..end]).unwrap_or("")
    }

    /// The kernel command line.
    #[must_use]
    pub fn cmdline(&self) -> &'a str {
        self.string(tag::CMDLINE)
    }

    /// The boot loader's name.
    #[must_use]
    pub fn loader_name(&self) -> &'a str {
        self.string(tag::LOADER_NAME)
    }

    /// How many modules were loaded alongside the kernel.
    #[must_use]
    pub fn module_count(&self) -> usize {
        self.tags().filter(|t| t.typ == tag::MODULE).count()
    }

    /// Whether an ACPI RSDP was passed.
    #[must_use]
    pub fn has_acpi(&self) -> bool {
        self.rsdp().is_some()
    }

    /// The ACPI RSDP the loader passed, as the bytes of the structure itself
    /// — a **copy** GRUB made, not the firmware's original, so its own address
    /// means nothing but the table pointers inside it are the real ones.
    ///
    /// The 2.0 tag (36 bytes, with the XSDT pointer) is preferred over the 1.0
    /// one (20 bytes, RSDT only) when both are present, which GRUB on UEFI does
    /// produce. This is how a UEFI machine's tables are found at all: there is
    /// no RSDP in the BIOS window there to scan for.
    #[must_use]
    pub fn rsdp(&self) -> Option<&'a [u8]> {
        self.tag(tag::ACPI_NEW)
            .or_else(|| self.tag(tag::ACPI_OLD))
            .map(|t| t.body)
    }

    /// Where the image was loaded, if the loader said.
    #[must_use]
    pub fn load_base(&self) -> Option<u32> {
        let t = self.tag(tag::LOAD_BASE_ADDR)?;
        Some(u32::from_le_bytes(t.body.get(0..4)?.try_into().ok()?))
    }

    /// The memory map, if present.
    #[must_use]
    pub fn memory_regions(&self) -> Option<MemoryRegions<'a>> {
        let t = self.tag(tag::MEMORY_MAP)?;
        let entry_size =
            u32::from_le_bytes(t.body.get(0..4)?.try_into().ok()?) as usize;
        // 24 is the specified minimum; anything smaller is a corrupt tag, and
        // a zero would loop forever.
        if entry_size < 24 {
            return None;
        }
        Some(MemoryRegions { body: t.body, entry_size, offset: 8 })
    }

    /// Total usable RAM, in bytes.
    #[must_use]
    pub fn usable_memory(&self) -> u64 {
        self.memory_regions()
            .into_iter()
            .flatten()
            .filter(MemoryRegion::is_usable)
            .fold(0u64, |acc, r| acc.saturating_add(r.length))
    }

    /// Every module the loader placed in memory, in the order it reported them.
    #[must_use]
    pub fn modules(&self) -> impl Iterator<Item = Module<'a>> + '_ {
        self.tags().filter(|t| t.typ == tag::MODULE).filter_map(|t| {
            let start = u32::from_le_bytes(t.body.get(0..4)?.try_into().ok()?);
            let end = u32::from_le_bytes(t.body.get(4..8)?.try_into().ok()?);
            if end <= start {
                return None;
            }
            let rest = t.body.get(8..).unwrap_or(&[]);
            let n = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
            let string = core::str::from_utf8(&rest[..n]).unwrap_or("");
            Some(Module { start, end, string })
        })
    }

    /// The first module, which by convention is the root filesystem.
    #[must_use]
    pub fn first_module(&self) -> Option<Module<'a>> {
        self.modules().next()
    }

    /// The highest address any module occupies, for the caller's reservation.
    #[must_use]
    pub fn modules_end(&self) -> u64 {
        self.modules().fold(0u64, |acc, m| acc.max(u64::from(m.end)))
    }

    /// Usable memory, sorted and with adjacent ranges merged.
    ///
    /// Writes `(base, length)` pairs into `out` and returns how many. Regions
    /// past `out.len()` are dropped, largest-first ordering being the caller's
    /// business rather than this function's.
    ///
    /// # Why merging is not tidiness
    ///
    /// A VMM reports two or three big regions. **UEFI firmware reports dozens**,
    /// carved up by how the firmware itself used them, and GRUB passes that
    /// fragmentation straight through: contiguous RAM arrives as a run of
    /// separate "available" entries that happen to abut. A kernel that picks
    /// "the region containing my image" out of that raw list gets whichever
    /// fragment it landed in -- measured on real hardware as a **7 MiB** answer
    /// on a machine with 16 GiB of memory, which then failed to fit a 64 MiB
    /// heap. Merging first turns the same map into a handful of large regions
    /// and the question into the one the kernel meant to ask.
    #[must_use]
    pub fn usable_coalesced(&self, out: &mut [(u64, u64)]) -> usize {
        let mut n = 0;
        // Insertion sort by base as they arrive: no allocator here, and the
        // counts involved are dozens.
        for r in self.memory_regions().into_iter().flatten() {
            if !r.is_usable() || r.length == 0 {
                continue;
            }
            if n == out.len() {
                continue;
            }
            let mut i = n;
            while i > 0 && out[i - 1].0 > r.base {
                out[i] = out[i - 1];
                i -= 1;
            }
            out[i] = (r.base, r.length);
            n += 1;
        }

        // Merge anything that touches or overlaps its predecessor.
        let mut w = 0;
        for i in 0..n {
            if w > 0 {
                let (pb, pl) = out[w - 1];
                let pend = pb.saturating_add(pl);
                if out[i].0 <= pend {
                    let end = out[i].0.saturating_add(out[i].1).max(pend);
                    out[w - 1] = (pb, end - pb);
                    continue;
                }
            }
            out[w] = out[i];
            w += 1;
        }
        w
    }

    /// The framebuffer, if the loader provided one.
    ///
    /// # The offsets
    ///
    /// From the start of the tag: `type` 0, `size` 4, `addr` 8, `pitch` 16,
    /// `width` 20, `height` 24, `bpp` 28, `type` 29, **`reserved` 30 as a
    /// `u16`**, and the colour fields from **32**. That `u16` is the whole
    /// reason this function has a doc comment: reading it as a `u8` shifts every
    /// colour field one byte early, which yields a zero-width blue channel and a
    /// framebuffer that validates as unusable.
    #[must_use]
    pub fn framebuffer(&self) -> Option<Framebuffer> {
        let t = self.tag(tag::FRAMEBUFFER)?;
        let b = t.body; // body offsets are tag offsets minus 8
        if b.len() < 22 {
            return None;
        }
        let addr = u64::from_le_bytes(b.get(0..8)?.try_into().ok()?);
        let pitch = u32::from_le_bytes(b.get(8..12)?.try_into().ok()?);
        let width = u32::from_le_bytes(b.get(12..16)?.try_into().ok()?);
        let height = u32::from_le_bytes(b.get(16..20)?.try_into().ok()?);
        let bpp = *b.get(20)?;
        let kind = FramebufferKind::from_raw(*b.get(21)?);
        // b[22..24] is the u16 reserved. Colour fields start at b[24].

        let reported = if b.len() >= 30 {
            ChannelLayout {
                red_pos: b[24],
                red_size: b[25],
                green_pos: b[26],
                green_size: b[27],
                blue_pos: b[28],
                blue_size: b[29],
            }
        } else {
            ChannelLayout::default()
        };

        let (format, format_assumed) = if reported.is_plausible() {
            (reported, false)
        } else {
            match ChannelLayout::assumed_for(bpp) {
                Some(guess) => (guess, true),
                None => (reported, false),
            }
        };

        Some(Framebuffer { addr, pitch, width, height, bpp, kind, format, format_assumed })
    }
}
