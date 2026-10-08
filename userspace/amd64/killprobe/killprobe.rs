//! A process to be killed: threads parked where Chromium's park, and one that
//! never makes a syscall, so `usermode::kill_test` can send it `SIGKILL` from
//! the boot thread and watch how the kernel takes the group down.
//!
//! Same raw shape as `threadprobe` (no libc, one `clone` per thread) and the
//! same reason: a regression here must fail a *boot*, not a probe someone has
//! to remember to run. What the kernel test asserts is on the kernel side —
//! the group is gone inside a bounded number of yields, every `THREADS` row
//! came back, no thread was hard-terminated by a peer, the status is `-9`, and
//! the address space is fully freed — so this program's only job is to get
//! into the state and stay there:
//!
//! * two threads in an untimed `FUTEX_WAIT` on a word nothing wakes;
//! * one thread in a compute loop with no syscall in it, which only a tick
//!   can reach (`signal::deliver_pending_on_tick`);
//! * the main thread, after spawning them, in the same untimed wait.
//!
//! It reports nothing and exits only by being killed. A program that reached
//! `exit_group` here would mean a thread was released without the kill, which
//! is itself the failure the kernel test is looking for (`EXIT_STATUS`
//! would then be `0`, not `-9`).

#![no_std]
#![no_main]

const SYS_FUTEX: u64 = 202;
const SYS_EXIT_GROUP: u64 = 231;

const CLONE_VM: u64 = 0x0000_0100;
const CLONE_FS: u64 = 0x0000_0200;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_THREAD: u64 = 0x0001_0000;
const CLONE_SETTLS: u64 = 0x0008_0000;

const FUTEX_WAIT: u64 = 0;
const FUTEX_PRIVATE: u64 = 128;

const THREADS: usize = 3;
const STACK: usize = 16384;

#[repr(align(16))]
struct Stacks([[u8; STACK]; THREADS]);
static mut STACKS: Stacks = Stacks([[0; STACK]; THREADS]);
static mut TLS: [[u64; 8]; THREADS] = [[0; 8]; THREADS];
/// A word that stays 0 and that nothing ever wakes.
static mut PARK_WORD: u32 = 0;

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

core::arch::global_asm!(
    r#"
    .section .text._start
    .global _start
_start:
    mov rdi, rsp
    and rsp, -16
    call rust_start
1:  jmp 1b

    /* spawn_child(flags, child_stack_top, parent_tid, child_tid, tls, fn) —
     * the same trampoline as threadprobe's; see its comment for why both
     * sides of the `syscall` are assembly. */
    .global spawn_child
spawn_child:
    mov rax, 56                     /* SYS_clone */
    sub rsi, 8
    mov [rsi], r9
    syscall
    test rax, rax
    jnz 2f
    pop rax
    call rax
    mov rax, 60                     /* SYS_exit: this thread only */
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

/// Park in an untimed `FUTEX_WAIT` forever; re-park on every spurious wake.
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

/// Compute forever, with no syscall anywhere in the loop.
extern "C" fn spin_forever() {
    let mut x: u64 = 1;
    loop {
        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        // Keep the value live so the loop cannot be folded away.
        unsafe { core::ptr::write_volatile(&raw mut SINK, x) };
    }
}
static mut SINK: u64 = 0;

/// # Safety
/// `_sp` must be the System V initial stack pointer.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_start(_sp: *const u64) -> ! {
    let entries: [extern "C" fn(); THREADS] = [park_forever, park_forever, spin_forever];
    // SAFETY: every pointer is into this program's own statics; each thread
    // gets its own stack and TLS block.
    unsafe {
        for (i, entry) in entries.iter().enumerate() {
            let stack_top = (&raw mut STACKS).cast::<u8>().add(STACK * (i + 1)) as u64;
            let tls = (&raw mut TLS).cast::<u8>().add(64 * i) as u64;
            let tid = spawn_child(
                CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS,
                stack_top,
                0,
                0,
                tls,
                *entry as usize as u64,
            );
            if (tid as i64) <= 0 {
                // A thread that could not be made is a different failure, and
                // one the kernel test must not mistake for a kill that worked.
                syscall6(SYS_EXIT_GROUP, 0x40 | i as u64, 0, 0, 0, 0, 0);
            }
        }
    }
    park_forever();
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // SAFETY: the only thing left to do.
    unsafe { syscall6(SYS_EXIT_GROUP, 0xFF, 0, 0, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}
