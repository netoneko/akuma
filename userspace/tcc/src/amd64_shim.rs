//! `libakuma`, re-derived for `x86_64-unknown-none` from raw syscalls.
//!
//! `libakuma` is written against `aarch64-unknown-linux-musl` — its syscall
//! wrappers assume a hosted musl target with a libc `_start`, and it does not
//! build for a bare-metal `x86_64-unknown-none` at all. Rather than touch
//! `main.rs`'s ~700 lines of `libakuma::X(...)` call sites (every one of them
//! is plumbing that has nothing to do with which architecture it runs on),
//! `main.rs` aliases this module's name to `libakuma` on x86_64 — see the
//! `use` at the top of that file — so it is a drop-in for exactly the subset
//! of `libakuma`'s surface `main.rs` actually calls, nothing more. Anything
//! reached through `libakuma::X` in `main.rs` must have a same-named,
//! same-signature item here.
//!
//! This is also the crate's process entry point, global allocator, panic
//! handler and OOM handler on x86_64 — all four are normally `libakuma`'s job
//! (its `_start` sets up the initial stack pointer this module's [`args`]
//! reads, and it registers `#[global_allocator]`/`#[panic_handler]`/
//! `#[alloc_error_handler]`), and none of that exists without it.
//!
//! # What this deliberately does not do
//!
//! - **No environment variables, no threads, no signals** — same as the rest
//!   of this target. `getenv` was already a stub returning NULL before this
//!   module existed (`main.rs`); nothing here changes that.
//! - **The allocator never frees.** A single eager `mmap` (this target's
//!   `mmap` is fully eager and W^X-enforced — real physical frames at
//!   `mmap()` time, not on first fault; see `amd64/src/mm.rs`) backs a bump
//!   allocator whose `dealloc` is a no-op. tcc's own C code does call `free`
//!   through the `malloc`/`free`/`realloc` trio `main.rs` defines, and those
//!   calls reach this allocator's `dealloc` — they just do not reclaim
//!   anything. For a single `tcc -o file src.c` invocation that runs to
//!   completion and exits, "never reclaim" and "reclaim, but only within this
//!   process's own lifetime" are the same thing; the difference would only
//!   matter for a long-lived process, which nothing running through this
//!   module is.
//! - **No `unlink`/`rename`/`mkdir` on the kernel side.** The raw syscalls
//!   below (87/82/83) issue real x86_64 syscalls, but nothing in
//!   `amd64/src/usermode.rs`'s dispatch answers those numbers yet, so they
//!   come back `ENOSYS`. That is fine for tcc's own use:
//!   `tcc_write_elf_file` (`tinycc/tccelf.c`) calls `unlink(filename)` before
//!   `open(..., O_CREAT|O_TRUNC)` and never checks its return value — the
//!   `open` is what actually matters, and `O_TRUNC` makes the `unlink`
//!   redundant for a plain file anyway.

// This mirrors `libakuma`'s surface (`GETCWD`, `SEEK_END`, …) rather than
// only what `main.rs` happens to call today — leaving `SEEK_SET`/`SEEK_CUR`
// defined but not `SEEK_END`, say, would be a stranger module to hand a
// reader than one with an unused constant or two.
#![allow(dead_code)]

use core::alloc::{GlobalAlloc, Layout};
use core::ffi::{c_char, c_int};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicUsize, Ordering};

// ============================================================================
// Raw syscalls
// ============================================================================

/// x86_64 Linux syscall numbers this module issues. Spelled out rather than
/// pulled from `akuma-syscalls-abi`: this crate builds standalone (via
/// `amd64/build.rs`'s `cargo build -p tcc --target x86_64-unknown-none`, not
/// as a workspace member — see `Cargo.toml`), so it cannot see that crate any
/// more than `userspace/amd64/hello/hello.rs` can (that program's own header
/// makes the same choice, for the same reason).
mod nr {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const OPEN: u64 = 2;
    pub const CLOSE: u64 = 3;
    pub const FSTAT: u64 = 5;
    pub const LSEEK: u64 = 8;
    pub const MMAP: u64 = 9;
    pub const MUNMAP: u64 = 11;
    pub const RENAME: u64 = 82;
    pub const MKDIR: u64 = 83;
    pub const UNLINK: u64 = 87;
    pub const GETCWD: u64 = 79;
    pub const TIME: u64 = 201;
    pub const EXIT_GROUP: u64 = 231;
}

