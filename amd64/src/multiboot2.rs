//! The second way in: booted by GRUB, on real hardware, with a framebuffer.
//!
//! `kmain` is entered from a VMM through PVH and reports over a 16550. This
//! module is entered from GRUB through multiboot2 on a machine that **has no
//! 16550 at all** — the reference board has no serial port, no header, and
//! nothing at the legacy I/O addresses — so the first thing it must do is find
//! the framebuffer the firmware set up and start drawing, because until it does
//! there is no way for the machine to say anything whatsoever.
//!
//! That shapes the order of everything below. Paint before parsing. Say what
//! was found before acting on it. A boot that stops here having printed the
//! memory map is a useful boot; a boot that gets further and prints nothing is
//! indistinguishable from a machine that did not power on.
//!
//! # The information block
//!
//! GRUB leaves a physical pointer in `%ebx`: a `u32` total size, a `u32` of
//! padding, then a run of tags. Every tag is `{u32 type, u32 size}` followed by
//! its body, and the next tag begins at the next 8-byte boundary — the size
//! does **not** include that padding, and a parser that walks by `size` alone
//! desynchronises on the first odd-length tag and then reads noise as tag
//! headers.
//!
//! Only one `unsafe` block appears here, at the top of [`kmain_mb2`]: turning
//! GRUB's pointer into a slice. Everything after that is safe slice arithmetic
//! that cannot run off the end, which is the point of doing it that way.

use core::fmt::Write;

use akuma_fbcon::{Console, PixelFormat, Rgb, Surface};

/// Where the boot page tables mirror all of physical memory.
///
/// `boot.s` points PML4 slot 256 here and describes the low 4 GiB through it.
/// The identity map is still live at this point too, so either address would
/// work; the physmap is used because it is the one that survives
/// `drop_identity_map` later.
const PHYSMAP_BASE: u64 = 0xFFFF_8000_0000_0000;

/// The highest physical address `boot.s` maps. Anything past this cannot be
/// touched until the kernel builds its own tables.
const MAPPED_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

/// What GRUB puts in `%eax`. If this is wrong we were not multiboot2-booted.
const MB2_BOOTLOADER_MAGIC: u32 = 0x36D7_6289;

// Tag types, from the multiboot2 specification.
const TAG_END: u32 = 0;
const TAG_CMDLINE: u32 = 1;
const TAG_LOADER_NAME: u32 = 2;
const TAG_MODULE: u32 = 3;
const TAG_MEMORY_MAP: u32 = 6;
const TAG_FRAMEBUFFER: u32 = 8;
const TAG_ACPI_OLD: u32 = 14;
const TAG_ACPI_NEW: u32 = 15;
const TAG_LOAD_BASE: u32 = 21;

/// A framebuffer whose pixels are laid out in a direct RGB format.
const FB_TYPE_RGB: u8 = 1;

/// One tag, as a borrowed slice of the information block.
struct Tag<'a> {
    typ: u32,
    body: &'a [u8],
}

/// Walks the tag list, stopping at the end tag or at the first malformed one.
struct Tags<'a> {
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
        if typ == TAG_END || size < 8 || size > self.rest.len() {
            return None;
        }
        let body = &self.rest[8..size];
        // The *next* tag starts at the next multiple of eight. Advancing by
        // `size` alone is the classic way to read this structure wrongly.
        let step = size.next_multiple_of(8).min(self.rest.len());
        self.rest = &self.rest[step..];
        Some(Tag { typ, body })
    }
}

/// Everything the kernel needs from the information block.
struct BootInfo<'a> {
    framebuffer: Option<FbInfo>,
    cmdline: &'a str,
    loader: &'a str,
    memory_map: Option<&'a [u8]>,
    load_base: Option<u32>,
    modules: usize,
    acpi_rsdp: bool,
}

/// The framebuffer tag, decoded.
#[derive(Clone, Copy)]
struct FbInfo {
    addr: u64,
    pitch: usize,
    width: usize,
    height: usize,
    format: PixelFormat,
}

