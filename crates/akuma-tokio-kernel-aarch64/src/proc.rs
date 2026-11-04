//! Init: the one boot program, run as pid 1 on a pool thread of its own.
//!
//! The boot thread loads the image into its task record (`core::loader`), hands
//! the record to init's slot, registers the `Process` the shared paths look for
//! (`Process::register_adopted`), seeds the slot to cross into EL0 and parks. Init's
//! exit wakes it to report and power off. The amd64 half's `child::start_init_thread`,
//! for this machine; fork, `execve` and `wait4` are the process step (A5.6c).

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};

use akuma_primitives::safe_print;
use akuma_tokio_kernel_core::loader::TaskImage;
use akuma_vectors_aarch64::{Frame, SPSR_EL0};

use crate::sched;

static INIT_TID: AtomicUsize = AtomicUsize::new(usize::MAX);
static INIT_ENTRY: AtomicU64 = AtomicU64::new(0);
static INIT_SP: AtomicU64 = AtomicU64::new(0);
/// Init's exit code as reported (`128 + signal` for a death by signal), or [`RUNNING`].
static INIT_EXIT: AtomicU64 = AtomicU64::new(RUNNING);
const RUNNING: u64 = u64::MAX;

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

/// Init's first instruction on its own slot: name the slot, then cross into
/// EL0 at the image's entry, traps landing at the top of the slot's stack.
extern "C" fn init_entry() -> ! {
    let tid = INIT_TID.load(Relaxed);
    sched::set_current_slot(tid);
    crate::mm::set_task_slot(sched::task_of(tid));
    let mut f = Frame::zeroed();
    f.elr = INIT_ENTRY.load(Relaxed);
    f.sp = INIT_SP.load(Relaxed);
    f.spsr = SPSR_EL0;
    let Some(top) = sched::kstack_top(tid) else {
        safe_print!(64, "[init] slot {} has no kernel stack\n", tid);
        crate::halt();
    };
    akuma_vectors_aarch64::enter_user(&f, top)
}

/// Start the image the boot thread just loaded into record 0 as pid 1, then
/// wait for it. Never returns.
pub fn start_init(path: &str, ti: &TaskImage) -> ! {
    let Some(tid) = akuma_threading::x86_claim_slot().filter(|&t| t < crate::mm::NSLOTS) else {
        safe_print!(64, "  [init] no pool slot for init\n");
        crate::halt();
    };
    // The record moves to the slot's own (`tid + 1`); the boot record keeps none.
    let record = tid + 1;
    let layout = crate::mm::with_task_slot(0, |t| t.layout.fork());
    crate::mm::with_task_slot(record, |t| {
        t.root_pa = ti.root_pa;
        t.layout = layout;
        t.exit = None;
    });
    crate::mm::with_task_slot(0, |t| t.root_pa = 0);
    sched::bind(tid, 1, record);
    if !akuma_exec::process::Process::register_adopted(1, 0, path, ti.root_pa as usize, ti.entry as usize, ti.sp as usize, tid) {
        safe_print!(64, "  [init] no root view for the process table\n");
        crate::halt();
    }
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
    loop {
        let code = INIT_EXIT.load(Acquire);
        if code != RUNNING {
            safe_print!(96, "[init] {} exited with {}\n", path, code);
            crate::power_off();
        }
        akuma_threading::schedule_blocking(crate::fs::uptime_us() + 10_000);
    }
}

/// `exit`/`exit_group` (one thread per process until A5.6c): the process
/// becomes a zombie with `code`, its descriptors close, and init's exit wakes
/// the boot thread. The thread is finished for good.
pub fn exit(code: u64) -> ! {
    let slot = sched::current_slot();
    let code = code as i32;
    if let Some(p) = akuma_exec::process::current_process_shared() {
        p.exit_code.store(code, Relaxed);
        p.state.store(akuma_exec::process::ProcessState::Zombie(code));
        p.exited.store(true, Release);
        p.fds.close_all();
    }
    if slot == INIT_TID.load(Relaxed) {
        let shown = if code < 0 { 128 + u64::from(code.unsigned_abs()) } else { code as u64 };
        INIT_EXIT.store(shown, Release);
        akuma_threading::get_waker_for_thread(0).wake();
    }
    akuma_threading::x86_finish_current()
}