/// `syscall` with up to six arguments. Every wrapper below goes through this
/// one primitive rather than one `asm!` block each — `userspace/amd64/hello`
/// has three (`syscall3`/`syscall6`/the register-preservation probe) because
/// its whole point is exercising the ABI at different arities; this file's
/// job is calling into the kernel, not testing the boundary, so one general
/// helper is the right amount of code.
///
/// # Safety
/// `nr` must be a syscall this kernel implements with the calling convention
/// `(a1..a6)` matches; an unimplemented number returns an error rather than
/// faulting, but a *wrong* argument for an implemented one can still corrupt
/// memory the same way any raw syscall can (e.g. `write` with a bad pointer).
#[inline(always)]
unsafe fn syscall6(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    let ret: u64;
    // SAFETY: caller's obligation; `rcx`/`r11` are declared clobbered because
    // the `syscall` instruction itself overwrites them (return address and
    // flags), exactly as `userspace/amd64/hello/hello.rs`'s `syscall3` notes.
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
            options(nostack),
        );
    }
    ret
}

/// Does a raw return value carry a negated errno? Linux's convention: any of
/// the top 4095 values, cast back from `u64`.
#[inline(always)]
fn is_err(r: u64) -> bool {
    r > u64::MAX - 4096
}

// ============================================================================
// Process entry
// ============================================================================

/// The System V initial stack pointer, captured once at `_start` — [`args`]'s
/// only source of argv, since this target has no `argv`/`argc` global libc
/// sets up for it. `0` means "not yet set" (or "called from a context with no
/// process", which cannot happen here, but the sentinel costs nothing).
static INITIAL_SP: AtomicUsize = AtomicUsize::new(0);

core::arch::global_asm!(
    r#"
    .section .text._start
    .global _start
_start:
    /* The System V process entry state: rsp points at argc, and there is no
     * return address — this is not a call. Hand rsp over as the first
     * argument before aligning, exactly as userspace/amd64/hello/hello.rs's
     * _start does (see that file's comment on why: the whole argv/envp/auxv
     * block is addressed relative to the ORIGINAL rsp, so it has to be read
     * out before the alignment below moves it). */
    mov rdi, rsp
    and rsp, -16
    call rust_start
1:  jmp 1b
"#
);

/// # Safety
/// `sp` must be the System V initial stack pointer tcc's ELF was entered
/// with: `argc`, then `argc` pointers, a NULL, envp, a NULL, and the
/// auxiliary vector. Discharged by `_start` above, which is the only caller.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_start(sp: *const u64) -> ! {
    INITIAL_SP.store(sp as usize, Ordering::Release);
    init_heap();
    // SAFETY: the heap is up, so `main`'s argv parsing (`Vec`/`String`) has
    // somewhere to allocate. `main` is `main.rs`'s real entry point — the one
    // libakuma's own `_start` calls on aarch64 — and never returns; it exits
    // through `exit` at the end of its own body.
    unsafe {
        extern "C" {
            fn main();
        }
        main();
    }
    // `main` exiting without calling `exit` would fall through to here;
    // treat that as success rather than running off the end of `.text`.
    exit(0);
}