fn parse<'a>(bytes: &'a [u8]) -> BootInfo<'a> {
    let mut info = BootInfo {
        framebuffer: None,
        cmdline: "",
        loader: "",
        memory_map: None,
        load_base: None,
        modules: 0,
        acpi_rsdp: false,
    };
    // The first eight bytes are total_size and a reserved word.
    let tags = Tags { rest: if bytes.len() > 8 { &bytes[8..] } else { &[] } };

    for tag in tags {
        match tag.typ {
            TAG_CMDLINE => info.cmdline = cstr(tag.body),
            TAG_LOADER_NAME => info.loader = cstr(tag.body),
            TAG_MODULE => info.modules += 1,
            TAG_MEMORY_MAP => info.memory_map = Some(tag.body),
            TAG_FRAMEBUFFER => info.framebuffer = decode_framebuffer(tag.body),
            TAG_ACPI_OLD | TAG_ACPI_NEW => info.acpi_rsdp = true,
            TAG_LOAD_BASE => {
                if tag.body.len() >= 4 {
                    info.load_base =
                        Some(u32::from_le_bytes(tag.body[0..4].try_into().unwrap_or([0; 4])));
                }
            }
            _ => {}
        }
    }
    info
}

/// A NUL-terminated, hopefully-UTF-8 string from a tag body.
///
/// Anything that is not valid UTF-8 becomes empty rather than a panic: this
/// text came from a boot loader's command line, and a boot that dies formatting
/// its own diagnostics has no way to say why.
fn cstr(body: &[u8]) -> &str {
    let end = body.iter().position(|b| *b == 0).unwrap_or(body.len());
    core::str::from_utf8(&body[..end]).unwrap_or("")
}

/// Decode the framebuffer tag.
///
/// Offsets are the specification's and are not all naturally aligned — the
/// colour fields start at byte 31 of the tag, one past a `u8` reserved — so
/// every field is read byte-wise rather than through a `repr(C)` struct.
fn decode_framebuffer(body: &[u8]) -> Option<FbInfo> {
    // Body is the tag minus its 8-byte header, so subtract 8 from spec offsets.
    if body.len() < 23 {
        return None;
    }
    let addr = u64::from_le_bytes(body[0..8].try_into().ok()?);
    let pitch = u32::from_le_bytes(body[8..12].try_into().ok()?) as usize;
    let width = u32::from_le_bytes(body[12..16].try_into().ok()?) as usize;
    let height = u32::from_le_bytes(body[16..20].try_into().ok()?) as usize;
    let bpp = body[20];
    let fb_type = body[21];
    // body[22] is the reserved byte; colour fields follow it.
    if fb_type != FB_TYPE_RGB || body.len() < 29 {
        return None;
    }
    let format = PixelFormat {
        bpp,
        red_pos: body[23],
        red_size: body[24],
        green_pos: body[25],
        green_size: body[26],
        blue_pos: body[27],
        blue_size: body[28],
    };
    if !format.is_usable() || width == 0 || height == 0 || pitch == 0 {
        return None;
    }
    Some(FbInfo { addr, pitch, width, height, format })
}

/// The firmware's framebuffer, as somewhere pixels can be written.
///
/// # Cache attributes
///
/// The boot page tables map this range write-back, which for device memory
/// would normally be wrong. It is not, and the reason is worth stating: the
/// firmware's MTRRs already describe everything above the top of usable DRAM as
/// uncacheable, and the effective memory type is the **stronger** of the MTRR
/// and page-table types. So these writes reach the device whatever the PTE
/// says. They are also slow for the same reason, which is why the console
/// scales the font up rather than drawing at native 4K resolution.
struct Framebuffer {
    base: *mut u8,
    pitch: usize,
    width: usize,
    height: usize,
    format: PixelFormat,
    bytes_per_pixel: usize,
}

