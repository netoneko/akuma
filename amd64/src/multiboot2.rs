//! The second way in: booted by GRUB, on real hardware, with a framebuffer.
//!
//! `kmain` is entered from a VMM through PVH and reports over a 16550. This is
//! entered from GRUB on a machine that **has no 16550 at all** — no port, no
//! header, nothing at the legacy addresses — so everything here is arranged
//! around one problem: until something is drawn, the machine cannot say
//! anything, including that it failed.
//!
//! # Three output paths, tried in order of how little they assume
//!
//! 1. **The EGA text buffer at `0xB8000`.** Written unconditionally, first,
//!    before anything is parsed. On a UEFI machine in a graphics mode this is
//!    ordinary RAM and nothing appears — it costs six stores to find out, and
//!    on any machine where it *is* live it is the earliest possible output.
//! 2. **A flood of colour.** Proves the framebuffer address, pitch and pixel
//!    format without involving a font. Each stage floods a different colour, so
//!    a screen that stops changing says how far the boot got.
//! 3. **Text.** Everything above has to be right first.
//!
//! And at the end, [`cycle_forever`] instead of halting: a band that keeps
//! changing colour is the difference between "the kernel finished" and "the
//! machine died and the screen kept the last thing on it".
//!
//! # The parsing lives in a crate
//!
//! `akuma-multiboot2` holds it, with tests. The first version of this file
//! parsed the information block inline and had a one-byte error — the
//! framebuffer tag's `reserved` field is a `u16`, so the colour fields start at
//! tag offset 32, not 31 — which produced a zero-width blue channel, a format
//! that failed validation, and a black screen with no way to report it. That
//! cost a reboot cycle on a machine in another room. It is now a unit test.

use core::fmt::Write;

use akuma_fbcon::{Console, PixelFormat, Rgb, Surface};
use akuma_multiboot2::{BootInfo, FramebufferKind};

/// Where the boot page tables mirror all of physical memory.
const PHYSMAP_BASE: u64 = 0xFFFF_8000_0000_0000;

/// The highest physical address `boot.s` maps.
const MAPPED_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

/// The legacy colour-text buffer.
const EGA_TEXT_BASE: u64 = 0xB_8000;
/// Bright white on blue, the attribute byte for [`EGA_TEXT_BASE`].
const EGA_ATTR: u8 = 0x1F;

/// Roughly how long a colour stays up in [`cycle_forever`]. Not calibrated —
/// there is no timer yet — just a spin long enough to be seen.
const CYCLE_SPINS: u64 = 40_000_000;

/// Write a line to the EGA text buffer, in case anything is watching it.
///
/// This is a shot in the dark by design. Under UEFI in a graphics mode the
/// address is plain memory and nothing comes of it; with a CSM, or on a machine
/// whose firmware left the text buffer live, it is the first and cheapest
/// output there is. Either way it happens before any parsing, so it survives
/// every failure below it.
fn ega_text(row: usize, s: &str) {
    let base = (PHYSMAP_BASE + EGA_TEXT_BASE) as *mut u8;
    for (i, b) in s.bytes().take(80).enumerate() {
        let off = (row * 80 + i) * 2;
        // SAFETY: the identity/physmap window covers the first megabyte, so
        // this address is mapped. Writing it is either visible text or a store
        // to unused low RAM; neither can fault, and nothing else claims that
        // range this early in boot.
        unsafe {
            base.add(off).write_volatile(b);
            base.add(off + 1).write_volatile(EGA_ATTR);
        }
    }
}

/// The firmware's framebuffer, as somewhere pixels can be written.
///
/// # Cache attributes
///
/// The boot page tables map this range write-back, which for device memory
/// would normally be wrong. It is not: the firmware's MTRRs already describe
/// everything above the top of usable DRAM as uncacheable, and the effective
/// memory type is the **stronger** of the MTRR and page-table types. The writes
/// reach the device whatever the PTE says — and are slow for the same reason,
/// which is why the console scales a small font up rather than drawing at
/// native 4K.
struct Framebuffer {
    base: *mut u8,
    pitch: usize,
    width: usize,
    height: usize,
    format: PixelFormat,
    bytes_per_pixel: usize,
}

