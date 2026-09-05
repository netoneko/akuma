//! The parser, against information blocks built byte by byte to the
//! specification.
//!
//! These fixtures are assembled here rather than captured from a machine on
//! purpose: the question they answer is "does this code agree with the spec",
//! and a capture can only answer "does it agree with the one loader we tried".
//! The offsets below are written out as literal byte positions so a reader can
//! check them against the specification without reading the implementation.

use akuma_multiboot2::{BootInfo, ChannelLayout, FramebufferKind, tag};

/// Assembles an information block the way GRUB does.
struct Builder {
    tags: Vec<u8>,
}

impl Builder {
    fn new() -> Self {
        Builder { tags: Vec::new() }
    }

    /// Append a tag, padding to the 8-byte boundary the next one starts on.
    fn tag(mut self, typ: u32, body: &[u8]) -> Self {
        let size = 8 + body.len();
        self.tags.extend_from_slice(&(typ as u32).to_le_bytes());
        self.tags.extend_from_slice(&(size as u32).to_le_bytes());
        self.tags.extend_from_slice(body);
        while self.tags.len() % 8 != 0 {
            self.tags.push(0);
        }
        self
    }

    fn build(self) -> Vec<u8> {
        let mut out = Vec::new();
        let total = 8 + self.tags.len() + 8;
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved
        out.extend_from_slice(&self.tags);
        out.extend_from_slice(&0u32.to_le_bytes()); // end tag type
        out.extend_from_slice(&8u32.to_le_bytes()); // end tag size
        out
    }
}

/// A framebuffer tag body, laid out exactly as the specification says.
///
/// Offsets **from the start of the tag**: addr 8, pitch 16, width 20,
/// height 24, bpp 28, type 29, reserved 30 (a `u16`), colour fields 32.
fn framebuffer_body(addr: u64, pitch: u32, w: u32, h: u32, bpp: u8, kind: u8) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&addr.to_le_bytes()); // tag offset 8
    b.extend_from_slice(&pitch.to_le_bytes()); // 16
    b.extend_from_slice(&w.to_le_bytes()); // 20
    b.extend_from_slice(&h.to_le_bytes()); // 24
    b.push(bpp); // 28
    b.push(kind); // 29
    b.extend_from_slice(&0u16.to_le_bytes()); // 30: RESERVED IS A u16
    // 32: colour fields, XRGB8888 as x86 firmware reports it
    b.extend_from_slice(&[16, 8, 8, 8, 0, 8]);
    assert_eq!(b.len(), 30, "body is the tag minus its 8-byte header");
    b
}

/// The bug this crate exists to make impossible: reading `reserved` as one byte
/// puts every colour field one early, and the first visible consequence is a
/// zero-width blue channel.
#[test]
fn the_colour_fields_start_at_tag_offset_32() {
    let body = framebuffer_body(0xE000_0000, 4096, 1024, 768, 32, 1);
    let info_bytes = Builder::new().tag(tag::FRAMEBUFFER, &body).build();
    let fb = BootInfo::new(&info_bytes).unwrap().framebuffer().unwrap();

    assert_eq!(fb.format.red_pos, 16);
    assert_eq!(fb.format.red_size, 8);
    assert_eq!(fb.format.green_pos, 8);
    assert_eq!(fb.format.green_size, 8);
    assert_eq!(fb.format.blue_pos, 0);
    assert_eq!(fb.format.blue_size, 8, "a one-byte shift shows up here first");
    assert!(!fb.format_assumed, "the tag's own layout was usable");
    assert!(fb.is_drawable());
}

#[test]
fn framebuffer_geometry_round_trips() {
    let body = framebuffer_body(0x1234_5678_9ABC_D000, 7680, 1920, 1080, 32, 1);
    let info_bytes = Builder::new().tag(tag::FRAMEBUFFER, &body).build();
    let fb = BootInfo::new(&info_bytes).unwrap().framebuffer().unwrap();

    assert_eq!(fb.addr, 0x1234_5678_9ABC_D000);
    assert_eq!(fb.pitch, 7680);
    assert_eq!(fb.width, 1920);
    assert_eq!(fb.height, 1080);
    assert_eq!(fb.bpp, 32);
    assert_eq!(fb.kind, FramebufferKind::Rgb);
    // Pitch is not width*bpp/8 in general, and the size must follow the pitch.
    assert_eq!(fb.size_bytes(), 7680 * 1080);
}

