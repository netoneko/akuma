//! Taoxichuan (淘熙川) — the minimal composed Akuma amd64 kernel.
//!
//! The composition root of the minimal-self-hosting experiment's metal track
//! (`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`): a kernel built by
//! *pulling crates*, one line per stage, with as few crates as will serve —
//! and where the logic was still stuck in `amd64/src/`, extracting it into a
//! crate first and pulling that (`akuma-boot-amd64`, `akuma-comconsole` are
//! stage 1's extractions).
//!
//! # Stage 1 — boots in KVM
//!
//! The gate: reach init spawn, print the banner on the guest console, zero
//! `[Fault]`/`PANIC`/`[sig!]` lines in the ring. **Userspace actually running
//! is stage 2** — this kernel stops at the spawn point, having proved
//! everything the spawn stands on: the boot, the console, the machine's own
//! description, heap + PMM, virtio-blk, and an ext2 root it can read init's
//! bytes off.
//!
//! # What is deliberately NOT here (pinned exclusions, load-bearing)
//!
//! No pty layer, no users/multi-user machinery, no herd — init is
//! kernel-spawned (the extreme-size pattern), single vCPU, no PCI walk
//! (Firecracker-style virtio-mmio only), and no scheduler: nothing yields,
//! because at this stage there is nothing to yield to. The BKL is not even
//! compiled in — every crate here is built without `smp-shared`, where the
//! lock's entry points are no-ops, which is *correct* for one core rather than
//! a compromise.
//!
//! # The boot order, and why
//!
//! Mirrors `akuma-amd64`'s `boot::early_init` order, which is load-bearing and
//! paid for: console → GDT (before the IDT loads; the CPU reads the GDT on
//! every exception delivery) → IDT (so everything after can *report* a fault
//! instead of triple-faulting into guest silence) → `CR0.WP` → machine
//! description → heap + PMM → virtio-blk → ext2 → banner → init spawn.
//!
//! The identity map is NOT dropped, deliberately: that retirement exists to
//! hand the lower half to userspace, and stage 1 has none. Keeping it also
//! keeps `boot.s`'s low GDT reachable, so fault delivery has one fewer way to
//! be wrong. Stage 2 drops it when the lower half starts earning its keep.

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![feature(abi_x86_interrupt)]
// Stage 1 registers every trap kind it does not specifically expect; until
// stage 2 gives some of them real jobs, that is the whole surface.
#![allow(function_casts_as_integer)]

extern crate alloc;

use akuma_boot_amd64::{mem, paging, phys};
use akuma_comconsole as console;
use akuma_idt_amd64 as idt_lib;
use akuma_trap_amd64 as trap;
use akuma_trap_amd64::{InterruptStackFrame, Kind, Trap};
use akuma_vfs::Filesystem;

/// The heap. `#[global_allocator]` is a binary-level declaration; the crate
/// exports the implementation, this kernel installs it.
#[global_allocator]
static ALLOCATOR: akuma_alloc::KernelAllocator = akuma_alloc::KernelAllocator;

// ---------------------------------------------------------------------------
// The machine, as `akuma-ryzen-amd64` reads it: the physmap as a `PhysMem`.
// ---------------------------------------------------------------------------

/// Physical memory as this kernel sees it: the physmap, and nothing outside it
/// (the same bound and the same reasons as `akuma-amd64`'s `machine::Physmap`
/// — every address here comes from the VMM, and a read past the physmap is a
/// page fault during early boot, i.e. a guest that vanishes with no output).
struct Physmap;

impl akuma_ryzen_amd64::PhysMem for Physmap {
    fn read(&self, pa: u64, buf: &mut [u8]) -> bool {
        let Some(end) = pa.checked_add(buf.len() as u64) else {
            return false;
        };
        if end > phys::PHYSMAP_LIMIT {
            return false;
        }
        for (i, slot) in buf.iter_mut().enumerate() {
            // SAFETY: the whole range was just proved to be inside the physmap,
            // which `boot.s` maps before `kmain` runs.
            *slot = unsafe { phys::phys_ptr::<u8>(pa + i as u64).read_volatile() };
        }
        true
    }
}

