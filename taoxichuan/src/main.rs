//! Taoxichuan (淘熙川) — the minimal composed Akuma amd64 kernel.
//!
//! The composition root of the minimal-self-hosting experiment's metal track
//! (`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`): a kernel built by
//! *pulling crates*, one line per stage, with as few crates as will serve —
//! and where the logic was still stuck in `amd64/src/`, extracting it into a
//! crate first and pulling that (`akuma-boot-amd64`, `akuma-comconsole` are
//! stage 1's extractions).
//!
//! # Stage 2 — ring 3
//!
//! The gate: the same static musl `hello` the host built runs in ring 3,
//! prints its line, and exits 0 — the kernel reports the captured exit code,
//! and the zero-`[Fault]` grep still holds. The spawn point stage 1 stopped
//! at is now the real thing: a fresh root page table (the kernel's upper half
//! shared by PML4 copy, the identity map left behind in the kernel's own
//! root), the ELF's `PT_LOAD`s placed eagerly at their link addresses, a
//! Linux initial stack, and `sysretq`. Syscalls come back through
//! `akuma-ring3-amd64`'s stub: `write` into the console, `exit_group`
//! captured and reported, `brk`/`mmap`/`arch_prctl`/`set_tid_address` enough
//! for musl's startup. One task, one core, no scheduler — the task that only
//! exits does not need one.
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
// Same allow as akuma-boot-amd64: several entry-point docs open with a long
// paragraph on purpose (the why lives in the first block).
#![allow(clippy::too_long_first_doc_paragraph)]

extern crate alloc;

use akuma_boot_amd64::{mem, paging, phys};
use akuma_comconsole as console;
use akuma_idt_amd64 as idt_lib;
use akuma_trap_amd64 as trap;
use akuma_trap_amd64::{InterruptStackFrame, Kind, Trap};
use akuma_vfs::Filesystem;
use akuma_boot_amd64::paging::{MemAttr, PteProt};
use akuma_ring3_amd64 as ring3;

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
    // Stage 2: a fault *from ring 3* is the task's death, not the kernel's —
    // say which, and park. There is no scheduler to reap through and no other
    // task to keep running; what matters for the gate is that this prints
    // `[Fault]` and stops instead of triple-faulting into guest silence.
    if t.from_ring3() {
        akuma_primitives::safe_print!(
            192,
            "[Fault] ring3 {:?} rip=0x{:016x} rsp=0x{:016x} cr2=0x{:016x} code=0x{:x} -- task killed\n",
            t.kind,
            t.frame.rip,
            t.frame.rsp,
            akuma_cpu::ctlreg::cr2(),
            code
        );
        halt();
    }
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
///
/// The picture: Jingdezhen as the emperor's kilns knew it -- the Chang in
/// front (junks on the water, kiln-fire glow on the current), hills behind,
/// and along the bank the egg-shaped hulu kilns with tall chimneys breathing
/// smoke, a house with upturned eaves, and a pagoda. The crates kernel sits
/// on the bank in the middle of it, where the wharf shed would be.
const ART: &str = concat!(
    "     ~      _       _                      ~      _         _  ~\n",
    "    ( )   __/ \\_______/ \\__/ \\            ( )   ___/ \\______/ (_)\n",
    "      |                    /--\\             |                  |\n",
    "   .-----.       ,^.      //[]\\\\         .-----.              .--.\n",
    "  ( |^^| )      /___\\     | [] |        ( |^^| )             ( () )\n",
    "   '-----'      |___|    /|____|\\        '-----'              '--'\n",
    "  +------------------------------------------------------+\n",
    "  |   Taoxichuan  --  tao xi chuan, the kiln-fire market  |\n",
    "  |   A kernel composed from Akuma's crates, one stage   |\n",
    "  |   at a time, fired in Akuma's own kiln: booted under |\n",
    "  |   Akuma's hypervisor, on Akuma's metal.              |\n",
    "  +------------------------------------------------------+\n",
    "  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~  ~\n",
    "        |\\                            |\\\n",
    "   ~*~\\__|_\\______   ~  ~  ~  ~   ~*~\\__|_\\______   ~  ~  ~  ~  ~\n",
);

