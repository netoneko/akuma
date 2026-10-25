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
//! # Stage 3 — busybox, sequentially
//!
//! The gate: the guest runs `busybox echo <fixed string>` and
//! `busybox ls <fixed directory on the ext2 root>` as kernel-spawned
//! programs, each byte-identical to the reference (the same binary run on the
//! box against the same file set), exit code 0, zero fault lines. No shell,
//! no pty, no `fork`: the two programs are *sequential kernel-reaped tasks* —
//! spawn, run to `exit`, then spawn the next. `exit_group` does not halt the
//! machine any more; it reports, and the boot sequence continues with the
//! next program on the command line (`init=`/`initargs=` for the first,
//! `then=`/`thenargs=` for each follow-up). What the applets needed beyond
//! stage 2: an fd table (pulled, not written: `akuma-fdtable`, the extracted
//! fd-*decision* crate — this kernel supplies the effects, a few fixed slots
//! of it per task), and the syscalls their startup and their work ask for:
//! `openat` (absolute paths only, `AT_FDCWD` only, read-only), `getdents64`
//! (served from the directory listing cached on the open description),
//! `close`, `read` (a file fd, or `EOF` on the console's stdin — there is no
//! input path), and `rt_sigaction`/`rt_sigprocmask` as accepted no-ops: there
//! is no signal delivery to register against, and saying so beats failing
//! startup on a service the kernel never promised.
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

mod child;
mod net;
mod sock;

use akuma_boot_amd64::{mem, paging, phys};
use akuma_comconsole as console;
use akuma_idt_amd64 as idt_lib;
use akuma_ring3_amd64::err;
use akuma_trap_amd64 as trap;
use akuma_trap_amd64::{InterruptStackFrame, Kind, Trap};
use akuma_vfs::Filesystem;
use akuma_boot_amd64::paging::{MemAttr, PteProt};
use akuma_ring3_amd64 as ring3;
use akuma_fdtable as fdtable;
use alloc::vec::Vec;
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

/// The mounted ext2 root, from `kmain`'s mount point and from every fd
/// syscall. Stage 3's move out of `kmain`'s frame: `openat`/`getdents64`
/// answer from ring-0 syscall context, which has no access to a local.
///
/// SAFETY: one core; every touch is either before ring 3 exists or inside
/// the syscall dispatcher (`IF` clear from `syscall`'s own masking) — the
/// same discipline `task()` documents. `Ext2Filesystem` is `Send + Sync`
/// (`akuma-vfs`'s `Filesystem` supertraits); the interior lock is a spinlock
/// that never blocks on anything this kernel runs.
static mut ROOT_FS: Option<akuma_ext2::Ext2Filesystem<Vda>> = None;

/// The mounted root, for the dispatcher. Panics if called before the mount —
/// which no caller does: the mount precedes the first ring-3 crossing.
fn root_fs() -> &'static akuma_ext2::Ext2Filesystem<Vda> {
    // SAFETY: see above; the Option is always Some past `kmain`'s mount.
    unsafe { (&*core::ptr::addr_of!(ROOT_FS)).as_ref().unwrap() }
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
    console::puts(" (Akuma/amd64 crates) x86_64 -- stage 3\n\n");
}

// ---------------------------------------------------------------------------
// Stage 2: the task, its syscalls, and the crossing into ring 3.
// ---------------------------------------------------------------------------

/// Top of the task's stack: canonical user half, far from anything the kernel
/// places. 16 pages — far more than musl's startup touches, and cheap.
const USER_STACK_TOP: u64 = 0x0000_7000_0000_0000;
const USER_STACK_PAGES: u64 = 16;
/// `brk` and anonymous `mmap` each get a fixed slab, registered as user
/// regions up front so every user-pointer check is one range scan. Stage 6:
/// 64 MiB each — apk.static's musl heap (realloc→mremap traffic) blew past
/// the old 1 MiB on the aarch64 bring-up; the cost is only pages actually
/// touched, and the range is far from PIE code (`PIE_BASE`) and the stack.
const SLAB: u64 = 0x0400_0000;
const MMAP_BASE: u64 = 0x0000_6000_0000_0000;
/// Cap on one `write(2)`: the console is a serial line, not a disk.
const WRITE_LIMIT: u64 = 1 << 20;

/// The fd table's fixed slot ceiling — "a few fixed slots" is the pinned
/// shape for this stage. fds 0/1/2 are the console (write-only) and never
/// enter the table; 3..SLOT_CEILING is what `openat` may hand out. The
/// *decisions* (lowest free, close, the flag sets) are `akuma-fdtable`'s.
const SLOT_CEILING: u32 = 16;

/// One open description: what an fd in the table names. A directory caches
/// its `read_dir` listing at open time (this kernel's `getdents64`
/// serves the cache; the ext2 root's listing cannot go stale behind the
/// task's back), with the cursor the next `getdents64` resumes from, and —
/// stage 6 — the directory's path, because apk opens a directory and then
/// passes its fd to `openat`/`renameat` for dirfd-relative resolution.
/// A regular file carries the inode number and its own cursor. Paths are not
/// kept on files: an fd-relative *write* target names a directory fd.
/// Stage 4 adds the two non-file kinds the sshd stage needs: a socket (the
/// index into `akuma-net`'s socket table) and a spawned child's stdout (the
/// index into `child.rs`'s spawn slots — the "pipe" of the child model).
enum OpenFile {
    Dir {
        path: alloc::string::String,
        entries: Vec<akuma_vfs::DirEntry>,
        cursor: usize,
    },
    File {
        inode: u32,
        offset: u64,
        size: u64,
    },
    /// A TCP/UDP socket: `akuma_net`'s socket-table index. `sock.rs` serves
    /// every access through it.
    Socket(usize),
    /// A spawned child's stdout buffer: `child.rs`'s spawn-slot index.
    /// Drained by `read`, then EOF — a pipe that is just a buffer.
    ChildOut(usize),
}

/// The one task: its root page table, the user ranges a pointer may name, the
/// heap cursors, its fd table's three `akuma-fdtable` parts, and the exit
/// code once captured.
struct Task {
    root_pa: u64,
    regions: [(u64, u64); 16],
    n_regions: usize,
    brk_base: u64,
    brk_cur: u64,
    brk_end: u64,
    mmap_cur: u64,
    exit: Option<u64>,
    fd_names: fdtable::Names<OpenFile>,
    fd_cloexec: fdtable::FlagSet,
    fd_nonblock: fdtable::FlagSet,
}

static mut TASKS: [Task; 2] = [Task::new(), Task::new()];
/// Which slot `task()` hands out: 0 is the boot's init task (sshd for this
/// stage's gate), 1 is the child running inside a SPAWN (`child.rs`).
static mut TASK_SLOT: usize = 0;

impl Task {
    const fn new() -> Self {
        Self {
            root_pa: 0,
            regions: [(0, 0); 16],
            n_regions: 0,
            brk_base: 0,
            brk_cur: 0,
            brk_end: 0,
            mmap_cur: MMAP_BASE,
            exit: None,
            fd_names: fdtable::Names::new(),
            fd_cloexec: fdtable::FlagSet::new(),
            fd_nonblock: fdtable::FlagSet::new(),
        }
    }
}

/// Switch which task slot the syscall path acts on (the SPAWN arm and the
/// child exit, both in `child.rs`).
fn set_task_slot(i: usize) {
    // SAFETY: single core, `IF` clear — the same discipline as `task()`.
    unsafe {
        *core::ptr::addr_of_mut!(TASK_SLOT) = i;
    }
}

/// Both slots, for the spawn path (it snapshots the parent's `root_pa`).
fn task_slots() -> &'static mut [Task; 2] {
    // SAFETY: as `task`.
    unsafe { &mut *core::ptr::addr_of_mut!(TASKS) }
}

/// The task, for the spawn path and the dispatcher.
///
/// SAFETY: one core; every touch is either before ring 3 exists or inside the
/// syscall dispatcher (`IF` clear from `syscall`'s own masking). No references
/// to the static are held across the boundary itself.
fn task() -> &'static mut Task {
    // SAFETY: see above — the aliasing conditions are the single-core,
    // interrupts-off discipline of this kernel, and the pointer is never
    // published anywhere else.
    unsafe {
        let slot = core::ptr::read_volatile(&raw const TASK_SLOT);
        &mut (*core::ptr::addr_of_mut!(TASKS))[slot]
    }
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

/// Copy `src` into the task's address space at `dst_va`, one page at a time
/// through the physmap — `copy_from_user`'s mirror, for syscalls that fill
/// user buffers (`getdents64`, `read`). False (`-EFAULT`) when a page does
/// not translate; the regions check is the gate, this is the backstop.
fn copy_to_user(dst_va: u64, src: &[u8]) -> bool {
    let root = task().root_pa;
    let mut done = 0usize;
    while done < src.len() {
        let va = dst_va + done as u64;
        let page_rem = 4096 - (va & 0xFFF) as usize;
        let n = page_rem.min(src.len() - done);
        let Some(pa) = paging::translate_in(root, (va & !0xFFF) as usize) else {
            return false;
        };
        // SAFETY: `pa + off` is a PMM frame inside the physmap (alloc_page's
        // contract), `n` stays within the frame by construction, and the page
        // is a task page this kernel mapped RW (or RO, for image bytes —
        // every caller writes only into stack/heap/scratch regions).
        let dst = phys::phys_ptr::<u8>(pa + (va & 0xFFF));
        unsafe {
            core::ptr::copy_nonoverlapping(src[done..].as_ptr(), dst, n);
        }
        done += n;
    }
    true
}

