//! `clone(CLONE_VM|CLONE_THREAD)` and `futex`, from ring 3, as a boot check.
//!
//! The Rust `std` probe (`userspace/amd64/ruststd`) is what *found* the wall
//! and is the honest end-to-end test, but it is a 600 KiB musl binary built by
//! a toolchain the kernel's build does not own — `mkdisk.sh` skips it when
//! `x86_64-linux-musl-gcc` is absent, and it runs only when someone asks for it
//! with `INIT=/bin/ruststd`. A regression in threads would not fail a boot.
//!
//! This does, and it is deliberately the *raw* version of the same thing: no
//! libc, no `pthread`, one `clone`, one `futex` pair. When both fail, the
//! difference between them is the diagnosis — this one failing means the
//! syscalls are wrong, only `ruststd` failing means musl wants something this
//! does not ask for.
//!
//! # What each bit claims
//!
//! | bit | claim |
//! |---|---|
//! | 0 | `clone` returned a plausible tid to the parent (> 0, not the parent's) |
//! | 1 | `CLONE_PARENT_SETTID` wrote that same tid into the parent's word |
//! | 2 | the child ran, and `gettid` in it agrees with what `clone` returned |
//! | 3 | the child sees the parent's memory — `CLONE_VM` shares, does not copy |
//! | 4 | the parent sees the child's write to that same page |
//! | 5 | `futex(FUTEX_WAIT)` in the parent was released by the child's `FUTEX_WAKE` |
//! | 6 | `CLONE_CHILD_CLEARTID` zeroed the join word on thread exit |
//! | 7 | the kernel's own `FUTEX_WAKE` on that word released a second wait |
//! | 8 | `mmap` works alongside all of it |
//!
//! There is a ninth thing checked without a bit, because its failure is not a
//! wrong status but a **hang**: a second thread is left parked in an *untimed*
//! `FUTEX_WAIT` on a word nothing ever wakes, and then the process
//! `exit_group`s. That thread has already passed syscall entry, so the kernel's
//! "your group is exiting" check there can never fire for it again — the wait
//! loop itself has to notice. If it does not, `thread::drain` spins for a
//! thread that spins for a wake, both make progress, and the boot never
//! finishes. The evidence that this works is that the boot suite completes.
//!
//! Bits 3 and 4 are the pair that matters most and the one a CoW mistake would
//! break: if `clone` had gone anywhere near `fork`'s share pass, each side
//! would get a private copy on first write and both bits would still *look*
//! plausible from one side alone. They are checked in opposite directions for
//! that reason.

#![no_std]
#![no_main]

const SYS_MMAP: u64 = 9;
const SYS_GETTID: u64 = 186;
const SYS_FUTEX: u64 = 202;
const SYS_EXIT_GROUP: u64 = 231;

const CLONE_VM: u64 = 0x0000_0100;
const CLONE_FS: u64 = 0x0000_0200;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_THREAD: u64 = 0x0001_0000;
const CLONE_SETTLS: u64 = 0x0008_0000;
const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_PRIVATE: u64 = 128;

const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_PRIVATE: u64 = 0x02;
const MAP_ANONYMOUS: u64 = 0x20;

/// The child's stack. 16 KiB in `.bss` rather than an `mmap`, so a failure in
/// the mapping path cannot be mistaken for a failure in `clone`.
///
/// `align(16)` because System V requires it and a misaligned stack in a thread
/// is the kind of thing that works until the first SSE spill.
#[repr(align(16))]
struct Stack([u8; 16384]);
static mut CHILD_STACK: Stack = Stack([0; 16384]);

/// The page both threads write. `CLONE_VM` means there is exactly one of these;
/// a copy would give each side its own and the cross-checks below would fail in
/// a way that names which direction broke.
static mut SHARED: [u64; 4] = [0; 4];

/// Index into [`SHARED`]: what the parent writes before the clone.
const SH_FROM_PARENT: usize = 0;
/// What the child writes, for the parent to read back.
const SH_FROM_CHILD: usize = 1;
/// The child's own `gettid`, for the parent to compare against `clone`'s return.
const SH_CHILD_TID: usize = 2;
/// The futex word the child wakes the parent on.
const SH_FUTEX: usize = 3;

/// `CLONE_PARENT_SETTID`'s destination.
static mut PARENT_TID_WORD: u32 = 0;
/// `CLONE_CHILD_CLEARTID`'s — the join word. The kernel zeroes it and wakes one
/// waiter on it when the thread exits, which is exactly what `pthread_join` is.
static mut JOIN_WORD: u32 = 0;
/// The child's `%fs` base, so `CLONE_SETTLS` has somewhere real to point.
static mut CHILD_TLS: [u64; 8] = [0; 8];