/// The command-line argument at index `i`, or `None` past the end.
///
/// # Safety
/// Reads through [`INITIAL_SP`], which must already be set (true for every
/// caller reachable after `rust_start` has run — i.e. everything).
unsafe fn arg(i: u32) -> Option<&'static str> {
    let sp = INITIAL_SP.load(Ordering::Acquire);
    if sp == 0 {
        return None;
    }
    // SAFETY: `sp` is the process's own initial stack pointer (see this
    // function's doc); `argc` is the first word there, and `i < argc` (the
    // caller-facing contract `Args::next` enforces) keeps the pointer read
    // that follows in bounds.
    unsafe {
        let sp = sp as *const u64;
        let argc = *sp as u32;
        if i >= argc {
            return None;
        }
        let argv_ptr = *sp.add(1 + i as usize) as *const u8;
        if argv_ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *argv_ptr.add(len) != 0 {
            len += 1;
        }
        core::str::from_utf8(core::slice::from_raw_parts(argv_ptr, len)).ok()
    }
}

fn argc() -> u32 {
    let sp = INITIAL_SP.load(Ordering::Acquire);
    if sp == 0 {
        return 0;
    }
    // SAFETY: as `arg` — `argc` is the first word at the initial stack pointer.
    unsafe { *(sp as *const u64) as u32 }
}

/// Iterator over command line arguments — mirrors `libakuma::Args`.
pub struct Args {
    current: u32,
    count: u32,
}

impl Iterator for Args {
    type Item = &'static str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current >= self.count {
            return None;
        }
        // SAFETY: `INITIAL_SP` was set by `rust_start` before `main` (the only
        // place an `Args` can be constructed) ever ran.
        let res = unsafe { arg(self.current) };
        self.current += 1;
        res
    }
}

#[must_use]
pub fn args() -> Args {
    Args { current: 0, count: argc() }
}

/// Terminate the process. `exit_group` (231), not the single-thread `exit`
/// (60): this target has no threads, so the two are equivalent, and
/// `exit_group` is what every other exit path on this target already uses
/// (`amd64/src/usermode.rs`'s `Syscall::Exit | Syscall::ExitGroup` arm).
///
/// `#[no_mangle]`, unlike everything else in this file, because it needs to
/// exist as the C symbol `exit` — `tinycc/tcc.c`'s own C code (`tcc_relocate`,
/// `rt_exit`, `default_reallocator`) calls it directly, the same way
/// `libakuma`'s `exit` (also `#[no_mangle]`) is `tcc.c`'s `exit` on aarch64.
/// Everything else here is only ever called from `main.rs`'s Rust, through
/// the `use amd64_shim as libakuma;` alias, so nothing else needs the C ABI's
/// unmangled name.
#[no_mangle]
pub extern "C" fn exit(code: i32) -> ! {
    // SAFETY: exit_group takes one argument and never returns.
    unsafe {
        syscall6(nr::EXIT_GROUP, code as u64, 0, 0, 0, 0, 0);
    }
    // The syscall never returns; this is unreachable but satisfies `-> !`
    // without an `intrinsics::unreachable` dependency this crate has no
    // other reason to pull in.
    loop {
        core::hint::spin_loop();
    }
}

/// Stub. `tinycc/tcctools.c`'s `tcc_tool_cross` calls the real `execvp` to
/// re-exec `tcc` itself with adjusted arguments (its cross-compiler-invocation
/// tool mode) — dead code for a plain `tcc -o file src.c` compile, but its
/// *reference* still has to resolve or the link fails, which is what surfaced
/// this: nothing upstream of `-o file` calls it, `--gc-sections` still could
/// not prove that statically, and the AArch64 build apparently gets away
/// without a definition only because its object layout let `--gc-sections`
/// drop the whole enclosing function first — not because the call is
/// somehow absent there too.
///
/// Always fails, matching `execvp(3)`'s own contract ("returns only on
/// error") — there is no `fork`/exec surface exposed to tcc's C code on this
/// target to actually honour a call here, and failing cleanly is correct
/// regardless: if this path is ever hit for real, the right fix is wiring
/// `sys_spawn` under it, not making failure quieter.
#[no_mangle]
pub extern "C" fn execvp(_prog: *const c_char, _argv: *const *const c_char) -> c_int {
    -1
}

// ============================================================================
// Heap: one eager anonymous mmap, bump-allocated
// ============================================================================

