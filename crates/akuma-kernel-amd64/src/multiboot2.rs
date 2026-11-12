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
//!
//! # The console is a driver
//!
//! Paths 2 and 3 are the framebuffer console, `akuma-drivers-amd64`'s `fbcon`
//! feature, reached through the display class ([`crate::drivers::Display`]). A
//! build without it (the `headless` image) keeps path 1 and boots headless as on a
//! machine with no framebuffer tag: the banner says `fb: none, headless (..)`, the
//! console is the serial port, and the `fbtrace` stand-in below gets what the
//! screen would have.

use akuma_primitives::mmio::MmioWindow;
use akuma_multiboot2::BootInfo;
use akuma_ryzen_amd64::{MAX_REGIONS, MachineDescription, MemRegion};

use crate::serial;

/// Where the boot page tables mirror all of physical memory.
const PHYSMAP_BASE: u64 = 0xFFFF_8000_0000_0000;

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
pub fn ega_text(row: usize, s: &str) {
    // SAFETY: the identity/physmap window covers the first megabyte, so this
    // range (80x25 cells of two bytes) is mapped. Writing it is either visible
    // text or a store to unused low RAM; neither can fault, and nothing else
    // claims that range this early in boot.
    let win = unsafe { MmioWindow::new((PHYSMAP_BASE + EGA_TEXT_BASE) as usize, 80 * 25 * 2) };
    for (i, b) in s.bytes().take(80).enumerate() {
        let off = (row * 80 + i) * 2;
        // A row past the screen is dropped, not wrapped into whatever follows.
        let _ = win.try_write::<u8>(off, b);
        let _ = win.try_write::<u8>(off + 1, EGA_ATTR);
    }
}

/// The firmware framebuffer of **this** boot: physical address and length, recorded by
/// [`kmain_mb2`] from the information block GRUB handed over, before anything else could
/// run. `0` length = none (no tag, not direct-colour, or above the physmap).
static BOOT_FB_ADDR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static BOOT_FB_LEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Set once [`framebuffer_window`] has handed the window out.
static BOOT_FB_TAKEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Record the framebuffer of the boot that actually happened. Private on purpose: a
/// `BootInfo` can be built from any bytes, so only the one GRUB passed may decide what
/// [`framebuffer_window`] maps.
fn record_boot_framebuffer(info: &BootInfo<'_>) {
    use core::sync::atomic::Ordering;
    let Some(fb) = info.framebuffer() else { return };
    if !matches!(fb.kind, akuma_multiboot2::FramebufferKind::Rgb) {
        return;
    }
    let len = fb.size_bytes();
    // A framebuffer above what boot.s maps cannot be written; refuse it rather than fault.
    if len == 0 || fb.addr.checked_add(len).is_none_or(|end| end > crate::phys::PHYSMAP_LIMIT) {
        return;
    }
    BOOT_FB_ADDR.store(fb.addr, Ordering::Relaxed);
    BOOT_FB_LEN.store(len, Ordering::Release);
}