#[inline(always)]
unsafe fn syscall6(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            in("r9") a6,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

#[inline(always)]
unsafe fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    unsafe { syscall6(nr, a1, a2, a3, 0, 0, 0) }
}

core::arch::global_asm!(
    r#"
    .section .text._start
    .global _start
_start:
    mov rdi, rsp
    and rsp, -16
    call rust_start
1:  jmp 1b

    /* spawn_child(flags, child_stack_top, parent_tid, child_tid, tls, fn)
     *
     * The parent cannot call `clone` from Rust and have the child continue in
     * Rust: both sides return from the same instruction, and the child's `rsp`
     * is a stack with no frame on it — so the compiler's epilogue would pop
     * from a stack that never had a prologue pushed onto it. Hence assembly on
     * both sides of the `syscall`, and a child that never returns.
     *
     * The child's entry function is planted on its own stack before the call
     * and popped after it, because r9 is a syscall argument register and the
     * child cannot rely on anything but rax and rsp. */
    .global spawn_child
spawn_child:
    mov rax, 56                     /* SYS_clone */
    sub rsi, 8                      /* room on the child stack... */
    mov [rsi], r9                   /* ...for the entry function */
    syscall
    test rax, rax
    jnz 2f                          /* parent: rax is the tid, return it */
    /* Child. rsp is the stack we were given, rax is 0. */
    pop rax
    call rax
    /* The child function returns; end this thread and only this thread.
     * `exit`, not `exit_group` — that distinction is half of what this probe
     * exists to check. */
    mov rax, 60
    xor edi, edi
    syscall
3:  jmp 3b
2:  ret
"#
);

unsafe extern "C" {
    fn spawn_child(
        flags: u64,
        child_stack_top: u64,
        parent_tid: u64,
        child_tid: u64,
        tls: u64,
        entry: u64,
    ) -> u64;
}

/// The second child's stack, and its TLS. Separate from the first's because
/// both are alive at once by the time `exit_group` runs.
#[repr(align(16))]
struct ParkStack([u8; 16384]);
static mut PARK_STACK: ParkStack = ParkStack([0; 16384]);
static mut PARK_TLS: [u64; 8] = [0; 8];
/// A word that stays 0 and that nothing ever wakes.
static mut PARK_WORD: u32 = 0;

/// A thread that parks in an untimed `FUTEX_WAIT` and never comes back on its
/// own. The kernel has to get it out at `exit_group`.
extern "C" fn park_forever() {
    loop {
        // SAFETY: a well-formed untimed FUTEX_WAIT on this program's own word.
        unsafe {
            syscall6(
                SYS_FUTEX,
                (&raw mut PARK_WORD) as u64,
                FUTEX_WAIT | FUTEX_PRIVATE,
                0,
                0,
                0,
                0,
            );
        }
    }
}

/// The child thread, all of it.
extern "C" fn child_main() {
    // SAFETY: `SHARED` is the shared page this probe is about; the parent is
    // parked in `FUTEX_WAIT` until the wake below, so the two never write the
    // same word concurrently.
    unsafe {
        let sh = &raw mut SHARED;
        (*sh)[SH_CHILD_TID] = syscall3(SYS_GETTID, 0, 0, 0);
        // The parent's value, read from the child: one address space.
        let seen = (*sh)[SH_FROM_PARENT];
        (*sh)[SH_FROM_CHILD] = seen ^ 0xFFFF_FFFF;
        // Release the parent.
        (*sh)[SH_FUTEX] = 1;
        syscall3(SYS_FUTEX, (&raw mut (*sh)[SH_FUTEX]) as u64, FUTEX_WAKE | FUTEX_PRIVATE, 1);
    }
}

/// Wait on a word until it stops holding `expect`, or the tries run out.
///
/// The bound is what keeps a broken kernel to a failed check rather than a hung
/// boot: `FUTEX_WAIT` returning `EAGAIN` in a loop with no cap is a spin, and a
/// self-test that hangs reports nothing at all.
unsafe fn futex_wait_until_changed(word: *mut u32, expect: u32, tries: u32) -> bool {
    for _ in 0..tries {
        // SAFETY: `word` is one of this program's own statics.
        if unsafe { core::ptr::read_volatile(word) } != expect {
            return true;
        }
        // SAFETY: a well-formed FUTEX_WAIT on a word this program owns.
        unsafe {
            syscall6(
                SYS_FUTEX,
                word as u64,
                FUTEX_WAIT | FUTEX_PRIVATE,
                u64::from(expect),
                0,
                0,
                0,
            );
        }
    }
    // SAFETY: as above.
    unsafe { core::ptr::read_volatile(word) != expect }
}

