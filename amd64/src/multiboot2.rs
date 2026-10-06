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

use akuma_fbcon::{Console, PixelFormat, Rgb, Surface};
use spinning_top::Spinlock;
use akuma_multiboot2::{BootInfo, FramebufferKind};
use akuma_ryzen_amd64::{MAX_REGIONS, MachineDescription, MemRegion};

use crate::serial;

/// Where the boot page tables mirror all of physical memory.
const PHYSMAP_BASE: u64 = 0xFFFF_8000_0000_0000;

/// The highest physical address `boot.s` maps — the physmap's own limit, not a
/// copy of it. This was a literal 4 GiB from when `boot.s` mapped four
/// directories, and stayed 4 GiB after the map grew to [`crate::phys::PHYSMAP_LIMIT`].
/// A firmware framebuffer above it was then refused and the boot halted on an
/// EGA text line a UEFI machine cannot display: ryzen's GOP framebuffer is the
/// Radeon's 64-bit BAR at `0x4b0000000` (18.75 GiB), and the first boot there
/// was a black screen (2026-10-06, `docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`).
const MAPPED_LIMIT: u64 = crate::phys::PHYSMAP_LIMIT;

/// The legacy colour-text buffer.
const EGA_TEXT_BASE: u64 = 0xB_8000;
/// Bright white on blue, the attribute byte for [`EGA_TEXT_BASE`].
const EGA_ATTR: u8 = 0x1F;