/// GRUB reports an EGA text buffer when it could not set a graphics mode. That
/// is not something to draw pixels into, and saying so is the difference
/// between a diagnosable boot and a black screen.
#[test]
fn an_ega_text_buffer_is_not_drawable() {
    let body = framebuffer_body(0xB8000, 160, 80, 25, 16, 2);
    let info_bytes = Builder::new().tag(tag::FRAMEBUFFER, &body).build();
    let fb = BootInfo::new(&info_bytes).unwrap().framebuffer().unwrap();
    assert_eq!(fb.kind, FramebufferKind::EgaText);
    assert!(!fb.is_drawable());
}

/// A loader that reports nonsense colour fields still gets a usable console,
/// because on this machine the alternative is no output at all.
#[test]
fn an_unusable_layout_falls_back_to_one_implied_by_the_depth() {
    let mut body = framebuffer_body(0xE000_0000, 4096, 1024, 768, 32, 1);
    body[24..30].copy_from_slice(&[0, 0, 0, 0, 0, 0]); // all channels zero-width
    let info_bytes = Builder::new().tag(tag::FRAMEBUFFER, &body).build();
    let fb = BootInfo::new(&info_bytes).unwrap().framebuffer().unwrap();

    assert!(fb.format_assumed, "the fallback should have been used");
    assert!(fb.format.is_plausible());
    assert!(fb.is_drawable(), "a guessed layout still draws");
    assert_eq!(fb.format, ChannelLayout::assumed_for(32).unwrap());
}

#[test]
fn strings_and_counts_are_read() {
    let info_bytes = Builder::new()
        .tag(tag::CMDLINE, b"init=/bin/paws quiet\0")
        .tag(tag::LOADER_NAME, b"GRUB 2.12\0")
        .tag(tag::MODULE, &[0u8; 16])
        .tag(tag::MODULE, &[0u8; 16])
        .tag(tag::ACPI_NEW, &[0u8; 20])
        .build();
    let info = BootInfo::new(&info_bytes).unwrap();

    assert_eq!(info.cmdline(), "init=/bin/paws quiet");
    assert_eq!(info.loader_name(), "GRUB 2.12");
    assert_eq!(info.module_count(), 2);
    assert!(info.has_acpi());
}

/// Tags are 8-byte aligned but their sizes are not rounded up. A walker that
/// steps by `size` desynchronises here and then reads payload bytes as tag
/// headers — which is how a parser ends up "finding" tags that do not exist.
#[test]
fn an_odd_length_tag_does_not_desynchronise_the_walk() {
    let info_bytes = Builder::new()
        .tag(tag::CMDLINE, b"x\0") // body 2 bytes: tag size 10, padded to 16
        .tag(tag::LOADER_NAME, b"GRUB\0") // would be missed if we stepped by 10
        .build();
    let info = BootInfo::new(&info_bytes).unwrap();
    assert_eq!(info.cmdline(), "x");
    assert_eq!(info.loader_name(), "GRUB", "the walk lost alignment");
    assert_eq!(info.tags().count(), 2);
}

#[test]
fn the_memory_map_is_summed() {
    let mut body = Vec::new();
    body.extend_from_slice(&24u32.to_le_bytes()); // entry_size
    body.extend_from_slice(&0u32.to_le_bytes()); // entry_version
    let mut entry = |base: u64, len: u64, kind: u32| {
        body.extend_from_slice(&base.to_le_bytes());
        body.extend_from_slice(&len.to_le_bytes());
        body.extend_from_slice(&kind.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
    };
    entry(0, 640 * 1024, 1);
    entry(0x10_0000, 3 * 1024 * 1024 * 1024, 1);
    entry(0xE000_0000, 0x1000_0000, 2); // reserved: must not be counted
    let info_bytes = Builder::new().tag(tag::MEMORY_MAP, &body).build();
    let info = BootInfo::new(&info_bytes).unwrap();

    let regions: Vec<_> = info.memory_regions().unwrap().collect();
    assert_eq!(regions.len(), 3);
    assert!(regions[2].kind == 2 && !regions[2].is_usable());
    assert_eq!(info.usable_memory(), 640 * 1024 + 3 * 1024 * 1024 * 1024);
}

/// A memory-map tag claiming an impossible entry size must not loop forever or
/// walk into the middle of records.
#[test]
fn a_corrupt_entry_size_is_refused() {
    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_le_bytes()); // entry_size = 0
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&[0u8; 24]);
    let info_bytes = Builder::new().tag(tag::MEMORY_MAP, &body).build();
    let info = BootInfo::new(&info_bytes).unwrap();
    assert!(info.memory_regions().is_none());
    assert_eq!(info.usable_memory(), 0);
}