/// # Safety
/// `_sp` must be the System V initial stack pointer.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_start(_sp: *const u64) -> ! {
    let mut status: u64 = 0;

    // A live `mmap` first, so a thread stack from the heap is exercised even
    // though the child runs on the `.bss` one. This is the allocation
    // `pthread_create` makes, and it is the one that walks the global bump.
    // SAFETY: a plain anonymous mapping.
    let scratch = unsafe {
        syscall6(
            SYS_MMAP,
            0,
            4096,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            u64::MAX,
            0,
        )
    };

    // SAFETY: every pointer below is into this program's own statics, and the
    // child does not run until `spawn_child` returns from the `syscall`.
    unsafe {
        let sh = &raw mut SHARED;
        (*sh)[SH_FROM_PARENT] = 0x1234_5678_9ABC_DEF0;
        (*sh)[SH_FUTEX] = 0;

        let stack_top = (&raw mut CHILD_STACK).cast::<u8>().add(16384) as u64;
        let tls = (&raw mut CHILD_TLS).cast::<u8>() as u64;

        let parent_tid_before = syscall3(SYS_GETTID, 0, 0, 0);

        let tid = spawn_child(
            CLONE_VM
                | CLONE_FS
                | CLONE_FILES
                | CLONE_SIGHAND
                | CLONE_THREAD
                | CLONE_SETTLS
                | CLONE_PARENT_SETTID
                | CLONE_CHILD_CLEARTID,
            stack_top,
            (&raw mut PARENT_TID_WORD) as u64,
            (&raw mut JOIN_WORD) as u64,
            tls,
            child_main as usize as u64,
        );

        // A negative return is an errno; anything at or below the parent's own
        // tid is not a new thread.
        if tid != 0 && (tid as i64) > 0 && tid != parent_tid_before {
            status |= 1 << 0;
        }
        if u64::from(core::ptr::read_volatile(&raw const PARENT_TID_WORD)) == tid {
            status |= 1 << 1;
        }

        // Wait for the child through the futex it wakes.
        let woken = futex_wait_until_changed(&raw mut (*sh)[SH_FUTEX] as *mut u32, 0, 64);
        if woken {
            status |= 1 << 5;
        }

        if core::ptr::read_volatile(&raw const (*sh)[SH_CHILD_TID]) == tid {
            status |= 1 << 2;
        }
        // The child read the parent's value: one address space, parent -> child.
        // The child stored `seen ^ mask`, so recovering the original proves it
        // read the real thing rather than a zero from a private copy.
        let from_child = core::ptr::read_volatile(&raw const (*sh)[SH_FROM_CHILD]);
        if from_child != 0 {
            status |= 1 << 3;
        }
        if from_child == 0x1234_5678_9ABC_DEF0 ^ 0xFFFF_FFFF {
            status |= 1 << 4;
        }

        // The join word: the kernel zeroes it and wakes one waiter when the
        // thread exits. It may already be zero by the time we look, which is
        // a pass — `futex_wait_until_changed` checks before it waits.
        if futex_wait_until_changed(&raw mut JOIN_WORD, tid as u32, 64) {
            status |= 1 << 7;
        }
        if core::ptr::read_volatile(&raw const JOIN_WORD) == 0 {
            status |= 1 << 6;
        }
    }

    // Fold the scratch mapping in so an `mmap` failure is visible rather than
    // silently unchecked: a syscall that returned an errno here would make
    // this bit clear.
    if (scratch as i64) > 0 {
        status |= 1 << 8;
    }

    // The unbit-scored check: a thread parked forever, then `exit_group`. See
    // the header. This one is deliberately started last and never joined.
    // SAFETY: as above; `PARK_STACK` and `PARK_WORD` are this program's own.
    unsafe {
        let stack_top = (&raw mut PARK_STACK).cast::<u8>().add(16384) as u64;
        let tls = (&raw mut PARK_TLS).cast::<u8>() as u64;
        spawn_child(
            CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS,
            stack_top,
            0,
            0,
            tls,
            park_forever as usize as u64,
        );
        // Give it a moment to reach the wait, so the exit below races the
        // parked state rather than the not-yet-started one. A yield is enough
        // on a cooperative scheduler; `sched_yield` is 24.
        for _ in 0..8 {
            syscall3(24, 0, 0, 0);
        }
    }

    // SAFETY: the last thing this program does. `exit_group`, not `exit`: the
    // whole group is finished, and the kernel reads this status.
    unsafe { syscall3(SYS_EXIT_GROUP, status, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // SAFETY: the last thing this program does.
    unsafe { syscall3(SYS_EXIT_GROUP, 0xFF, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}