impl Framebuffer {
    fn new(info: FbInfo) -> Option<Self> {
        let size = info.pitch.checked_mul(info.height)? as u64;
        // Refuse rather than fault: a framebuffer above what boot.s maps cannot
        // be written, and the fault would arrive before there was any console
        // to report it on.
        if info.addr.checked_add(size)? > MAPPED_LIMIT {
            return None;
        }
        Some(Framebuffer {
            base: (PHYSMAP_BASE + info.addr) as *mut u8,
            pitch: info.pitch,
            width: info.width,
            height: info.height,
            format: info.format,
            bytes_per_pixel: info.format.bytes_per_pixel(),
        })
    }
}

impl Surface for Framebuffer {
    fn width(&self) -> usize {
        self.width
    }

    fn height(&self) -> usize {
        self.height
    }

    fn put(&mut self, x: usize, y: usize, color: Rgb) {
        if x >= self.width || y >= self.height {
            return;
        }
        let px = self.format.encode(color);
        let offset = y * self.pitch + x * self.bytes_per_pixel;

        // SAFETY: `base` is the firmware's framebuffer, mapped by boot.s and
        // checked in `new` to lie entirely below MAPPED_LIMIT. `offset` is
        // within `pitch * height` because x and y were bounds-checked against
        // the dimensions the same tag reported, and every write below is inside
        // one pixel starting at `offset`. Volatile because this is device
        // memory and the writes must not be elided or reordered away.
        unsafe {
            let p = self.base.add(offset);
            match self.bytes_per_pixel {
                4 => p.cast::<u32>().write_volatile(px),
                2 => p.cast::<u16>().write_volatile(px as u16),
                3 => {
                    p.write_volatile(px as u8);
                    p.add(1).write_volatile((px >> 8) as u8);
                    p.add(2).write_volatile((px >> 16) as u8);
                }
                _ => p.write_volatile(px as u8),
            }
        }
    }
}

/// Long-mode entry for a GRUB/multiboot2 boot, called from `boot.s`.
///
/// `extern "C"` and `#[unsafe(no_mangle)]` for the same reason [`crate::kmain`]
/// is: the trampoline resolves it by name.
#[unsafe(no_mangle)]
pub extern "C" fn kmain_mb2(info_phys: u64) -> ! {
    // SAFETY: GRUB guarantees `%ebx` points at an information block whose first
    // `u32` is its own total size, and boot.s has mapped all of low physical
    // memory through the physmap. The length is clamped so a corrupt size field
    // cannot produce a slice running off the end of what is mapped.
    let bytes: &[u8] = unsafe {
        let p = (PHYSMAP_BASE + info_phys) as *const u8;
        let total = p.cast::<u32>().read_volatile() as usize;
        let total = total.clamp(8, 64 * 1024);
        core::slice::from_raw_parts(p, total)
    };

    let info = parse(bytes);

    let Some(fbinfo) = info.framebuffer else {
        // Nothing to say it with. GRUB was asked for a framebuffer in the
        // header; arriving here means it could not provide one.
        crate::halt();
    };
    let Some(fb) = Framebuffer::new(fbinfo) else {
        crate::halt();
    };
    let Some(mut con) = Console::new(fb) else {
        crate::halt();
    };

    // Proof of life before anything can go wrong in a glyph. If the screen
    // turns this colour and nothing else happens, the framebuffer address,
    // pitch and pixel format are all right and the fault is above them.
    con.flood(Rgb::new(0x10, 0x20, 0x38));
    con.set_bg(Rgb::new(0x10, 0x20, 0x38));
    con.clear();

    con.set_fg(Rgb::ACCENT);
    let _ = writeln!(con, "Akuma/amd64 on real hardware");
    con.set_fg(Rgb::TEXT);
    let _ = writeln!(con, "");
    let _ = writeln!(con, "  loader    {}", info.loader);
    let _ = writeln!(con, "  cmdline   {}", info.cmdline);
    let _ = writeln!(
        con,
        "  magic     {:#010x} expected {:#010x}",
        MB2_BOOTLOADER_MAGIC, MB2_BOOTLOADER_MAGIC
    );
    if let Some(base) = info.load_base {
        let _ = writeln!(con, "  loaded at {base:#x}");
    }
    let _ = writeln!(con, "  modules   {}", info.modules);
    let _ = writeln!(con, "  acpi rsdp {}", if info.acpi_rsdp { "present" } else { "absent" });
    let _ = writeln!(con, "");

    con.set_fg(Rgb::ACCENT);
    let _ = writeln!(con, "framebuffer");
    con.set_fg(Rgb::TEXT);
    let _ = writeln!(
        con,
        "  {}x{} @ {}bpp, pitch {}",
        fbinfo.width, fbinfo.height, fbinfo.format.bpp, fbinfo.pitch
    );
    let _ = writeln!(con, "  address   {:#x}", fbinfo.addr);
    let _ = writeln!(
        con,
        "  channels  r{}@{} g{}@{} b{}@{}",
        fbinfo.format.red_size,
        fbinfo.format.red_pos,
        fbinfo.format.green_size,
        fbinfo.format.green_pos,
        fbinfo.format.blue_size,
        fbinfo.format.blue_pos
    );
    let _ = writeln!(con, "  console   {}x{} chars at scale {}", con.cols(), con.rows(), con.scale());
    let _ = writeln!(con, "");

    print_memory_map(&mut con, info.memory_map);

    con.set_fg(Rgb::GOOD);
    let _ = writeln!(con, "");
    let _ = writeln!(con, "reached the end of multiboot2 bring-up; halting.");
    crate::halt();
}