/// Truncation is the realistic corruption: a block whose declared size does not
/// match what is there. Nothing may read past the slice.
#[test]
fn a_truncated_block_stops_cleanly() {
    let body = framebuffer_body(0xE000_0000, 4096, 1024, 768, 32, 1);
    let full = Builder::new()
        .tag(tag::CMDLINE, b"hello\0")
        .tag(tag::FRAMEBUFFER, &body)
        .build();

    for cut in 8..full.len() {
        let info = BootInfo::new(&full[..cut]).unwrap();
        // The only requirement is that none of these panic or run away.
        let _ = info.cmdline();
        let _ = info.framebuffer();
        let _ = info.usable_memory();
        assert!(info.tags().count() <= 2);
    }
}

#[test]
fn a_block_with_no_framebuffer_says_so() {
    let info_bytes = Builder::new().tag(tag::CMDLINE, b"\0").build();
    assert!(BootInfo::new(&info_bytes).unwrap().framebuffer().is_none());
}

#[test]
fn a_block_too_short_to_be_one_is_refused() {
    assert!(BootInfo::new(&[]).is_none());
    assert!(BootInfo::new(&[0; 7]).is_none());
    assert!(BootInfo::new(&[0; 8]).is_some());
}

/// A module tag carries `mod_start`, `mod_end` and a string. The end is
/// exclusive, and a kernel that reserves `start..end` inclusive of nothing else
/// keeps its own root filesystem out of the allocator's hands.
#[test]
fn modules_are_located_and_measured() {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0100_0000u32.to_le_bytes()); // start: 16 MiB
    body.extend_from_slice(&0x0180_0000u32.to_le_bytes()); // end: 24 MiB
    body.extend_from_slice(b"rootfs\0");
    let info_bytes = Builder::new().tag(tag::MODULE, &body).build();
    let info = BootInfo::new(&info_bytes).unwrap();

    let m = info.first_module().expect("one module");
    assert_eq!(m.start, 0x0100_0000);
    assert_eq!(m.end, 0x0180_0000);
    assert_eq!(m.len(), 8 * 1024 * 1024);
    assert_eq!(m.string, "rootfs");
    assert!(!m.is_empty());
    assert_eq!(info.modules_end(), 0x0180_0000);
}

#[test]
fn several_modules_report_the_highest_end() {
    let mk = |s: u32, e: u32| {
        let mut b = Vec::new();
        b.extend_from_slice(&s.to_le_bytes());
        b.extend_from_slice(&e.to_le_bytes());
        b.extend_from_slice(b"m\0");
        b
    };
    let info_bytes = Builder::new()
        .tag(tag::MODULE, &mk(0x0200_0000, 0x0210_0000))
        .tag(tag::MODULE, &mk(0x0100_0000, 0x0108_0000))
        .build();
    let info = BootInfo::new(&info_bytes).unwrap();
    assert_eq!(info.modules().count(), 2);
    // The highest end, not the last one reported: reserving to the last would
    // leave the earlier module exposed if the loader ordered them by address.
    assert_eq!(info.modules_end(), 0x0210_0000);
}

/// A module whose end is not past its start is a truncated or corrupt tag, and
/// admitting it would produce a zero-length filesystem and a confusing mount
/// failure rather than an honest "no module".
#[test]
fn a_degenerate_module_is_refused() {
    let mut body = Vec::new();
    body.extend_from_slice(&0x0100_0000u32.to_le_bytes());
    body.extend_from_slice(&0x0100_0000u32.to_le_bytes()); // end == start
    body.extend_from_slice(b"empty\0");
    let info_bytes = Builder::new().tag(tag::MODULE, &body).build();
    let info = BootInfo::new(&info_bytes).unwrap();
    assert!(info.first_module().is_none());
    assert_eq!(info.modules_end(), 0);
}

#[test]
fn no_modules_is_not_an_error() {
    let info_bytes = Builder::new().tag(tag::CMDLINE, b"\0").build();
    let info = BootInfo::new(&info_bytes).unwrap();
    assert!(info.first_module().is_none());
    assert_eq!(info.modules().count(), 0);
    assert_eq!(info.modules_end(), 0);
}