/// `PROT_READ | PROT_WRITE`. Never `PROT_EXEC`: this heap holds tcc's own
/// data structures, not generated code — there is no JIT path here (tcc runs
/// in `-o file` mode on this target, not `-run`) — so it never needs to be
/// executable, which sidesteps this target's W^X enforcement entirely rather
/// than fighting it.
const PROT_RW: u64 = 0x1 | 0x2;
/// `MAP_PRIVATE | MAP_ANONYMOUS`. Linux's mmap flag bits are the same
/// constants on every architecture (unlike syscall numbers), so these are not
/// an x86_64-specific encoding — just spelled out locally for the same reason
/// the syscall numbers are: this crate cannot see `akuma-syscalls-linux`.
const MAP_PRIVATE_ANON: u64 = 0x02 | 0x20;

/// One heap arena, reserved in a single `mmap` at first allocation. This
/// target's `mmap` is capped at 64 MiB per call (`amd64/src/mm.rs`'s
/// `MAX_MAPPING`) and is fully eager — every page becomes a real physical
/// frame immediately, not on first touch — so this number is real memory
/// committed up front, not a reservation. 16 MiB is comfortably under that
/// cap and well above the ~4 MiB peak working set tcc's own aarch64
/// acceptance test measures for a `-static` compile
/// (`acceptance/05_meow_tcc_extreme_4mb.md`) on a much tighter machine; if a
/// larger source file needs more, this is the number to raise, not the design
/// to replace — `sys_mmap`'s frame budget is set by the PMM's free pool, not
/// by anything this crate can see, and QEMU's default run boots with
/// ~957 MiB free.
const HEAP_SIZE: usize = 16 * 1024 * 1024;

static HEAP_NEXT: AtomicUsize = AtomicUsize::new(0);
static HEAP_END: AtomicUsize = AtomicUsize::new(0);

/// Reserve the arena. Called once, from `rust_start`, before anything that
/// could allocate.
fn init_heap() {
    // SAFETY: an anonymous, fixed-size mapping request; `mmap`'s only
    // failure mode here is the kernel refusing (out of frames), handled
    // below rather than assumed away.
    let base = unsafe { syscall6(nr::MMAP, 0, HEAP_SIZE as u64, PROT_RW, MAP_PRIVATE_ANON, u64::MAX, 0) };
    if is_err(base) {
        // No allocator yet, so no `eprintln` (it may itself allocate a
        // formatted `String` on some paths) — write directly.
        raw_eprint(b"tcc: fatal: could not reserve the heap (mmap failed)\n");
        exit(137); // 128 + SIGKILL-ish, matching "ran out of memory" conventions
    }
    HEAP_NEXT.store(base as usize, Ordering::Release);
    HEAP_END.store(base as usize + HEAP_SIZE, Ordering::Release);
}

/// Print straight to stderr with no formatting machinery — for the one
/// message that might need to fire before the allocator exists.
fn raw_eprint(msg: &[u8]) {
    // SAFETY: a real buffer with a real length; `write(2)` on fd 2.
    unsafe {
        syscall6(nr::WRITE, 2, msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0);
    }
}

/// A bump allocator over the arena `init_heap` reserved. `dealloc` never
/// reclaims — see the module header for why that is the right tradeoff here,
/// not a shortcut taken without noticing the cost.
struct BumpAllocator;