/// Roughly how long a colour stays up in [`cycle_forever`]. Not calibrated —
/// there is no timer yet — just a spin long enough to be seen.
const CYCLE_SPINS: u64 = 400_000_000;

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
/// `boot.s` maps this range write-back, which for device memory would normally be
/// wrong. It was harmless but slow: the firmware's MTRRs describe everything above
/// the top of usable DRAM as uncacheable, and **UC in the MTRR wins over WB in the
/// PTE**, so every pixel store was its own uncached bus write -- measured on the
/// HP box as 71 MB/s for a full-screen clear and 43 ms for one splash frame.
///
/// [`map_wc`] re-types the framebuffer's own pages write-combining through the PAT
/// (`PAT=WC` beats `MTRR=UC` or `WB` in Intel SDM Table 11-7), so stores gather in the
/// CPU's write-combining buffers and leave as full-line bursts. A WC buffer is
/// **not** guaranteed to drain promptly: [`Framebuffer::flush`] (`sfence`) is called
/// where output goes quiet, or the last glyph can sit in the CPU indefinitely.
/// Reads are still never issued -- `Console` keeps its text in RAM.
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
        Some(Self {
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

    /// Row-wise fill. For 32 bpp a row is written with `rep stosd` (or a few plain
    /// stores when it is short -- a scaled glyph pixel is 2-4 wide and the string
    /// instruction's start-up costs more than the stores); other depths take the
    /// per-pixel default. Clipped to the surface like `put`.
    #[allow(clippy::cast_ptr_alignment, clippy::many_single_char_names)]
    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, color: Rgb) {
        if FB_MUTED.load(core::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let x1 = x.saturating_add(w).min(self.width);
        let y1 = y.saturating_add(h).min(self.height);
        if x >= x1 || y >= y1 {
            return;
        }
        if self.bytes_per_pixel != 4 {
            for yy in y..y1 {
                for xx in x..x1 {
                    self.put(xx, yy, color);
                }
            }
            return;
        }
        let px = self.format.encode(color);
        let n = x1 - x;
        for row in y..y1 {
            // SAFETY: x < x1 <= width and row < y1 <= height, so the span
            // `[offset, offset + 4n)` lies inside `pitch * height` bytes of the
            // mapping `new` checked. The direction flag is clear (SysV ABI), so
            // `stosd` ascends; `rep stosd` stores `n` dwords of `eax` at `rdi`.
            unsafe {
                let p = self.base.add(row * self.pitch + x * 4);
                if n <= 8 {
                    let p = p.cast::<u32>();
                    for i in 0..n {
                        p.add(i).write_volatile(px);
                    }
                } else {
                    core::arch::asm!(
                        "rep stosd",
                        inout("rdi") p => _,
                        inout("rcx") n => _,
                        in("eax") px,
                        options(nostack, preserves_flags),
                    );
                }
            }
        }
    }

    // `base` is a page-aligned framebuffer address and `offset` a multiple of
    // the pixel size, so the cast never misaligns; clippy cannot see that.
    #[allow(clippy::cast_ptr_alignment)]
    fn put(&mut self, x: usize, y: usize, color: Rgb) {
        if x >= self.width || y >= self.height || FB_MUTED.load(core::sync::atomic::Ordering::Relaxed) {
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

impl Framebuffer {
    /// Drain this core's write-combining buffers to the device.
    ///
    /// `sfence` orders and flushes WC stores; ordinary stores and `lock`ed
    /// instructions do not reliably do it. Cheap when nothing is pending.
    #[allow(clippy::unused_self)] // a method so call sites say which surface they drained
    fn flush(&self) {
        // SAFETY: `sfence` has no operands and no effect beyond store ordering.
        unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
    }
}

// ---------------------------------------------------------------------------
// Write-combining the framebuffer
// ---------------------------------------------------------------------------

const IA32_PAT: u32 = 0x277;
/// PAT memory-type encoding for write-combining.
const PAT_WC: u64 = 0x01;
/// The PAT entry we repurpose: index 4, selected by `PAT=1, PCD=0, PWT=0`. Firmware
/// resets it to WB, and **nothing in this kernel sets the PAT bit in any entry**, so
/// no existing mapping changes type. (Entries 0-3 -- the ones PWT/PCD pick -- are not
/// touched at all.)
const PAT_INDEX: u32 = 4;
/// Bit 12 of a 2 MiB PDE / bit 7 of a 4 KiB PTE: the PAT selector bit.
const PDE_PAT: u64 = 1 << 12;
const PTE_PAT: u64 = 1 << 7;
const PDE_LARGE_PRESENT: u64 = 0x81;
const TWO_MIB: u64 = 1 << 21;

unsafe extern "C" {
    /// `boot.s`'s page directories, one per GiB, contiguous: entry `i` maps
    /// `[i * 2 MiB, (i + 1) * 2 MiB)`. In `.bootbss`, whose VMA is its LMA, so the
    /// symbol's address is physical. Shared by the identity map and the physmap, and
    /// by every kernel root and every AP, which copy only the PML4.
    static __pd0: u8;
}

/// A 4 KiB page-table page, for splitting a 2 MiB PDE that the framebuffer only
/// partly covers.
#[repr(C, align(4096))]
struct PtPage(core::cell::UnsafeCell<[u64; 512]>);

// SAFETY: written only by `map_wc`, once, on the boot CPU before any other core is
// running; afterwards the CPU's page walker is the only reader.
unsafe impl Sync for PtPage {}

/// Spare page tables: a framebuffer has at most two partly-covered 2 MiB ends.
static SPLIT_PT: [PtPage; 2] = [const { PtPage(core::cell::UnsafeCell::new([0; 512])) }; 2];

/// Did [`map_wc`] succeed? APs read this to know whether to program their own PAT.
static WC_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// CPUID.01H:EDX bit 16.
fn pat_supported() -> bool {
    core::arch::x86_64::__cpuid(1).edx & (1 << 16) != 0
}

/// Make PAT entry [`PAT_INDEX`] write-combining on **this** core. The PAT MSR is
/// per-core, so every core that may touch the framebuffer needs this, and a core
/// that skips it would see the WC-marked pages as write-back (and, under a UC MTRR,
/// UC) -- slow but not wrong.
fn pat_set_wc_this_cpu() {
    let (lo, hi): (u32, u32);
    // SAFETY: reading IA32_PAT has no side effects; PAT support was checked.
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") IA32_PAT, out("eax") lo, out("edx") hi,
                         options(nomem, nostack, preserves_flags));
    }
    let mut pat = (u64::from(hi) << 32) | u64::from(lo);
    let shift = PAT_INDEX * 8;
    pat = (pat & !(0xFF << shift)) | (PAT_WC << shift);
    // SAFETY: rewrites one PAT byte to a valid encoding (WC) and keeps the other
    // seven as firmware left them. No live mapping selects entry 4 (see
    // `PAT_INDEX`), so no translation changes type under us.
    unsafe {
        core::arch::asm!("wrmsr", in("ecx") IA32_PAT, in("eax") pat as u32,
                         in("edx") (pat >> 32) as u32, options(nostack, preserves_flags));
    }
}

/// Called by each AP early in `ap_entry64`: join the BSP's PAT setting.
pub fn pat_init_ap() {
    if WC_ON.load(core::sync::atomic::Ordering::Acquire) {
        pat_set_wc_this_cpu();
    }
}

/// Re-type the physical range `[phys, phys + len)` write-combining in the boot
/// page tables. Returns whether it took.
///
/// Only the framebuffer's own pages change type. A 2 MiB PDE wholly inside the
/// range just gets the PAT bit; one it covers only partly (the ragged ends -- a
/// 3840x2160x4 framebuffer is 16.9 MiB) is split into 512 4 KiB PTEs, with the PAT
/// bit on exactly the pages that overlap the range, using [`SPLIT_PT`]. The
/// neighbouring MMIO in that 2 MiB window keeps the type it had.
///
/// # Contract
/// Boot CPU, before any AP is started (the TLB shootdown is local `invlpg`), with
/// `phys + len` at most `MAPPED_LIMIT` (checked by `Framebuffer::new`).
fn map_wc(phys: u64, len: u64) -> bool {
    if len == 0 || !pat_supported() {
        return false;
    }
    let end = phys + len;
    // Every PDE in range must be a present 2 MiB entry, or nothing is touched.
    let pd = crate::phys::phys_ptr::<u64>((&raw const __pd0) as u64);
    let (first, last) = ((phys / TWO_MIB) as usize, ((end - 1) / TWO_MIB) as usize);
    let splits = [first, last].iter().filter(|&&i| {
        let base = i as u64 * TWO_MIB;
        !(phys <= base && end >= base + TWO_MIB)
    }).count();
    let splits = if first == last { splits.min(1) } else { splits };
    // SAFETY: indices are below 512 * PHYSMAP_PDS (end <= MAPPED_LIMIT, which is
    // PHYSMAP_LIMIT), so inside `__pd0`.
    let all_large = (first..=last).all(|i| unsafe { pd.add(i).read_volatile() } & PDE_LARGE_PRESENT == PDE_LARGE_PRESENT);
    if !all_large || splits > SPLIT_PT.len() {
        return false;
    }

    pat_set_wc_this_cpu();

    let mut next_pt = 0;
    for i in first..=last {
        let base = i as u64 * TWO_MIB;
        // SAFETY: as above.
        let slot = unsafe { pd.add(i) };
        // SAFETY: as above.
        let old = unsafe { slot.read_volatile() };
        if phys <= base && end >= base + TWO_MIB {
            // SAFETY: setting the PAT selector on a live large PDE re-types its 2 MiB
            // (entry 4 = WC), nothing else.
            unsafe { slot.write_volatile(old | PDE_PAT) };
        } else {
            let pt = SPLIT_PT[next_pt].0.get().cast::<u64>();
            next_pt += 1;
            let flags = old & 0xFFF & !0x80; // keep P/RW; drop PS
            for j in 0..512u64 {
                let pa = base + j * 4096;
                let in_range = pa + 4096 > phys && pa < end;
                // SAFETY: `pt` is this static's 512 entries; `j` < 512.
                unsafe {
                    pt.add(j as usize).write_volatile(pa | flags | if in_range { PTE_PAT } else { 0 });
                }
            }
            let pt_phys = pt as u64 - crate::phys::KERNEL_VMA;
            // The PT is complete before the PDE points at it (x86 stores retire in
            // order; the compiler fence stops reordering the volatile pair).
            core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
            // SAFETY: replaces a live large PDE by a table mapping the same 2 MiB
            // identically except for the PAT bit on the framebuffer's pages.
            unsafe { slot.write_volatile(pt_phys | flags) };
        }
        // SAFETY: invalidates the (large-page) translation of this window on this
        // core; other cores are not running yet (contract above).
        unsafe {
            core::arch::asm!("invlpg [{}]", in(reg) PHYSMAP_BASE + base,
                             options(nostack, preserves_flags));
        }
    }
    WC_ON.store(true, core::sync::atomic::Ordering::Release);
    true
}

/// A program owns the screen through `/dev/fb0` (`crate::fbdev`): every pixel
/// write the console makes is dropped. The console itself keeps running — bytes
/// still reach its grid, the cursor still moves — so handing the screen back is
/// a repaint ([`fb_unmute_and_repaint`]), not a loss of everything printed
/// meanwhile. Read on every `fill`/`put`; a relaxed load is the whole cost.
static FB_MUTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Stop the console drawing: a `/dev/fb0` owner has the pixels.
pub fn fb_mute() {
    FB_MUTED.store(true, core::sync::atomic::Ordering::Release);
}

/// Give the screen back to the console and redraw it from its grid.
///
/// Blocking `lock`: called from `close`/exit and from the console pump, never
/// from an interrupt or fault path (that is [`fb_force_unmute`]).
pub fn fb_unmute_and_repaint() {
    FB_MUTED.store(false, core::sync::atomic::Ordering::Release);
    let mut g = CONSOLE.lock();
    if let Some(c) = g.as_mut() {
        c.0.repaint();
        c.0.surface_mut().flush();
    }
}

/// The crash path's unmute: no lock, no repaint — the fatal dump that follows
/// draws over whatever the program left, which is the point.
pub fn fb_force_unmute() {
    FB_MUTED.store(false, core::sync::atomic::Ordering::Release);
}

/// TSC cycles the boot-time full-screen clear took (see `kmain_mb2`); reported by
/// [`fb_summary`] once the TSC rate is known.
static CLEAR_CYCLES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The framebuffer console, once there is one.
///
/// A `Spinlock<Option<..>>` for the same reason the root filesystem is: there
/// is a window at the start of boot where it does not exist yet, and every
/// print before that has to be a no-op rather than a fault.
static CONSOLE: Spinlock<Option<FbConsole>> = Spinlock::new(None);

/// Wrapper carrying the `Send` the raw framebuffer pointer does not imply.
struct FbConsole(Console<Framebuffer>);

// SAFETY: the pointer inside is a device mapping established by `boot.s`, valid
// for the whole life of the kernel, never freed and never moved. What `Send`
// asks about is whether it may cross threads, and the `Spinlock` around it is
// what actually serialises access -- the raw pointer carries no aliasing claim
// of its own. (That is also the answer to clippy's `non_send_fields_in_send_ty`:
// the field is a raw pointer, and this impl is the statement about it.)
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for FbConsole {}

/// Write one byte to the framebuffer console, if it exists.
///
/// Called from `serial::putb`, which is what makes this the console for
/// **everything** -- kernel diagnostics, the self-test harness, and the output
/// of user programs -- without any of them knowing there is a screen.
pub fn mirror_byte(byte: u8) {
    if let Some(c) = CONSOLE.lock().as_mut() {
        c.0.write_byte(byte);
        // Per line, not per byte: a drain waits for the device. A trailing partial
        // line is flushed by `cursor_idle` when output goes quiet.
        if byte == b'\n' {
            c.0.surface_mut().flush();
        }
    } else if FB_TRACE.load(core::sync::atomic::Ordering::Relaxed) {
        // SAFETY: port 0xE9 is QEMU's `isa-debugcon` and nothing on real PC
        // hardware; the flag that gets us here is a development one.
        unsafe { crate::port::outb(0xE9, byte) };
    }
}

/// Where the bytes the framebuffer **would** get go when there is no
/// framebuffer: QEMU's debug port, so a PVH boot (which has none) can show what
/// the quiet policy lets through (`serial::set_fb_quiet`). Set by the `fbtrace`
/// boot flag; `FBTRACE=<file> sh amd64/run.sh` wires the other end.
static FB_TRACE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Turn the `fbtrace` stand-in on. See [`FB_TRACE`].
pub fn set_fb_trace(on: bool) {
    FB_TRACE.store(on, core::sync::atomic::Ordering::Relaxed);
}

/// Draw the framebuffer console's cursor, if there is a framebuffer console.
///
/// Called by the console pump on a lap that moved nothing — i.e. when output has
/// gone quiet — and never from the output path (`fbcon::Console::show_cursor`
/// says why). `try_lock`, not `lock`: this runs every idle tick from a daemon,
/// and a tick that lands while a printer holds the console should skip a frame
/// of cursor rather than queue behind it. The next lap draws it.
///
/// **Also reports a changed grid**: a program can change the screen margin
/// (`CSI ? 9001 ; x ; y h`), which changes how many rows and columns there are.
/// The new `(rows, columns)` comes back here, once, and the pump tells the console's
/// terminal state, so `stty size` and every program asking `TIOCGWINSZ` follow.
pub fn cursor_idle() -> Option<(u16, u16)> {
    let mut g = CONSOLE.try_lock()?;
    let c = &mut g.as_mut()?.0;
    c.show_cursor();
    c.surface_mut().flush();
    let (rows, cols) = c.take_geometry()?;
    Some((u16::try_from(rows).ok()?, u16::try_from(cols).ok()?))
}

/// The framebuffer console's grid as `(rows, columns)`, or `None` without one.
///
/// What the shell should report for `TIOCGWINSZ`: until 2026-10-01 the console's
/// terminal state said 24x80 whatever the screen was, so `stty size`, `ls`'s
/// column layout and the line editor's wrapping all described a terminal that was
/// not the one on the glass. A blocking `lock`, not `try_lock`: this runs once, from
/// `run_init`, and other cores are printing diagnostics through the same lock at
/// that moment — the first version used `try_lock`, lost that race on the box, and
/// silently did nothing.
#[must_use]
pub fn fb_grid() -> Option<(u16, u16)> {
    let g = CONSOLE.lock();
    let c = &g.as_ref()?.0;
    Some((u16::try_from(c.rows()).ok()?, u16::try_from(c.cols()).ok()?))
}

/// Apply `/etc/console.conf`'s text to the framebuffer console (margin, printing
/// area; `fbcon::config`) and return the `(rows, columns)` now in force, or `None`
/// without a framebuffer console.
///
/// Called once from `run_init`, after the root filesystem is mounted, so a machine's
/// screen setup is a file on its own disk rather than a boot-loader edit. A margin
/// change clears the screen (every cell moved); a printing-area change keeps what is
/// inside it. Blocking `lock`, for the reason [`fb_grid`] gives.
pub fn fb_apply_config(text: &str) -> Option<(u16, u16)> {
    let cfg = akuma_fbcon::config::ConsoleConfig::parse(text);
    let mut g = CONSOLE.lock();
    let c = &mut g.as_mut()?.0;
    let (rows, cols) = if cfg.is_empty() { (c.rows(), c.cols()) } else { c.apply_config(&cfg) };
    Some((u16::try_from(rows).ok()?, u16::try_from(cols).ok()?))
}

/// Draw one frame of the boot splash (`akuma_fbcon::splash`). `try_lock`: a frame
/// that finds the console busy is simply skipped.
pub fn fb_splash_frame(art: &str, info: &[&str], t_ms: u64) {
    let Some(mut g) = CONSOLE.try_lock() else { return };
    if let Some(c) = g.as_mut() {
        akuma_fbcon::splash::paint(&mut c.0, art, info, t_ms);
        c.0.surface_mut().flush();
    }
}

/// End the splash: clear the screen, apply `conf` (the text of `/etc/console.conf`)
/// if given, and return the printing area `(rows, columns)` now in force.
///
/// `blocking` is `false` on the crash path (see `splash::crash`): if the console is
/// busy, skip the clear rather than wait for it — the log replay that follows draws
/// over whatever is there.
pub fn fb_splash_end(conf: Option<&str>, blocking: bool) -> Option<(u16, u16)> {
    let mut g = if blocking { CONSOLE.lock() } else { CONSOLE.try_lock()? };
    let c = &mut g.as_mut()?.0;
    c.clear();
    c.surface_mut().flush();
    if let Some(text) = conf {
        let cfg = akuma_fbcon::config::ConsoleConfig::parse(text);
        if !cfg.is_empty() {
            c.apply_config(&cfg);
        }
    }
    Some((u16::try_from(c.rows()).ok()?, u16::try_from(c.cols()).ok()?))
}

/// Bytes the framebuffer console wants typed back into the program on the
/// console — answers to cursor-position, device-attribute and colour queries
/// (`fbcon::Console::take_reply`). Copied into `out`; returns the count.
///
/// `try_lock`: the pump calls this every lap, and a lap that finds the console
/// busy simply asks again next time. A reply is not lost by waiting.
pub fn fb_take_reply(out: &mut [u8]) -> usize {
    let Some(mut g) = CONSOLE.try_lock() else { return 0 };
    g.as_mut().map_or(0, |c| c.0.take_reply(out))
}

/// Restrict printing to the top-left `cols` x `rows` of the screen
/// (`fbcon::Console::set_view`), returning the size actually in force — the
/// request clamped to what the screen holds, which is what `TIOCGWINSZ` must then
/// report. `None` with no framebuffer console.
///
/// A blocking `lock`: this is an `ioctl`, not the pump, and the answer matters.
pub fn fb_set_view(cols: u16, rows: u16) -> Option<(u16, u16)> {
    let mut g = CONSOLE.lock();
    let c = &mut g.as_mut()?.0;
    c.set_view(usize::from(cols), usize::from(rows));
    Some((u16::try_from(c.rows()).ok()?, u16::try_from(c.cols()).ok()?))
}

/// One `[fb]` line with everything needed to tell *which kind* of "part of the
/// screen does not work" this is: the framebuffer GRUB handed over (width,
/// height, pitch, depth), the cell and scale the console chose, the grid, and the
/// overscan margin. A pitch that is not `width * bytes_per_pixel` (sheared lines)
/// and a grid taller than the visible area (a blank band) look different here.
///
/// Printed at the **end** of the boot log so it is the last thing above the
/// prompt and survives in `dmesg` — the `font:` line at the top scrolls off the
/// screen and out of the 64 KiB ring within minutes.
pub fn fb_summary() {
    let mut g = CONSOLE.lock();
    let Some(c) = g.as_mut() else { return };
    let (cols, rows, scale) = (c.0.max_cols(), c.0.max_rows(), c.0.scale());
    let (vcols, vrows) = (c.0.cols(), c.0.rows());
    let (fw, fh) = (c.0.font().width(), c.0.font().height());
    let (mx, my) = c.0.margin();
    let (w, h, pitch, bpp) = {
        let f = c.0.surface_mut();
        (f.width, f.height, f.pitch, f.bytes_per_pixel)
    };
    drop(g);
    serial::puts("[fb] ");
    serial::put_dec(w as u64);
    serial::puts("x");
    serial::put_dec(h as u64);
    serial::puts(" pitch ");
    serial::put_dec(pitch as u64);
    serial::puts(" (");
    serial::puts(if pitch == w * bpp { "= width*bpp" } else { "NOT width*bpp" });
    serial::puts(") bpp ");
    serial::put_dec(bpp as u64 * 8);
    serial::puts(" cell ");
    serial::put_dec((fw * scale) as u64);
    serial::puts("x");
    serial::put_dec((fh * scale) as u64);
    serial::puts(" grid ");
    serial::put_dec(cols as u64);
    serial::puts("x");
    serial::put_dec(rows as u64);
    serial::puts(" margin ");
    serial::put_dec(mx as u64);
    serial::puts(",");
    serial::put_dec(my as u64);
    // The printing area, when `/etc/console.conf` or `stty` narrowed it.
    if (vcols, vrows) != (cols, rows) {
        serial::puts(" view ");
        serial::put_dec(vcols as u64);
        serial::puts("x");
        serial::put_dec(vrows as u64);
    }
    serial::puts(if WC_ON.load(core::sync::atomic::Ordering::Relaxed) { " wc on" } else { " wc off" });
    // How fast the screen can be written: the whole-surface fill at boot, in ms and as
    // megabytes a second. A framebuffer mapped uncached is the usual cause of a slow one.
    let hz = crate::lapic::tsc_hz();
    let cycles = CLEAR_CYCLES.load(core::sync::atomic::Ordering::Relaxed);
    if hz > 0 && cycles > 0 {
        let us = cycles * 1_000_000 / hz;
        let bytes = (w * h * bpp) as u64;
        serial::puts(" clear ");
        serial::put_dec(us / 1000);
        serial::puts(".");
        serial::put_dec(us % 1000 / 100);
        serial::puts("ms = ");
        serial::put_dec(bytes * 1_000_000 / us.max(1) / 1_000_000);
        serial::puts("MB/s");
    }
    serial::puts("\n");
}

/// What [`init_console`] brought up, for the `font:`/`fb:` lines of the banner.
struct ConsoleUp {
    fb: akuma_multiboot2::Framebuffer,
    font: &'static str,
    fw: usize,
    fh: usize,
    fcols: usize,
    frows: usize,
    fscale: usize,
}

/// The firmware framebuffer and the console on it, or why there is none.
///
/// **Not having one is not fatal.** Every failure here used to `halt` after an
/// [`ega_text`] line, which on a UEFI machine (no VGA text memory) is a black
/// screen with nothing on it — exactly what ryzen's first boot showed
/// (2026-10-06, framebuffer above the old 4 GiB limit). A kernel that keeps
/// going headless still reaches `init`, `dmesg` and whatever log sink or
/// network the machine has, and the reason lands in `dmesg`. Every `CONSOLE`
/// user already treats "no console" as a no-op.
fn init_console(info: &BootInfo<'_>) -> Result<ConsoleUp, &'static str> {
    let Some(fb) = info.framebuffer() else {
        ega_text(1, "no framebuffer tag: booting headless");
        return Err("GRUB provided no framebuffer tag");
    };
    if !matches!(fb.kind, FramebufferKind::Rgb) {
        ega_text(1, "framebuffer is not direct-colour: booting headless");
        return Err("framebuffer is not direct-colour (EGA text mode?)");
    }
    let Some(mut surface) = Framebuffer::new(&fb) else {
        return Err("framebuffer lies above the boot physmap");
    };
    // The same framebuffer, offered to userspace as `/dev/fb0` (`fbdev`).
    crate::fbdev::register(akuma_fbdev::Geometry {
        phys: fb.addr,
        width: fb.width,
        height: fb.height,
        pitch: fb.pitch,
        bpp: u32::from(fb.bpp),
        red: akuma_fbdev::Channel { pos: fb.format.red_pos, len: fb.format.red_size },
        green: akuma_fbdev::Channel { pos: fb.format.green_pos, len: fb.format.green_size },
        blue: akuma_fbdev::Channel { pos: fb.format.blue_pos, len: fb.format.blue_size },
    });

    // Colour before glyphs: the smallest proof that address, pitch and pixel
    // format are all right, with no font and almost no stack involved.
    let (w, h) = (surface.width(), surface.height());
    // Write-combining first: every pixel store after this is cheap, including the
    // first fill. The result is on the `[fb]` line (`wc on|off`).
    map_wc(fb.addr, fb.size_bytes());
    surface.fill(0, 0, w, h, Rgb::new(0x30, 0x00, 0x50));
    surface.flush();

    // Built **straight into the static** rather than into a local first. The
    // console owns its whole character grid (about 54 KiB: code point, colours and
    // flags per cell), and a named local plus the moves around it are copies of
    // that on a boot stack with no guard page beneath it. `Option::map` writes the
    // wrapper in place of the construction, and everything after goes through the
    // lock guard.
    let (font, fw, fh, fcols, frows, fscale) = {
        let mut slot = CONSOLE.lock();
        *slot = Console::new(surface).map(FbConsole);
        let Some(con) = slot.as_mut() else {
            return Err("framebuffer too small for a console");
        };
        let con = &mut con.0;
        con.set_bg(Rgb::new(0x08, 0x0C, 0x14));
        // Timed: a full-screen fill is the one operation whose cost is purely "how fast
        // can this machine write to its framebuffer", reported on the `[fb]` line.
        // SAFETY: RDTSC is unprivileged and present on every x86_64.
        let t0 = unsafe { core::arch::x86_64::_rdtsc() };
        con.clear();
        con.surface_mut().flush(); // so the time includes draining the WC buffers
        let t1 = unsafe { core::arch::x86_64::_rdtsc() };
        CLEAR_CYCLES.store(t1.wrapping_sub(t0), core::sync::atomic::Ordering::Relaxed);
        // Which font and grid the console actually chose. `Console::choose_font`
        // takes that decision from the framebuffer size at runtime — IBM Plex Mono
        // whenever it reaches 80x24, Spleen when it cannot — so on a machine whose
        // only output IS this console, "what am I looking at" was a question only
        // answerable by re-deriving the arithmetic from the mode GRUB happened to
        // pick. Now the console says so itself, in itself.
        (con.font().name(), con.font().width(), con.font().height(), con.cols(), con.rows(), con.scale())
    };
    ega_text(2, "console up");
    Ok(ConsoleUp { fb, font, fw, fh, fcols, frows, fscale })
}

/// Long-mode entry for a GRUB/multiboot2 boot, called from `boot.s`.
///
/// Deliberately parallel to [`crate::kmain`], and in the same order, because
/// that order is load-bearing and documented there. What differs is only where
/// the facts come from: a multiboot2 information block instead of a PVH handoff
/// block, an ext2 image the loader left in RAM instead of a virtio disk, and a
/// framebuffer instead of a serial port.
// The information block's size field is a `u32` at an 8-byte-aligned address
// (the multiboot2 ABI); the byte pointer is how the physmap hands it over.
#[allow(clippy::cast_ptr_alignment)]
#[unsafe(no_mangle)]
pub extern "C" fn kmain_mb2(info_phys: u64) -> ! {
    ega_text(0, "Akuma/amd64: multiboot2 entry reached");
    // Probe for a UART and configure it if one answers. This path never called
    // `serial::init` — the reference machine has no serial port — and got away
    // with it because an absent port ignores writes. Since the probe gates every
    // port access (see `serial::PRESENT`), skipping it would mean no serial
    // output at all on a machine that does have one.
    serial::init();

    // SAFETY: GRUB guarantees %ebx points at an information block whose first
    // u32 is its own total size, and boot.s has mapped all of low physical
    // memory through the physmap -- which survives `drop_identity_map` below,
    // so this slice stays valid for the whole function. The length is clamped
    // so a corrupt size field cannot produce a slice running off the end of
    // what is mapped.
    let bytes: &[u8] = unsafe {
        let p = (PHYSMAP_BASE + info_phys) as *const u8;
        let total = p.cast::<u32>().read_volatile() as usize;
        core::slice::from_raw_parts(p, total.clamp(8, 64 * 1024))
    };

    let Some(info) = BootInfo::new(bytes) else {
        ega_text(1, "FAIL: information block too short");
        crate::halt();
    };
    // `nofb`: do not touch the framebuffer at all — for a machine run as a
    // headless test target (ryzen's reboot loop, `overlays/ryzen/`).
    let console = if info.cmdline().split_ascii_whitespace().any(|t| t == "nofb") {
        Err("`nofb` on the command line")
    } else {
        init_console(&info)
    };

    // **Quiet boot**, decided now and started before the first message below so none
    // of them reaches the screen: the splash instead of a scrolling log. On by
    // default for a `no-tests` build (the build for a machine that is used), `quiet`
    // forces it on for any build, `fbverbose` forces it off. Everything still goes to
    // `dmesg`, and a crash or a boot that never reaches the console brings the log
    // back (`splash`).
    {
        let cmd = info.cmdline();
        let has = |w: &str| cmd.split_ascii_whitespace().any(|t| t == w);
        serial::set_fb_verbose(has("fbverbose"));
        if console.is_ok() && (cfg!(feature = "no-tests") || has("quiet")) && !has("fbverbose") {
            crate::splash::begin();
        }
    }

    // FROM HERE, `serial::puts` REACHES THE SCREEN. `serial::putb` mirrors into
    // the console above, so everything below -- the kernel's own diagnostics,
    // the self-test harness, and anything a user program writes to stdout --
    // appears without knowing a framebuffer exists.
    serial::puts("\nAkuma/amd64 - bare metal, booted by ");
    serial::puts(info.loader_name());
    // The command line, verbatim and early. Every boot option this kernel has
    // — `init=`, `initargs=`, `netprobe`, `nosmp`, `ip=`, `strace` — is a token
    // in here, and without echoing it "did the flag take?" is answered by
    // inferring from behaviour. That inference cost a reboot: `[BKL] stuck:
    // cpu 3` in a photograph said the secondaries were running, which could
    // have meant `nosmp` was broken *or* that the machine had booted a kernel
    // staged five minutes earlier, and nothing on the screen distinguished them.
    serial::puts("\n  cmd:  ");
    serial::puts(info.cmdline());
    serial::puts("\n  uart: ");
    serial::puts(if serial::present() { "present" } else { "absent (reads report no data)" });
    // The keyboard. USB on this board, but firmware's legacy emulation presents
    // it on the i8042 ports for as long as no OS claims the USB controllers —
    // which this kernel never does. See `kbd`.
    serial::puts("  kbd: ");
    serial::puts(if crate::kbd::init() { "i8042 present" } else { "no i8042" });
    match &console {
        Ok(c) => {
            serial::puts("\n  font: ");
            serial::puts(c.font);
            serial::puts(" ");
            serial::put_dec(c.fw as u64);
            serial::puts("x");
            serial::put_dec(c.fh as u64);
            serial::puts(" scale ");
            serial::put_dec(c.fscale as u64);
            serial::puts(" -> ");
            serial::put_dec(c.fcols as u64);
            serial::puts("x");
            serial::put_dec(c.frows as u64);
            serial::puts(" cells");
            serial::puts("\n  fb:   ");
            serial::put_dec(u64::from(c.fb.width));
            serial::puts("x");
            serial::put_dec(u64::from(c.fb.height));
            serial::puts(" @ ");
            serial::put_dec(u64::from(c.fb.bpp));
            serial::puts("bpp, pitch ");
            serial::put_dec(u64::from(c.fb.pitch));
            serial::puts(", at 0x");
            serial::put_hex(c.fb.addr);
        }
        Err(why) => {
            serial::puts("\n  fb:   none, headless (");
            serial::puts(why);
            serial::puts(")");
        }
    }
    serial::puts("\n");

    // Descriptor tables, the BSP's per-CPU block, SMAP, then drop the identity
    // map. Same order and the same reasons as `kmain`; see the comments there.
    // `smp::init_bsp` is not optional on this path either: the scheduler and
    // the syscall stubs reach their per-core state through `%gs`, and without
    // the block installed the first `yield_now` reads address 0.
    // Descriptor tables, per-CPU block, IDT, SMAP/SMEP/WP, the scheduler, and
    // dropping the identity map — the same six steps in the same order as the
    // PVH entry point, and now literally the same code. `boot::early_init` has
    // the reason for each; the ordering constraints between them are subtle
    // enough that keeping two copies was the hazard.
    #[cfg(not(feature = "no-tests"))]
    let smap = crate::boot::early_init();
    #[cfg(feature = "no-tests")]
    let _ = crate::boot::early_init();

    // The machine, as multiboot2 describes it.
    let machine = machine_from(&info);
    serial::puts("  ram:  ");
    serial::put_dec(machine.usable_ram() / (1024 * 1024));
    serial::puts(" MiB usable across ");
    serial::put_dec(machine.regions().len() as u64);
    serial::puts(" regions\n");

    // PCI enumeration. This is the whole reason it matters on this entry: a
    // VMM announces its devices, real firmware announces nothing. The xHCI /
    // EHCI controllers, the Realtek NIC and the AHCI disk are all found here.
    crate::pci::scan();
    crate::pci::report();

    // Immediately: stop any xHCI controller a PREVIOUS boot left running. This
    // is before the memory map is trusted and before anything decides whether
    // to drive USB, because what it defends against is already in flight —
    // see `xhci::quiesce_all`.
    crate::xhci::quiesce_all();

    // The root filesystem is already in memory, and NOTHING IN THE MEMORY MAP
    // SAYS SO. Reserve it before the physical allocator is told anything.
    let module = info.first_module();
    // Every module, as one span: `mem::usable_of` carves it out of each region
    // it touches.
    let reserved = info
        .modules()
        .fold((u64::MAX, 0u64), |(lo, hi), m| (lo.min(u64::from(m.start)), hi.max(u64::from(m.end))));
    let reserved = if reserved.1 == 0 { (0, 0) } else { reserved };
    if let Some(m) = module {
        serial::puts("  mod:  root image at 0x");
        serial::put_hex(u64::from(m.start));
        serial::puts(" + ");
        serial::put_dec(m.len() as u64 / 1024);
        serial::puts(" KiB\n");
    }

    // The information block too: `info` — the command line, the module list —
    // is read for the rest of this function, long after the PMM starts
    // handing out frames, and nothing in the memory map marks its pages either.
    let info_span = (info_phys, info_phys + bytes.len() as u64);
    if !crate::mem::init_reserving(&machine, &[reserved, info_span]) {
        serial::puts("\nAkuma/amd64 - memory bring-up FAILED\n");
        crate::halt();
    }

    // The console hook and the `akuma-exec` runtime, exactly as `kmain` does —
    // one call, because "exactly as `kmain` does" was two lines on that path and
    // one on this one, and the missing one only ever showed up on the metal.
    // See `boot::install_shared_sinks`.
    crate::boot::install_shared_sinks();

    // Intel HDA (runbook add-intel-hda-audio.md). The PVH path gates this on
    // the `pci` cmdline flag (see `kmain`); a firmware boot always has PCI —
    // the scan above already found it — so it runs unconditionally here. It
    // sits after `mem::init_reserving` because `map_bar` allocates frames, and
    // it is best-effort like every other device bring-up on this path: a miss
    // prints one line and boots on. `hdatest` plays a one-second tone.
    crate::hda::init(info.cmdline().split_ascii_whitespace().any(|t| t == "hdatest"));

    // The root filesystem. With `root=/dev/sda1` on the command line, bring up
    // the xHCI + USB mass-storage stack and mount the persistent partition; on
    // any failure fall back to the RAM image the loader left in memory, which is
    // also what a boot with no `root=` token uses. The module pages are reserved
    // (above) regardless — the RAM image stays the recovery root.
    let want_usb_root =
        info.cmdline().split_ascii_whitespace().any(|t| t == "root=/dev/sda1");
    // `usb` alone drives the controller without mounting from it — the shape a
    // bisect wants: prove the bring-up survives before trusting a root on it.
    #[cfg(not(feature = "no-tests"))]
    let want_usb =
        want_usb_root || info.cmdline().split_ascii_whitespace().any(|t| t == "usb");
    let mount_ram = || {
        module
            .and_then(|m| crate::ramdisk::RamDisk::new(u64::from(m.start), m.len()))
            .is_some_and(|rd| {
                crate::fs::mount_root_on(crate::fs::RootDevice::Ram(rd), "module")
            })
    };
    // The hardware watchdog (`wdt`), before anything below can hang: the root
    // mount drives the NVMe or USB controller. Needs the page tables and the
    // PCI scan, both done above; petting starts with the timer tick.
    crate::watchdog::init(info.cmdline());
    // `/dev/wifi0` and its backend (`wifisim` today; the radio later).
    crate::wifi::init(info.cmdline());

    // `root=/dev/nvme0n1pN`: the NVMe disk's GPT partition N (ryzen).
    let want_nvme_root = info
        .cmdline()
        .split_ascii_whitespace()
        .find_map(|t| t.strip_prefix("root=/dev/nvme0n1p"))
        .map(|n| (n.parse::<u32>().ok(), n));
    let have_fs = if want_usb_root && try_usb_root() {
        true
    } else if let Some((Some(part), _)) = want_nvme_root
        && try_nvme_root(part)
    {
        true
    } else {
        if want_usb_root {
            serial::puts("  fs:   USB root unavailable — using the RAM image\n");
        }
        if let Some((_, n)) = want_nvme_root {
            serial::puts("  fs:   NVMe root nvme0n1p");
            serial::puts(n);
            serial::puts(" unavailable — using the RAM image\n");
        }
        mount_ram()
    };

    // Networking: the Realtek NIC if this box has one, loopback only otherwise.
    // Either way `socket(AF_INET)` works for `busybox ifconfig` and `127.0.0.1`.
    let have_net = crate::net::init_bare_metal(info.cmdline());

    // `skiptests` on the command line: bring the machine up to `init` without
    // running the ~200-check self-test suite. The suite is the right default —
    // it is how a regression in a shared crate is caught before the metal — but
    // once a build is trusted, re-proving demand paging and the ELF loader on
    // every reboot is time spent watching a television scroll. This still does
    // the handful of `init_*` calls the suite happens to also perform (the LAPIC,
    // the console fd, the syscall MSRs, the secondary cores): those are real
    // bring-up, not tests.
    let mb2_keep_out = || {
        [
            (info_phys, info_phys + bytes.len() as u64),
            info.first_module().map_or((0, 0), |m| (u64::from(m.start), u64::from(m.end))),
        ]
    };
    let start_secondaries = || {
        let nosmp = info.cmdline().split_ascii_whitespace().any(|t| t == "nosmp");
        if !nosmp && crate::smp::trampoline_page_available(&machine, &mb2_keep_out()) {
            crate::smp::start_secondaries(machine.madt.as_ref());
        }
    };

    // No suite in this build: `boot::late_init` is the same bring-up the
    // `skiptests` arm below performs, and this path is why it is a function.
    // Both arms end in `cycle_forever` (see its note at the tail of the tested
    // one) rather than falling through to a shared call, because the first arm
    // diverges and the compiler is right to call anything after it unreachable.
    #[cfg(feature = "no-tests")]
    {
        crate::boot::late_init(start_secondaries);
        boot_to_init(&info, have_net, have_fs);
        cycle_forever(have_net)
    }

    #[cfg(not(feature = "no-tests"))]
    {
    let skiptests = info.cmdline().split_ascii_whitespace().any(|t| t == "skiptests");
    if skiptests {
        serial::puts("  boot: skiptests — self-test suite bypassed\n");
        crate::boot::late_init(start_secondaries);
        boot_to_init(&info, have_net, have_fs);
        cycle_forever(have_net)
    }

    let mut t = akuma_selftest::Suite::new("Akuma/amd64 self-test", crate::boot::suite_emit);

    // The whole suite, shared with the PVH entry point.
    //
    // This path used to carry its own copy, and the two had drifted: it was
    // **not running** `blk::smoke_test`, `sched::block_smoke_test`, or
    // `usermode::{spawn,console_notify,busybox,execve,fork}_test`. Seven
    // checks — the entire process-lifecycle suite — missing from the one path
    // that runs on real silicon, and invisible because both paths still said
    // `0 failed`. See `boot::self_tests`.
    let verdict = crate::boot::self_tests(
        &mut t,
        &crate::boot::SuiteCtx {
            machine: &machine,
            cmdline: info.cmdline(),
            smap,
            // Firmware boots always have PCI, and the scan already happened
            // above — the Realtek NIC and the xHCI controller are both found
            // through it.
            have_pci: true,
            // Only when asked: bringing the controller up on a box booted from
            // the RAM image is a slow no-op with a real chance of hanging on
            // whatever is plugged in.
            want_xhci: want_usb,
            // No virtio-blk on a firmware boot — the root is a GRUB module in
            // RAM, or ext2 on USB.
            have_disk: false,
            have_fs,
            have_net,
            // What might be sitting on the trampoline page here is not a PVH
            // start-info block but the information block this function is still
            // reading, or the root filesystem GRUB left in RAM.
            keep_out: [
                (info_phys, info_phys + bytes.len() as u64),
                info.first_module().map_or((0, 0), |m| (u64::from(m.start), u64::from(m.end))),
            ],
        },
    );
    if verdict.passed {
        serial::puts("Akuma/amd64 - all self-tests passed\n");
    } else {
        // **The shell starts anyway**, and this is a deliberate reversal.
        //
        // It used to be withheld unless every failure was USB's. On a headless
        // box that is the wrong trade by a wide margin: measured 2026-09-07,
        // one flaky check — `net: the netpoll daemon is being scheduled`,
        // starved for the length of a `[BKL] stuck` window at `SMP=4` — left a
        // machine with DHCP, a synced clock and a running network stack and
        // **no sshd**. No way in, on a box whose only other console is a
        // television in another room; it had to be power-cycled by hand.
        //
        // Withholding the shell protects nothing the log does not already
        // record. Every failure is in `dmesg`, and `dmesg` is exactly what you
        // ssh in to read.
        serial::puts(
            "Akuma/amd64 - SELF-TESTS FAILED; starting init anyway \
             (`dmesg | grep FAILED` for what)\n",
        );
    }
    boot_to_init(&info, have_net, have_fs);

    // Keep driving the scheduler as long as there is a network stack behind it:
    // the netpoll daemon is what answers ARP and ICMP and services a listening
    // socket, and an `init` that exits must not take the machine off the
    // network with it. On a box whose only console is this framebuffer, being
    // pingable after `init` is done is the difference between diagnosable and
    // silent.
    cycle_forever(have_net)
    } // end #[cfg(not(feature = "no-tests"))]
}

/// Bring the network up (DHCP, then the wall clock, then the netpoll daemon),
/// then hand the machine to the `init=` program. Shared by the full-suite path
/// and the `skiptests` path so they cannot drift.
///
/// `run_shell` is `false` when the self-test suite failed — a shell on a kernel
/// whose own tests failed is a way to spend an hour debugging the wrong layer.
fn boot_to_init(info: &BootInfo<'_>, have_net: bool, run_shell: bool) {
    let cmdline = info.cmdline();
    // The scheduler and the timer exist now: the splash can animate.
    crate::splash::spawn_daemon();
    crate::splash::phase(1);
    if have_net {
        // The wall clock, and the order it has to happen in.
        //
        // Until 2026-09-05 this path never fetched one at all — `sync_via_sntp`
        // was called only from `kmain` (PVH), so a bare-metal boot ran at the
        // epoch for ever. That is not cosmetic: `date` said 1970, and **every
        // TLS certificate looked not-yet-valid**, which `apk` reports as
        // `server certificate not trusted`. Hours of "the CA bundle must be
        // wrong" are available to anyone who does not check the clock first;
        // the bundle was fine and present the whole time.
        //
        // DHCP first (SNTP needs an address and a route), then the clock, then
        // the daemon — see `net::settle_for_dhcp` for why the daemon must be
        // last. `clock::sync_tick` in the daemon keeps retrying if this first
        // attempt did not land.
        if crate::net::settle_for_dhcp(SETTLE_BUDGET_MS) {
            crate::clock::sync_via_sntp();
        } else {
            serial::puts("  net:  DHCP did not settle; no wall clock yet (SNTP will retry)\n");
        }
        // A no-op when `boot::self_tests` already spawned one — `spawn_netpoll`
        // is idempotent since 2026-09-07, and it had to become so: this call
        // used to be unconditional, so the full-suite bare-metal boot ran two
        // netpoll daemons while the PVH path ran one. Still called, because the
        // `skiptests` path shares this function and there is no suite to have
        // spawned it.
        crate::net::spawn_netpoll();
        if cmdline.split_ascii_whitespace().any(|t| t == "netprobe") {
            crate::net::enable_probe();
        }
    }
    // `fbverbose`: keep diagnostics on the TV even with a console shell, for
    // debugging a hang on the metal. See `serial::set_fb_quiet`.
    crate::serial::set_fb_verbose(cmdline.split_ascii_whitespace().any(|t| t == "fbverbose"));

    if run_shell {
        crate::splash::phase(2);
        let path = init_path(cmdline);
        // `initargs=a,b,c`, comma-separated as on the PVH path: `init=/bin/busybox
        // initargs=uname,-a` runs one applet and exits.
        let args: alloc::vec::Vec<&str> = cmdline
            .split_ascii_whitespace()
            .find_map(|t| t.strip_prefix("initargs="))
            .map(|v| v.split(',').filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        // stdout reaches the screen through the mirror, and stdin is the i8042
        // (`kbd`, the firmware's USB emulation) through `console`'s pump, which
        // `run_init` starts. A program that reads fd 0 here gets the keyboard
        // — but only the one `init=` names: under `init=/bin/herd` it is herd's
        // `console = true` service that is attached, because herd's other
        // services get pipes (`docs/runbooks/amd64-console-shell.md`).
        // `wdttest`: prove the watchdog resets a wedged kernel, instead of init.
        if cmdline.split_ascii_whitespace().any(|t| t == "wdttest") {
            crate::watchdog::wedge_for_test();
        }
        crate::usermode::run_init(path, &args);
    }
}

/// The `init=` argument from the boot loader's command line, or `/bin/sh`.
fn init_path(cmdline: &str) -> &str {
    for word in cmdline.split(' ') {
        if let Some(rest) = word.strip_prefix("init=")
            && !rest.is_empty()
        {
            return rest;
        }
    }
    "/bin/sh"
}

/// Bring up the xHCI + USB mass-storage stack, sanity-check `/dev/sda`'s MBR,
/// and mount `sda1` as the root filesystem. `false` on any failure — the caller
/// falls back to the RAM image.
pub fn try_usb_root() -> bool {
    if let Err(e) = crate::xhci::init() {
        serial::puts("  fs:   xHCI/USB disk: ");
        serial::puts(e);
        serial::puts("\n");
        return false;
    }
    let mut mbr = [0u8; 512];
    if crate::xhci::read_bytes(0, &mut mbr).is_err() || !crate::xhci::mbr_looks_right(&mbr) {
        serial::puts("  fs:   /dev/sda MBR check failed\n");
        return false;
    }
    crate::fs::mount_root_on(
        crate::fs::RootDevice::Usb(crate::fs::UsbDisk::new(crate::xhci::SDA1_OFFSET)),
        // `/dev/`-prefixed like the virtio root, so `device_is_mounted` can
        // strip it and recognise the partition as one a filesystem is caching.
        "/dev/sda1",
    )
}

/// Bring up the NVMe controller, select GPT partition `part` as the only range
/// it may touch, and mount it as the root filesystem. `false` on any failure —
/// the caller falls back to the RAM image. Nothing is written unless the
/// partition already holds a readable ext2 superblock: `mount_root_on` refuses
/// before any write if it does not.
fn try_nvme_root(part: u32) -> bool {
    if let Err(e) = crate::nvme::init().and_then(|()| crate::nvme::open_partition(part)) {
        serial::puts("  fs:   NVMe: ");
        serial::puts(e);
        serial::puts("\n");
        return false;
    }
    // A fixed table of names because the mount keeps a name and this path
    // must not allocate to make one; partitions past 9 are not expected here.
    const NAMES: [&str; 9] = [
        "/dev/nvme0n1p1", "/dev/nvme0n1p2", "/dev/nvme0n1p3", "/dev/nvme0n1p4", "/dev/nvme0n1p5",
        "/dev/nvme0n1p6", "/dev/nvme0n1p7", "/dev/nvme0n1p8", "/dev/nvme0n1p9",
    ];
    let name = NAMES.get(part as usize - 1).copied().unwrap_or("/dev/nvme0n1");
    crate::fs::mount_root_on(crate::fs::RootDevice::Nvme(crate::fs::NvmeDisk), name)
}

/// Build the description the rest of the kernel expects, from multiboot2 tags.
///
/// Usable regions are copied first. The description holds a fixed number of
/// them and a real machine reports more than that -- this one reported 24, of
/// which most are small reserved ranges -- so an in-order copy would fill the
/// array with reserved entries and drop the RAM.
fn machine_from(info: &BootInfo<'_>) -> MachineDescription {
    // Coalesced, not raw. UEFI reports contiguous RAM as a run of abutting
    // entries, and `mem::init` chooses the region *containing the kernel* to
    // carve the heap out of -- so on a raw map it gets whichever fragment the
    // image happened to land in. Measured on this machine: a 7 MiB answer, on a
    // box with 16 GiB, and a 64 MiB heap that then did not fit.
    let mut usable = [(0u64, 0u64); MAX_REGIONS];
    let n = info.usable_coalesced(&mut usable);

    let mut regions = [MemRegion { addr: 0, size: 0, kind: 0 }; MAX_REGIONS];
    for (i, (base, len)) in usable[..n].iter().enumerate() {
        regions[i] = MemRegion { addr: *base, size: *len, kind: 1 };
    }

    // ACPI, through the loader's copy of the RSDP. On a UEFI machine there is
    // no RSDP in the BIOS window to scan for — the `describe` path's
    // `find_rsdp` would come up empty — but the copy's XSDT pointer is the
    // firmware's real one, and the tables live below 4 GiB where the physmap
    // reaches. The MADT is what tells `smp` how many cores this box has.
    let rsdp = info
        .rsdp()
        .and_then(akuma_ryzen_amd64::acpi::rsdp_from_bytes);
    let madt = rsdp.as_ref().and_then(|r| {
        akuma_ryzen_amd64::acpi::find_table(&crate::machine::Physmap, r, b"APIC")
            .and_then(|t| akuma_ryzen_amd64::acpi::parse_madt(&crate::machine::Physmap, &t))
    });
    MachineDescription::from_memory_map(&regions[..n], rsdp, madt)
}

/// How long to drive the stack by hand waiting for a DHCP lease before giving
/// up and booting without a wall clock.
///
/// Generous, because on this machine the receiver has to be restarted once
/// before anything arrives (`akuma-net-nic`'s `rtl8169` stall recovery) and
/// that takes a couple of seconds of polling to detect. A boot that waits eight
/// seconds and gets a clock is worth more than one that gives up in two and
/// cannot verify a certificate.
const SETTLE_BUDGET_MS: u64 = 8_000;

/// Cycle a band of colour along the bottom, for ever.
///
/// This replaces halting, and it is not decoration. A halted kernel and a
/// crashed one look identical -- both leave whatever was last drawn on the
/// screen. A band that keeps changing says the CPU is still executing our code,
/// which is the single fact hardest to establish on a machine with no serial
/// port, no network and no disk output.
fn cycle_forever(keep_scheduling: bool) -> ! {
    // The BSP is done with kernel code; the secondaries are not (their idle
    // loops keep taking ticks). See `smp::bkl_abandon`.
    //
    // NOT when this loop is still going to drive the scheduler: abandoning the
    // lock and then yielding into tasks that take it is a contradiction. The
    // caller decides, and it decides on whether anything is left to schedule.
    if !keep_scheduling {
        crate::smp::bkl_abandon();
    }
    let palette = [
        Rgb::new(0xE0, 0x50, 0x50),
        Rgb::new(0xE0, 0xC0, 0x40),
        Rgb::new(0x50, 0xD0, 0x60),
        Rgb::new(0x60, 0xA0, 0xE0),
        Rgb::new(0xC0, 0x60, 0xE0),
    ];

    let mut i = 0usize;
    loop {
        // The lock is taken and released around each fill rather than held
        // across the wait: anything else printing would block for the whole
        // cycle, and on this machine printing is the only way to be heard.
        if let Some(c) = CONSOLE.lock().as_mut() {
            let s = c.0.surface_mut();
            let (w, h) = (s.width(), s.height());
            let (mx, my) = (w / 24, h / 24);
            let band_h = (h / 14).max(8);
            let band_y = h.saturating_sub(my + band_h);
            s.fill(mx, band_y, w.saturating_sub(mx * 2), band_h, palette[i % palette.len()]);
            s.flush();
        }
        i += 1;
        for _ in 0..CYCLE_SPINS {
            // Yielding rather than only spinning, when there is something left
            // to run. Without this the netpoll daemon stops the instant `init`
            // exits, and the machine goes from "reachable" to answering
            // nothing — no ARP, no ICMP, no listening socket — while the colour
            // band happily keeps cycling to say the CPU is fine. That cost a
            // whole reboot cycle on the HP box: a boot whose `init` was
            // `busybox ifconfig` (prints, exits in milliseconds) looked
            // identical to a NIC that could not receive.
            if keep_scheduling {
                crate::sched::yield_now();
            } else {
                core::hint::spin_loop();
            }
        }
    }
}