/// Build a memory-map tag body from `(base, length, kind)` triples.
fn memory_map_body(entries: &[(u64, u64, u32)]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&24u32.to_le_bytes()); // entry_size
    body.extend_from_slice(&0u32.to_le_bytes()); // entry_version
    for (base, len, kind) in entries {
        body.extend_from_slice(&base.to_le_bytes());
        body.extend_from_slice(&len.to_le_bytes());
        body.extend_from_slice(&kind.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
    }
    body
}

/// The bug this exists to prevent, in the shape it actually appeared: UEFI
/// reports contiguous RAM as a run of separate abutting entries, so the region
/// "containing the kernel" is a 7 MiB fragment of a 16 GiB machine.
#[test]
fn abutting_usable_regions_merge_into_one() {
    let body = memory_map_body(&[
        (0x0010_0000, 0x0070_0000, 1), // 1 MiB .. 8 MiB
        (0x0080_0000, 0x0080_0000, 1), // 8 MiB .. 16 MiB  -- abuts the above
        (0x0100_0000, 0x3F00_0000, 1), // 16 MiB .. 1 GiB  -- abuts again
    ]);
    let info_bytes = Builder::new().tag(tag::MEMORY_MAP, &body).build();
    let info = BootInfo::new(&info_bytes).unwrap();

    let mut out = [(0u64, 0u64); 16];
    let n = info.usable_coalesced(&mut out);
    assert_eq!(n, 1, "three abutting regions are one region");
    assert_eq!(out[0], (0x0010_0000, 0x3FF0_0000));
    // And the total is preserved: merging must not invent or lose memory.
    assert_eq!(out[0].1, info.usable_memory());
}

#[test]
fn a_gap_keeps_regions_apart() {
    let body = memory_map_body(&[
        (0x0010_0000, 0x0010_0000, 1),
        (0x0100_0000, 0x0010_0000, 1), // a real gap between them
    ]);
    let info_bytes = Builder::new().tag(tag::MEMORY_MAP, &body).build();
    let info = BootInfo::new(&info_bytes).unwrap();
    let mut out = [(0u64, 0u64); 16];
    assert_eq!(info.usable_coalesced(&mut out), 2);
}

/// Firmware does not promise sorted entries, and merging an unsorted list
/// without sorting first silently leaves fragments behind.
#[test]
fn out_of_order_regions_are_sorted_before_merging() {
    let body = memory_map_body(&[
        (0x0100_0000, 0x0100_0000, 1), // 16..32 MiB, reported first
        (0x0010_0000, 0x00F0_0000, 1), // 1..16 MiB, reported second
    ]);
    let info_bytes = Builder::new().tag(tag::MEMORY_MAP, &body).build();
    let info = BootInfo::new(&info_bytes).unwrap();
    let mut out = [(0u64, 0u64); 16];
    assert_eq!(info.usable_coalesced(&mut out), 1);
    assert_eq!(out[0], (0x0010_0000, 0x0200_0000 - 0x0010_0000));
}

#[test]
fn reserved_regions_are_not_merged_in() {
    let body = memory_map_body(&[
        (0x0010_0000, 0x0070_0000, 1),
        (0x0080_0000, 0x0010_0000, 2), // reserved, abutting: must not join
        (0x0090_0000, 0x0010_0000, 1),
    ]);
    let info_bytes = Builder::new().tag(tag::MEMORY_MAP, &body).build();
    let info = BootInfo::new(&info_bytes).unwrap();
    let mut out = [(0u64, 0u64); 16];
    let n = info.usable_coalesced(&mut out);
    assert_eq!(n, 2, "a reserved range in the middle keeps them separate");
    assert_eq!(out[0], (0x0010_0000, 0x0070_0000));
    assert_eq!(out[1], (0x0090_0000, 0x0010_0000));
}

#[test]
fn overlapping_regions_do_not_double_count() {
    let body = memory_map_body(&[
        (0x0010_0000, 0x0100_0000, 1),
        (0x0080_0000, 0x0100_0000, 1), // overlaps the first
    ]);
    let info_bytes = Builder::new().tag(tag::MEMORY_MAP, &body).build();
    let info = BootInfo::new(&info_bytes).unwrap();
    let mut out = [(0u64, 0u64); 16];
    assert_eq!(info.usable_coalesced(&mut out), 1);
    assert_eq!(out[0], (0x0010_0000, 0x0180_0000 - 0x0010_0000));
}