fn banner() {
    console::puts("\n");
    console::puts(ART);
    console::puts("\n  Taoxichuan ");
    console::puts(env!("CARGO_PKG_VERSION"));
    console::puts(" (Akuma/amd64 crates) x86_64 -- stage 2\n\n");
}

// ---------------------------------------------------------------------------
// Stage 2: the task, its syscalls, and the crossing into ring 3.
// ---------------------------------------------------------------------------

/// Top of the task's stack: canonical user half, far from anything the kernel
/// places. 16 pages — far more than musl's startup touches, and cheap.
const USER_STACK_TOP: u64 = 0x0000_7000_0000_0000;
const USER_STACK_PAGES: u64 = 16;
/// `brk` and anonymous `mmap` each get a fixed 1 MiB slab, registered as user
/// regions up front so every user-pointer check is one range scan.
const SLAB: u64 = 0x0010_0000;
const MMAP_BASE: u64 = 0x0000_6000_0000_0000;
/// Cap on one `write(2)`: the console is a serial line, not a disk.
const WRITE_LIMIT: u64 = 1 << 20;

/// The one task: its root page table, the user ranges a pointer may name, the
/// heap cursors, and the exit code once captured.
struct Task {
    root_pa: u64,
    regions: [(u64, u64); 16],
    n_regions: usize,
    brk_base: u64,
    brk_cur: u64,
    brk_end: u64,
    mmap_cur: u64,
    exit: Option<u64>,
}

static mut TASK: Task = Task {
    root_pa: 0,
    regions: [(0, 0); 16],
    n_regions: 0,
    brk_base: 0,
    brk_cur: 0,
    brk_end: 0,
    mmap_cur: MMAP_BASE,
    exit: None,
};

/// The task, for the spawn path and the dispatcher.
///
/// SAFETY: one core; every touch is either before ring 3 exists or inside the
/// syscall dispatcher (`IF` clear from `syscall`'s own masking). No references
/// to the static are held across the boundary itself.
fn task() -> &'static mut Task {
    // SAFETY: see above — the aliasing conditions are the single-core,
    // interrupts-off discipline of this kernel, and the pointer is never
    // published anywhere else.
    unsafe { &mut *core::ptr::addr_of_mut!(TASK) }
}

fn add_region(start: u64, end: u64) {
    let t = task();
    t.regions[t.n_regions] = (start, end);
    t.n_regions += 1;
}

/// Is `[va, va+len)` fully inside a region this kernel mapped for the task?
/// The check that makes `write(1, ptr, len)` honest: a bogus pointer is
/// `-EFAULT`, not a kernel fault.
fn user_ok(va: u64, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    let Some(end) = va.checked_add(len) else {
        return false;
    };
    if end > 0x0000_8000_0000_0000 {
        return false; // the user half only
    }
    let t = task();
    t.regions[..t.n_regions].iter().any(|&(s, e)| va >= s && end <= e)
}

/// errno returns are the negated value in `RAX` — plain wrapping arithmetic
/// on `u64`, no signed casts.
fn e(code: u64) -> u64 {
    0u64.wrapping_sub(code)
}

/// Copy `dst.len()` bytes from the task's address space at `src_va`, one page
/// at a time through the physmap. False (`-EFAULT`) when a page does not
/// translate — the regions check above should make that unreachable, so it is
/// a backstop, not the gate.
fn copy_from_user(dst: &mut [u8], src_va: u64) -> bool {
    let root = task().root_pa;
    let mut done = 0usize;
    while done < dst.len() {
        let va = src_va + done as u64;
        let page_rem = 4096 - (va & 0xFFF) as usize;
        let n = page_rem.min(dst.len() - done);
        let Some(pa) = paging::translate_in(root, (va & !0xFFF) as usize) else {
            return false;
        };
        // SAFETY: `pa + off` is a PMM frame inside the physmap (alloc_page's
        // contract), and `n` stays within the frame by construction.
        let src = phys::phys_ptr::<u8>(pa + (va & 0xFFF));
        unsafe {
            core::ptr::copy_nonoverlapping(src, dst[done..].as_mut_ptr(), n);
        }
        done += n;
    }
    true
}

