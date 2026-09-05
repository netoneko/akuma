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

/// The highest physical address `boot.s` maps.
const MAPPED_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

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

    // `base` is a page-aligned framebuffer address and `offset` a multiple of
    // the pixel size, so the cast never misaligns; clippy cannot see that.
    #[allow(clippy::cast_ptr_alignment)]
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
    }
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

    // Colour before glyphs: the smallest proof that address, pitch and pixel
    // format are all right, with no font and almost no stack involved.
    let (w, h) = (surface.width(), surface.height());
    surface.fill(0, 0, w, h, Rgb::new(0x30, 0x00, 0x50));

    let Some(mut con) = Console::new(surface) else {
        ega_text(2, "FAIL: framebuffer too small for a console");
        crate::halt();
    };
    con.set_bg(Rgb::new(0x08, 0x0C, 0x14));
    con.clear();
    *CONSOLE.lock() = Some(FbConsole(con));
    ega_text(2, "console up");

    // FROM HERE, `serial::puts` REACHES THE SCREEN. `serial::putb` mirrors into
    // the console above, so everything below -- the kernel's own diagnostics,
    // the self-test harness, and anything a user program writes to stdout --
    // appears without knowing a framebuffer exists.
    serial::puts("\nAkuma/amd64 - bare metal, booted by ");
    serial::puts(info.loader_name());
    serial::puts("\n  uart: ");
    serial::puts(if serial::present() { "present" } else { "absent (reads report no data)" });
    // The keyboard. USB on this board, but firmware's legacy emulation presents
    // it on the i8042 ports for as long as no OS claims the USB controllers —
    // which this kernel never does. See `kbd`.
    serial::puts("  kbd: ");
    serial::puts(if crate::kbd::init() { "i8042 present" } else { "no i8042" });
    serial::puts("\n  fb:   ");
    serial::put_dec(u64::from(fb.width));
    serial::puts("x");
    serial::put_dec(u64::from(fb.height));
    serial::puts(" @ ");
    serial::put_dec(u64::from(fb.bpp));
    serial::puts("bpp, pitch ");
    serial::put_dec(u64::from(fb.pitch));
    serial::puts(", at 0x");
    serial::put_hex(fb.addr);
    serial::puts("\n");

    // Descriptor tables, the BSP's per-CPU block, SMAP, then drop the identity
    // map. Same order and the same reasons as `kmain`; see the comments there.
    // `smp::init_bsp` is not optional on this path either: the scheduler and
    // the syscall stubs reach their per-core state through `%gs`, and without
    // the block installed the first `yield_now` reads address 0.
    crate::gdt::init();
    crate::smp::init_bsp();
    crate::idt::init();
    let smap = crate::uaccess::init_smap();
    crate::paging::drop_identity_map();

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

    // The root filesystem is already in memory, and NOTHING IN THE MEMORY MAP
    // SAYS SO. Reserve it before the physical allocator is told anything.
    let module = info.first_module();
    let reserve_to = info.modules_end();
    if let Some(m) = module {
        serial::puts("  mod:  root image at 0x");
        serial::put_hex(u64::from(m.start));
        serial::puts(" + ");
        serial::put_dec(m.len() as u64 / 1024);
        serial::puts(" KiB\n");
    }

    if !crate::mem::init_reserving(&machine, reserve_to) {
        serial::puts("\nAkuma/amd64 - memory bring-up FAILED\n");
        crate::halt();
    }

    // Give the shared crates a console, exactly as `kmain` does.
    akuma_primitives::console::set_print_hook(serial::puts);

    // Mount the image the loader left for us. No storage driver is involved:
    // this is a real ext2 filesystem whose block device happens to be RAM.
    let have_fs = module
        .and_then(|m| crate::ramdisk::RamDisk::new(u64::from(m.start), m.len()))
        .is_some_and(|rd| {
            crate::fs::mount_root_on(crate::fs::RootDevice::Ram(rd), "module")
        });

    // Networking: the Realtek NIC if this box has one, loopback only otherwise.
    // Either way `socket(AF_INET)` works for `busybox ifconfig` and `127.0.0.1`.
    let have_net = crate::net::init_bare_metal();

    let mut t = akuma_selftest::Suite::new("Akuma/amd64 self-test", serial::puts);

    // The same bypass window `kmain` runs its tests in: they drive syscall
    // bodies with kernel-stack buffers where a program would pass user
    // pointers, and `uaccess` refuses kernel addresses by design.
    let user_ptr_bypass = akuma_user_access::BypassValidationGuard::new();

    crate::mem::smoke_test(&mut t);
    crate::paging::smoke_test(&mut t);
    crate::pci::smoke_test(&mut t);
    crate::reboot::smoke_test(&mut t);
    crate::idt::smoke_test(&mut t);
    crate::idt::user_copy_smoke_test(&mut t);
    crate::uaccess::smoke_test(&mut t, smap);

    if t.check("lapic: initialised", crate::lapic::init()) {
        crate::lapic::smoke_test(&mut t);
        crate::lapic::start_timer();
        crate::sched::smoke_test(&mut t);
        crate::lapic::stop_timer();
    }

    // No `blk`: this machine has no virtio transports. `net`/`sock` run on the
    // loopback-only stack — enough to prove `socket(AF_INET)`, `bind`, `listen`.
    crate::fs::smoke_test(&mut t, have_fs);
    crate::fd::smoke_test(&mut t, have_fs);
    crate::sock::smoke_test(&mut t, have_net);
    crate::mm::smoke_test(&mut t);

    // `init_syscall` writes IA32_STAR/LSTAR/SFMASK **and sets `EFER.SCE`**, and
    // without it `sysretq` is an invalid opcode. Leaving it out is how the first
    // bare-metal run of this suite died: `#UD` at `enter_user_mode`, one
    // instruction into the first ring-3 entry, having passed everything else.
    crate::fd::init_console();
    crate::usermode::init_syscall();
    crate::usermode::smoke_test(&mut t);
    crate::usermode::preempt_test(&mut t);

    // The other cores, exactly where `kmain` starts them. What is different on
    // a firmware boot is what might be sitting on the trampoline page: the
    // information block this function is still reading, or the root filesystem
    // GRUB left in RAM. Either one there means single core rather than a copy
    // over it.
    let keep_out = [
        (info_phys, info_phys + bytes.len() as u64),
        info.first_module().map_or((0, 0), |m| (u64::from(m.start), u64::from(m.end))),
    ];
    let expected_aps = machine
        .madt
        .as_ref()
        .map_or(0, |m| m.cpus().len().saturating_sub(1).min(crate::smp::MAX_CPUS - 1));
    let started = if crate::smp::trampoline_page_available(&machine, &keep_out) {
        crate::smp::start_secondaries(machine.madt.as_ref())
    } else {
        serial::puts("  smp:  trampoline page is not free RAM — single core\n");
        0
    };
    crate::smp::smoke_test(&mut t, expected_aps, started);
    crate::usermode::smp_parallel_test(&mut t);

    crate::lapic::start_timer();
    crate::usermode::elf_test(&mut t);
    crate::usermode::fdprobe_test(&mut t);
    drop(user_ptr_bypass);

    let passed = t.report();
    if passed {
        serial::puts("Akuma/amd64 - all self-tests passed\n");
    } else {
        serial::puts("Akuma/amd64 - SELF-TESTS FAILED\n");
    }

    // Drive the stack: `poll()` has to run between socket calls for a
    // connection to complete (loopback or the wire). `ifconfig` needs none of
    // this, but a shell that opens a socket does.
    if have_net {
        crate::lapic::start_timer();
        crate::net::spawn_netpoll();
    }

    // Hand the machine to a shell. After the verdict and only on a passing run,
    // for the reason `kmain` gives: a shell on a kernel whose own tests failed
    // is a way to spend an hour debugging the wrong layer.
    if passed && have_fs {
        let path = init_path(info.cmdline());
        // `initargs=a,b,c`, comma-separated as on the PVH path: `init=/bin/busybox
        // initargs=uname,-a` runs one applet and exits, which on a machine with
        // no input device is the whole shape of "run something and see".
        let args: alloc::vec::Vec<&str> = info
            .cmdline()
            .split_ascii_whitespace()
            .find_map(|t| t.strip_prefix("initargs="))
            .map(|v| v.split(',').filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        serial::puts("\n-- running ");
        serial::puts(path);
        for a in &args {
            serial::puts(" ");
            serial::puts(a);
        }
        serial::puts(" --\n");
        // NOTE: stdout reaches the screen through the mirror, but STDIN IS NOT
        // CONNECTED. This board's keyboard is USB and there is no HID stack, so
        // an interactive shell will print its prompt and then block on a read
        // that nothing can satisfy. That is expected, and it is the next gap.
        crate::usermode::run_init(path, &args);
    }

    cycle_forever()
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

/// Cycle a band of colour along the bottom, for ever.
///
/// This replaces halting, and it is not decoration. A halted kernel and a
/// crashed one look identical -- both leave whatever was last drawn on the
/// screen. A band that keeps changing says the CPU is still executing our code,
/// which is the single fact hardest to establish on a machine with no serial
/// port, no network and no disk output.
fn cycle_forever() -> ! {
    // The BSP is done with kernel code; the secondaries are not (their idle
    // loops keep taking ticks). See `smp::bkl_abandon`.
    crate::smp::bkl_abandon();
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
        }
        i += 1;
        for _ in 0..CYCLE_SPINS {
            core::hint::spin_loop();
        }
    }
}