/// The whole boot command line, copied into `buf` (empty when absent).
fn cmdline(start_info_pa: u64, buf: &mut [u8]) -> &str {
    let mut scratch = [0u8; 512];
    let Some(si) = akuma_ryzen_amd64::StartInfo::parse(&Physmap, start_info_pa) else {
        return "";
    };
    let Some(line) = si.cmdline(&Physmap, &mut scratch) else {
        return "";
    };
    let n = line.len().min(buf.len());
    buf[..n].copy_from_slice(&line.as_bytes()[..n]);
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

/// The value of one `token=` on the command line, copied into `buf`.
fn cmdline_token<'a>(start_info_pa: u64, prefix: &str, buf: &'a mut [u8]) -> Option<&'a str> {
    let mut scratch = [0u8; 512];
    let si = akuma_ryzen_amd64::StartInfo::parse(&Physmap, start_info_pa)?;
    let line = si.cmdline(&Physmap, &mut scratch)?;
    let value = line.split_ascii_whitespace().find_map(|t| t.strip_prefix(prefix))?;
    let n = value.len().min(buf.len());
    buf[..n].copy_from_slice(&value.as_bytes()[..n]);
    core::str::from_utf8(&buf[..n]).ok()
}

// ---------------------------------------------------------------------------
// GDT/TSS: one core, the finished table shape.
// ---------------------------------------------------------------------------

/// The stack a ring-3 trap would switch to. Nothing runs in ring 3 yet, but
/// the TSS must be loadable for `ltr`, and a valid `rsp0` costs one static.
const TRAP_STACK_SIZE: usize = 16 * 1024;

#[repr(align(16))]
struct TrapStack(#[allow(dead_code)] [u8; TRAP_STACK_SIZE]);

static mut TRAP_STACK: TrapStack = TrapStack([0; TRAP_STACK_SIZE]);static mut TSS: akuma_gdt::Tss = akuma_gdt::Tss::new();
static mut GDT: [u64; akuma_gdt::SLOTS] = [0; akuma_gdt::SLOTS];

/// Build the GDT and TSS, load both. One core; the entries are byte-identical
/// to `boot.s`'s at the same selectors, so `CS`/`SS` stay valid across the
/// `lgdt` and nothing needs a far return.
fn gdt_init() {
    // SAFETY: interrupts are masked from `boot.s` until this kernel chooses
    // otherwise, and nothing else touches these statics (one core). Reached
    // only through raw pointers, never references.
    unsafe {
        let tss = &raw mut TSS;
        (*tss).set_rsp0((&raw mut TRAP_STACK).cast::<u8>().add(TRAP_STACK_SIZE) as u64);
        let gdt = &raw mut GDT;
        *gdt = akuma_gdt::table(tss as u64);
        #[repr(C, packed)]
        struct Gdtr {
            limit: u16,
            base: u64,
        }
        let gdtr = Gdtr { limit: akuma_gdt::GDT_LIMIT, base: gdt as u64 };
        core::arch::asm!(
            "lgdt [{gdtr}]",
            "ltr {tr:x}",
            gdtr = in(reg) &raw const gdtr,
            tr = in(reg) akuma_gdt::TSS_SELECTOR,
            options(readonly, nostack, preserves_flags)
        );
    }
}

// ---------------------------------------------------------------------------
// IDT: every vector a way to *report*, not a way to vanish.
// ---------------------------------------------------------------------------

static IDT: idt_lib::Idt = idt_lib::Idt::new();

/// The one trap handler: name the kind, the rip, the cr2, and stop. A fault in
/// this kernel is a bug in this kernel; there is no userspace to deliver it to
/// and no scheduler to defer it through, so the honest report is the full
/// frame and a halt. The `[Fault]` token is the gate's vocabulary.
fn fault_trap(t: &mut Trap<'_>) {
    let code = t.error_code.unwrap_or(0);
    akuma_primitives::safe_print!(
        192,
        "[Fault] {:?} rip=0x{:016x} rsp=0x{:016x} cr2=0x{:016x} code=0x{:x}\n",
        t.kind,
        t.frame.rip,
        t.frame.rsp,
        akuma_cpu::ctlreg::cr2(),
        code
    );
    halt();
}

/// Plain (callable) reporters: an `extern "x86-interrupt"` fn can only be
/// *entered* by the CPU, so the body lives here and each stub forwards to it —
/// the same split as `akuma-amd64`'s `fatal`.
fn report_unhandled(frame: &InterruptStackFrame) -> ! {
    akuma_primitives::safe_print!(
        160,
        "[Fault] unhandled vector frame rip=0x{:016x} rsp=0x{:016x}\n",
        frame.rip,
        frame.rsp
    );
    halt();
}

fn report_unhandled_code(frame: &InterruptStackFrame, code: u64) -> ! {
    akuma_primitives::safe_print!(
        160,
        "[Fault] unhandled vector frame rip=0x{:016x} rsp=0x{:016x} code=0x{:x}\n",
        frame.rip,
        frame.rsp,
        code
    );
    halt();
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, code: u64) -> ! {
    report_unhandled_code(&frame, code)
}

/// Vectors no `Kind` owns, each with a stub that reports and halts — the same
/// shape as `akuma-amd64`'s `exception_stubs`, reduced to the unhandled
/// reporter.
macro_rules! plain_stubs {
    ($($name:ident = $vector:literal;)*) => {
        $(
            extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
                report_unhandled(&frame);
            }
        )*
    };
}