/// Copy `len` bytes of task memory at `buf_va` into the console, chunked.
/// False on a bad pointer (`-EFAULT` at the call site).
fn emit(buf_va: u64, len: u64) -> bool {
    let mut chunk = [0u8; 512];
    let mut done = 0u64;
    while done < len {
        let n = (len - done).min(chunk.len() as u64) as usize;
        if !copy_from_user(&mut chunk[..n], buf_va + done) {
            return false;
        }
        if let Ok(s) = core::str::from_utf8(&chunk[..n]) {
            console::puts(s);
        } else {
            // The relay sanitises non-ASCII anyway; name what would
            // have been silently mangled.
            console::puts("  [tty] write: non-utf8 bytes elided\n");
            break;
        }
        done += n as u64;
    }
    true
}

/// The syscall table, minimal on purpose: what static musl's startup and a
/// `printf`-and-exit `main` issue, and nothing else. Unknown numbers answer
/// `-ENOSYS` — the honest answer, and the one musl's `printf` path survives
/// for everything it merely probes.
fn dispatch(fr: &mut ring3::SyscallFrame) -> u64 {
    match fr.rax {
        ring3::nr::WRITE => {
            let (fd, buf, len) = (fr.arg1(), fr.arg2(), fr.arg3());
            if fd != 1 && fd != 2 {
                return e(ring3::err::EBADF);
            }
            if len > WRITE_LIMIT || !user_ok(buf, len) || !emit(buf, len) {
                return e(ring3::err::EFAULT);
            }
            len
        }
        ring3::nr::WRITEV => {
            // musl's stdio flush: `writev(fd, iov, iovcnt)`, 16-byte iovecs.
            let (fd, iov_va, iovcnt) = (fr.arg1(), fr.arg2(), fr.arg3());
            if fd != 1 && fd != 2 {
                return e(ring3::err::EBADF);
            }
            if iovcnt == 0 || iovcnt > 1024 || !user_ok(iov_va, iovcnt * 16) {
                return e(ring3::err::EFAULT);
            }
            let mut total = 0u64;
            for i in 0..iovcnt {
                let mut iv = [0u8; 16];
                if !copy_from_user(&mut iv, iov_va + i * 16) {
                    return e(ring3::err::EFAULT);
                }
                let (base, len) = (
                    u64::from_le_bytes(iv[0..8].try_into().unwrap()),
                    u64::from_le_bytes(iv[8..16].try_into().unwrap()),
                );
                if len > WRITE_LIMIT || !user_ok(base, len) || !emit(base, len) {
                    return e(ring3::err::EFAULT);
                }
                total += len;
            }
            total
        }
        ring3::nr::EXIT | ring3::nr::EXIT_GROUP => {
            let code = fr.arg1() & 0xFF; // what wait(2) would see
            let t = task();
            t.exit = Some(code);
            console::puts("Taoxichuan: hello exit code ");
            console::put_dec(code);
            console::puts("\n");
            if code == 0 {
                console::puts("Taoxichuan: stage 2 gate reached -- ring-3 ran, exited clean, output above\n");
            }
            halt();
        }
        ring3::nr::BRK => {
            let want = fr.arg1();
            let t = task();
            if want < t.brk_base || want >= t.brk_end {
                return t.brk_cur; // failure: Linux returns the *old* break
            }
            if want > t.brk_cur {
                // Grow: map fresh zeroed pages. Shrink is a bookkeeping move
                // (no unmap) — a `printf`-and-exit task never asks for it.
                let mut page = t.brk_cur & !0xFFF;
                let end = (want + 0xFFF) & !0xFFF;
                while page < end {
                    let Some(pa) = akuma_pmm::alloc_page_zeroed() else {
                        return t.brk_cur;
                    };
                    if !paging::map_page_in(t.root_pa, page as usize, pa as u64, PteProt::USER_RW, MemAttr::WriteBack) {
                        return t.brk_cur;
                    }
                    page += 4096;
                }
            }
            t.brk_cur = want;
            want
        }
        ring3::nr::ARCH_PRCTL => {
            let (code, addr) = (fr.arg1(), fr.arg2());
            match code {
                ring3::arch::ARCH_SET_FS => {
                    if addr >= 0x0000_8000_0000_0000 {
                        return e(ring3::err::EPERM); // the kernel half is not a TLS base
                    }
                    // `akuma_cpu::msr::set_fs_base` is the closed wrmsr path,
                    // canonical-check included.
                    match akuma_cpu::msr::set_fs_base(addr) {
                        Ok(()) => 0,
                        Err(_) => e(ring3::err::ERANGE),
                    }
                }
                ring3::arch::ARCH_GET_FS => {
                    if !user_ok(addr, 8) {
                        return e(ring3::err::EFAULT);
                    }
                    let v = akuma_cpu::msr::read(akuma_cpu::msr::Msr::FsBase);
                    // SAFETY: `addr` passed `user_ok` — a mapped task page;
                    // the physmap translation is the same walk the copy above
                    // makes.
                    let dst = phys::phys_ptr::<u64>(paging::translate_in(task().root_pa, addr as usize).unwrap_or(0) + (addr & 0xFFF));
                    unsafe {
                        dst.write_volatile(v);
                    }
                    0
                }
                other => {
            // Name the gap: a stage-3 task (busybox) will hit numbers this
            // table does not serve, and the console should say which.
            akuma_primitives::safe_print!(160, "  [sc] unhandled nr {}\n", other);
            e(ring3::err::ENOSYS)
        }
            }
        }
        ring3::nr::SET_TID_ADDRESS => 1, // one task: the tid, and nothing to clear
        ring3::nr::GETPID => 1,
        ring3::nr::GETUID | ring3::nr::GETEUID => 0,
        ring3::nr::GETGID | ring3::nr::GETEGID => 0,
        ring3::nr::IOCTL => e(ring3::err::ENOTTY), // the console is a serial line, not a tty
        ring3::nr::MMAP => {
            let (addr, len, prot, flags, fd) = (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5());
            if len == 0 {
                return e(ring3::err::EINVAL);
            }
            if flags & ring3::mmap::MAP_ANONYMOUS == 0 || fd != 0xFFFF_FFFF_FFFF_FFFF {
                return e(ring3::err::ENODEV); // no file-backed mappings in this kernel
            }
            let t = task();
            let pages = (len + 0xFFF) & !0xFFF;
            let base = if flags & ring3::mmap::MAP_FIXED != 0
                && addr >= MMAP_BASE
                && addr + pages <= MMAP_BASE + SLAB
            {
                addr
            } else {
                (t.mmap_cur + 0xF) & !0xF
            };
            if base + pages > MMAP_BASE + SLAB {
                return e(ring3::err::ENOMEM);
            }
            // Anonymous memory is RW; exec is honoured by widening, at the
            // cost of W^X — a task that maps RWX here tells the kernel it
            // wants it, and there is no W^X policy layer to argue with.
            let pte = if prot & ring3::mmap::PROT_EXEC != 0 {
                PteProt::USER_RWX
            } else {
                PteProt::USER_RW
            };
            let mut page = base & !0xFFF;
            while page < base + pages {
                let Some(pa) = akuma_pmm::alloc_page_zeroed() else {
                    return e(ring3::err::ENOMEM);
                };
                if !paging::map_page_in(t.root_pa, page as usize, pa as u64, pte, MemAttr::WriteBack) {
                    return e(ring3::err::ENOMEM);
                }
                page += 4096;
            }
            t.mmap_cur = base + pages;
            base
        }
        ring3::nr::MUNMAP | ring3::nr::MPROTECT => {
            // Unmap is a no-op (the slabs live as long as the task); mprotect
            // is accepted and ignored — nothing in this kernel enforces
            // per-page protection changes after placement.
            if fr.arg1() & 0xFFF != 0 {
                e(ring3::err::EINVAL)
            } else {
                0
            }
        }
        other => {
            // Name the gap: a stage-3 task (busybox) will hit numbers this
            // table does not serve, and the console should say which.
            akuma_primitives::safe_print!(160, "  [sc] unhandled nr {}\n", other);
            e(ring3::err::ENOSYS)
        }
    }
}