// SAFETY: `GlobalAlloc`'s contract — `alloc`/`dealloc` are safe to call
// concurrently, `alloc` never returns a dangling or misaligned pointer for a
// request it does not fail, and `dealloc` is only ever called with a
// pointer/layout this allocator itself handed out (upheld by every caller:
// `main.rs`'s `malloc`/`free`/`realloc` trio, and `alloc`/`alloc::vec::Vec`
// et al. via the `#[global_allocator]` below).
unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(8);
        loop {
            let cur = HEAP_NEXT.load(Ordering::Relaxed);
            let aligned = (cur + align - 1) & !(align - 1);
            let Some(new_next) = aligned.checked_add(layout.size()) else {
                return core::ptr::null_mut();
            };
            if new_next > HEAP_END.load(Ordering::Relaxed) {
                return core::ptr::null_mut();
            }
            // A CAS loop rather than a lock: this target is single-threaded
            // (no `pthread_create` reaches this process), so the loop body
            // runs exactly once in practice — this is defensive against a
            // future where that stops being true, not evidence it already
            // isn't.
            if HEAP_NEXT
                .compare_exchange_weak(cur, new_next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return aligned as *mut u8;
            }
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Never reclaimed — see the module header.
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator;

#[alloc_error_handler]
fn alloc_error(layout: Layout) -> ! {
    raw_eprint(b"tcc: out of memory (heap arena exhausted)\n");
    let _ = layout;
    exit(137);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    raw_eprint(b"tcc: panic\n");
    exit(134) // 128 + SIGABRT, matching a native abort's convention
}

// ============================================================================
// Files
// ============================================================================

pub mod open_flags {
    pub const O_RDONLY: u32 = 0;
    pub const O_WRONLY: u32 = 1;
    pub const O_RDWR: u32 = 2;
    pub const O_CREAT: u32 = 0o100;
    pub const O_TRUNC: u32 = 0o1000;
    pub const O_APPEND: u32 = 0o2000;
}

pub mod seek_mode {
    pub const SEEK_SET: i32 = 0;
    pub const SEEK_CUR: i32 = 1;
    pub const SEEK_END: i32 = 2;
}

/// The x86_64 `struct stat` — 144 bytes, field-for-field what
/// `amd64/src/fd.rs`'s `encode_stat` writes on the kernel side (that
/// function's own doc has the layout table this mirrors). **Not**
/// `libakuma::Stat`, whose layout is `asm-generic`/aarch64's — `st_nlink` at
/// 16 there is 4 bytes; here it is 8, and every offset from `st_mode` on
/// differs by the same kind of arch-specific padding difference. Getting this
/// wrong would not fail to compile (both are `#[repr(C)]` structs of plain
/// integers) — it would read `st_size` out of what is actually `st_blksize`,
/// silently.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    __pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: i64,
    pub st_mtime: i64,
    pub st_mtime_nsec: i64,
    pub st_ctime: i64,
    pub st_ctime_nsec: i64,
    __reserved: [i64; 3],
}

const _: () = assert!(core::mem::size_of::<Stat>() == 144);

pub fn open(path: &str, flags: u32) -> i32 {
    // Real syscall 2 (`open`), not `openat` — `amd64/src/usermode.rs` wires
    // this to the exact same handler openat's `AT_FDCWD` form reaches
    // (`crate::fd::sys_openat`), so there is no `AT_FDCWD` sentinel to spell
    // here the way `libakuma::open` (which only has `openat` on aarch64) has
    // to.
    let path_c = alloc::format!("{}\0", path);
    // SAFETY: `path_c` is NUL-terminated and lives across the call.
    unsafe { syscall6(nr::OPEN, path_c.as_ptr() as u64, flags as u64, 0o644, 0, 0, 0) as i32 }
}

pub fn close(fd: i32) -> i32 {
    // SAFETY: a plain fd close; no memory touched.
    unsafe { syscall6(nr::CLOSE, fd as u64, 0, 0, 0, 0, 0) as i32 }
}

pub fn fstat(fd: i32) -> Result<Stat, i32> {
    let mut stat = Stat::default();
    // SAFETY: `stat` is a real, correctly-sized `#[repr(C)]` buffer the
    // kernel writes into.
    let ret = unsafe { syscall6(nr::FSTAT, fd as u64, (&raw mut stat) as u64, 0, 0, 0, 0) };
    if is_err(ret) {
        Err((ret.wrapping_neg()) as i32)
    } else {
        Ok(stat)
    }
}

pub fn lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    // SAFETY: no memory touched.
    unsafe { syscall6(nr::LSEEK, fd as u64, offset as u64, whence as u64, 0, 0, 0) as i64 }
}

pub fn read_fd(fd: i32, buf: &mut [u8]) -> isize {
    // SAFETY: `buf` is a real slice the kernel writes up to `buf.len()` bytes
    // into; `read(2)`'s own contract bounds it there.
    unsafe { syscall6(nr::READ, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0) as isize }
}