macro_rules! plain_stubs_code {
    ($($name:ident = $vector:literal;)*) => {
        $(
            extern "x86-interrupt" fn $name(frame: InterruptStackFrame, code: u64) {
                report_unhandled_code(&frame, code);
            }
        )*
    };
}

// No-error-code vectors outside `Kind::ALL`.
plain_stubs! {
    vec_nmi =  2;
    vec_of  =  4;
    vec_br  =  5;
    vec_nm  =  7;
    vec_cso =  9;
    vec_mc  = 18;
    vec_ve  = 20;
    vec_hv  = 28;
}
// Error-code vectors outside `Kind::ALL`.
plain_stubs_code! {
    vec_ts  = 10;
    vec_cp  = 21;
    vec_vc  = 29;
    vec_sx  = 30;
}

/// Fill and load the IDT: a reporter for every vector, the trap crate's stubs
/// for every `Kind` (whose handlers are registered first — `Registered`
/// panics naming itself if a stub fires with none, a boot-order bug rather
/// than a state to degrade through).
fn idt_init() {
    for kind in Kind::ALL {
        if kind.vector().is_some() {
            trap::register(kind, fault_trap);
        }
    }
    IDT.fill(double_fault as usize);
    IDT.set(2, vec_nmi as usize);
    IDT.set(4, vec_of as usize);
    IDT.set(5, vec_br as usize);
    IDT.set(7, vec_nm as usize);
    IDT.set(9, vec_cso as usize);
    IDT.set(10, vec_ts as usize);
    IDT.set(18, vec_mc as usize);
    IDT.set(20, vec_ve as usize);
    IDT.set(21, vec_cp as usize);
    IDT.set(28, vec_hv as usize);
    IDT.set(29, vec_vc as usize);
    IDT.set(30, vec_sx as usize);
    IDT.set(8, double_fault as usize);
    for kind in Kind::ALL {
        let Some(vector) = kind.vector() else { continue };
        let entry = trap::entry(kind);
        if kind.user_callable() {
            IDT.set_user(vector, entry);
        } else {
            IDT.set(vector, entry);
        }
    }
    IDT.load();
}

// ---------------------------------------------------------------------------
// virtio-blk: map the announced transports, hand the geometry to the driver.
// ---------------------------------------------------------------------------

const PAGE_SIZE: u64 = 4096;

/// Map one device's register file into the device window, uncached — the
/// direct-window half of `akuma-amd64`'s `blk.rs`, pulled no further than
/// stage 1 needs (one transport class, no partitions).
fn map_device(base: u64, len: u64) -> bool {
    let first = base & !(PAGE_SIZE - 1);
    let last = (base + len).div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let mut pa = first;
    while pa < last {
        if !paging::map_page(
            (phys::DEVMAP_BASE + pa) as usize,
            pa,
            paging::PteProt::KERNEL_RW,
            paging::MemAttr::Device,
        ) {
            return false;
        }
        pa += PAGE_SIZE;
    }
    true
}

/// Map the machine's virtio transports, set the slot window, bring up the
/// block driver. False when no device was announced (not an error — a
/// diskless boot is a legitimate machine, just not this stage's gate).
fn blk_init(devices: &akuma_ryzen_amd64::MmioDevices) -> bool {
    let devs = devices.as_slice();
    let Some(first) = devs.first() else {
        akuma_primitives::addr::clear_virtio_window();
        return false;
    };
    for d in devs {
        if !map_device(d.base, d.len) {
            akuma_primitives::safe_print!(96, "  [blk] could not map a virtio transport\n");
            return false;
        }
    }
    let Some((base, stride, slots)) = devices.geometry() else {
        return false;
    };
    akuma_primitives::addr::set_virtio_window(
        (phys::DEVMAP_BASE + base) as usize,
        stride as usize,
        slots,
    );
    akuma_primitives::safe_print!(
        128,
        "  blk:  {} virtio slot(s) at pa 0x{:016x} stride 0x{:x} irq {}\n",
        slots,
        base,
        stride,
        first.irq
    );
    akuma_virtio::block::init().is_ok()
}

// ---------------------------------------------------------------------------
// ext2 root on that disk.
// ---------------------------------------------------------------------------

