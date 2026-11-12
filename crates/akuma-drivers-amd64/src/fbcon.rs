//! The framebuffer console: GRUB's linear framebuffer as a terminal, write-combined, double
//! buffered, with the boot splash and the console's pointer cell.
//!
//! Moved from `crates/akuma-kernel-amd64/src/multiboot2.rs` (2026-11-11) unchanged but for the paths; it is the
//! display class of `akuma_kernel_amd64::drivers`. The machine half keeps the EGA text line
//! (`ega_text`, the output of last resort) and the `fbtrace` stand-in, and reaches everything
//! here through the registered `Display` table, so a build without the `fbcon` feature has
//! no framebuffer console, no `/dev/fb0` (its geometry is registered from [`init_console`])
//! and no `akuma-fbcon`; its console is the serial port.

use akuma_fbcon::{Console, PixelFormat, Rgb, Surface};
use akuma_kernel_amd64::drivers::services::{boot, fbdev, lapic, net, polltime, serial};
use akuma_kernel_amd64::drivers::ConsoleUp;
use akuma_multiboot2::{BootInfo, FramebufferKind};
use akuma_pagetable::Table;
use akuma_primitives::mmio::MmioWindow;
use spinning_top::Spinlock;

/// Where the boot page tables mirror all of physical memory.
const PHYSMAP_BASE: u64 = 0xFFFF_8000_0000_0000;

/// The highest physical address `boot.s` maps — the physmap's own limit, not a
/// copy of it. This was a literal 4 GiB from when `boot.s` mapped four
/// directories, and stayed 4 GiB after the map grew to `PHYSMAP_LIMIT`.
/// A firmware framebuffer above it was then refused and the boot halted on an
/// EGA text line a UEFI machine cannot display: ryzen's GOP framebuffer is the
/// Radeon's 64-bit BAR at `0x4b0000000` (18.75 GiB), and the first boot there
/// was a black screen (2026-10-06, `docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`).
const MAPPED_LIMIT: u64 = akuma_boot_amd64::phys::PHYSMAP_LIMIT;

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
    /// The pixels: `pitch * height` bytes of the firmware's framebuffer.
    win: MmioWindow,
    pitch: usize,
    width: usize,
    height: usize,
    format: PixelFormat,
    bytes_per_pixel: usize,
    /// The RAM copy every draw lands in once [`fb_enable_double_buffer`] has run;
    /// `None` until the heap exists (and for a mode it cannot mirror), in which
    /// case draws go straight to the scanout as they always did.
    back: Option<Back>,
}

/// A cached full-screen copy of the pixels plus, per row, the span (in pixels,
/// half-open) that changed since the last [`Framebuffer::flush`].
///
/// **Why it exists** (`docs/archive/FBCON_DOUBLE_BUFFER.md`): the scanout is
/// write-combining device memory that the panel reads while it is being written, so
/// a program that erases and rewrites lines (Claude Code's TUI, a spinner) showed
/// every intermediate state as flicker. Drawing here is cheap RAM stores; the
/// panel only sees a finished picture, copied in one pass per flush, and a program
/// that asks for it (`CSI ? 2026 h`) controls when that pass happens.
struct Back {
    pix: alloc::vec::Vec<u32>,
    /// Dwords per row (`pitch / 4`).
    stride: usize,
    /// First changed pixel of each row (`u32::MAX` = clean) and one past the last.
    dlo: alloc::vec::Vec<u32>,
    dhi: alloc::vec::Vec<u32>,
}

impl Back {
    /// A clean copy of `stride * height` dwords, or `None` if the heap cannot
    /// hold it. Called **without** the console lock (see [`fb_enable_double_buffer`]).
    fn new(stride: usize, height: usize) -> Option<Self> {
        let len = stride.checked_mul(height)?;
        let mut pix = alloc::vec::Vec::new();
        let mut dlo = alloc::vec::Vec::new();
        let mut dhi = alloc::vec::Vec::new();
        if pix.try_reserve_exact(len).is_err()
            || dlo.try_reserve_exact(height).is_err()
            || dhi.try_reserve_exact(height).is_err()
        {
            return None;
        }
        pix.resize(len, 0u32);
        dlo.resize(height, u32::MAX);
        dhi.resize(height, 0u32);
        Some(Self { pix, stride, dlo, dhi })
    }