/// The firmware framebuffer as an MMIO window, **once** per boot; `None` without one, or
/// on a second call.
///
/// The display driver's only way to the framebuffer. It cannot name an address: the
/// window is the one [`record_boot_framebuffer`] took from this boot's information
/// block, so safe code cannot point it at RAM.
#[must_use]
pub fn framebuffer_window() -> Option<MmioWindow> {
    use core::sync::atomic::Ordering;
    let len = BOOT_FB_LEN.load(Ordering::Acquire);
    if len == 0 || BOOT_FB_TAKEN.swap(true, Ordering::AcqRel) {
        return None;
    }
    let addr = BOOT_FB_ADDR.load(Ordering::Relaxed);
    // SAFETY: the firmware's framebuffer, as GRUB reported it for this boot, checked by
    // `record_boot_framebuffer` to lie entirely below `PHYSMAP_LIMIT`, which boot.s maps
    // through the physmap. It is device memory no allocator owns, never freed or moved,
    // and handed out once (`BOOT_FB_TAKEN`), so this is the only window over it.
    Some(unsafe { MmioWindow::new((PHYSMAP_BASE + addr) as usize, len as usize) })
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

/// A byte the framebuffer console did not draw (there is none, or no console driver in
/// this build): to the `fbtrace` stand-in if it is on. See [`FB_TRACE`].
pub fn fb_trace_byte(byte: u8) {
    if FB_TRACE.load(core::sync::atomic::Ordering::Relaxed) {
        // QEMU's `isa-debugcon`, and nothing on real PC hardware; the flag that
        // gets us here is a development one.
        akuma_port::DEBUGCON.write_byte(byte);
    }
}

/// The GRUB/multiboot2 boot, from long mode on; the bin's `kmain_mb2` symbol calls it.
///
/// Deliberately parallel to [`crate::kmain`], and in the same order, because
/// that order is load-bearing and documented there. What differs is only where
/// the facts come from: a multiboot2 information block instead of a PVH handoff
/// block, an ext2 image the loader left in RAM instead of a virtio disk, and a
/// framebuffer instead of a serial port.
// The information block's size field is a `u32` at an 8-byte-aligned address
// (the multiboot2 ABI); the byte pointer is how the physmap hands it over.
#[allow(clippy::cast_ptr_alignment)]
pub fn kmain_mb2(info: akuma_primitives::addr::PhysAddr, before_main: fn()) -> ! {
    // The bin's hook (`akuma_kernel::entry!`'s argument): it registers the boot suite, which
    // lives in `akuma-kernel-tests-amd64` because this crate cannot depend on it. It only
    // stores function pointers, so it runs first, before anything it could depend on.
    before_main();
    let info_phys = info.get();
    ega_text(0, "Akuma/amd64: multiboot2 entry reached");
    // Probe for a UART and configure it if one answers. This path never called
    // `serial::init` — the reference machine has no serial port — and got away
    // with it because an absent port ignores writes. Since the probe gates every
    // port access (see `serial::PRESENT`), skipping it would mean no serial
    // output at all on a machine that does have one.
    serial::init();
    // The shared-crate print hook, for the same reason `kmain` sets it before
    // its memory bring-up: `mem::init` reports through `safe_print!`, and on
    // this path the sink it wants is serial's framebuffer mirror.
    akuma_primitives::console::set_print_hook(serial::puts);

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
    record_boot_framebuffer(&info);
    crate::boot::keep_cmdline(info.cmdline());
    // `nofb`: do not touch the framebuffer at all — for a machine run as a
    // headless test target (ryzen's reboot loop, `overlays/ryzen/`).
    let console = if info.cmdline().split_ascii_whitespace().any(|t| t == "nofb") {
        Err("`nofb` on the command line")
    } else {
        crate::drivers::display_init(&info)
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
    serial::puts(if crate::drivers::keyboard_init() { "i8042 present" } else { "no i8042" });
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

    // `smp=N` and the idle wake budget, as on the PVH path.
    crate::boot::apply_cmdline_policies(info.cmdline());

    // The machine, as multiboot2 describes it.
    let machine = machine_from(&info);
    serial::puts("  ram:  ");
    serial::put_dec(machine.usable_ram() / (1024 * 1024));
    serial::puts(" MiB usable across ");
    serial::put_dec(machine.regions().len() as u64);
    serial::puts(" regions\n");
    let facts = machine.facts(akuma_machine_facts::Provenance::Multiboot2);
    crate::machine::report_facts(&facts);

    // PCI enumeration. This is the whole reason it matters on this entry: a
    // VMM announces its devices, real firmware announces nothing. The xHCI /
    // EHCI controllers, the Realtek NIC and the AHCI disk are all found here.
    crate::pci::scan();
    crate::pci::report();

    // Immediately: stop any xHCI controller a PREVIOUS boot left running. This
    // is before the memory map is trusted and before anything decides whether
    // to drive USB, because what it defends against is already in flight —
    // see `pci::quiesce_xhci_controllers`.
    crate::pci::quiesce_xhci_controllers();

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
    if !crate::mem::init_reserving(&facts, &[reserved, info_span]) {
        serial::puts("\nAkuma/amd64 - memory bring-up FAILED\n");
        crate::halt();
    }

    // Double-buffer the console now that there is a heap (`FBCON_DOUBLE_BUFFER.md`).
    // The result lands in `dmesg`; a failure keeps direct drawing.
    serial::puts(if crate::drivers::fb_enable_double_buffer() { "[fb] double buffer on\n" } else { "[fb] double buffer off\n" });

    // The console hook and the `akuma-exec` runtime, exactly as `kmain` does —
    // one call, because "exactly as `kmain` does" was two lines on that path and
    // one on this one, and the missing one only ever showed up on the metal.
    // See `boot::install_shared_sinks`.
    crate::boot::install_shared_sinks();

    // Intel HDA (runbook AKUMA_AMD64_HDA_AUDIO_BRIEF.md). The PVH path gates this on
    // the `pci` cmdline flag (see `kmain`); a firmware boot always has PCI —
    // the scan above already found it — so it runs unconditionally here. It
    // sits after `mem::init_reserving` because `map_bar` allocates frames, and
    // it is best-effort like every other device bring-up on this path: a miss
    // prints one line and boots on. `hdatest` plays a one-second tone.
    crate::drivers::sound_init(info.cmdline().split_ascii_whitespace().any(|t| t == "hdatest"));

    // The SVM hypervisor seed (AKUMA_KVM.md): one `[KVM]` probe line per
    // boot; `kvmtest` on the command line runs the trivial guest. After
    // `mem::init_reserving` (it allocates frames) and on the BSP only —
    // EFER.SVME and VM_HSAVE_PA are per-core, rung 2 enables the APs.
    crate::drivers::hypervisor_init(info.cmdline());

    // `/proc/power`. After `mem::init_reserving` for the same reason as `hda`:
    // mapping the EC window allocates page-table frames.
    crate::power::init(&machine, info.cmdline());
    // The touchpad: only decide here (a table scan); bring-up waits for `boot_to_init`.
    crate::drivers::pointer_discover(&machine, info.cmdline());

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
            // SAFETY: the module's pages were reserved from the PMM above, so no
            // allocator hands them out, and this closure is the only thing that
            // names them; `claim` checks they lie inside the physmap.
            .and_then(|m| unsafe { akuma_phys::RamSpan::claim(akuma_phys::PhysAddr::new(u64::from(m.start)), m.len()) })
            .map(crate::ramdisk::RamDisk::new)
            .is_some_and(|rd| {
                crate::fs::mount_root_on(crate::fs::RootDevice::Ram(rd), "module")
            })
    };
    // The hardware watchdog (`wdt`), before anything below can hang: the root
    // mount drives the NVMe or USB controller. Needs the page tables and the
    // PCI scan, both done above; petting starts with the timer tick.
    crate::watchdog::init(info.cmdline());
    // `/dev/wifi0` and its backend (`wifisim` today; the radio later).
    crate::drivers::wireless_init(info.cmdline());

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

    // `rtw89` (ryzen): the wifi card up to running firmware, then shut down
    // again — wifi stage W1; `rtw89wifi` keeps it up for the station
    // (W3/W4). After the root mount: the firmware is a file.
    if have_fs {
        crate::drivers::wireless_card_init(info.cmdline());
    }

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
        let cmdline = info.cmdline();
        // The KVM rungs that need the root filesystem (`kvmtest kvmlinux`): the
        // Linux guest reads `/boot/vmlinux` off it. Here — after `late_init`'s
        // `lapic::init`, before the secondaries start — and not right after the
        // mount: the run needs the host tick (its wall-clock budget, the
        // scheduler, the hardware watchdog), and before the LAPIC timer there is
        // none, so a guest that spins without exiting never gives the host a
        // chance to stop it (found rehearsing under QEMU/KVM, 2026-10-22). Same
        // place as the PVH path's call in `lib.rs`; the BSP only, so the
        // thread cannot migrate off the core that enabled SVM.
        if have_fs {
            crate::drivers::hypervisor_late(cmdline);
        }
        crate::boot::bring_up_secondaries(machine.madt.as_ref(), &facts, cmdline, &mb2_keep_out());
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

    // The whole suite, shared with the PVH entry point.
    //
    // This path used to carry its own copy, and the two had drifted: it was
    // **not running** `blk::smoke_test`, `sched::block_smoke_test`, or
    // `usermode::{spawn,console_notify,busybox,execve,fork}_test`. Seven
    // checks — the entire process-lifecycle suite — missing from the one path
    // that runs on real silicon, and invisible because both paths still said
    // `0 failed`. See `boot::self_tests`.
    let verdict = crate::boot::self_tests(
        &crate::boot::SuiteCtx {
            machine: &machine,
            facts: &facts,
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
    // The wifi station, if `rtw89wifi` kept the card: it needs the scheduler.
    crate::drivers::wireless_spawn();
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
        //
        // Not on a wifi link (`rtw89wifi`): there the link exists only once
        // userspace has asked for a join, which is after `init`, so waiting
        // here would spend the whole budget on a link that cannot come up
        // yet. DHCP runs when the join lands, and `clock::sync_tick` (in the
        // netpoll daemon) does SNTP once the interface has an address.
        if crate::drivers::wireless_is_link() {
            serial::puts("  net:  wifi link: DHCP and the wall clock follow the join\n");
        } else {
            crate::boot::dhcp_then_clock();
        }
        // A no-op when `boot::self_tests` already spawned one — `spawn_netpoll`
        // is idempotent since 2026-09-07, and it had to become so: this call
        // used to be unconditional, so the full-suite bare-metal boot ran two
        // netpoll daemons while the PVH path ran one. Still called, because the
        // `skiptests` path shares this function and there is no suite to have
        // spawned it.
        crate::net::spawn_netpoll();
        // A service guest (`kvmserve`) listens on this host's stack: start it now.
        crate::drivers::hypervisor_spawn_service(cmdline);
        if cmdline.split_ascii_whitespace().any(|t| t == "netprobe") {
            crate::net::enable_probe();
        }
    }
    // `fbverbose`, `fbtrace`, `strace*`, `nopidfd`: shared with the PVH path.
    crate::boot::apply_runtime_flags(cmdline);

    // The laptop's I2C-HID touchpad (`touchpad`): found by its DSDT name or
    // `touchpad=`, a no-op elsewhere. After the LAPIC is calibrated because its
    // bring-up delays (60 ms power-on, 100 ms reset) spin on the TSC.
    crate::drivers::pointer_init();

    if run_shell {
        crate::splash::phase(2);
        // `init=`/`initargs=` (`akuma_params::init`, shared with the PVH path and the
        // AArch64 kernel), `/bin/sh` when the line names none: a GRUB boot is a machine
        // someone sits at. `init=/bin/busybox initargs=uname,-a` runs one applet and exits.
        let init = akuma_params::init::Init::from_line_or(
            akuma_params::line(),
            akuma_params::init::Init::new("/bin/sh", ""),
        );
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
        crate::usermode::run_init(init);
    }
}

/// Bring up the xHCI + USB mass-storage stack, sanity-check `/dev/sda`'s MBR,
/// and mount `sda1` as the root filesystem. `false` on any failure — the caller
/// falls back to the RAM image.
pub fn try_usb_root() -> bool {
    if let Err(e) = crate::drivers::usb_init() {
        serial::puts("  fs:   xHCI/USB disk: ");
        serial::puts(e);
        serial::puts("\n");
        return false;
    }
    let mut mbr = [0u8; 512];
    if crate::drivers::usb_read_bytes(0, &mut mbr).is_err() || !crate::fs::mbr_looks_right(&mbr) {
        serial::puts("  fs:   /dev/sda MBR check failed\n");
        return false;
    }
    crate::fs::mount_root_on(
        crate::fs::RootDevice::Usb(crate::fs::UsbDisk::new(crate::fs::SDA1_OFFSET)),
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
    if let Err(e) = crate::drivers::nvme_init().and_then(|()| crate::drivers::nvme_open_partition(part)) {
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
    let mut i = 0usize;
    loop {
        crate::drivers::display_band(i);
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