/// The virtio-blk device, as something `akuma-ext2` can read — device 0,
/// `vda`. No `flush`: the cache in front of the media is the *host's*, which a
/// guest cannot reach past (same reasoning as `akuma-amd64`'s `fs::VirtioBlk`).
struct Vda;

impl akuma_ext2::BlockDevice for Vda {
    fn read_bytes(&self, offset: u64, buf: &mut [u8]) -> Result<(), ()> {
        akuma_virtio::block::read_bytes(offset, buf).map_err(|_| ())
    }
    fn write_bytes(&self, offset: u64, data: &[u8]) -> Result<(), ()> {
        akuma_virtio::block::write_bytes(offset, data).map_err(|_| ())
    }
}

/// No wall clock: nothing at this stage stamps an inode (the root is mounted
/// read-only in spirit — no syscall path can write), and `|| 0` is the honest
/// pre-sync answer the full kernel documents, not a guess dressed up as one.
fn no_clock() -> u64 {
    0
}

// ---------------------------------------------------------------------------
// The banner — printed at the init-spawn point, where the full kernel prints
// its own: the last thing on the console before the machine would hand over.
// ---------------------------------------------------------------------------

/// ASCII only, on purpose: the guest console's host-side relay sanitises
/// non-ASCII bytes to `?`, and the gate's evidence is this log.
const ART: &str = concat!(
    "  +------------------------------------------------------+
",
    "  |   Taoxichuan  --  tao xi chuan, the kiln-fire market  |
",
    "  |   A kernel composed from Akuma's crates, one stage   |
",
    "  |   at a time, fired in Akuma's own kiln: booted under |
",
    "  |   Akuma's hypervisor, on Akuma's metal.              |
",
    "  +------------------------------------------------------+
",
);

fn banner() {
    console::puts("\n");
    console::puts(ART);
    console::puts("\n  Taoxichuan ");
    console::puts(env!("CARGO_PKG_VERSION"));
    console::puts(" (Akuma/amd64 crates) x86_64 -- stage 1\n\n");
}

// ---------------------------------------------------------------------------
// Entry.
// ---------------------------------------------------------------------------