/// The ext2 lookup does not follow symlinks (a deliberate crate boundary:
/// `akuma-ext2` resolves components, `akuma-vfs::path` owns the chain
/// walker). The serve image's `/bin/sh` — what sshd's exec path spawns — is
/// a symlink, so every path that names a program goes through the chain
/// walker with ext2 as the effect provider: `is_symlink` to decide, the
/// link inode's bytes for the target (relative targets joined by the
/// walker), 8 hops to ELOOP. The stage-3 `open` path gets the same walker,
/// because `/etc/sshd/sshd.conf`'s `shell = /bin/sh` is the exact case.
fn read_link_target(path: &str) -> Option<alloc::string::String> {
    let fs = root_fs();
    if !fs.is_symlink(path) {
        return None;
    }
    // `read_symlink`, never `read_file`: a fast symlink (every busybox link
    // on the serve image — `/bin/sh`, `/bin/apk` — fits the 60-byte inline
    // form) keeps its target in the inode's pointer-word window and has no
    // data blocks. `read_file` handed those first target bytes to the block
    // mapper as a bogus block number, the read failed, spawn answered ENOENT,
    // and sshd fell back to built-in paws on every exec — three stages of
    // gates quietly ran on shell builtins because of this one provider.
    // `read_symlink` returns the target verbatim (`read_symlink_inode`
    // handles both the fast inline form and the slow block form); strip one
    // trailing newline only as belt-and-braces.
    fs.read_symlink(path)
        .ok()
        .map(|s| alloc::string::String::from(s.trim_end_matches('\n')))
}

/// Resolve symlinks (bounded), returning the effective path to look up.
fn resolve_effective(path: &str) -> alloc::string::String {
    akuma_vfs::resolve_symlink_chain(path, 8, read_link_target)
}

/// Read a NUL-terminated path from the task at `src_va` into `buf`, returning
/// the `&str` it held. `None` (`-EFAULT` at the call site) on a bad byte or
/// no terminator inside `buf.len()`.
fn user_str(src_va: u64, buf: &mut [u8]) -> Option<&str> {
    for i in 0..buf.len() {
        let mut one = [0u8; 1];
        if !copy_from_user(&mut one, src_va + i as u64) {
            return None;
        }
        if one[0] == 0 {
            return core::str::from_utf8(&buf[..i]).ok();
        }
        buf[i] = one[0];
    }
    None
}

/// Borrow the task's fd table as one `akuma-fdtable` view. The single-core,
/// `IF`-off discipline that owns `task()` owns this too.
fn fd_table() -> fdtable::FdTable<'static, OpenFile> {
    let t = task();
    fdtable::FdTable::of(&mut t.fd_names, &mut t.fd_cloexec, &mut t.fd_nonblock)
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
/// The open body shared by `openat` (with its `AT_FDCWD` check at the call
/// site) and legacy `open`: resolve the absolute path on the ext2 root, build
/// the open description, take the lowest free fd slot. Absolute paths only,
/// read-only access only: the root is read-only in spirit and this kernel has
/// no write path to lie about.
fn open_common(path_va: u64, flags: u64) -> u64 {
    let mut pbuf = [0u8; 128];
    let Some(path) = user_str(path_va, &mut pbuf) else {
        return e(ring3::err::EFAULT);
    };
    if !path.starts_with('/') {
        return e(ring3::err::ENOENT); // nothing to resolve ".." against
    }
    // Stage 6: write access is served — apk unpacks with O_WRONLY|O_CREAT|
    // O_TRUNC. The description is an ext2 lookup (or create) — no VFS mount
    // table, one root.
    let acc = flags & ring3::open::O_ACCMODE;
    // Stage 4: symlinks resolve (`/bin/sh` is one). A fresh O_CREAT file has
    // no links to resolve; `resolve_effective` passes such a path through.
    let path = resolve_effective(path);
    let fs = root_fs();
    let md = match fs.metadata(&path) {
        Ok(md) => Some(md),
        Err(_) if flags & ring3::open::O_CREAT != 0 && acc != ring3::open::O_RDONLY => {
            // Create empty; the fd's writes fill it. `write_file` on a
            // missing path allocates the inode and the directory entry.
            match fs.write_file(&path, &[]) {
                Ok(()) => fs.metadata(&path).ok(),
                Err(_) => return e(ring3::err::EACCES),
            }
        }
        Err(_) => return e(ring3::err::ENOENT),
    };
    let Some(md) = md else { return e(ring3::err::EIO) };
    if md.is_dir {
        if acc != ring3::open::O_RDONLY {
            return e(ring3::err::EISDIR);
        }
        if flags & ring3::open::O_DIRECTORY == 0 {
            // a directory opened for reading without O_DIRECTORY is still a
            // directory; Linux allows it (getdents on it works)
        }
    } else if flags & ring3::open::O_DIRECTORY != 0 {
        return e(ring3::err::ENOTDIR);
    }
    let desc = if md.is_dir {
        // The listing is the open description's payload: `getdents64`
        // serves the cache, the root cannot change behind the task.
        match fs.read_dir(&path) {
            Ok(entries) => {
                OpenFile::Dir { path, entries, cursor: 0 }
            }
            Err(_) => return e(ring3::err::EIO),
        }
    } else {
        match fs.resolve_inode(&path) {
            Ok(inode) => {
                // O_TRUNC is a decision, not just a size: fold it once so the
                // `if` stays an expression and the side effect stays explicit.
                let trunc = flags & ring3::open::O_TRUNC != 0
                    && acc != ring3::open::O_RDONLY;
                if trunc && fs.truncate_by_inode(inode, 0).is_err() {
                    return e(ring3::err::EIO);
                }
                let size = if trunc { 0 } else { md.size };
                OpenFile::File { inode, offset: 0, size }
            }
            Err(_) => return e(ring3::err::EIO),
        }
    };
    let mut table = fd_table();
    let Some(_slot) = table.free_below(3, SLOT_CEILING) else {
        return e(ring3::err::EMFILE);
    };
    let fd = table.alloc_fd_from(3, desc);
    if flags & ring3::open::O_CLOEXEC != 0 {
        table.set_cloexec(fd); // O_CLOEXEC: recorded, nothing execs yet
    }
    u64::from(fd)
}

/// Stage 6, phase A: write a task buffer into the open file fd. Chunks
/// through the staging buffer the `read` arm uses; the ext2 write path
/// (`write_at_by_inode`) extends the file as the offset passes its end, and
/// the fd's size/cursor follow the bytes that landed.
fn write_file_arm(fd: u64, buf: u64, len: u64) -> u64 {
    let mut chunk = [0u8; 1024];
    let mut total = 0u64;
    let mut res: Result<u64, u64> = Ok(0);
    let found = fd_table().update(fd as u32, |of| {
        let OpenFile::File { inode, offset, size } = of else {
            res = Err(ring3::err::EBADF);
            return;
        };
        let mut done = 0u64;
        while done < len {
            let want = ((len - done) as usize).min(chunk.len());
            if !copy_from_user(&mut chunk[..want], buf + done) {
                res = Err(ring3::err::EFAULT);
                return;
            }
            match root_fs().write_at_by_inode(*inode, (*offset + done) as usize, &chunk[..want]) {
                Ok(n) if n > 0 => {
                    *offset += n as u64;
                    *size = (*size).max(*offset);
                    done += n as u64;
                    total += n as u64;
                    if n < want {
                        break; // short write: stop where the disk stopped
                    }
                }
                _ => {
                    res = Err(ring3::err::EIO);
                    return;
                }
            }
        }
        res = Ok(total);
    });
    match (found, res) {
        (false, _) => e(ring3::err::EBADF),
        (_, Err(no)) => e(no),
        (_, Ok(n)) => n,
    }
}

/// Stage 6: dirfd-relative path resolution. Absolute paths pass through; a
/// relative path resolves against an open directory fd's path (apk opens the
/// db / cache directory and then passes its fd to `openat`/`renameat`); a
/// relative path at `AT_FDCWD` resolves against `/` — this kernel has no
/// working directory, and every task it boots runs at the root. `None` means
/// the caller sees `EBADF` (a non-directory fd where a dirfd was required).
fn resolve_at(dirfd: u64, path: &str) -> Option<alloc::string::String> {
    if path.starts_with('/') {
        return Some(alloc::string::String::from(path));
    }
    if dirfd == ring3::open::AT_FDCWD {
        return Some(alloc::format!("/{path}"));
    }
    let dir = fd_table().with(dirfd as u32, |d| match d {
        OpenFile::Dir { path, .. } => Some(path.clone()),
        _ => None,
    })??;
    Some(alloc::format!("{}/{}", dir.trim_end_matches('/'), path))
}

/// Pull a NUL-terminated path out of the task, resolve it through a dirfd,
/// and hand it to `f`. `EBADF` for a non-directory dirfd, `EFAULT` for a bad
/// task pointer. The `at` family's shared front half.
fn path_at(path_arg: u64, dirfd: u64, f: impl FnOnce(&str) -> u64) -> u64 {
    let mut pbuf = [0u8; 128];
    let Some(path) = user_str(path_arg, &mut pbuf) else {
        return e(ring3::err::EFAULT);
    };
    if path.is_empty() {
        return e(ring3::err::ENOENT);
    }
    let Some(full) = resolve_at(dirfd, path) else {
        return e(ring3::err::EBADF);
    };
    f(&full)
}

/// `stat` semantics over a fully-resolved path (symlinks followed by the
/// caller's `resolve_effective`).
fn stat_path(path: &str, buf: u64) -> u64 {
    match root_fs().metadata(path) {
        Ok(md) => match fill_stat64(
            buf,
            md.inode,
            md.mode,
            md.size,
            u64::from(md.links.unwrap_or(if md.is_dir { 2 } else { 1 })),
        ) {
            Ok(()) => 0,
            Err(no) => e(no),
        },
        Err(_) => e(ring3::err::ENOENT),
    }
}