/// Load the image, build the task's address space, and cross. Never returns:
/// the task ends in the dispatcher's `exit` or the trap handler's `[Fault]`.
fn spawn(path: &str, image: &[u8]) -> ! {
    let img = match ring3::Image::parse(image) {
        Ok(img) => img,
        Err(err) => {
            akuma_primitives::safe_print!(160, "  init: ELF refused: {:?}\n", err);
            halt();
        }
    };
    akuma_primitives::safe_print!(
        160,
        "  init: {} PT_LOAD segment(s), entry 0x{:016x}\n",
        img.segments().len(),
        img.entry()
    );

    // The task's root: a fresh page table whose upper half is the kernel's
    // (one PML4 copy shares every kernel subtree — physmap, device window,
    // kernel image), whose lower half holds only the task. The kernel's own
    // root keeps its identity map; the task's root simply has none.
    let Some(root_page) = akuma_pmm::alloc_page_zeroed() else {
        console::puts("  init: no free frame for the task's page root\n");
        halt();
    };
    let root = root_page as u64;
    let kroot = akuma_cpu::ctlreg::cr3() & 0x000f_ffff_ffff_f000;
    // SAFETY: both frames are PMM/established pages inside the physmap; the
    // copies are plain RAM writes through the physmap, and the task's tables
    // are not live until the CR3 load below.
    unsafe {
        let ktbl = phys::phys_ptr::<[u64; 512]>(kroot);
        let utbl = phys::phys_ptr::<[u64; 512]>(root);
        for i in 256..512 {
            (*utbl)[i] = (*ktbl)[i];
        }
    }
    let t = task();
    t.root_pa = root;

    // Place every PT_LOAD at its link address, page by page: fresh zeroed
    // frame, map, copy the file-backed slice through the physmap. The zeroed
    // tail (`memsz > filesz` — `.bss`) needs nothing more.
    let mut placed = 0u64;
    for seg in img.segments() {
        let pte = match seg.prot {
            ring3::Prot::Ro => PteProt::USER_RO,
            ring3::Prot::Rx => PteProt::USER_RX,
            ring3::Prot::Rw => PteProt::USER_RW,
        };
        let first = seg.vaddr & !0xFFF;
        let last = (seg.vaddr + seg.memsz + 0xFFF) & !0xFFF;
        let file = img.segment_bytes(seg);
        let mut pva = first;
        while pva < last {
            let Some(pa) = akuma_pmm::alloc_page_zeroed() else {
                console::puts("  init: out of frames placing the image\n");
                halt();
            };
            if !paging::map_page_in(root, pva as usize, pa as u64, pte, MemAttr::WriteBack) {
                console::puts("  init: map_page_in refused a user page\n");
                halt();
            }
            // The page's overlap with the segment's file-backed range.
            let from = pva.max(seg.vaddr);
            let to = (pva + 4096).min(seg.vaddr + seg.filesz);
            if from < to {
                let src = &file[(from - seg.vaddr) as usize..(to - seg.vaddr) as usize];
                // SAFETY: `pa` is this page's fresh frame; `from - pva` stays
                // inside it because `from >= pva` and `to <= pva + 4096`.
                let dst = phys::phys_ptr::<u8>(pa as u64 + (from - pva));
                unsafe {
                    core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
                }
            }
            placed += 1;
            pva += 4096;
        }
        add_region(first, last);
    }

    // The stack, then the initial block on it. The block is < half a page and
    // ends at `USER_STACK_TOP`, so it lands entirely in the top page.
    let stack_base = USER_STACK_TOP - USER_STACK_PAGES * 4096;
    let mut stack_top_pa = 0u64;
    let mut pva = stack_base;
    while pva < USER_STACK_TOP {
        let Some(pa) = akuma_pmm::alloc_page_zeroed() else {
            console::puts("  init: out of frames for the user stack\n");
            halt();
        };
        if pva + 4096 == USER_STACK_TOP {
            stack_top_pa = pa as u64;
        }
        if !paging::map_page_in(root, pva as usize, pa as u64, PteProt::USER_RW, MemAttr::WriteBack) {
            console::puts("  init: map_page_in refused a stack page\n");
            halt();
        }
        pva += 4096;
    }
    add_region(stack_base, USER_STACK_TOP);

    // argc/argv/envp/auxv. argv[0] is the path the command line named; no
    // env. `AT_RANDOM`/`AT_EXECFN` values are the builder's to fill.
    const AUX_N: usize = 14;
    let mut aux = [(0u64, 0u64); AUX_N];
    aux[0] = (ring3::AT_BASE, 0); // no interpreter
    aux[1] = (ring3::AT_PHDR, img.phdr_va());
    aux[2] = (ring3::AT_PHENT, img.phent());
    aux[3] = (ring3::AT_PHNUM, img.phnum());
    aux[4] = (ring3::AT_PAGESZ, 4096);
    aux[5] = (ring3::AT_ENTRY, img.entry());
    aux[6] = (ring3::AT_UID, 0);
    aux[7] = (ring3::AT_EUID, 0);
    aux[8] = (ring3::AT_GID, 0);
    aux[9] = (ring3::AT_EGID, 0);
    aux[10] = (ring3::AT_SECURE, 0);
    aux[11] = (ring3::AT_HWCAP, 0);
    aux[12] = (ring3::AT_CLKTCK, 100);
    aux[13] = (ring3::AT_RANDOM, 0);
    let mut block = [0u8; 512];
    let rsp = ring3::build_stack(&mut block, USER_STACK_TOP, &[path], &[], &aux);

    // SAFETY: the top stack page is a mapped task frame; the block ends at
    // `USER_STACK_TOP`, so its bytes sit at `page_pa + (4096 - block.len())`.
    let dst = phys::phys_ptr::<u8>(stack_top_pa + (4096 - block.len() as u64));
    unsafe {
        core::ptr::copy_nonoverlapping(block.as_ptr(), dst, block.len());
    }

    // The heap slabs: `brk` above the image, `mmap` in its own window. Both
    // registered now, so a pointer check is one scan.
    let brk0 = (img.image_end() + 0xFFF) & !0xFFF;
    t.brk_base = brk0;
    t.brk_cur = brk0;
    t.brk_end = brk0 + SLAB;
    add_region(brk0, t.brk_end);
    add_region(MMAP_BASE, MMAP_BASE + SLAB);

    akuma_primitives::safe_print!(
        160,
        "  init: {} user page(s) mapped at root 0x{:016x}, stack 0x{:016x}..0x{:016x}\n",
        placed + USER_STACK_PAGES,
        root,
        stack_base,
        USER_STACK_TOP
    );

    // Arm the syscall path, then cross. `mov cr3` is the write the ctlreg
    // crate deliberately does not own (reads safe, writes unsafe at the call
    // site): it loads the task's root, flushing the non-global TLB — the
    // kernel half survives through the shared PML4 entries.
    ring3::enable_syscalls(dispatch);
    console::puts("Taoxichuan: entering ring 3 -- ");
    console::puts(path);
    console::puts("\n");
    // SAFETY: one documented control-register write; interrupts are masked,
    // one core, and the root was built entirely before this line.
    unsafe {
        core::arch::asm!(
            "mov cr3, {r}",
            r = in(reg) root,
            options(nostack, preserves_flags)
        );
    }
    // SAFETY: `EntryFrame` is plain `u64` fields; all-zero is a valid value.
    let mut ef: ring3::EntryFrame = unsafe { core::mem::zeroed() };
    ef.rip = img.entry();
    ef.rflags = 0x202; // IF set, reserved bit 1 set
    ef.rsp = rsp;
    // SAFETY: the address space maps the kernel half (shared PML4 entries)
    // and the task's pages; `ef` names image-placed code on the built stack.
    unsafe { ring3::enter_user(&ef) }
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

    // Stage 2: the spawn is real now. Load, cross, and the task's own exit
    // (or its fault) is the next line on this console.
    spawn(path, &image);
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