impl Framebuffer {
    fn new(fb: &akuma_multiboot2::Framebuffer) -> Option<Self> {
        let size = fb.size_bytes();
        // Refuse rather than fault: a framebuffer above what boot.s maps cannot
        // be written, and the fault would arrive before there was a console to
        // report it on.
        if fb.addr.checked_add(size)? > MAPPED_LIMIT {
            return None;
        }
        let format = PixelFormat {
            bpp: fb.bpp,
            red_pos: fb.format.red_pos,
            red_size: fb.format.red_size,
            green_pos: fb.format.green_pos,
            green_size: fb.format.green_size,
            blue_pos: fb.format.blue_pos,
            blue_size: fb.format.blue_size,
        };
        Some(Framebuffer {
            base: (PHYSMAP_BASE + fb.addr) as *mut u8,
            pitch: fb.pitch as usize,
            width: fb.width as usize,
            height: fb.height as usize,
            format,
            bytes_per_pixel: format.bytes_per_pixel(),
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
        // inside `pitch * height` because x and y were bounds-checked against
        // the dimensions the same tag reported. Volatile because this is device
        // memory: the writes must not be elided or reordered away.
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
#[unsafe(no_mangle)]
pub extern "C" fn kmain_mb2(info_phys: u64) -> ! {
    ega_text(0, "Akuma/amd64: multiboot2 entry reached");

    // SAFETY: GRUB guarantees %ebx points at an information block whose first
    // u32 is its own total size, and boot.s has mapped all of low physical
    // memory. The length is clamped so a corrupt size field cannot produce a
    // slice running off the end of what is mapped.
    let bytes: &[u8] = unsafe {
        let p = (PHYSMAP_BASE + info_phys) as *const u8;
        let total = p.cast::<u32>().read_volatile() as usize;
        core::slice::from_raw_parts(p, total.clamp(8, 64 * 1024))
    };

    let Some(info) = BootInfo::new(bytes) else {
        ega_text(1, "FAIL: information block too short");
        crate::halt();
    };

    let Some(fb) = info.framebuffer() else {
        ega_text(1, "FAIL: GRUB provided no framebuffer tag");
        crate::halt();
    };

    if !matches!(fb.kind, FramebufferKind::Rgb) {
        ega_text(1, "FAIL: framebuffer is not direct-colour (EGA text mode?)");
        crate::halt();
    }

    let Some(mut surface) = Framebuffer::new(&fb) else {
        ega_text(1, "FAIL: framebuffer lies above the mapped 4 GiB");
        crate::halt();
    };
    ega_text(1, "framebuffer mapped; painting");

    // STAGE ONE, and it happens before the console exists on purpose.
    //
    // This is the smallest possible proof that the address, the pitch and the
    // pixel format are all correct: a few hundred kilobytes of stores through
    // nothing but `Surface::fill`, with no font, no character grid and barely
    // any stack. If the screen turns purple and stops there, everything below
    // this line is what failed. If it stays black, the framebuffer itself is
    // wrong and nothing above this line can be trusted either.
    let (w, h) = (surface.width(), surface.height());
    surface.fill(0, 0, w, h, Rgb::new(0x30, 0x00, 0x50));

    let Some(mut con) = Console::new(surface) else {
        ega_text(2, "FAIL: framebuffer too small for a console");
        crate::halt();
    };

    // Stage two: the console exists. A different colour, so the two stages are
    // told apart by looking rather than by guessing.
    con.set_bg(Rgb::new(0x08, 0x0C, 0x14));
    con.clear();
    ega_text(2, "console up");

    con.set_fg(Rgb::ACCENT);
    let _ = writeln!(con, "Akuma/amd64 on real hardware");
    con.set_fg(Rgb::TEXT);
    let _ = writeln!(con);
    let _ = writeln!(con, "  loader     {}", info.loader_name());
    let _ = writeln!(con, "  cmdline    {}", info.cmdline());
    if let Some(base) = info.load_base() {
        let _ = writeln!(con, "  loaded at  {base:#x}");
    }
    let _ = writeln!(con, "  modules    {}", info.module_count());
    let _ = writeln!(con, "  acpi rsdp  {}", if info.has_acpi() { "present" } else { "absent" });
    let _ = writeln!(con);

    con.set_fg(Rgb::ACCENT);
    let _ = writeln!(con, "framebuffer");
    con.set_fg(Rgb::TEXT);
    let _ = writeln!(con, "  {}x{} at {} bpp, pitch {}", fb.width, fb.height, fb.bpp, fb.pitch);
    let _ = writeln!(con, "  address    {:#x}", fb.addr);
    let _ = writeln!(
        con,
        "  channels   r {}@{}  g {}@{}  b {}@{}{}",
        fb.format.red_size,
        fb.format.red_pos,
        fb.format.green_size,
        fb.format.green_pos,
        fb.format.blue_size,
        fb.format.blue_pos,
        if fb.format_assumed { "  (assumed)" } else { "" }
    );
    let _ = writeln!(con, "  console    {}x{} chars, scale {}", con.cols(), con.rows(), con.scale());
    let _ = writeln!(con);

    print_memory_map(&mut con, &info);

    con.set_fg(Rgb::GOOD);
    let _ = writeln!(con);
    let _ = writeln!(con, "bring-up complete. The band below cycles: the kernel is alive.");

    cycle_forever(&mut con);
}

/// Print the memory map and the total of what is usable.
fn print_memory_map<S: Surface>(con: &mut Console<S>, info: &BootInfo<'_>) {
    con.set_fg(Rgb::ACCENT);
    let _ = writeln!(con, "memory map");
    con.set_fg(Rgb::TEXT);

    let Some(regions) = info.memory_regions() else {
        con.set_fg(Rgb::BAD);
        let _ = writeln!(con, "  absent or malformed");
        return;
    };

    let mut shown = 0;
    let mut total = 0;
    for r in regions {
        total += 1;
        // A real machine reports a dozen-plus regions and would push everything
        // above this off the screen; the usable ones are what matter and the
        // total below accounts for all of them.
        if r.is_usable() && shown < 8 {
            shown += 1;
            let _ = writeln!(
                con,
                "  {:#012x} + {:>6} MiB  usable",
                r.base,
                r.length / (1024 * 1024)
            );
        }
    }
    let usable = info.usable_memory();
    let _ = writeln!(con, "  {total} regions, {} MiB usable in total", usable / (1024 * 1024));
    if usable == 0 {
        con.set_fg(Rgb::BAD);
        let _ = writeln!(con, "  no usable memory reported: this cannot be right");
    }
}

/// Cycle a band of colour along the bottom, for ever.
///
/// This replaces halting, and it is not decoration. A halted kernel and a
/// crashed one look identical — both leave whatever was last drawn on the
/// screen. A band that keeps changing says the CPU is still executing our code,
/// which is the single fact hardest to establish on a machine with no serial
/// port, no network and no disk output.
fn cycle_forever<S: Surface>(con: &mut Console<S>) -> ! {
    let palette = [
        Rgb::new(0xE0, 0x50, 0x50),
        Rgb::new(0xE0, 0xC0, 0x40),
        Rgb::new(0x50, 0xD0, 0x60),
        Rgb::new(0x60, 0xA0, 0xE0),
        Rgb::new(0xC0, 0x60, 0xE0),
    ];

    let surface = con.surface_mut();
    let w = surface.width();
    let h = surface.height();
    let (margin_x, margin_y) = (w / 24, h / 24);
    let band_h = (h / 14).max(8);
    let band_y = h.saturating_sub(margin_y + band_h);
    let band_w = w.saturating_sub(margin_x * 2);

    let mut i = 0usize;
    loop {
        surface.fill(margin_x, band_y, band_w, band_h, palette[i % palette.len()]);
        i += 1;
        for _ in 0..CYCLE_SPINS {
            core::hint::spin_loop();
        }
    }
}