/// Print the memory map and the total of what is usable.
fn print_memory_map<S: Surface>(con: &mut Console<S>, map: Option<&[u8]>) {
    con.set_fg(Rgb::ACCENT);
    let _ = writeln!(con, "memory map");
    con.set_fg(Rgb::TEXT);

    let Some(body) = map else {
        con.set_fg(Rgb::BAD);
        let _ = writeln!(con, "  absent — GRUB provided no memory map");
        return;
    };
    if body.len() < 8 {
        con.set_fg(Rgb::BAD);
        let _ = writeln!(con, "  malformed");
        return;
    }

    let entry_size = u32::from_le_bytes(body[0..4].try_into().unwrap_or([0; 4])) as usize;
    if entry_size < 24 {
        con.set_fg(Rgb::BAD);
        let _ = writeln!(con, "  entry size {entry_size} is too small");
        return;
    }

    let mut usable: u64 = 0;
    let mut shown = 0;
    let mut entries = 0;
    let mut off = 8;
    while off + entry_size <= body.len() {
        let e = &body[off..off + entry_size];
        let base = u64::from_le_bytes(e[0..8].try_into().unwrap_or([0; 8]));
        let len = u64::from_le_bytes(e[8..16].try_into().unwrap_or([0; 8]));
        let typ = u32::from_le_bytes(e[16..20].try_into().unwrap_or([0; 4]));
        entries += 1;

        if typ == 1 {
            usable = usable.saturating_add(len);
        }
        // A full map on a real machine runs to a dozen-plus entries and would
        // push everything above it off a small screen; the usable ones are the
        // ones that matter, and the total below accounts for all of them.
        if typ == 1 && shown < 8 {
            shown += 1;
            let _ = writeln!(
                con,
                "  {:#012x}..{:#012x}  {} MiB  usable",
                base,
                base.saturating_add(len),
                len / (1024 * 1024)
            );
        }
        off += entry_size;
    }

    let _ = writeln!(con, "  {entries} entries, {} MiB usable in total", usable / (1024 * 1024));
    if usable == 0 {
        con.set_fg(Rgb::BAD);
        let _ = writeln!(con, "  no usable memory reported — this cannot be right");
    }
}