/// Long-mode entry, called from `boot.s` (in `akuma-boot-amd64`) with the
/// `hvm_start_info` pointer in `%rdi`.
#[unsafe(no_mangle)]
pub extern "C" fn kmain(hvm_start_info: u64) -> ! {
    // The console first: everything after this reports through it.
    console::init();
    // And the shared crates' sink, before the first one of them can print:
    // the boot kit's memory bring-up reports through `safe_print!`.
    akuma_primitives::console::set_print_hook(console::puts);

    console::puts("\nTaoxichuan/amd64 -- long mode reached\n");
    console::puts("  uart: ");
    console::puts(if console::present() { "present\n" } else { "absent (reads report no data)\n" });
    console::puts("  hvm_start_info @ 0x");
    console::put_hex(hvm_start_info);
    console::puts("\n");

    // GDT, then IDT, then WP — the order of `boot::early_init`, minus the
    // steps this kernel has no caller for (per-CPU block, scheduler).
    gdt_init();
    idt_init();
    // `CR0.WP`: a kernel *write* to a read-only page honours the page tables.
    // Without it, `.rodata` protection is a fiction and copy-on-write later
    // becomes impossible to reason about. The only CR0 bit this kernel sets.
    // SAFETY: setting a single documented control-register bit; interrupts
    // are masked, one core, no thread of execution to race.
    unsafe {
        core::arch::asm!(
            "mov {tmp:r}, cr0",
            "or {tmp:r}, {wp:r}",
            "mov cr0, {tmp:r}",
            tmp = out(reg) _,
            wp = in(reg) 1u64 << 16,
            options(nostack, preserves_flags)
        );
    }

    // The machine's own description of itself.
    // The command line is parsed per-token below (init=, initargs=) straight
    // off the start info; a full copy exists only if a stage needs to grep it.
    let mut cmdline_buf = [0u8; 512];
    let _cmdline = cmdline(hvm_start_info, &mut cmdline_buf);
    let mut describe_buf = [0u8; 512];
    let Ok(machine) = akuma_ryzen_amd64::describe(&Physmap, hvm_start_info, &mut describe_buf) else {
        console::puts("  [FATAL] no hvm_start_info magic at the PVH handoff address\n");
        halt();
    };
    akuma_primitives::safe_print!(
        160,
        "  machine: {} MiB RAM, {} memmap entries, {} virtio-mmio slot(s)\n",
        machine.usable_ram() / 1024 / 1024,
        machine.start_info.memmap_entries,
        machine.virtio.as_slice().len()
    );

    // Heap then PMM. The no-op reapers are the honest hooks for a kernel with
    // no processes, no page cache and no retired list (each is a `|| 0`).
    if !mem::init(&machine, false, akuma_pmm::PmmHooks {
        heap_reclaim: || 0,
        drain_retired: || 0,
        evict_clean_file_pages: |_| 0,
        shrink_page_cache: |_| 0,
    }) {
        console::puts("\nTaoxichuan/amd64 -- memory bring-up FAILED\n");
        halt();
    }

    // The root disk, and the ext2 root on it.
    if !blk_init(&machine.virtio) {
        console::puts("  [FATAL] no virtio block device -- the pinned root (ext2 on virtio-blk) cannot mount\n");
        halt();
    }
    let Ok(fs) = akuma_ext2::Ext2Filesystem::new(Vda, no_clock) else {
        console::puts("  fs:   /dev/vda holds no readable ext2 image\n");
        halt();
    };
    console::puts("  fs:   ext2 root mounted on /dev/vda\n");

    // ---- The init-spawn point ------------------------------------------------
    //
    // The full kernel resolves `init=` against the root, loads the ELF, builds
    // the address space and enters user mode. Stage 1's gate is *reaching*
    // this point on a mounted root: the banner goes here (the full kernel
    // prints its own in exactly this position, right before `run_init` starts
    // init), and the spawn's evidence is that init's bytes are on the disk and
    // are an ELF. Loading and entering user mode is stage 2's line.
    banner();

    let mut init_buf = [0u8; 128];
    let init_path = cmdline_token(hvm_start_info, "init=", &mut init_buf);
    let Some(path) = init_path else {
        console::puts("  init: no init= on the command line -- nothing to spawn\n");
        console::puts("Taoxichuan: stage 1 gate NOT reached (no init)\n");
        halt();
    };
    let mut args_buf = [0u8; 128];
    let args = cmdline_token(hvm_start_info, "initargs=", &mut args_buf).unwrap_or("");

    console::puts("Taoxichuan: init spawn -- init=");
    console::puts(path);
    if !args.is_empty() {
        console::puts(" argv=[");
        console::puts(args);
        console::puts("]");
    }
    console::puts("\n");

    if !fs.exists(path) {
        console::puts("  init: NOT on the mounted root\n");
        halt();
    }
    // Read the image head: the spawn stands on these bytes being an ELF.
    let image = fs.read_file(path).unwrap_or_default();
    let elf = image.len() >= 4 && image[..4] == [0x7f, b'E', b'L', b'F'];
    console::puts("  init: ");
    console::put_dec(image.len() as u64);
    console::puts(" bytes on the ext2 root, ELF magic ");
    console::puts(if elf { "ok" } else { "MISSING" });
    console::puts("\n");
    console::puts("Taoxichuan: stage 1 gate reached -- init spawn (userspace is stage 2)\n");

    // Park: interrupts masked, core halted. A quiet core, not a spinning one.
    halt();
}

/// The multiboot2 entry — called by `boot.s` when GRUB's magic says the info
/// block in `%rdi` is a multiboot2 one. This composition is a VMM guest
/// (Firecracker-style virtio-mmio assumptions are pinned in the experiment
/// brief); rather than growing a second boot protocol, it says so and stops.
/// The symbol must exist: `boot.s` names it unconditionally.
#[unsafe(no_mangle)]
pub extern "C" fn kmain_mb2(_info_phys: u64) -> ! {
    console::init();
    console::puts("\nTaoxichuan: multiboot2 boot -- not composed in this kernel (PVH/VMM only)\n");
    halt();
}

/// Park the core forever with interrupts masked.
pub fn halt() -> ! {
    loop {
        // SAFETY: `cli` and `hlt` are unconditionally safe to execute at ring
        // 0 and this function never returns, so masking interrupts permanently
        // is the intent rather than a leaked side effect.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Out of memory. The honest response at this stage is to say so and stop —
/// there is no process to kill and no pressure ladder to climb.
#[alloc_error_handler]
fn alloc_error_handler(layout: core::alloc::Layout) -> ! {
    akuma_primitives::safe_print!(
        96,
        "[OOM] allocation of {} bytes failed\n",
        layout.size()
    );
    halt();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if let Some(loc) = info.location() {
        akuma_primitives::safe_print!(192, "PANIC {}:{}\n", loc.file(), loc.line());
    } else {
        akuma_primitives::safe_print!(64, "PANIC <no location>\n");
    }
    akuma_primitives::safe_print!(256, "       {}\n", info.message());
    halt();
}