/// `lstat` semantics: the final component is looked up in its parent's
/// listing instead of followed, so a symlink names itself (mode
/// `S_IFLNK`, the link's own size). Same shape as the `LSTAT` arm.
fn lstat_path(path: &str, buf: u64) -> u64 {
    let Some(idx) = path.rfind('/') else {
        return e(ring3::err::ENOENT);
    };
    let (parent, name) = (&path[..idx.max(1)], &path[idx + 1..]);
    if name.is_empty() {
        stat_path(path, buf)
    } else {
        let fs = root_fs();
        let Ok(entries) = fs.read_dir(parent) else {
            return e(ring3::err::ENOENT);
        };
        let Some(ent) = entries.iter().find(|e| e.name == name) else {
            return e(ring3::err::ENOENT);
        };
        let mode = if ent.is_symlink {
            0o120777 // S_IFLNK | rwxrwxrwx: the convention for links
        } else if ent.is_dir {
            0o40755
        } else {
            0o100644
        };
        match fill_stat64(buf, 0, mode, ent.size, if ent.is_dir { 2 } else { 1 }) {
            Ok(()) => 0,
            Err(no) => e(no),
        }
    }
}

/// Fill the x86_64 `struct statfs` (120 bytes): ext2's magic `0xEF53` and
/// plausible counts — aarch64 measured that `ENOSYS` here made apk treat
/// later operations as forbidden, so this arm must exist and look sane.
fn fill_statfs(buf: u64) -> u64 {
    if !user_ok(buf, 120) {
        return e(ring3::err::EFAULT);
    }
    let mut s = [0u8; 120];
    let mut w = |i: usize, v: u64| s[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    w(0, 0xEF53); // f_type
    w(1, 4096); // f_bsize
    w(2, 4096); // f_blocks (16 MiB / 4 KiB)
    w(3, 2053); // f_bfree — the fresh image's free-block count
    w(4, 2053); // f_bavail
    w(5, 4096); // f_files
    w(6, 2048); // f_ffree
    // f_fsid (slot 7) stays zero; nothing reads it.
    w(8, 255); // f_namelen
    w(9, 4096); // f_frsize
    w(10, 0); // f_flags: not read-only — this root writes now
    if !copy_to_user(buf, &s) {
        return e(ring3::err::EFAULT);
    }
    0
}

/// `fstat` for an fd of any kind: files carry their inode (real mode from
/// the on-disk inode), directories get the conventional 0o40755, the two
/// non-fs kinds (socket, child stdout) get a plausible pipe-ish mode.
fn fstat_fd(fd: u64, buf: u64) -> u64 {
    if !user_ok(buf, 144) {
        return e(ring3::err::EFAULT);
    }
    if fd <= 2 || fd as u32 >= SLOT_CEILING {
        return e(ring3::err::EBADF);
    }
    let info = fd_table().with(fd as u32, |d| match d {
        OpenFile::File { inode, size, .. } => Some((u64::from(*inode), *size, 0u8)),
        OpenFile::Dir { .. } => Some((0, 0, 1)),
        _ => Some((0, 0, 2)),
    });
    let Some(Some((inode, size, kind))) = info else {
        return e(ring3::err::EBADF);
    };
    let (mode, links) = match kind {
        0 => match root_fs().metadata_by_inode(inode as u32) {
            Ok(md) => (md.mode, u64::from(md.links.unwrap_or(1))),
            Err(_) => (0o100644, 1),
        },
        1 => (0o40755, 2),
        _ => (0o100600, 1),
    };
    match fill_stat64(buf, inode, mode, size, links) {
        Ok(()) => 0,
        Err(no) => e(no),
    }
}

/// A fresh open description equal to `d` — `dup(2)`'s copy. The offset is
/// *copied*, not shared (the table stores descriptions by value); nothing in
/// the apk path reads through two fds sharing one offset.
fn clone_desc(d: &OpenFile) -> OpenFile {
    match d {
        OpenFile::File { inode, offset, size } => {
            OpenFile::File { inode: *inode, offset: *offset, size: *size }
        }
        OpenFile::Dir { path, entries, cursor } => OpenFile::Dir {
            path: path.clone(),
            entries: entries.clone(),
            cursor: *cursor,
        },
        OpenFile::Socket(i) => OpenFile::Socket(*i),
        OpenFile::ChildOut(i) => OpenFile::ChildOut(*i),
    }
}

/// Map the shared crate's `FsError` onto the errno the syscall arm returns.
fn fs_err(err: akuma_vfs::FsError) -> u64 {
    use akuma_vfs::FsError as F;
    match err {
        F::NotFound | F::NotInitialized => ring3::err::ENOENT,
        F::AlreadyExists => ring3::err::EEXIST,
        F::NotADirectory => ring3::err::ENOTDIR,
        F::NotAFile => ring3::err::EISDIR,
        F::DirectoryNotEmpty => ring3::err::ENOTEMPTY,
        F::PermissionDenied | F::ReadOnly => ring3::err::EACCES,
        F::InvalidPath => ring3::err::EINVAL,
        F::NoSpace => ring3::err::ENOSPC,
        F::TooManyOpenFiles => ring3::err::EMFILE,
        F::CrossDevice => ring3::err::EXDEV,
        _ => ring3::err::EIO,
    }
}

/// Reverse-map a syscall number to its name, for the unhandled arm — each
/// boot then reads as a checklist instead of a table of bare numbers.
fn nr_name(nr: u64) -> &'static str {
    match nr {
        ring3::nr::READ => "read",
        ring3::nr::WRITE => "write",
        ring3::nr::OPEN => "open",
        ring3::nr::CLOSE => "close",
        ring3::nr::STAT => "stat",
        ring3::nr::FSTAT => "fstat",
        ring3::nr::LSTAT => "lstat",
        ring3::nr::LSEEK => "lseek",
        ring3::nr::MMAP => "mmap",
        ring3::nr::MPROTECT => "mprotect",
        ring3::nr::MUNMAP => "munmap",
        ring3::nr::MREMAP => "mremap",
        ring3::nr::BRK => "brk",
        ring3::nr::IOCTL => "ioctl",
        ring3::nr::POLL => "poll/ppoll",
        ring3::nr::DUP => "dup",
        ring3::nr::NANOSLEEP => "nanosleep",
        ring3::nr::GETPID => "getpid",
        ring3::nr::UNAME => "uname",
        ring3::nr::FLOCK => "flock",
        ring3::nr::FCNTL => "fcntl",
        ring3::nr::FCHMOD => "fchmod",
        ring3::nr::FCHOWNAT => "fchownat",
        ring3::nr::UMASK => "umask",
        ring3::nr::GETTIMEOFDAY => "gettimeofday",
        ring3::nr::GETRUSAGE => "getrusage",
        ring3::nr::GETCWD => "getcwd",
        ring3::nr::READLINKAT => "readlinkat",
        ring3::nr::SYSINFO => "sysinfo",
        ring3::nr::FSTATFS => "fstatfs",
        ring3::nr::STATFS => "statfs",
        ring3::nr::UNLINKAT => "unlinkat",
        ring3::nr::LINKAT => "linkat",
        ring3::nr::SYMLINKAT => "symlinkat",
        ring3::nr::RENAMEAT => "renameat",
        ring3::nr::UTIMENSAT => "utimensat",
        ring3::nr::MKDIRAT => "mkdirat",
        ring3::nr::SOCKET => "socket",
        ring3::nr::CONNECT => "connect",
        ring3::nr::ACCEPT => "accept",
        ring3::nr::SENDTO => "sendto",
        ring3::nr::RECVFROM => "recvfrom",
        ring3::nr::SENDMSG => "sendmsg",
        ring3::nr::RECVMSG => "recvmsg",
        ring3::nr::SHUTDOWN => "shutdown",
        ring3::nr::BIND => "bind",
        ring3::nr::LISTEN => "listen",
        ring3::nr::GETSOCKNAME => "getsockname",
        ring3::nr::GETPEERNAME => "getpeername",
        ring3::nr::SETSOCKOPT => "setsockopt",
        ring3::nr::GETSOCKOPT => "getsockopt",
        ring3::nr::CLONE => "clone",
        ring3::nr::EXECVE => "execve",
        ring3::nr::EXIT => "exit",
        ring3::nr::WAIT4 => "wait4",
        ring3::nr::KILL => "kill",
        ring3::nr::RT_SIGACTION => "rt_sigaction",
        ring3::nr::RT_SIGPROCMASK => "rt_sigprocmask",
        ring3::nr::PIPE2 => "pipe2",
        ring3::nr::SELECT_PSELECT6 => "select/pselect6",
        ring3::nr::SCHED_YIELD => "sched_yield",
        ring3::nr::SET_TID_ADDRESS => "set_tid_address",
        ring3::nr::GETDENTS64 => "getdents64",
        ring3::nr::GETRANDOM => "getrandom",
        ring3::nr::FACCESSAT => "faccessat",
        ring3::nr::NEWFSTATAT => "newfstatat",
        ring3::nr::CLOCK_GETTIME => "clock_gettime",
        ring3::nr::CLOCK_NANOSLEEP => "clock_nanosleep",
        ring3::nr::EXIT_GROUP => "exit_group",
        ring3::nr::OPENAT => "openat",
        ring3::nr::ARCH_PRCTL => "arch_prctl",
        ring3::nr::GETTID => "gettid",
        ring3::nr::SET_ROBUST_LIST => "set_robust_list",
        ring3::nr::GETUID => "getuid",
        ring3::nr::GETGID => "getgid",
        ring3::nr::GETEUID => "geteuid",
        ring3::nr::GETEGID => "getegid",
        ring3::nr::GETPPID => "getppid",
        ring3::nr::PR_LIMIT64 => "prlimit64",
        ring3::nr::SIGALTSTACK => "sigaltstack",
        ring3::nr::FUTEX => "futex",
        ring3::nr::FCHDIR => "fchdir",
        ring3::nr::CHDIR => "chdir",
        ring3::nr::DUP2 | ring3::nr::DUP3 => "dup2/dup3",
        ring3::nr::WAITPID => "akuma-waitpid",
        ring3::nr::CLOSE_CHILD_STDIN => "akuma-close-child-stdin",
        ring3::nr::UPTIME => "akuma-uptime",
        _ => "?",
    }
}