pub fn write_fd(fd: i32, buf: &[u8]) -> isize {
    // SAFETY: `buf` is a real slice the kernel reads up to `buf.len()` bytes
    // from.
    unsafe { syscall6(nr::WRITE, fd as u64, buf.as_ptr() as u64, buf.len() as u64, 0, 0, 0) as isize }
}

pub fn println(s: &str) {
    write_fd(1, s.as_bytes());
    write_fd(1, b"\n");
}

pub fn eprintln(s: &str) {
    write_fd(2, s.as_bytes());
    write_fd(2, b"\n");
}

pub fn mkdir(path: &str) -> i32 {
    let path_c = alloc::format!("{}\0", path);
    // SAFETY: NUL-terminated, lives across the call. See the module header:
    // this syscall number is not implemented on the kernel side yet, so the
    // real return here is `-ENOSYS` — spelled out anyway so a future kernel
    // that does implement it needs no change here.
    unsafe { syscall6(nr::MKDIR, path_c.as_ptr() as u64, 0o755, 0, 0, 0, 0) as i32 }
}

pub fn unlink(path: &str) -> i32 {
    let path_c = alloc::format!("{}\0", path);
    // SAFETY: as `mkdir`. See the module header — tcc never checks this call's
    // result, so `-ENOSYS` here is silently fine.
    unsafe { syscall6(nr::UNLINK, path_c.as_ptr() as u64, 0, 0, 0, 0, 0) as i32 }
}

pub fn rename(old_path: &str, new_path: &str) -> i32 {
    let old_c = alloc::format!("{}\0", old_path);
    let new_c = alloc::format!("{}\0", new_path);
    // SAFETY: both NUL-terminated, both live across the call.
    unsafe { syscall6(nr::RENAME, old_c.as_ptr() as u64, new_c.as_ptr() as u64, 0, 0, 0, 0) as i32 }
}

/// This target has no per-process working directory
/// (`amd64/src/usermode.rs`'s `getcwd` — x86_64 syscall 79 — always answers
/// `/`), so this mirrors that rather than caching a wrong answer.
#[must_use]
pub fn getcwd() -> &'static str {
    "/"
}

// ============================================================================
// Memory
// ============================================================================

pub fn mmap(addr: usize, len: usize, prot: u32, flags: u32) -> usize {
    // SAFETY: anonymous-only on this target in practice (a file-backed
    // request is refused by the kernel, not by this wrapper — see
    // `amd64/src/mm.rs`), so there is no descriptor or offset to pass; `-1`/`0`
    // match what a real anonymous `mmap(2)` call passes for `fd`/`offset`.
    let ret = unsafe { syscall6(nr::MMAP, addr as u64, len as u64, prot as u64, flags as u64, u64::MAX, 0) };
    ret as usize
}

pub fn munmap(addr: usize, len: usize) -> isize {
    // SAFETY: unmapping a range this process itself mapped (the caller's
    // obligation, same as real `munmap(2)`).
    unsafe { syscall6(nr::MUNMAP, addr as u64, len as u64, 0, 0, 0, 0) as isize }
}

// ============================================================================
// Misc
// ============================================================================

/// A real `time(2)` since `amd64/src/clock.rs` (2026-09-05, boot-time SNTP)
/// — `0` before that module's `sync_via_sntp` has run or if it failed
/// (no route, DNS blocked, …), matching `time(2)`'s own "seconds since the
/// epoch" contract rather than inventing a value. `amd64/src/fs.rs`'s
/// `no_clock` (inode timestamps) is a different, still-open gap — this only
/// answers the *wall-clock* question, not "does every file have a real
/// mtime".
#[must_use]
pub fn time() -> u64 {
    // SAFETY: `NULL` for `tloc` is a documented, valid `time(2)` call — the
    // seconds count comes back in `rax` either way.
    unsafe { syscall6(nr::TIME, 0, 0, 0, 0, 0, 0) }
}