    #[inline]
    fn mark(&mut self, y: usize, x0: usize, x1: usize) {
        // x1 <= width < 2^32 and y < height: both checked by the callers.
        if (x0 as u32) < self.dlo[y] {
            self.dlo[y] = x0 as u32;
        }
        if (x1 as u32) > self.dhi[y] {
            self.dhi[y] = x1 as u32;
        }
    }
}

impl Framebuffer {
    /// The shape of the RAM copy this mode needs, `(dwords per row, rows)`, or
    /// `None` for a mode it cannot mirror (not 32 bits per pixel on a dword pitch).
    fn back_shape(&self) -> Option<(usize, usize)> {
        if self.bytes_per_pixel != 4 || !self.pitch.is_multiple_of(4) {
            return None;
        }
        Some((self.pitch / 4, self.height))
    }

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
        // The machine half maps the framebuffer of this boot, once; the driver cannot
        // name an address of its own.
        let win = boot::framebuffer_window()?;
        if win.len() < size as usize {
            return None;
        }
        Some(Self {
            win,
            pitch: fb.pitch as usize,
            width: fb.width as usize,
            height: fb.height as usize,
            format,
            bytes_per_pixel: format.bytes_per_pixel(),
            back: None,
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
        if let Some(b) = self.back.as_mut() {
            for row in y..y1 {
                let at = row * b.stride + x;
                b.pix[at..at + n].fill(px);
                b.mark(row, x, x1);
            }
            return;
        }
        for row in y..y1 {
            // x < x1 <= width and row < y1 <= height, so the span lies inside
            // `pitch * height` bytes; the window refuses it otherwise.
            let off = row * self.pitch + x * 4;
            if n <= 8 {
                for i in 0..n {
                    let _ = self.win.try_write::<u32>(off + i * 4, px);
                }
            } else {
                // `rep stosd` (`MmioWindow::fill32`): the string instruction's
                // start-up costs more than a few stores, but on a write-combining
                // mapping it is what makes a long span leave as full-line bursts.
                let _ = self.win.fill32(off, n, px);
            }
        }
    }

    fn put(&mut self, x: usize, y: usize, color: Rgb) {
        if x >= self.width || y >= self.height || FB_MUTED.load(core::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let px = self.format.encode(color);
        if let Some(b) = self.back.as_mut() {
            b.pix[y * b.stride + x] = px;
            b.mark(y, x, x + 1);
            return;
        }
        let offset = y * self.pitch + x * self.bytes_per_pixel;

        // `offset` is inside `pitch * height` because x and y were bounds-checked
        // against the dimensions the same tag reported; the window refuses (drops)
        // anything else, or a misaligned wide store. Volatile because this is
        // device memory: the writes must not be elided or reordered away.
        let w = &self.win;
        match self.bytes_per_pixel {
            4 => {
                let _ = w.try_write::<u32>(offset, px);
            }
            2 => {
                let _ = w.try_write::<u16>(offset, px as u16);
            }
            3 => {
                let _ = w.try_write::<u8>(offset, px as u8);
                let _ = w.try_write::<u8>(offset + 1, (px >> 8) as u8);
                let _ = w.try_write::<u8>(offset + 2, (px >> 16) as u8);
            }
            _ => {
                let _ = w.try_write::<u8>(offset, px as u8);
            }
        }
    }
}

impl Framebuffer {
    /// Drain this core's write-combining buffers to the device.
    ///
    /// `sfence` orders and flushes WC stores; ordinary stores and `lock`ed
    /// instructions do not reliably do it. Cheap when nothing is pending.
    ///
    /// With a back buffer this is the **present**: every row's changed span is
    /// copied to the scanout (one `rep movsd` each) and marked clean, then the WC
    /// buffers are drained. A flush with nothing changed is a scan of the dirty
    /// arrays and the `sfence`.
    fn flush(&mut self) {
        if let Some(b) = self.back.as_mut() {
            for y in 0..self.height {
                let (lo, hi) = (b.dlo[y] as usize, b.dhi[y] as usize);
                if lo >= hi {
                    continue;
                }
                let at = y * b.stride;
                // In range by construction: marked spans lie inside the row.
                let _ = self.win.copy32_from(y * self.pitch + lo * 4, &b.pix[at + lo..at + hi]);
                b.dlo[y] = u32::MAX;
                b.dhi[y] = 0;
            }
        }
        // `sfence` on this target.
        akuma_cpu::barrier::dsb_ishst();
    }
}

// ---------------------------------------------------------------------------
// Write-combining the framebuffer
// ---------------------------------------------------------------------------

/// The PAT entry we repurpose: index 4, selected by `PAT=1, PCD=0, PWT=0`. Firmware
/// resets it to WB, and **nothing in this kernel sets the PAT bit in any entry**, so
/// no existing mapping changes type. (Entries 0-3 -- the ones PWT/PCD pick -- are not
/// touched at all.) The write-combining encoding itself (`0x01`) is spelt once, in
/// `akuma_cpu::msr::set_pat_entry_wc`.
const PAT_INDEX: u32 = 4;
/// Bit 12 of a 2 MiB PDE / bit 7 of a 4 KiB PTE: the PAT selector bit.
const PDE_PAT: u64 = 1 << 12;
const PTE_PAT: u64 = 1 << 7;
const PDE_LARGE_PRESENT: u64 = 0x81;
const TWO_MIB: u64 = 1 << 21;

/// Spare page tables, for splitting a 2 MiB PDE that the framebuffer only partly
/// covers: a framebuffer has at most two partly-covered 2 MiB ends. Written only
/// by `map_wc`, once, on the boot CPU before any other core is running;
/// afterwards the CPU's page walker is the only reader.
static SPLIT_PT: [Table; 2] = [const { Table::zeroed() }; 2];

use akuma_boot_amd64::paging::boot_pd;

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
    // One PAT byte (ours) to a valid encoding (WC), the other seven as
    // firmware left them; no live mapping selects our index, which is the
    // helper's stated obligation.
    akuma_cpu::msr::set_pat_entry_wc(PAT_INDEX);
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
    let (first, last) = ((phys / TWO_MIB) as usize, ((end - 1) / TWO_MIB) as usize);
    let splits = [first, last].iter().filter(|&&i| {
        let base = i as u64 * TWO_MIB;
        !(phys <= base && end >= base + TWO_MIB)
    }).count();
    let splits = if first == last { splits.min(1) } else { splits };
    // Indices are below 512 * PHYSMAP_PDS (end <= MAPPED_LIMIT, which is
    // PHYSMAP_LIMIT); `boot_pd` asserts it.
    let all_large = (first..=last).all(|i| {
        let (pd, idx) = boot_pd(i);
        pd.get(idx) & PDE_LARGE_PRESENT == PDE_LARGE_PRESENT
    });
    if !all_large || splits > SPLIT_PT.len() {
        return false;
    }

    pat_set_wc_this_cpu();

    let mut next_pt = 0;
    for i in first..=last {
        let base = i as u64 * TWO_MIB;
        let (pd, idx) = boot_pd(i);
        let old = pd.get(idx);
        if phys <= base && end >= base + TWO_MIB {
            // Setting the PAT selector on a live large PDE re-types its 2 MiB
            // (entry 4 = WC), nothing else.
            pd.set(idx, old | PDE_PAT);
        } else {
            let pt = &SPLIT_PT[next_pt];
            next_pt += 1;
            let flags = old & 0xFFF & !0x80; // keep P/RW; drop PS
            for j in 0..512u64 {
                let pa = base + j * 4096;
                let in_range = pa + 4096 > phys && pa < end;
                pt.set(j as usize, pa | flags | if in_range { PTE_PAT } else { 0 });
            }
            let pt_phys = pt.addr() as u64 - akuma_boot_amd64::phys::KERNEL_VMA;
            // The PT is complete before the PDE points at it (x86 stores retire in
            // order; the compiler fence stops reordering the relaxed stores).
            core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
            // Replaces a live large PDE by a table mapping the same 2 MiB
            // identically except for the PAT bit on the framebuffer's pages.
            pd.set(idx, pt_phys | flags);
        }
        // Invalidates the (large-page) translation of this window on this core;
        // other cores are not running yet (contract above).
        akuma_cpu::tlb::invlpg((PHYSMAP_BASE + base) as usize);
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

/// Apply the power daemon's dim factor to the console and repaint the glass
/// from the grid — the repaint is what makes already-printed text follow.
///
/// A no-op with no console (headless, the PVH rigs): the factor is recorded by
/// the caller (`power::set_dim_pct`) either way, so `/proc/power`'s `dim=`
/// stays honest on a machine with nothing to dim. Blocking `lock`, like
/// [`fb_unmute_and_repaint`] — a daemon's once-per-step call, never an
/// interrupt path.
pub fn fb_set_dim(pct: u8) {
    let mut g = CONSOLE.lock();
    if let Some(c) = g.as_mut() {
        c.0.set_dim_pct(pct);
        c.0.repaint();
        c.0.surface_mut().flush();
    }
}

/// The crash path's unmute: no lock, no repaint — the fatal dump that follows
/// draws over whatever the program left, which is the point.
pub fn fb_force_unmute() {
    CRASHED.store(true, core::sync::atomic::Ordering::Release);
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

/// The console over the framebuffer. A newtype so the `Spinlock<Option<..>>`
/// above names one thing; the window inside is plain data (an address and a
/// length), so it is `Send` without a promise.
struct FbConsole(Console<Framebuffer>);

/// A frame is open: the last byte seen left `CSI ? 2026` set. [`mirror_byte`] only.
static SYNC_ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// When the open frame began (`net::uptime_us`), so a program that never sends the
/// closing `l` cannot freeze the screen: [`cursor_idle`] presents after
/// [`SYNC_TIMEOUT_US`] whatever the mode says.
static SYNC_SINCE_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
const SYNC_TIMEOUT_US: u64 = 150_000;
/// The crash path ran: present after every byte.
static CRASHED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Mirror every console draw in RAM and present it in one pass per flush (see
/// [`Back`]). Call once the heap exists; a no-op without a console, and a failed
/// allocation leaves the direct-drawing console exactly as it was. Returns whether
/// the back buffer is on, for the `[fb]` line.
///
/// **The allocation happens without the console lock.** The copy is `pitch * height`
/// bytes — 34 MB at 3840x2160 — and `akuma-alloc`'s heap monitor prints when usage
/// crosses a threshold. That print reaches [`mirror_byte`], which takes this same
/// lock, so allocating under it deadlocked the core on a 4K panel: the boot froze
/// after the `pmm:` line (`docs/archive/AMD64_FB_BACK_BUFFER_DEADLOCK.md`).
pub fn fb_enable_double_buffer() -> bool {
    let shape = {
        let mut g = CONSOLE.lock();
        let Some(c) = g.as_mut() else { return false };
        let fb = c.0.surface_mut();
        if fb.back.is_some() {
            return true;
        }
        fb.back_shape()
    };
    let Some((stride, height)) = shape else { return false };
    let Some(back) = Back::new(stride, height) else { return false };
    let mut g = CONSOLE.lock();
    let Some(c) = g.as_mut() else { return false };
    let fb = c.0.surface_mut();
    // Only the boot core calls this, once; a copy installed meanwhile would be
    // dropped here, and freeing does not print.
    if fb.back.is_none() {
        fb.back = Some(back);
    }
    // What is on the glass is not in the copy: redraw it all from the grid.
    c.0.repaint();
    c.0.surface_mut().flush();
    true
}

/// Write one byte to the framebuffer console, if it exists; `false` if it does not.
///
/// Called from `serial::putb`, which is what makes this the console for
/// **everything** -- kernel diagnostics, the self-test harness, and the output
/// of user programs -- without any of them knowing there is a screen.
pub fn mirror_byte(byte: u8) -> bool {
    if let Some(c) = CONSOLE.lock().as_mut() {
        let t0 = polltime::tsc();
        c.0.write_byte(byte);
        // `CSI ? 2026 h .. l`: the program is mid-frame. Draws still land (in the
        // back buffer), but nothing is presented until the frame ends — an
        // atomic repaint instead of a visible erase-and-rewrite.
        let syncing = c.0.sync_update();
        let was_syncing = SYNC_ACTIVE.swap(syncing, core::sync::atomic::Ordering::Relaxed);
        if syncing && !was_syncing {
            SYNC_SINCE_US.store(net::uptime_us(), core::sync::atomic::Ordering::Relaxed);
        }
        if was_syncing && !syncing {
            c.0.surface_mut().flush();
        }
        // Per line, not per byte: a drain waits for the device. A trailing partial
        // line is flushed by `cursor_idle` when output goes quiet. After a crash
        // (`fb_force_unmute`) every byte is presented: the dump may be the last
        // thing this machine ever draws, and nothing will call `cursor_idle`.
        if !syncing && (byte == b'\n' || CRASHED.load(core::sync::atomic::Ordering::Relaxed)) {
            c.0.surface_mut().flush();
        }
        BURST_CYCLES.fetch_add(polltime::tsc().wrapping_sub(t0), core::sync::atomic::Ordering::Relaxed);
        BURST_BYTES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        true
    } else {
        false
    }
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
    let geometry = {
        let mut g = CONSOLE.try_lock()?;
        let c = &mut g.as_mut()?.0;
        // Mid-frame (`CSI ? 2026 h`): a quiet lap between two writes of one frame is
        // not the end of it. Wait for the program's `l`, up to the timeout.
        if c.sync_update()
            && net::uptime_us().saturating_sub(SYNC_SINCE_US.load(core::sync::atomic::Ordering::Relaxed))
                < SYNC_TIMEOUT_US
        {
            return None;
        }
        c.show_cursor();
        c.surface_mut().flush();
        c.take_geometry()
    };
    report_burst();
    let (rows, cols) = geometry?;
    Some((u16::try_from(rows).ok()?, u16::try_from(cols).ok()?))
}

/// TSC cycles and bytes the framebuffer console has spent on output since the
/// last quiet moment: what a TUI's frames cost the screen (`kami tui` scrolling
/// felt slow with the program itself taking 3 ms a frame).
static BURST_CYCLES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static BURST_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// A burst of at least this many bytes is worth a `dmesg` line.
const BURST_REPORT_MIN: u64 = 1024;

/// Output has gone quiet: say what the burst cost, to `dmesg` only. Called from
/// [`cursor_idle`] with no lock held; the line itself is printed through the
/// serial path, which the console shell keeps off the screen (`set_fb_quiet`).
fn report_burst() {
    use core::sync::atomic::Ordering::Relaxed;
    let bytes = BURST_BYTES.swap(0, Relaxed);
    let cycles = BURST_CYCLES.swap(0, Relaxed);
    if bytes < BURST_REPORT_MIN {
        return;
    }
    let khz = (polltime::tsc_hz() / 1000).max(1);
    serial::puts("[fbcon] burst: ");
    serial::put_dec(bytes);
    serial::puts(" bytes, ");
    serial::put_dec(cycles / khz);
    serial::puts(" ms drawing\n");
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

/// What the program on the console asked of the pointer, and the grid to place
/// it on.
///
/// `(tracking, sgr, [origin_x, origin_y, cell_w, cell_h, cols, rows])`; `None` with no framebuffer console or when the console is busy this lap
/// (`try_lock`, like [`fb_take_reply`]).
pub fn fb_mouse() -> Option<(u16, bool, [u32; 6])> {
    let g = CONSOLE.try_lock()?;
    let c = &g.as_ref()?.0;
    let (tracking, sgr) = c.mouse_mode();
    let (ox, oy, cw, ch, cols, rows) = c.text_grid();
    let u = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
    Some((tracking, sgr, [u(ox), u(oy), u(cw), u(ch), u(cols), u(rows)]))
}

/// Mark the cell under the pointer (0-based row, col), or stop marking it. Drawn
/// on the next idle lap; see `fbcon::Console::set_pointer`.
pub fn fb_set_pointer(cell: Option<(usize, usize)>) {
    if let Some(mut g) = CONSOLE.try_lock()
        && let Some(c) = g.as_mut()
    {
        c.0.set_pointer(cell);
    }
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
    let hz = lapic::tsc_hz();
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


/// The firmware framebuffer and the console on it, or why there is none.
///
/// **Not having one is not fatal.** Every failure here used to `halt` after an
/// [`ega_text`] line, which on a UEFI machine (no VGA text memory) is a black
/// screen with nothing on it — exactly what ryzen's first boot showed
/// (2026-10-06, framebuffer above the old 4 GiB limit). A kernel that keeps
/// going headless still reaches `init`, `dmesg` and whatever log sink or
/// network the machine has, and the reason lands in `dmesg`. Every `CONSOLE`
/// user already treats "no console" as a no-op.
pub fn init_console(info: &BootInfo<'_>) -> Result<ConsoleUp, &'static str> {
    let Some(fb) = info.framebuffer() else {
        boot::ega_text(1, "no framebuffer tag: booting headless");
        return Err("GRUB provided no framebuffer tag");
    };
    if !matches!(fb.kind, FramebufferKind::Rgb) {
        boot::ega_text(1, "framebuffer is not direct-colour: booting headless");
        return Err("framebuffer is not direct-colour (EGA text mode?)");
    }
    let Some(mut surface) = Framebuffer::new(&fb) else {
        return Err("framebuffer lies above the boot physmap");
    };
    // The same framebuffer, offered to userspace as `/dev/fb0` (`fbdev`).
    fbdev::register(akuma_fbdev::Geometry {
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
    // console owns its whole character grid (98 KiB: code point, colours and
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
        let t0 = akuma_cpu::tsc::rdtsc();
        con.clear();
        con.surface_mut().flush(); // so the time includes draining the WC buffers
        let t1 = akuma_cpu::tsc::rdtsc();
        CLEAR_CYCLES.store(t1.wrapping_sub(t0), core::sync::atomic::Ordering::Relaxed);
        // Which font and grid the console actually chose. `Console::choose_font`
        // takes that decision from the framebuffer size at runtime — IBM Plex Mono
        // whenever it reaches 80x24, Spleen when it cannot — so on a machine whose
        // only output IS this console, "what am I looking at" was a question only
        // answerable by re-deriving the arithmetic from the mode GRUB happened to
        // pick. Now the console says so itself, in itself.
        (con.font().name(), con.font().width(), con.font().height(), con.cols(), con.rows(), con.scale())
    };
    boot::ega_text(2, "console up");
    Ok(ConsoleUp { fb, font, fw, fh, fcols, frows, fscale })
}

/// The colour band the multiboot2 path's `cycle_forever` paints after `init` is done: step
/// `i` of the palette, along the bottom of the screen.
///
/// The lock is taken and released around each fill rather than held
/// across the wait: anything else printing would block for the whole
/// cycle, and on this machine printing is the only way to be heard.
pub fn band(i: usize) {
    let palette = [
        Rgb::new(0xE0, 0x50, 0x50),
        Rgb::new(0xE0, 0xC0, 0x40),
        Rgb::new(0x50, 0xD0, 0x60),
        Rgb::new(0x60, 0xA0, 0xE0),
        Rgb::new(0xC0, 0x60, 0xE0),
    ];
    if let Some(c) = CONSOLE.lock().as_mut() {
        let s = c.0.surface_mut();
        let (w, h) = (s.width(), s.height());
        let (mx, my) = (w / 24, h / 24);
        let band_h = (h / 14).max(8);
        let band_y = h.saturating_sub(my + band_h);
        s.fill(mx, band_y, w.saturating_sub(mx * 2), band_h, palette[i % palette.len()]);
        s.flush();
    }
}

/// The splash's colour for art character `ch` at `(x, y)`, `t_ms` into the boot, as
/// `(r, g, b)`: what `banner::print_visible` paints the mark with.
pub fn art_color(ch: u8, x: usize, y: usize, t_ms: u64) -> (u8, u8, u8) {
    let c = akuma_fbcon::splash::art_color(ch, x, y, t_ms);
    (c.r, c.g, c.b)
}

/// This driver, as the machine half's display class.
pub static DISPLAY: akuma_kernel_amd64::drivers::Display = akuma_kernel_amd64::drivers::Display {
    init: init_console,
    enable_double_buffer: fb_enable_double_buffer,
    mirror_byte,
    mute: fb_mute,
    unmute_and_repaint: fb_unmute_and_repaint,
    set_dim: fb_set_dim,
    force_unmute: fb_force_unmute,
    cursor_idle,
    grid: fb_grid,
    apply_config: fb_apply_config,
    splash_frame: fb_splash_frame,
    splash_end: fb_splash_end,
    take_reply: fb_take_reply,
    mouse: fb_mouse,
    set_pointer: fb_set_pointer,
    set_view: fb_set_view,
    summary: fb_summary,
    pat_init_ap,
    band,
    art_color,
};