/// Fill the x86_64 `struct stat` (144 bytes) at the task's `buf`: device
/// numbers and all times zero (one disk, no clock), the rest from the
/// arguments. `Err(errno)` on a bad task pointer.
fn fill_stat64(buf: u64, inode: u64, mode: u32, size: u64, links: u64) -> Result<(), u64> {    if !user_ok(buf, 144) {
        return Err(ring3::err::EFAULT);
    }
    let mut st = [0u8; 144];
    let le = |v: u64| v.to_le_bytes();
    st[0..8].copy_from_slice(&le(0)); // st_dev
    st[8..16].copy_from_slice(&le(inode)); // st_ino
    st[16..24].copy_from_slice(&le(links)); // st_nlink
    st[24..28].copy_from_slice(&mode.to_le_bytes()); // st_mode
    st[28..32].copy_from_slice(&0u32.to_le_bytes()); // st_uid
    st[32..36].copy_from_slice(&0u32.to_le_bytes()); // st_gid
    st[40..48].copy_from_slice(&le(0)); // st_rdev
    st[48..56].copy_from_slice(&le(size)); // st_size
    st[56..64].copy_from_slice(&le(4096)); // st_blksize
    st[64..72].copy_from_slice(&le(size.div_ceil(512))); // st_blocks
    // st_atime/mtime/ctime + nsec (offsets 72..120): zero — no clock, as
    // everywhere in this kernel; the unused tail (120..144) stays zero too.
    if !copy_to_user(buf, &st) {
        return Err(ring3::err::EFAULT);
    }
    Ok(())
}

