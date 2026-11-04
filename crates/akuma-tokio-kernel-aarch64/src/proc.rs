//! Processes, the machine's half: init's first instruction, a fork child's
//! and an exec'd image's crossing into EL0, and the user faults that end a
//! process. The slot ring, fork, `execve`'s load, exit and the root ownership
//! are `akuma_tokio_kernel_core::proc`'s, shared with amd64.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

use akuma_exec::process::UserContext;
use akuma_primitives::safe_print;
use akuma_tokio_kernel_core::loader::TaskImage;
use akuma_tokio_kernel_core::proc;
use akuma_vectors_aarch64::{Frame, SPSR_EL0};

use crate::mm::Aarch64;
use crate::sched;

static INIT_TID: AtomicUsize = AtomicUsize::new(usize::MAX);
static INIT_ENTRY: AtomicU64 = AtomicU64::new(0);
static INIT_SP: AtomicU64 = AtomicU64::new(0);

/// The ext2 inode behind `fd` in the caller's descriptor table, if it names a
/// regular file: the pin `openat` recorded, or the path resolved now.
#[must_use]
pub fn fd_inode(fd: u64) -> Option<u32> {
    use akuma_exec::process::FileDescriptor;
    let kf = akuma_exec::process::current_process_shared()?.with_fd(fd as u32, |d| match d {
        FileDescriptor::File(f) => Some(f.clone()),
        _ => None,
    })??;
    let fs = crate::fs::root_fs();
    let inode = if kf.inode() != 0 { kf.inode() } else { fs.resolve_inode(&kf.path).ok()? };
    let md = fs.metadata_by_inode(inode).ok()?;
    (!md.is_dir).then_some(inode)
}

/// Cross into EL0 at `f` on the running slot: its root live, the syscall path
/// on its task record, traps landing at the top of its kernel stack.
fn cross(root: u64, f: &Frame) -> ! {
    let cur = sched::current_slot();
    crate::mm::set_task_slot(proc::sched_task_slot_of(cur));
    if !akuma_boot_aarch64::user::switch_to(root) {
        safe_print!(96, "[exec] slot {} root {:#x} lacks the kernel's part\n", cur, root);
        crate::halt();
    }
    let Some(top) = sched::kstack_top(cur) else {
        safe_print!(64, "[exec] slot {} has no kernel stack\n", cur);
        crate::halt();
    };
    akuma_vectors_aarch64::enter_user(f, top)
}

/// Init's first instruction on its own slot: name the slot, then cross into
/// the image's entry.
extern "C" fn init_entry() -> ! {
    let tid = INIT_TID.load(Relaxed);
    sched::set_current_slot(tid);
    let mut f = Frame::zeroed();
    f.elr = INIT_ENTRY.load(Relaxed);
    f.sp = INIT_SP.load(Relaxed);
    f.spsr = SPSR_EL0;
    cross(proc::sched_root_of::<Aarch64>(tid), &f)
}

/// `ExecRuntime::enter_user`: a fork child's or a new thread's first and only
/// crossing, from the context `fork_process`/`clone_thread` wrote, with the
/// creator's FP state. The task record, not the bind, names the root.
pub fn enter_user(ctx: &UserContext) -> ! {
    let cur = sched::current_slot();
    let f = sched::frame_from(cur, ctx);
    cross(proc::sched_root_of::<Aarch64>(cur), &f)
}

/// `execve` for a ring task: core loads the new image, then the crossing with
/// a clean register file (no TLS base, no FP state from the old image).
#[must_use]
pub fn exec(path: u64, argv: u64, envp: u64) -> u64 {
    if !proc::is_real_task::<Aarch64>() {
        return 0u64.wrapping_sub(38);
    }
    let ti = match proc::exec::<Aarch64>(path, argv, envp) {
        Ok(ti) => ti,
        Err(r) => return r,
    };
    let mut f = Frame::zeroed();
    f.elr = ti.entry;
    f.sp = ti.sp;
    f.spsr = SPSR_EL0;
    cross(ti.root_pa, &f)
}

/// Start the image the boot thread loaded into its record as pid 1, then wait
/// for it and power off with its status. Never returns.
pub fn start_init(path: &str, ti: &TaskImage) -> ! {
    let Some(tid) = proc::bind_init::<Aarch64>(path, ti) else {
        safe_print!(64, "  [init] no pool slot for init\n");
        crate::halt();
    };
    INIT_TID.store(tid, Relaxed);
    INIT_ENTRY.store(ti.entry, Relaxed);
    INIT_SP.store(ti.sp, Relaxed);
    let Some(top) = sched::prepare(tid) else {
        safe_print!(64, "  [init] no kernel stack for init\n");
        crate::halt();
    };
    akuma_threading::x86_seed_entry(tid, top, init_entry);
    // The boot thread becomes slot 0: it is about to block, and a thread the
    // pool has never heard of has no slot to park.
    let _ = sched::prepare(0);
    akuma_threading::x86_adopt_running_thread(0);
    sched::set_current_slot(0);
    akuma_threading::x86_publish(tid);
    safe_print!(64, "[init] pid 1 on pool slot {}\n", tid);
    let code = proc::wait_init::<Aarch64>();
    safe_print!(96, "[init] {} exited with {}\n", path, code);
    crate::power_off()
}

/// A fault from EL0 nothing could resolve: the process dies as SIGSEGV.
pub fn die_segv() -> ! {
    if proc::is_real_task::<Aarch64>() {
        proc::die_segv::<Aarch64>()
    }
    crate::halt()
}