fn dispatch(fr: &mut ring3::SyscallFrame) -> u64 {
    // TEMP s6b: trace every nested child syscall — ash's exec failure
    // produced no [sc] unhandled line, so see what it actually calls.
    if child::nested_active() {
        akuma_primitives::safe_print!(96, "  [sc.c] {} ({:#x})\n", nr_name(fr.rax), fr.rax);
    }
    match fr.rax {
        ring3::nr::WRITE => {
            let (fd, buf, len) = (fr.arg1(), fr.arg2(), fr.arg3());
            if len > WRITE_LIMIT || !user_ok(buf, len) {
                return e(ring3::err::EFAULT);
            }
            // Stage 4: a nested child's fd 1 is the pipe (the buffer its
            // parent drains through the ChildOut fd); everything else —
            // sshd's own stdout/stderr included — is the console, as before.
            if child::nested_active() && fd == 1 {
                if !child::child_write(buf, len) {
                    return e(ring3::err::EFAULT);
                }
                return len;
            }
            if fd == 1 || fd == 2 {
                if !emit(buf, len) {
                    return e(ring3::err::EFAULT);
                }
                return len;
            }
            // The only other writable kinds: a socket fd (sock.rs answers
            // ENOTSOCK for anything else that is open, EBADF comes from the
            // get below) and — stage 6 — a regular file fd (the ext2 write
            // path; apk unpacks through this).
            let kind = fd_table().with(fd as u32, |d| match d {
                OpenFile::Socket(_) => 1,
                OpenFile::File { .. } => 2,
                _ => 0,
            });
            match kind {
                Some(1) => return sock::write_arm(fd, buf, len),
                Some(2) => return write_file_arm(fd, buf, len),
                _ => {}
            }
            e(ring3::err::EBADF)
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
            // The pipe branch of `write` applies per iovec: musl flushes a
            // buffered stdout as one writev, and a nested child's version
            // string belongs to the ssh session, not the console.
            let piped = child::nested_active() && fd == 1;
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
                if len > WRITE_LIMIT || !user_ok(base, len) {
                    return e(ring3::err::EFAULT);
                }
                if piped {
                    if !child::child_write(base, len) {
                        return e(ring3::err::EFAULT);
                    }
                } else if !emit(base, len) {
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
            // Stage 4: a nested child's exit does NOT reach the reaper — it
            // hands the machine back to the parent (`child.rs`'s model:
            // run-to-completion inside the SPAWN syscall, coroutine-style
            // return). The boot's own sequential tasks still chain through
            // `task_done` exactly as stage 3 left them.
            if child::nested_active() {
                child::child_exit(code)
            }
            // Stage 3: not a halt — the report and the next spawn (or the
            // final gate line) live in `task_done`. This arm never returns to
            // the stub: the task is gone, and its parked `syscall` frame on
            // the syscall stack is abandoned by design (the stub restarts
            // from the stack top on the next task's first syscall).
            task_done(code)
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
                    // Name the gap: an arch code this table does not serve.
                    // (The *syscall-level* checklist arm is at the bottom of
                    // the dispatcher's own match.)
                    akuma_primitives::safe_print!(160, "  [sc] arch_prctl code {}\n", other);
                    e(ring3::err::ENOSYS)
                }
            }
        }
        ring3::nr::SET_TID_ADDRESS => 1, // one task: the tid, and nothing to clear
        ring3::nr::GETPID | ring3::nr::GETTID => u64::from(child::current_pid()),
        ring3::nr::GETPPID => 1, // everything here is the init task's child
        ring3::nr::GETUID | ring3::nr::GETEUID => 0,
        ring3::nr::GETGID | ring3::nr::GETEGID => 0,
        ring3::nr::IOCTL => e(ring3::err::ENOTTY), // the console is a serial line, not a tty
        ring3::nr::MMAP => {
            let (addr, len, prot, flags) = (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4());
            let _ = fr.arg5(); // fd: ignored for MAP_ANONYMOUS
            if len == 0 {
                return e(ring3::err::EINVAL);
            }
            if flags & ring3::mmap::MAP_ANONYMOUS == 0 {
                return e(ring3::err::ENODEV); // no file-backed mappings in this kernel
            }
            // fd is ignored for MAP_ANONYMOUS (Linux semantics): libakuma's
            // mmap wrapper passes fd = 0, and the full kernel accepts it, so
            // demanding -1 here rejected sshd's very first allocation.
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
        ring3::nr::READ => {
            // fd 0: the console has no input path wired, so read(0) is EOF —
            // the honest answer, not a hang. fds 1/2 are write-only. Any
            // other number must name a file fd from `openat`; directories
            // refuse with `EISDIR` the way Linux's read does.
            let (fd, buf, len) = (fr.arg1(), fr.arg2(), fr.arg3());
            if len > WRITE_LIMIT || !user_ok(buf, len) {
                return e(ring3::err::EFAULT);
            }
            if fd == 0 {
                return 0;
            }
            // Stage 4: socket fds read through sock.rs; a spawned child's
            // stdout fd drains its pipe (then EOF).
            if (fd as u32) < SLOT_CEILING {
                let mut slot_idx: usize = usize::MAX;
                let kind = fd_table().with(fd as u32, |d| match d {
                    OpenFile::Socket(_) => 1,
                    OpenFile::ChildOut(slot) => {
                        slot_idx = *slot;
                        2
                    }
                    _ => 0,
                });
                match kind {
                    Some(1) => return sock::read_arm(fd, buf, len),
                    Some(2) => return child::read_child_out(slot_idx, buf, len),
                    _ => {}
                }
            }
            if fd == 1 || fd == 2 || fd as u32 >= SLOT_CEILING {
                return e(ring3::err::EBADF);
            }
            let mut chunk = [0u8; 512];
            let mut res: Result<u64, u64> = Ok(0);
            let found = fd_table().update(fd as u32, |of| match of {
                OpenFile::Dir { .. } => res = Err(ring3::err::EISDIR),
                OpenFile::File { inode, offset, size } => {
                    let mut total = 0u64;
                    while total < len {
                        let want = ((len - total) as usize).min(chunk.len());
                        let have = size.saturating_sub(*offset) as usize;
                        let n = want.min(have);
                        if n == 0 {
                            break;
                        }
                        match root_fs().read_at_by_inode(*inode, *offset as usize, &mut chunk[..n]) {
                            Ok(0) => break,
                            Ok(got) => {
                                if !copy_to_user(buf + total, &chunk[..got]) {
                                    res = Err(ring3::err::EFAULT);
                                    return;
                                }
                                *offset += got as u64;
                                total += got as u64;
                                if got < n {
                                    break; // short read: end of file
                                }
                            }
                            Err(_) => {
                                res = Err(ring3::err::EIO);
                                return;
                            }
                        }
                    }
                    res = Ok(total);
                }
                // Socket and ChildOut fds were served above; the closure's
                // match stays exhaustive so a new kind cannot silently fall
                // through to the file path.
                OpenFile::Socket(_) | OpenFile::ChildOut(_) => {}
            });
            match (found, res) {
                (false, _) => e(ring3::err::EBADF),
                (_, Err(no)) => e(no),
                (_, Ok(n)) => n,
            }
        }
        ring3::nr::STAT => {
            // musl's `stat()` on x86_64: the legacy number, the 144-byte
            // `struct stat`. Everything the ext2 root knows is filled
            // (mode/size/inode/nlink); everything it does not (times — no
            // clock; device numbers — one disk) is zero, the honest value.
            let (path_va, buf) = (fr.arg1(), fr.arg2());
            if !user_ok(buf, 144) {
                return e(ring3::err::EFAULT);
            }
            let mut pbuf = [0u8; 128];
            let Some(path) = user_str(path_va, &mut pbuf) else {
                return e(ring3::err::EFAULT);
            };
            // `stat(2)` follows the final symlink (lstat(6) is the one that
            // names the link itself). ash's PATH search stats each candidate
            // and refuses any that is not a regular file with an execute bit
            // — a link's own S_IFLNK|0777 reads as "not a program", which is
            // how every exec died as `Permission denied` before a fork was
            // ever attempted.
            match root_fs().metadata(&resolve_effective(path)) {
                Ok(md) => match fill_stat64(
                    buf,
                    md.inode,
                    md.mode,
                    md.size,
                    u64::from(md.links.unwrap_or(if md.is_dir { 2 } else { 1 })),
                ) {
                    Ok(()) => 0,
                    Err(no) => e(no),
                },
                Err(_) => e(ring3::err::ENOENT),
            }
        }
        ring3::nr::LSTAT => {
            // `lstat` does not follow a final symlink — and the ext2
            // `metadata` does — so the final component is looked up in its
            // parent's listing instead: the listing carries the kind and
            // size of the link itself. Only the name and kind reach the
            // caller on this stage's paths, but the shape is the real one.
            let (path_va, buf) = (fr.arg1(), fr.arg2());
            if !user_ok(buf, 144) {
                return e(ring3::err::EFAULT);
            }
            let mut pbuf = [0u8; 128];
            let Some(path) = user_str(path_va, &mut pbuf) else {
                return e(ring3::err::EFAULT);
            };
            let Some(idx) = path.rfind('/') else {
                return e(ring3::err::ENOENT);
            };
            let (parent, name) = (&path[..idx.max(1)], &path[idx + 1..]);
            if name.is_empty() {
                // The root itself (or a trailing slash): nothing to not-follow.
                match root_fs().metadata(path) {
                    Ok(md) => match fill_stat64(buf, md.inode, md.mode, md.size, 2) {
                        Ok(()) => 0,
                        Err(no) => e(no),
                    },
                    Err(_) => e(ring3::err::ENOENT),
                }
            } else {
                let fs = root_fs();
                let Ok(entries) = fs.read_dir(parent) else {
                    return e(ring3::err::ENOENT);
                };
                let Some(ent) = entries.iter().find(|e| e.name == name) else {
                    return e(ring3::err::ENOENT);
                };
                let mode = if ent.is_symlink {
                    0o120777 // S_IFLNK | rwxrwxrwx: the convention for links
                } else if ent.is_dir {
                    0o40755
                } else {
                    0o100644
                };
                match fill_stat64(buf, 0, mode, ent.size, if ent.is_dir { 2 } else { 1 }) {
                    Ok(()) => 0,
                    Err(no) => e(no),
                }
            }
        }
        ring3::nr::FCNTL => {
            // The fd-flag commands only; the table owns the cloexec bit.
            let (fd, cmd, arg) = (fr.arg1(), fr.arg2(), fr.arg3());
            match cmd {
                1 => {
                    // F_GETFD
                    if fd <= 2 || fd as u32 >= SLOT_CEILING {
                        0
                    } else {
                        u64::from(fd_table().is_cloexec(fd as u32))
                    }
                }
                2 => {
                    // F_SETFD: FD_CLOEXEC is the one bit in existence.
                    let ok_slot = fd > 2 && (fd as u32) < SLOT_CEILING;
                    if ok_slot && arg & 1 != 0 {
                        fd_table().set_cloexec(fd as u32);
                    }
                    0
                }
                3 => 0, // F_GETFL: O_RDONLY, the only access mode served
                4 => {
                    // F_SETFL: the one flag with a behaviour behind it is
                    // O_NONBLOCK — recorded in the fd table, read by the
                    // socket arms (and accepted silently on a child's
                    // stdout fd, where nonblock is the only mode there is).
                    const O_NONBLOCK: u64 = 0x800;
                    if arg & O_NONBLOCK != 0 {
                        fd_table().set_nonblock(fd as u32);
                    } else {
                        fd_table().clear_nonblock(fd as u32);
                    }
                    0
                }
                _ => e(ring3::err::EINVAL),
            }
        }
        ring3::nr::CLOSE => {
            // fds 0/1/2 are the console and hold no kernel reference to
            // release; closing them is the no-op Linux's stdio-close is.
            let fd = fr.arg1();
            if fd <= 2 {
                return 0;
            }
            if fd as u32 >= SLOT_CEILING {
                return e(ring3::err::EBADF);
            }
            // A socket fd releases its kernel side (the smoltcp handle)
            // before the slot itself goes.
            let is_sock =
                fd_table().with(fd as u32, |d| matches!(d, OpenFile::Socket(_))).unwrap_or(false);
            if is_sock {
                sock::close_arm(fd);
            }
            match fd_table().close(fd as u32) {
                Some(_) => 0,
                None => e(ring3::err::EBADF),
            }
        }
        ring3::nr::OPENAT => {
            // The one way in. Absolute paths only (`AT_FDCWD` only: there is
            // no working directory), read-only access only: the root is
            // read-only in spirit and this kernel has no write path to lie
            // about. The description is an ext2 lookup now — no VFS mount
            // table, one root.
            let (dirfd, path_va, flags) = (fr.arg1(), fr.arg2(), fr.arg3());
            if dirfd != ring3::open::AT_FDCWD {
                return e(ring3::err::EBADF);
            }
            open_common(path_va, flags)
        }
        ring3::nr::OPEN => {
            // musl's `open()` on x86_64: the legacy number, no dirfd.
            let (path_va, flags) = (fr.arg1(), fr.arg2());
            open_common(path_va, flags)
        }
        ring3::nr::GETDENTS64 => {
            // `ls`'s walk: fill `linux_dirent64` records from the open
            // description's cached listing. `d_ino` is 0 — the listing
            // carries names and kinds, not inode numbers, and nothing on
            // this path reads them. `d_off` is the running cursor, a cookie
            // that only has to be monotonic within one open.
            let (fd, buf, len) = (fr.arg1(), fr.arg2(), fr.arg3());
            if len == 0 {
                return e(ring3::err::EINVAL);
            }
            if !user_ok(buf, len) {
                return e(ring3::err::EFAULT);
            }
            if fd as u32 >= SLOT_CEILING {
                return e(ring3::err::EBADF);
            }
            let mut scratch = [0u8; 1024];
            let mut used = 0usize;
            let mut res: Result<(), u64> = Ok(());
            let found = fd_table().update(fd as u32, |of| match of {
                OpenFile::Dir { path: _, entries, cursor } => {
                    let cap = (len as usize).min(scratch.len());
                    while *cursor < entries.len() {
                        let ent = &entries[*cursor];
                        let name = ent.name.as_bytes();
                        let reclen = (19 + name.len() + 1 + 7) & !7;
                        if used + reclen > cap {
                            if used == 0 {
                                res = Err(ring3::err::EINVAL); // not even one fits
                            }
                            break;
                        }
                        let rec = &mut scratch[used..used + reclen];
                        rec[..8].copy_from_slice(&0u64.to_le_bytes());
                        rec[8..16].copy_from_slice(&(*cursor as u64 + 1).to_le_bytes());
                        rec[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
                        rec[18] = if ent.is_dir {
                            4 // DT_DIR
                        } else if ent.is_symlink {
                            10 // DT_LNK
                        } else {
                            8 // DT_REG
                        };
                        rec[19..19 + name.len()].copy_from_slice(name);
                        rec[19 + name.len()] = 0;
                        *cursor += 1;
                        used += reclen;
                    }
                }
                OpenFile::File { .. } | OpenFile::Socket(_) | OpenFile::ChildOut(_) => {
                    res = Err(ring3::err::ENOTDIR);
                }
            });
            if !found {
                return e(ring3::err::EBADF);
            }
            if let Err(no) = res {
                return e(no);
            }
            if used == 0 {
                return 0; // end of directory
            }
            if !copy_to_user(buf, &scratch[..used]) {
                return e(ring3::err::EFAULT);
            }
            used as u64
        }
        ring3::nr::RT_SIGACTION | ring3::nr::RT_SIGPROCMASK => {
            // No signal delivery exists: nothing can ever be delivered, so a
            // handler registration or a mask change is accepted as the no-op
            // it is here. musl's startup installs both before `main` —
            // failing them would fail a program that can never be signalled.
            0
        }
        ring3::nr::CLOCK_GETTIME => {
            // Stage 4: the calibrated TSC gives both clocks real elapsed
            // time (net.rs). CLOCK_REALTIME reports boot-relative seconds —
            // there is still no wall clock behind this kernel, and nothing
            // on this stage's path reads the absolute value (sshd's timers
            // are all deltas); musl's `struct timespec` is two u64s.
            let (clk, tp) = (fr.arg1(), fr.arg2());
            let _ = clk;
            if !user_ok(tp, 16) {
                return e(ring3::err::EFAULT);
            }
            let us = net::uptime_us();
            let mut ts = [0u8; 16];
            ts[0..8].copy_from_slice(&(us / 1_000_000).to_le_bytes());
            ts[8..16].copy_from_slice(&((us % 1_000_000) * 1_000).to_le_bytes());
            if !copy_to_user(tp, &ts) {
                return e(ring3::err::EFAULT);
            }
            0
        }
        ring3::nr::NANOSLEEP | ring3::nr::CLOCK_NANOSLEEP => {
            // The serve loop's tick: `sleep_ms(1)` lands here, and every lap
            // pumps the NIC (net.rs) — this arm is what keeps TCP alive
            // while sshd idles. Both spellings accepted, the same sniff the
            // full kernel makes: `a0` below one page is the raw
            // (seconds, nanos) register pair libakuma's non-linux-abi path
            // sends; anything above is a `*const timespec`.
            let (a0, a1, a2, a3) = (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4());
            let (sec, nanos) = if a0 < 4096 {
                (a0, a1)
            } else {
                let mut ts = [0u8; 16];
                if !user_ok(a0, 16) || !copy_from_user(&mut ts, a0) {
                    return e(ring3::err::EFAULT);
                }
                (
                    u64::from_le_bytes(ts[0..8].try_into().unwrap()),
                    u64::from_le_bytes(ts[8..16].try_into().unwrap()),
                )
            };
            let _ = (a2, a3);
            // Cap: a task may sleep at most 60 s per call. Nothing on this
            // kernel can signal a sleeper awake, so an unbounded request
            // would be an unbounded stall of the whole machine.
            let total_us = sec
                .saturating_mul(1_000_000)
                .saturating_add(nanos / 1_000)
                .min(60_000_000);
            child::sleep_us(total_us);
            0
        }
        ring3::nr::GETRANDOM => {
            // `getrandom(buf, len, flags)` — sshd's key exchange reads this.
            // net.rs's RDRAND-backed fill; a CPU without the instruction
            // gets the stated-weak source and the boot console says so.
            let (buf, len) = (fr.arg1(), fr.arg2());
            if len > WRITE_LIMIT || !user_ok(buf, len) {
                return e(ring3::err::EFAULT);
            }
            let mut tmp = alloc::vec![0u8; len as usize];
            net::rng_fill(&mut tmp);
            if !copy_to_user(buf, &tmp) {
                return e(ring3::err::EFAULT);
            }
            len
        }
        // ---- Stage 6, phase A: apk.static startup ----------------------
        ring3::nr::FSTAT => fstat_fd(fr.arg1(), fr.arg2()),
        ring3::nr::NEWFSTATAT => {
            // musl's stat()/fstatat() on x86_64: the *at number for all of
            // them. Follows the final symlink unless AT_SYMLINK_NOFOLLOW.
            let (dirfd, path_va, buf, flags) = (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4());
            if !user_ok(buf, 144) {
                return e(ring3::err::EFAULT);
            }
            let mut pbuf = [0u8; 128];
            let Some(path) = user_str(path_va, &mut pbuf) else {
                return e(ring3::err::EFAULT);
            };
            if path.is_empty() {
                if flags & ring3::open::AT_EMPTY_PATH != 0 {
                    return fstat_fd(dirfd, buf);
                }
                return e(ring3::err::ENOENT);
            }
            let Some(full) = resolve_at(dirfd, path) else {
                return e(ring3::err::EBADF);
            };
            if flags & ring3::open::AT_SYMLINK_NOFOLLOW != 0 {
                lstat_path(&full, buf)
            } else {
                stat_path(&resolve_effective(&full), buf)
            }
        }
        ring3::nr::LSEEK => {
            let (fd, off, whence) = (fr.arg1(), fr.arg2().cast_signed(), fr.arg3());
            if fd <= 2 || fd as u32 >= SLOT_CEILING {
                return e(ring3::err::EBADF);
            }
            let mut res: Result<u64, u64> = Err(ring3::err::EBADF);
            fd_table().update(fd as u32, |d| match d {
                OpenFile::File { offset, size, .. } => {
                    let base = match whence {
                        0 => 0i64,            // SEEK_SET
                        1 => (*offset).cast_signed(), // SEEK_CUR
                        2 => (*size).cast_signed(),   // SEEK_END
                        _ => {
                            res = Err(ring3::err::EINVAL);
                            return;
                        }
                    };
                    let next = base + off;
                    if next < 0 {
                        res = Err(ring3::err::EINVAL);
                    } else {
                        *offset = next as u64;
                        res = Ok(*offset);
                    }
                }
                OpenFile::Dir { entries, cursor, .. } => {
                    // seekdir/rewinddir shape: only the cursor moves.
                    match whence {
                        0 => *cursor = 0,
                        2 => *cursor = entries.len(),
                        _ => {}
                    }
                    res = Ok(0);
                }
                _ => res = Err(ring3::err::ESPIPE),
            });
            match res {
                Ok(v) => v,
                Err(no) => e(no),
            }
        }
        ring3::nr::UNAME => {
            // struct utsname: six NUL-terminated 65-byte fields. The release
            // string is a plausible Linux one — apk prints it, nothing
            // branches on it.
            let buf = fr.arg1();
            if !user_ok(buf, 65 * 6) {
                return e(ring3::err::EFAULT);
            }
            let fields: [&[u8]; 6] = [
                b"Linux\0",
                b"taoxichuan\0",
                b"6.6.0-akuma\0",
                b"#1 SMP akuma-taoxichuan\0",
                b"x86_64\0",
                b"(none)\0",
            ];
            for (i, f) in fields.iter().enumerate() {
                let mut one = [0u8; 65];
                one[..f.len()].copy_from_slice(f);
                if !copy_to_user(buf + (i as u64) * 65, &one) {
                    return e(ring3::err::EFAULT);
                }
            }
            0
        }
        ring3::nr::STATFS => {
            let mut pbuf = [0u8; 128];
            let Some(path) = user_str(fr.arg1(), &mut pbuf) else {
                return e(ring3::err::EFAULT);
            };
            if root_fs().metadata(&resolve_effective(path)).is_err() {
                return e(ring3::err::ENOENT);
            }
            fill_statfs(fr.arg2())
        }
        ring3::nr::FSTATFS => fill_statfs(fr.arg2()),
        ring3::nr::FACCESSAT => {
            let (dirfd, mode) = (fr.arg1(), fr.arg3());
            if mode > 7 {
                return e(ring3::err::EINVAL);
            }
            // Root, one task, one disk: existence is the whole check.
            path_at(fr.arg2(), dirfd, |p| match root_fs().metadata(p) {
                Ok(_) => 0,
                Err(_) => e(ring3::err::ENOENT),
            })
        }
        ring3::nr::READLINKAT => {
            // No final-component resolution: the ext2 read_symlink walks the
            // link raw, which is exactly lstat-style.
            let (dirfd, buf, bufsz) = (fr.arg1(), fr.arg3(), fr.arg4());
            if bufsz == 0 || !user_ok(buf, bufsz) {
                return e(ring3::err::EFAULT);
            }
            path_at(fr.arg2(), dirfd, |p| match root_fs().read_symlink(p) {
                Ok(t) => {
                    let bytes = t.as_bytes();
                    let n = (bytes.len() as u64).min(bufsz);
                    if !copy_to_user(buf, &bytes[..n as usize]) {
                        return e(ring3::err::EFAULT);
                    }
                    n
                }
                Err(_) => e(ring3::err::EINVAL),
            })
        }
        // ---- Stage 6, phase C: unpack + commit --------------------------
        ring3::nr::MKDIRAT => {
            // mode (arg3) is accepted and ignored: root owns everything.
            path_at(fr.arg2(), fr.arg1(), |p| match root_fs().create_dir(p) {
                Ok(()) => 0,
                Err(no) => e(fs_err(no)),
            })
        }
        ring3::nr::UNLINKAT => {
            let (dirfd, flags) = (fr.arg1(), fr.arg4());
            path_at(fr.arg2(), dirfd, |p| {
                let r = if flags & ring3::open::AT_REMOVEDIR != 0 {
                    root_fs().remove_dir(p)
                } else {
                    root_fs().remove_file(p)
                };
                match r {
                    Ok(()) => 0,
                    Err(no) => e(fs_err(no)),
                }
            })
        }
        ring3::nr::RENAMEAT => {
            let (od, old_va, nd, new_va) = (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4());
            let mut ob = [0u8; 128];
            let mut nb = [0u8; 128];
            let Some(old) = user_str(old_va, &mut ob) else {
                return e(ring3::err::EFAULT);
            };
            let Some(new) = user_str(new_va, &mut nb) else {
                return e(ring3::err::EFAULT);
            };
            let (Some(old), Some(new)) = (resolve_at(od, old), resolve_at(nd, new)) else {
                return e(ring3::err::EBADF);
            };
            match root_fs().rename(&old, &new) {
                Ok(()) => 0,
                Err(no) => e(fs_err(no)),
            }
        }
        ring3::nr::SYMLINKAT => {
            let (target_va, dirfd, link_va) = (fr.arg1(), fr.arg2(), fr.arg3());
            let mut tb = [0u8; 128];
            let mut lb = [0u8; 128];
            let Some(target) = user_str(target_va, &mut tb) else {
                return e(ring3::err::EFAULT);
            };
            let Some(link) = user_str(link_va, &mut lb) else {
                return e(ring3::err::EFAULT);
            };
            let Some(link) = resolve_at(dirfd, link) else {
                return e(ring3::err::EBADF);
            };
            match root_fs().create_symlink(&link, target) {
                Ok(()) => 0,
                Err(no) => e(fs_err(no)),
            }
        }
        ring3::nr::LINKAT => {
            // No hardlink arm on the ext2 trait yet; apk only needs it for
            // packages that ship hard-linked files. The checklist print
            // above will say if this one is real.
            e(ring3::err::ENOSYS)
        }
        ring3::nr::FCHMOD | ring3::nr::FCHMODAT => 0, // root's mode is the mode
        ring3::nr::FCHOWNAT => 0,                    // everything is root already
        ring3::nr::UMASK => 0o022,                   // conventional; no files inherit it
        ring3::nr::FLOCK => 0,                       // one task: nothing contends
        ring3::nr::GETCWD => {
            let (buf, size) = (fr.arg1(), fr.arg2());
            if size < 2 {
                return e(ring3::err::ERANGE);
            }
            if !user_ok(buf, 2) {
                return e(ring3::err::EFAULT);
            }
            if !copy_to_user(buf, b"/\0") {
                return e(ring3::err::EFAULT);
            }
            2
        }
        ring3::nr::DUP => {
            let fd = fr.arg1();
            if fd <= 2 || fd as u32 >= SLOT_CEILING {
                return e(ring3::err::EBADF);
            }
            let desc = fd_table().with(fd as u32, clone_desc);
            let Some(desc) = desc else {
                return e(ring3::err::EBADF);
            };
            let mut table = fd_table();
            let Some(_slot) = table.free_below(3, SLOT_CEILING) else {
                return e(ring3::err::EMFILE);
            };
            u64::from(table.alloc_fd_from(3, desc))
        }
        ring3::nr::MREMAP => {
            // musl's realloc on an mmapped chunk. Shrink keeps the address
            // (the slab never unmaps, so it stays valid); grow moves to a
            // fresh region at the mmap cursor and copies the overlap —
            // MAYMOVE semantics, which the caller accepts by using the
            // returned address. MREMAP_FIXED is refused: no fixed targets.
            let (old, old_sz, new_sz, flags) = (fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4());
            if new_sz == 0 || flags & 2 != 0 {
                return e(ring3::err::EINVAL);
            }
            if old_sz == 0 || !user_ok(old, old_sz) {
                return e(ring3::err::EFAULT);
            }
            if new_sz <= old_sz {
                return old;
            }
            let pages = (new_sz + 0xFFF) & !0xFFF;
            let t = task();
            let base = (t.mmap_cur + 0xF) & !0xF;
            if base + pages > MMAP_BASE + SLAB {
                return e(ring3::err::ENOMEM);
            }
            let mut page = base & !0xFFF;
            while page < base + pages {
                let Some(pa) = akuma_pmm::alloc_page_zeroed() else {
                    return e(ring3::err::ENOMEM);
                };
                if !paging::map_page_in(t.root_pa, page as usize, pa as u64, PteProt::USER_RW, MemAttr::WriteBack) {
                    return e(ring3::err::ENOMEM);
                }
                page += 4096;
            }
            let copy = old_sz.min(new_sz);
            let mut chunk = [0u8; 1024];
            let mut done = 0u64;
            while done < copy {
                let n = ((copy - done) as usize).min(chunk.len());
                if !copy_from_user(&mut chunk[..n], old + done)
                    || !copy_to_user(base + done, &chunk[..n])
                {
                    return e(ring3::err::EFAULT);
                }
                done += n as u64;
            }
            t.mmap_cur = base + pages;
            base
        }
        // Akuma private: libakuma's sshd/sh ask for uptime; the clock arm's
        // elapsed-time source, in microseconds.
        ring3::nr::UPTIME => net::uptime_us(),
        ring3::nr::SOCKET => sock::socket_arm(fr.arg1(), fr.arg2(), fr.arg3()),        ring3::nr::BIND => sock::bind_arm(fr.arg1(), fr.arg2(), fr.arg3()),
        ring3::nr::LISTEN => sock::listen_arm(fr.arg1(), fr.arg2()),
        ring3::nr::ACCEPT => sock::accept_arm(fr.arg1(), fr.arg2(), fr.arg3()),
        ring3::nr::CONNECT => sock::connect_arm(fr.arg1(), fr.arg2(), fr.arg3()),
        ring3::nr::SENDTO => {
            sock::sendto_arm(fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5(), 16)
        }
        ring3::nr::RECVFROM => {
            sock::recvfrom_arm(fr.arg1(), fr.arg2(), fr.arg3(), fr.arg4(), fr.arg5(), fr.arg6())
        }
        ring3::nr::SHUTDOWN => sock::shutdown_arm(fr.arg1(), fr.arg2()),
        ring3::nr::SPAWN => child::spawn_arm(fr),
        ring3::nr::WAITPID => child::waitpid_arm(fr.arg1(), fr.arg2()),
        ring3::nr::CLOSE_CHILD_STDIN => child::close_stdin_arm(fr.arg1()),
        ring3::nr::GETTIMEOFDAY => {
            // Same no-clock contract; struct timeval is two u64s, `tz` is
            // optional and never filled.
            let (tv, tz) = (fr.arg1(), fr.arg2());
            if !user_ok(tv, 16) || !copy_to_user(tv, &[0u8; 16]) {
                return e(ring3::err::EFAULT);
            }
            if tz != 0 && !user_ok(tz, 8) {
                return e(ring3::err::EFAULT);
            }
            0
        }
        ring3::nr::EXECVE => {
            // No fork exists to return to, so in the run-to-completion model
            // an exec from the nested child IS the handoff: ash's tail-call
            // exec of the one command in `sh -c` (busybox skips the fork for
            // a trailing simple command), and every program line the gates
            // run reaches the kernel this way. Outside a nested child there
            // is no task this kernel could replace — say ENOSYS.
            if !child::nested_active() {
                return e(ring3::err::ENOSYS);
            }
            child::exec_replace(fr)
        }
        other => {
            // Name the gap: a guest task hit a number this table does not
            // serve. The name first, then the raw number — each boot reads
            // as a checklist of exactly what is still missing.
            akuma_primitives::safe_print!(160, "  [sc] unhandled {} ({})\n", nr_name(other), other);
            e(ring3::err::ENOSYS)
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 3: the program list, and the sequential kernel-reaped tasks.
// ---------------------------------------------------------------------------

/// Programs the boot will run, one after another, each to `exit`. The gate
/// needs two (busybox echo, busybox ls); the list is the general shape and
/// the command line fills it (`init=`/`initargs=`, then `then=`/`thenargs=`
/// per follow-up). Fixed slots, no heap: the dispatcher's reaper reads these
/// after the task that booted them is gone.
const MAX_PROGRAMS: usize = 4;
/// argv entries per program beyond `argv[0]`, and their byte budget each.
/// The command line is 512 bytes total; 8×95 cannot overflow it.
const MAX_ARGV: usize = 8;
const ARG_LEN: usize = 96;

#[derive(Clone, Copy)]
struct Prog {
    path: [u8; ARG_LEN],
    args: [[u8; ARG_LEN]; MAX_ARGV],
    n_args: usize,
}

impl Prog {
    const fn new() -> Self {
        Self {
            path: [0; ARG_LEN],
            args: [[0; ARG_LEN]; MAX_ARGV],
            n_args: 0,
        }
    }

    /// argv[0], as the string it was stored as.
    fn path_str(&self) -> &str {
        str_until_nul(&self.path)
    }

    /// One argv entry beyond `argv[0]`.
    fn arg_str(&self, i: usize) -> &str {
        str_until_nul(&self.args[i])
    }
}

/// The `&str` a NUL-padded slot holds ("" when unset).
fn str_until_nul(buf: &[u8]) -> &str {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..end]).unwrap_or("")
}

static mut PROGRAMS: [Prog; MAX_PROGRAMS] = [Prog::new(); MAX_PROGRAMS];
static mut N_PROGRAMS: usize = 0;
static mut CURRENT: usize = 0;
static mut EXIT_CODES: [u64; MAX_PROGRAMS] = [0; MAX_PROGRAMS];

/// Fill the program list from the boot command line: `init=`/`initargs=`
/// name the first program (the stage-1/2 token pair, unchanged), each
/// `then=`/`thenargs=` pair appends one. `initargs`/`thenargs` are
/// comma-separated (the stage-1 busybox line's convention,
/// `initargs=sh,/etc/guest.sh`).
fn parse_programs(cmdline: &str) {
    let mut idx = 0usize;
    for token in cmdline.split_ascii_whitespace() {
        let (set_path, set_args) = if let Some(v) = token.strip_prefix("init=") {
            (Some(v), None)
        } else if let Some(v) = token.strip_prefix("initargs=") {
            (None, Some(v))
        } else if let Some(v) = token.strip_prefix("then=") {
            idx += 1;
            (Some(v), None)
        } else if let Some(v) = token.strip_prefix("thenargs=") {
            (None, Some(v))
        } else {
            (None, None)
        };
        // SAFETY: the program statics, boot-phase only — no ring 3 exists
        // while this runs, same discipline as every other boot static.
        let progs = unsafe { &mut *core::ptr::addr_of_mut!(PROGRAMS) };
        if idx >= MAX_PROGRAMS {
            break;
        }
        let p = &mut progs[idx];
        if let Some(v) = set_path {
            store_slot(&mut p.path, v);
        }
        if let Some(v) = set_args {
            for a in v.split(',').take(MAX_ARGV) {
                if p.n_args < MAX_ARGV {
                    store_slot(&mut p.args[p.n_args], a);
                    p.n_args += 1;
                }
            }
        }
    }
    // SAFETY: boot-phase only, see above.
    unsafe {
        N_PROGRAMS = idx + 1;
        CURRENT = 0;
    }
}

fn store_slot(dst: &mut [u8], s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&b[..n]);
    dst[n] = 0;
}

/// Spawn program `i` from the list, printing the spawn-evidence lines the
/// stage-1 gate was pinned on (they are per-program now: two tasks, two spawn
/// lines). Never returns.
fn run_program(i: usize) -> ! {
    // SAFETY: boot statics; `CURRENT` is read by the dispatcher's reaper, and
    // every write here precedes the first crossing of the program it names.
    let (path, n_args) = unsafe {
        CURRENT = i;
        let progs = &*core::ptr::addr_of!(PROGRAMS);
        (progs[i].path_str(), progs[i].n_args)
    };
    console::puts("Taoxichuan: init spawn -- init=");
    console::puts(path);
    if n_args > 0 {
        console::puts(" argv=[");
        // SAFETY: boot statics, see above.
        let args: [&str; MAX_ARGV] = unsafe {
            let progs = &*core::ptr::addr_of!(PROGRAMS);
            let mut a = [""; MAX_ARGV];
            for (k, slot) in a.iter_mut().enumerate() {
                if k < n_args {
                    *slot = progs[i].arg_str(k);
                }
            }
            a
        };
        for (k, a) in args.iter().enumerate() {
            if k >= n_args {
                break;
            }
            if k > 0 {
                console::puts(",");
            }
            console::puts(a);
        }
        console::puts("]");
    }
    console::puts("\n");

    let fs = root_fs();
    if !fs.exists(path) {
        console::puts("  init: NOT on the mounted root\n");
        halt();
    }
    let image = fs.read_file(path).unwrap_or_default();
    let elf = image.len() >= 4 && image[..4] == [0x7f, b'E', b'L', b'F'];
    console::puts("  init: ");
    console::put_dec(image.len() as u64);
    console::puts(" bytes on the ext2 root, ELF magic ");
    console::puts(if elf { "ok" } else { "MISSING" });
    console::puts("\n");
    console::puts("Taoxichuan: stage 1 gate reached -- init spawn (userspace is stage 2)\n");

    spawn(path, &image)
}

/// The dispatcher's reaper, in place of stage 2's `halt()`: report the exit,
/// then either spawn the next program or close the boot out. Runs on the
/// syscall stack inside the dying task's dispatcher frame; the stack the
/// stub parked is abandoned with the task (the stub restarts at its top on
/// the next task's first syscall).
fn task_done(code: u64) -> ! {
    // SAFETY: boot statics; single core, IF clear, no other reader.
    let (i, n, path) = unsafe {
        EXIT_CODES[CURRENT] = code;
        let progs = &*core::ptr::addr_of!(PROGRAMS);
        (CURRENT, N_PROGRAMS, progs[CURRENT].path_str())
    };
    // The stage-2 gate's own evidence lines, verbatim, on the boot that
    // re-checks that gate; every other program gets the generic line.
    if path == "/bin/hello" {
        console::puts("Taoxichuan: hello exit code ");
        console::put_dec(code);
        console::puts("\n");
        if code == 0 {
            console::puts("Taoxichuan: stage 2 gate reached -- ring-3 ran, exited clean, output above\n");
        }
    } else {
        console::puts("Taoxichuan: exit code ");
        console::put_dec(code);
        console::puts(" from ");
        console::puts(path);
        console::puts("\n");
    }
    if i + 1 < n {
        run_program(i + 1);
    }
    let all_zero = unsafe { EXIT_CODES[..n].iter().all(|&c| c == 0) };
    if n >= 2 && all_zero {
        // The stage-3 gate's own line: two kernel-reaped tasks, both clean.
        console::puts("Taoxichuan: stage 3 gate reached -- sequential tasks ran clean, outputs above\n");
    }
    console::puts("Taoxichuan: all programs done\n");
    halt();
}
/// Load an ELF image into the CURRENT task slot: fresh page root (the
/// kernel's upper half shared by PML4 copy), every `PT_LOAD` placed eagerly
/// at its link address, a Linux initial stack with argc/argv/envp/auxv on
/// it, heap slabs registered, the fd table reset. Returns where to cross:
/// the root to load into CR3, the entry, the initial `rsp`.
///
/// Shared by the boot spawn (`spawn`) and the SPAWN syscall's child
/// (`child.rs`) — one loader, one placement story. Runs with the kernel
/// half live (the physmap does all the copying); the caller chooses the
/// CR3 moment.
fn load_task(argv: &[&str], env: &[&str], image: &[u8]) -> Option<TaskImage> {
    let img = ring3::Image::parse(image).ok()?;
    let t = task();
    t.fd_names = fdtable::Names::new();
    t.fd_cloexec = fdtable::FlagSet::new();
    t.fd_nonblock = fdtable::FlagSet::new();
    t.exit = None;
    t.mmap_cur = MMAP_BASE;
    // A fresh address space, a fresh region list: `user_ok` checks against
    // exactly what THIS image mapped. Without the reset the 16 region slots
    // fill across spawns and an exec would panic on the append.
    t.n_regions = 0;

    // The task's root: a fresh page table whose upper half is the kernel's
    // (one PML4 copy shares every kernel subtree — physmap, device window,
    // kernel image), whose lower half holds only the task. The kernel's own
    // root keeps its identity map; the task's root simply has none.
    let root_page = akuma_pmm::alloc_page_zeroed()?;
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
            let pa = akuma_pmm::alloc_page_zeroed()?;
            if !paging::map_page_in(root, pva as usize, pa as u64, pte, MemAttr::WriteBack) {
                return None;
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
        let pa = akuma_pmm::alloc_page_zeroed()?;
        if pva + 4096 == USER_STACK_TOP {
            stack_top_pa = pa as u64;
        }
        if !paging::map_page_in(root, pva as usize, pa as u64, PteProt::USER_RW, MemAttr::WriteBack) {
            return None;
        }
        pva += 4096;
    }
    add_region(stack_base, USER_STACK_TOP);

    // argc/argv/envp/auxv. `AT_RANDOM`/`AT_EXECFN` values are the builder's
    // to fill.
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
    let mut full_argv: [&str; MAX_ARGV + 1] = [argv.first().copied().unwrap_or(""); MAX_ARGV + 1];
    let n_args = argv.len().min(MAX_ARGV);
    for (k, a) in argv[..n_args].iter().enumerate() {
        full_argv[k] = a;
    }
    let mut block = [0u8; 512];
    let rsp = ring3::build_stack(&mut block, USER_STACK_TOP, &full_argv[..=n_args], env, &aux);

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

    let _ = placed;
    Some(TaskImage { root_pa: root, entry: img.entry(), rsp })
}

/// Where a loaded task starts: the root for CR3, the entry, the stack.
pub struct TaskImage {
    pub root_pa: u64,
    pub entry: u64,
    pub rsp: u64,
}

/// Load the image, build the task's address space, and cross. Never returns:
/// the task ends in the dispatcher's `exit` or the trap handler's `[Fault]`.
fn spawn(path: &str, image: &[u8]) -> ! {
    if image.len() < 4 || image[..4] != [0x7f, b'E', b'L', b'F'] {
        akuma_primitives::safe_print!(160, "  init: ELF refused: bad magic\n");
        halt();
    }
    // SAFETY: boot statics; read before the crossing that consumes the copy.
    let (argv, n_args) = unsafe {
        let progs = &*core::ptr::addr_of!(PROGRAMS);
        let cur = CURRENT;
        let p = &progs[cur];
        let mut a: [&str; MAX_ARGV + 1] = [path; MAX_ARGV + 1];
        for k in 0..p.n_args {
            a[k + 1] = p.arg_str(k);
        }
        let n = p.n_args;
        let mut owned: [&str; MAX_ARGV + 1] = [path; MAX_ARGV + 1];
        owned[1..=n].copy_from_slice(&a[1..=n]);
        (owned, n)
    };
    let Some(ti) = load_task(&argv[..=n_args], &[], image) else {
        akuma_primitives::safe_print!(160, "  init: task load failed\n");
        halt();
    };

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
            r = in(reg) ti.root_pa,
            options(nostack, preserves_flags)
        );
    }
    let ef = ring3::EntryFrame {
        r15: 0, r14: 0, r13: 0, r12: 0, rbp: 0, rbx: 0, r10: 0, r8: 0, r9: 0,
        rdx: 0, rsi: 0, rdi: 0, rax: 0,
        rip: ti.entry,
        rflags: 0x202, // IF set, reserved bit 1 set
        rsp: ti.rsp,
    };
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
    // Publish the mount for the dispatcher's fd syscalls (`ROOT_FS` above);
    // from here on the local binding is dead.
    // SAFETY: boot phase, one core, no ring 3 yet.
    unsafe {
        *core::ptr::addr_of_mut!(ROOT_FS) = Some(fs);
    }

    // Stage 4: the network path — calibrate the TSC against the PIT, pull up
    // smoltcp on virtio-net, wait (bounded, pumping the NIC) for DHCP. A
    // guest with no NIC or no lease still boots; sshd will bind and simply
    // never answer, which is a legible failure, not a hang.
    let _ = net::net_init();

    // ---- The init-spawn point ------------------------------------------------
    //
    // The full kernel resolves `init=` against the root, loads the ELF, builds
    // the address space and enters user mode. Stage 1's gate is *reaching*
    // this point on a mounted root: the banner goes here (the full kernel
    // prints its own in exactly this position, right before `run_init` starts
    // init), and the spawn's evidence is that init's bytes are on the disk and
    // are an ELF. Loading and entering user mode is stage 2's line.
    banner();

    let mut init_buf = [0u8; 512];
    let cmdline_str = cmdline(hvm_start_info, &mut init_buf);
    if cmdline_token(hvm_start_info, "init=", &mut [0u8; 128]).is_none() {
        console::puts("  init: no init= on the command line -- nothing to spawn\n");
        console::puts("Taoxichuan: stage 1 gate NOT reached (no init)\n");
        halt();
    }
    // Stage 3: the whole program list comes off the command line here
    // (`init=`/`initargs=` for the first program, `then=`/`thenargs=` for
    // each follow-up), so the dispatcher's reaper never parses anything
    // after a task is live.
    parse_programs(cmdline_str);

    // Stage 2: the spawn is real now. Load, cross, and the task's own exit
    // (or its fault) is the next line on this console; stage 3 chains the
    // next program from there.
    run_program(0);
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
