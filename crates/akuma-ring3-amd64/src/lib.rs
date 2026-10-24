//! Ring 3 for the minimal amd64 composition (taoxichuan stage 2).
//!
//! Everything a kernel needs to take one static non-PIE ELF image, build its
//! initial user stack, and cross the ring boundary in both directions —
//! extracted as a crate rather than grown inline in `taoxichuan/`, per the
//! experiment's composition rule. The full kernel's counterparts live in
//! `amd64/src/usermode.rs` (9 k lines: demand paging, signals, threads, an
//! interpreter loader) and `amd64/src/loader.rs` (over `akuma-elf` +
//! `akuma-mmu`); none of that serves a task that only prints and exits, and
//! pulling it would pull the whole MMU. What is here is the irreducible core:
//!
//! - [`Image`]: the `PT_LOAD` walk over an already-read image (`akuma-elf`
//!   without `akuma-mmu` — segments are placed eagerly, no demand paging).
//! - [`build_stack`]: argc/argv/envp/auxv laid out the way Linux does, with
//!   the auxv entries static musl reads at startup.
//! - [`enable_syscalls`] / [`SYSCALL_ENTRY`]: `EFER.SCE` + `STAR`/`LSTAR`/
//!   `SFMASK`, and the naked stub the CPU lands on for every `syscall` —
//!   switch to a kernel stack, build a [`SyscallFrame`], call the kernel's
//!   dispatcher, [`sysretq`](enter_user) back.
//! - [`enter_user`]: the first crossing in, by `sysretq`.
//!
//! # What is deliberately NOT here
//!
//! No GS handling: neither the stub nor the kernel keeps a per-CPU block, so
//! there is nothing to `swapgs` for (the IDT stubs in `akuma-trap-amd64` do
//! swap, and stay correct: swapping a pair of zero bases changes nothing).
//! No return path other than `sysretq` from the stub itself — a task that
//! faults dies in the kernel's trap handler, it does not come back here.
//!
//! # The syscall contract
//!
//! `syscall` clears `IF` itself, so the stub runs with interrupts masked until
//! `sysretq` restores `RFLAGS` from `R11`. The dispatcher receives a
//! [`SyscallFrame`] (the number, the six argument registers in Linux order —
//! `R10` carries the fourth argument, not `RCX` — and the user `RIP`/`RFLAGS`
//! for anything that must look at them) and returns the `RAX` value, negative
//! errno on failure, by Linux convention.

#![no_std]
// Same allow as taoxichuan and akuma-boot-amd64: several entry-point docs open
// with a long paragraph on purpose (the why lives in the first block).
#![allow(clippy::too_long_first_doc_paragraph)]

#[cfg(target_arch = "x86_64")]
use akuma_cpu::msr::{self, Msr};

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// ELF64: the `PT_LOAD` walk, over bytes the caller already read.
// ---------------------------------------------------------------------------

/// Upper bound on `PT_LOAD` segments. The reference image has four (separate
/// code/rodata splits); 16 covers every static link this loader will meet.
pub const MAX_SEGMENTS: usize = 16;

/// Where an `ET_DYN` image (static-PIE — Alpine's `apk.static` is one) is
/// placed. The image's link addresses start near zero, so the kernel shifts
/// every `p_vaddr`, the entry and `AT_PHDR` by this base; musl's static-PIE
/// startup (`__dls2`) does its own relocation in userspace, so the kernel
/// processes no `RELA`s (the aarch64 kernel made the same move:
/// `docs/archive/APK_MISSING_SYSCALLS.md`). The amd64 map has PIE code here,
/// `mmap` at `0x6000_0000_0000` and the stack at `0x7000_0000_0000` — no
/// overlap; `brk` follows `image_end` above the placed image.
pub const PIE_BASE: u64 = 0x1000_0000;

/// Software permissions of one segment — the `p_flags` triple, reduced to the
/// three shapes a static link actually produces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prot {
    /// Read-only data.
    Ro,
    /// Read-execute code.
    Rx,
    /// Read-write data (its `memsz > filesz` tail is `.bss`: zeroed pages).
    Rw,
}

/// One `PT_LOAD` segment: where it sits, where its bytes are in the image,
/// and how much of it is file-backed versus zero-filled.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    /// Link address (the `p_vaddr` the image was linked at). For `ET_DYN`
    /// (static-PIE) images this is already shifted by [`PIE_BASE`]; the
    /// placement is the shift, and userspace relocates from there.
    pub vaddr: u64,
    /// Byte offset of the file-backed part within the image.
    pub offset: u64,
    /// File-backed length.
    pub filesz: u64,
    /// In-memory length, `>= filesz`; the tail is `.bss`.
    pub memsz: u64,
    pub prot: Prot,
}

/// Why [`Image::parse`] refused an image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadError {
    /// Missing or wrong `\x7fELF` magic.
    Magic,
    /// Not ELF64 little-endian (`EI_CLASS`/`EI_DATA`).
    NotElf64Le,
    /// Neither `ET_EXEC` nor `ET_DYN`.
    NotExec,
    /// The program header table is missing or outside the image.
    PhdrTable,
    /// A segment's file bytes run past the end of the image, or `memsz` is
    /// smaller than `filesz`.
    SegmentBounds,
    /// More `PT_LOAD` segments than [`MAX_SEGMENTS`].
    TooManySegments,
    /// No `PT_LOAD` at all: nothing to enter.
    NoSegments,
}

/// A parsed static ELF image: the entry point, the phdr facts musl needs in
/// its auxv, and the segments to place.
#[derive(Clone, Copy, Debug)]
pub struct Image<'a> {
    data: &'a [u8],
    entry: u64,
    /// Where the program header table lands once the image is placed —
    /// `AT_PHDR`. Taken from `PT_PHDR` when present, else computed from the
    /// segment that file-backs `e_phoff`.
    phdr_va: u64,
    phent: u64,
    phnum: u64,
    n_segments: usize,
    segments: [Segment; MAX_SEGMENTS],
    /// Highest byte of the placed image, page-rounded up: where `brk` starts.
    image_end: u64,
}

const PHDR_SIZE: u64 = 56;
const PT_LOAD: u32 = 1;
const PT_PHDR: u32 = 6;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

impl<'a> Image<'a> {
    /// Parse an already-read image. Bounds-checked throughout: every table and
    /// segment access is range-tested against `data` before it is read, so a
    /// truncated or hostile image is a [`LoadError`], never a panic.
    pub fn parse(data: &'a [u8]) -> Result<Self, LoadError> {
        if data.len() < 64 || data[..4] != [0x7f, b'E', b'L', b'F'] {
            return Err(LoadError::Magic);
        }
        // EI_CLASS = 2 (ELF64), EI_DATA = 1 (two's complement, little-endian).
        if data[4] != 2 || data[5] != 1 {
            return Err(LoadError::NotElf64Le);
        }
        let e_type = u16::from_le_bytes([data[16], data[17]]);
        if e_type != 2 && e_type != 3 {
            return Err(LoadError::NotExec);
        }
        // ET_DYN (static-PIE): shift every link address by [`PIE_BASE`] — the
        // one placement decision; userspace self-relocates from there.
        let base = if e_type == 3 { PIE_BASE } else { 0 };
        let rd = |off: usize| -> u64 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&data[off..off + 8]);
            u64::from_le_bytes(b)
        };
        let entry = rd(24) + base;
        let e_phoff = rd(32);
        let e_phentsize = u64::from(u16::from_le_bytes([data[54], data[55]]));
        let e_phnum = u64::from(u16::from_le_bytes([data[56], data[57]]));
        if e_phoff == 0 || e_phentsize < PHDR_SIZE || e_phnum == 0 {
            return Err(LoadError::PhdrTable);
        }
        let table_end = e_phoff.checked_add(e_phnum.checked_mul(e_phentsize).ok_or(LoadError::PhdrTable)?);
        let Some(table_end) = table_end else {
            return Err(LoadError::PhdrTable);
        };
        if table_end > data.len() as u64 {
            return Err(LoadError::PhdrTable);
        }

        let mut img = Self {
            data,
            entry,
            phdr_va: 0,
            phent: PHDR_SIZE,
            phnum: e_phnum,
            n_segments: 0,
            segments: [Segment { vaddr: 0, offset: 0, filesz: 0, memsz: 0, prot: Prot::Ro }; MAX_SEGMENTS],
            image_end: 0,
        };

        // First pass: collect the PT_LOADs.
        for i in 0..e_phnum {
            let ph = e_phoff + i * e_phentsize;
            let p_type = u32::from_le_bytes([data[ph as usize], data[ph as usize + 1], data[ph as usize + 2], data[ph as usize + 3]]);
            if p_type != PT_LOAD {
                continue;
            }
            let p_flags = u32::from_le_bytes([data[ph as usize + 4], data[ph as usize + 5], data[ph as usize + 6], data[ph as usize + 7]]);
            let p_offset = rd(ph as usize + 8);
            let p_vaddr = rd(ph as usize + 16);
            let p_filesz = rd(ph as usize + 32);
            let p_memsz = rd(ph as usize + 40);
            let seg_end = p_offset.checked_add(p_filesz).ok_or(LoadError::SegmentBounds)?;
            if seg_end > data.len() as u64 || p_memsz < p_filesz {
                return Err(LoadError::SegmentBounds);
            }
            if img.n_segments == MAX_SEGMENTS {
                return Err(LoadError::TooManySegments);
            }
            let prot = if p_flags & PF_W != 0 {
                Prot::Rw
            } else if p_flags & PF_X != 0 {
                Prot::Rx
            } else {
                Prot::Ro
            };
            img.segments[img.n_segments] =
                Segment { vaddr: p_vaddr + base, offset: p_offset, filesz: p_filesz, memsz: p_memsz, prot };
            img.n_segments += 1;
        }
        if img.n_segments == 0 {
            return Err(LoadError::NoSegments);
        }

        // `AT_PHDR`: `PT_PHDR` names its own link address; without it, the
        // table sits inside whatever segment file-backs `e_phoff`.
        img.phdr_va = Self::phdr_from_load(&img, e_phoff);
        for i in 0..e_phnum {
            let ph = e_phoff + i * e_phentsize;
            let p_type = u32::from_le_bytes([data[ph as usize], data[ph as usize + 1], data[ph as usize + 2], data[ph as usize + 3]]);
            if p_type == PT_PHDR {
                img.phdr_va = rd(ph as usize + 16) + base;
                break;
            }
        }

        for seg in &img.segments[..img.n_segments] {
            img.image_end = img.image_end.max(seg.vaddr + seg.memsz);
        }

        Ok(img)
    }

    /// The link address of the program header table, from the `PT_LOAD` whose
    /// file range covers `e_phoff` (the static-link case; a dynamic exe would
    /// carry `PT_PHDR` and be caught above).
    fn phdr_from_load(img: &Self, e_phoff: u64) -> u64 {
        for seg in &img.segments[..img.n_segments] {
            let end = seg.offset + seg.filesz;
            if e_phoff >= seg.offset && e_phoff < end {
                return seg.vaddr + (e_phoff - seg.offset);
            }
        }
        0
    }

    /// Entry point (`e_entry`).
    #[must_use]
    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// `AT_PHDR`.
    #[must_use]
    pub fn phdr_va(&self) -> u64 {
        self.phdr_va
    }

    /// `AT_PHENT`.
    #[must_use]
    pub fn phent(&self) -> u64 {
        self.phent
    }

    /// `AT_PHNUM`.
    #[must_use]
    pub fn phnum(&self) -> u64 {
        self.phnum
    }

    /// The placed segments, in program-header order.
    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.n_segments]
    }

    /// The image's file-backed bytes for one segment (empty for a fully
    /// zero-fill segment). Length `<= filesz`, by construction of [`Self::parse`].
    #[must_use]
    pub fn segment_bytes(&self, seg: &Segment) -> &[u8] {
        let end = (seg.offset + seg.filesz) as usize;
        &self.data[seg.offset as usize..end.min(self.data.len())]
    }

    /// Highest placed byte, page-rounded up: where the `brk` heap starts.
    #[must_use]
    pub fn image_end(&self) -> u64 {
        self.image_end
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// The initial user stack: argc/argv/envp/auxv, the Linux layout.
// ---------------------------------------------------------------------------

/// `AT_NULL` — ends the auxiliary vector.
pub const AT_NULL: u64 = 0;
/// `AT_PHDR` — the program header table's link address.
pub const AT_PHDR: u64 = 3;
/// `AT_PHENT` — size of one program header entry.
pub const AT_PHENT: u64 = 4;
/// `AT_PHNUM` — number of program headers.
pub const AT_PHNUM: u64 = 5;
/// `AT_PAGESZ` — page size.
pub const AT_PAGESZ: u64 = 6;
/// `AT_ENTRY` — the image entry point.
pub const AT_ENTRY: u64 = 9;
/// `AT_UID`/`AT_EUID`/`AT_GID`/`AT_EGID` — the credentials musl caches.
pub const AT_UID: u64 = 11;
/// `AT_EUID`.
pub const AT_EUID: u64 = 12;
/// `AT_GID`.
pub const AT_GID: u64 = 13;
/// `AT_EGID`.
pub const AT_EGID: u64 = 14;
/// `AT_HWCAP` — the ELF capability word (none: no HWCAP features offered).
pub const AT_HWCAP: u64 = 16;
/// `AT_CLKTCK` — `times(2)` resolution.
pub const AT_CLKTCK: u64 = 17;
/// `AT_SECURE` — whether the kernel elevated the process (no).
pub const AT_SECURE: u64 = 23;
/// `AT_RANDOM` — 16 bytes for the stack canary. The builder materialises them
/// inside the stack block and writes their address into the auxv itself.
pub const AT_RANDOM: u64 = 25;
/// `AT_EXECFN` — pointer to the program name string.
pub const AT_EXECFN: u64 = 31;
/// `AT_BASE` — the interpreter's base (none: static).
pub const AT_BASE: u64 = 7;

/// Most `argv`+`envp` strings whose addresses the builder will remember.
/// The composed kernel passes one argv entry and no env; the bound is a
/// sanity check, not a resource decision.
const MAX_STRS: usize = 32;

/// Lay out the initial user stack into `buf`, whose **last** byte is the last
/// byte of the stack page at `stack_top` (i.e. `buf` is the tail of the stack,
/// placed so that `buf.as_ptr() + buf.len() == stack_top`).
///
/// Returns the initial user `RSP`, 16-byte aligned, pointing at `argc`.
///
/// The layout, built from `stack_top` down: the strings (env, then argv, then
/// 16 canary bytes), padding to 16, the auxv (`AT_NULL`-terminated), a NULL,
/// envp pointers with a NULL, argv pointers with a NULL, and `argc` last — the
/// exact order Linux builds and musl's `__init_libc` walks. `AT_RANDOM` and
/// `AT_EXECFN` values are supplied by the builder itself (`AT_RANDOM` gets the
/// in-block canary address, `AT_EXECFN` the `argv[0]` string address), so the
/// caller states them as `(AT_RANDOM, 0)` / `(AT_EXECFN, 0)` placeholders.
pub fn build_stack(buf: &mut [u8], stack_top: u64, argv: &[&str], env: &[&str], aux: &[(u64, u64)]) -> u64 {
    assert!(
        argv.len() + env.len() <= MAX_STRS,
        "argv+envp exceed the builder's string table"
    );
    let mut top = stack_top;
    let mut addrs = [0u64; MAX_STRS];
    let mut n_strs = 0usize;
    for s in env.iter().chain(argv.iter()) {
        let bytes = s.as_bytes();
        top -= bytes.len() as u64 + 1; // NUL
        let i = buf.len() - (stack_top - top) as usize;
        buf[i..i + bytes.len()].copy_from_slice(bytes);
        buf[i + bytes.len()] = 0;
        addrs[n_strs] = top;
        n_strs += 1;
    }
    let env_addrs = &addrs[..env.len()];
    let argv_addrs = &addrs[env.len()..n_strs];
    let execfn = if argv.is_empty() { 0 } else { addrs[env.len()] };

    // The 16 canary bytes `AT_RANDOM` points at, then 16-byte alignment for
    // the pointer area (always applied: the vectors are u64s but the ABI
    // promises the initial rsp itself is 16-aligned).
    let mut random_va = 0u64;
    if aux.iter().any(|(k, _)| *k == AT_RANDOM) {
        top = (top - 16) & !0xF;
        random_va = top;
        let i = buf.len() - (stack_top - top) as usize;
        for (j, b) in buf[i..i + 16].iter_mut().enumerate() {
            *b = canary_byte(j as u64);
        }
    }
    top &= !0xF;

    // Everything from here down is 8-byte words, written back-to-front so the
    // final `rsp` lands on `argc`.
    let mut push = |v: u64| {
        top -= 8;
        let i = buf.len() - (stack_top - top) as usize;
        buf[i..i + 8].copy_from_slice(&v.to_le_bytes());
    };
    // The `AT_NULL` terminator is pushed first so it lands highest — musl
    // walks the vector upward and stops at a zero key — then the pairs in
    // reverse, so entry 0 ends up lowest, first above the envp NULL.
    push(AT_NULL);
    push(0);
    let mut i_aux = aux.len();
    while i_aux > 0 {
        i_aux -= 1;
        let (k, v) = aux[i_aux];
        let value = match k {
            AT_RANDOM => random_va,
            AT_EXECFN => execfn,
            _ => v,
        };
        push(value);
        push(k);
    }

    push(0); // envp NULL
    for p in env_addrs.iter().rev() {
        push(*p);
    }
    push(0); // argv NULL
    for p in argv_addrs.iter().rev() {
        push(*p);
    }
    push(argv.len() as u64); // argc
    top
}

/// Deterministic canary byte. There is no entropy source behind this kernel;
/// a fixed canary defeats naive overflow chains and costs nothing, and is
/// stated here rather than disguised as randomness.
#[must_use]
pub fn canary_byte(i: u64) -> u8 {
    0xA5_u8.rotate_left((i % 8) as u32 * 3) ^ (i as u8).wrapping_mul(0x5A)
}

// ---------------------------------------------------------------------------
// The syscall machinery: MSRs, the stub, the frames it builds.
// ---------------------------------------------------------------------------

/// One kernel stack for the syscall path. One task, one core, interrupts off
/// for the whole excursion — a single static is the honest shape, and the
/// crate owns it so the stub's `mov rsp` cannot drift from the stack it names.
pub const SYSCALL_STACK_SIZE: usize = 16 * 1024;

#[repr(align(16))]
#[cfg(target_arch = "x86_64")]
struct SyscallStack(#[allow(dead_code)] [u8; SYSCALL_STACK_SIZE]);
#[cfg(target_arch = "x86_64")]
static SYSCALL_STACK: SyscallStack = SyscallStack([0; SYSCALL_STACK_SIZE]);

/// The stack top the stub switches to. Fixed at first use: `enable_syscalls`
/// stores it (a static's address is a link-time fact, but casting one to an
/// integer is not const-evaluable, so the value is written at runtime rather
/// than computed in a `static` initializer).
#[cfg(target_arch = "x86_64")]
static SYSCALL_STACK_TOP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Where the stub parked the interrupted user `RSP` before switching away.
/// `syscall` does not hand over a stack pointer in a register, and the user
/// stack must not be touched before the kernel chooses to, so the stash is
/// kernel memory. Single core, `IF` off: no other writer exists. An
/// `UnsafeCell` behind a manual `Sync` because two writers exist on stage 4:
/// the stub's asm, and the nested-task handoff (`user_rsp_stash`/
/// `set_user_rsp_stash` below) — the cell changes nothing for the asm, which
/// takes the address.
#[cfg(target_arch = "x86_64")]
struct StashCell(core::cell::UnsafeCell<u64>);
// SAFETY: the stash is syscall-path memory — one core, `IF` clear while it
// is written (the stub, or the nested-task handoff). No cross-thread access
// exists in a kernel with one core and no scheduler.
#[cfg(target_arch = "x86_64")]
unsafe impl Sync for StashCell {}
#[cfg(target_arch = "x86_64")]
static USER_RSP_STASH: StashCell = StashCell(core::cell::UnsafeCell::new(0));

/// The kernel's dispatcher, stored as an address so the naked stub can `call`
/// it through memory (a naked function has no Rust-level names to reach).
#[cfg(target_arch = "x86_64")]
static DISPATCH: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The frame the stub builds on the kernel stack, one field per pushed
/// register in **memory order** — `syscall` pushes `r10`, `r8`, `r9` in that
/// sequence, so the *lowest* address holds the last push: `r9` sits at +48,
/// `r10` at +64. The offsets are load-bearing on both sides of the `call` —
/// the stub indexes `[rsp + N]` by hand, and `#[repr(C)]` fixes the Rust
/// side to match. `user_rip`/`user_rflags` are what `syscall` stashed in
/// `RCX`/`R11`. (Stage 2 wrote this struct in *push* order — `r10, r8, r9`
/// — which silently swapped args 4 and 6 for every syscall with more than
/// three arguments; nothing until stage 3's `openat`/`mmap` had that many,
/// so the pinned offset tests, which never named these three fields, stayed
/// green while the ABI was wrong.)
#[repr(C)]
pub struct SyscallFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    /// Sixth argument (`R9`), at the slot the stub's last of the three
    /// pushes occupies.
    pub r9: u64,
    /// Fifth argument (`R8`).
    pub r8: u64,
    /// Fourth argument. `syscall` takes it in `R10` because `RCX` carries
    /// the return address.
    pub r10: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    /// The syscall number on the way in; the stub stores the dispatcher's
    /// return value here before restoring, so the same slot carries the
    /// initial `RAX` on first entry.
    pub rax: u64,
    /// The interrupted user `RIP`.
    pub user_rip: u64,
    /// The interrupted user `RFLAGS`.
    pub user_rflags: u64,
}

/// The register file for the *first* crossing into ring 3 — what
/// [`enter_user`] loads. Distinct from [`SyscallFrame`]: that one is built by
/// the CPU's `syscall` contract; this one is built by the kernel, and names
/// the entry point, flags and stack explicitly.
#[repr(C)]
pub struct EntryFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub r10: u64,
    pub r8: u64,
    pub r9: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rax: u64,
    /// Where `sysretq` jumps: the image entry point.
    pub rip: u64,
    /// What `sysretq` restores: `IF` set, reserved bit 1 set.
    pub rflags: u64,
    /// The stack `build_stack` returned.
    pub rsp: u64,
}

/// The dispatcher's type: inspect the frame, return the `RAX` value.
pub type Dispatcher = fn(&mut SyscallFrame) -> u64;

// -- Nested-task knobs (taoxichuan stage 4) --------------------------------
//
// The minimal kernel's stage-4 SPAWN runs a child to completion *inside* the
// parent's syscall. During that nested run the child's own `syscall`
// crossings use the same two statics the stub uses — the parked user `RSP`
// and the kernel stack top — and both must be redirected for the child and
// restored for the parent. The statics stay private; these accessors are the
// whole of the new surface, and they are plain loads/stores on memory the
// stub itself treats as single-core, `IF`-off scratch.

/// The user `RSP` the stub stashed for the task currently in the kernel.
#[cfg(target_arch = "x86_64")]
#[must_use]
pub fn user_rsp_stash() -> u64 {
    // SAFETY: single core, `IF` clear inside the syscall path — the same
    // discipline that owns every write to this static (the stub's own asm
    // included).
    unsafe { core::ptr::read_volatile(USER_RSP_STASH.0.get()) }
}

/// Redirect the stub's user-`RSP` stash (see [`user_rsp_stash`]).
#[cfg(target_arch = "x86_64")]
pub fn set_user_rsp_stash(v: u64) {
    // SAFETY: as `user_rsp_stash`.
    unsafe { core::ptr::write_volatile(USER_RSP_STASH.0.get(), v) }
}

/// The stack top the stub switches to on the next `syscall`.
#[cfg(target_arch = "x86_64")]
#[must_use]
pub fn syscall_stack_top() -> u64 {
    SYSCALL_STACK_TOP.load(core::sync::atomic::Ordering::Relaxed)
}

/// Point the stub's kernel stack at a lower region for a nested run. The
/// caller owns the arithmetic: the value must leave the region between the
/// stack's base and the current top untouched (the live dispatch frames).
#[cfg(target_arch = "x86_64")]
pub fn set_syscall_stack_top(v: u64) {
    SYSCALL_STACK_TOP.store(v, core::sync::atomic::Ordering::Relaxed);
}

/// Install `dispatcher` and point `EFER.SCE`/`STAR`/`LSTAR`/`SFMASK` at the
/// stub. Call once, before the first ring-3 entry.
#[cfg(target_arch = "x86_64")]
pub fn enable_syscalls(dispatcher: Dispatcher) {
    DISPATCH.store(dispatcher as usize, core::sync::atomic::Ordering::Relaxed);
    SYSCALL_STACK_TOP.store(
        (&raw const SYSCALL_STACK) as usize as u64 + SYSCALL_STACK_SIZE as u64,
        core::sync::atomic::Ordering::Relaxed,
    );
    // SCE: the `syscall` instruction itself. Without it, `syscall` is `#GP`.
    let efer = msr::read(Msr::Efer) | 1;
    // SAFETY: the documented contract of each MSR write; the values are this
    // table's selectors (via `star()`), this stub, and a fixed mask.
    unsafe {
        msr::write(Msr::Efer, efer);
        msr::write(Msr::Star, akuma_gdt::star());
        msr::write(Msr::Lstar, syscall_entry as *const () as usize as u64);
        // Bits cleared on entry, over and above what `syscall` already does
        // (it clears `IF` unconditionally): `TF` (no single-step across the
        // boundary), `DF` (musl's `rep movsb` leaves it set; a kernel that
        // inherits `DF=1` faults in its own `memcpy`), `IOPL`/`NT`/`AC`
        // (ring 0 owns all three).
        msr::write(Msr::Fmask, 0x0004_7700);
    }
}

impl SyscallFrame {
    /// First argument (`RDI`).
    #[must_use]
    pub fn arg1(&self) -> u64 {
        self.rdi
    }
    /// Second argument (`RSI`).
    #[must_use]
    pub fn arg2(&self) -> u64 {
        self.rsi
    }
    /// Third argument (`RDX`).
    #[must_use]
    pub fn arg3(&self) -> u64 {
        self.rdx
    }
    /// Fourth argument (`R10`).
    #[must_use]
    pub fn arg4(&self) -> u64 {
        self.r10
    }
    /// Fifth argument (`R8`).
    #[must_use]
    pub fn arg5(&self) -> u64 {
        self.r8
    }
    /// Sixth argument (`R9`).
    #[must_use]
    pub fn arg6(&self) -> u64 {
        self.r9
    }
}

/// The `syscall` landing point. See [`enable_syscalls`] for the setup
/// contract. `no_mangle`d so the kernel can name the symbol for `LSTAR`.
///
/// # Safety
/// Not callable as a function: the CPU enters here, with `IF` clear, user
/// state in `RAX`/`RCX`/`R11`/`RDI`/`RSI`/`RDX`/`R10`/`R8`/`R9` and the
/// user's `RSP` still current.
#[doc(hidden)]
#[unsafe(no_mangle)]
#[unsafe(naked)]
#[cfg(target_arch = "x86_64")]
pub extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        // In: RAX = nr, RCX = user RIP, R11 = user RFLAGS, RSP = user RSP
        // (not switched), args in RDI RSI RDX R10 R8 R9. `IF` is clear.
        // No `swapgs`: this kernel has no per-CPU block, so `%gs` means
        // nothing to ring 0 and there is nothing to swap.
        "mov qword ptr [rip + {stash}], rsp",
        "mov rsp, qword ptr [rip + {top}]",
        // 15 pushes (120 bytes) on top of 8 bytes of alignment padding:
        // 16-aligned at the call, frame at [rsp] matching `SyscallFrame`.
        "sub rsp, 8",
        "push r11",
        "push rcx",
        "push rax",
        "push rdi",
        "push rsi",
        "push rdx",
        "push r10",
        "push r8",
        "push r9",
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov rdi, rsp",
        "call qword ptr [rip + {dispatch}]",
        // The result rides back in the frame's RAX slot.
        "mov [rsp + 96], rax",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "pop r9",
        "pop r8",
        "pop r10",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        // Now: [rsp] = saved RAX, [rsp+8] = user RIP, [rsp+16] = user RFLAGS.
        "mov rcx, [rsp + 8]",
        "mov r11, [rsp + 16]",
        "mov rax, [rsp]",
        // Back to the user stack — from the stash, not from the frame: the
        // user stack was never touched.
        "mov rsp, qword ptr [rip + {stash}]",
        "sysretq",
        stash = sym USER_RSP_STASH,
        top = sym SYSCALL_STACK_TOP,
        dispatch = sym DISPATCH,
    );
}

/// Enter ring 3: restore `f` into the registers and `sysretq`.
///
/// `sysretq` takes the target `RIP` from `RCX`, the target `RFLAGS` from
/// `R11`, and derives `CS`/`SS` from `IA32_STAR` — which is why this crate
/// depends on `akuma-gdt`: the selectors `sysretq` computes must name the user
/// rows, and the table's row order is that contract, stated at compile time.
/// `RCX`, `R11` and `RSP` are loaded last; they carry the target, not caller
/// state. This never returns: the task runs, and comes back only through the
/// stub or the kernel's trap handler.
///
/// # Safety
/// `f` must describe a ring-3-enterable state, and the active address space
/// must map both the kernel half (the CPU is in ring 0 until `sysretq` fires)
/// and the user half (it executes there afterwards). Naked on purpose: the
/// body is the crossing, and the pointer arrives in `RDI` per the C ABI.
#[unsafe(naked)]
#[cfg(target_arch = "x86_64")]
pub unsafe extern "C" fn enter_user(f: &EntryFrame) -> ! {
    core::arch::naked_asm!(
        // The frame pointer is RDI; every load is spent against it, and the
        // frame's own R10 is restored after the pointer's last use.
        "mov rax, [rdi + 0x60]",
        "mov rdx, [rdi + 0x48]",
        "mov rsi, [rdi + 0x50]",
        "mov rbp, [rdi + 0x20]",
        "mov r8,  [rdi + 0x38]",
        "mov r9,  [rdi + 0x40]",
        "mov rbx, [rdi + 0x28]",
        "mov r12, [rdi + 0x18]",
        "mov r13, [rdi + 0x10]",
        "mov r14, [rdi + 0x08]",
        "mov r15, [rdi + 0x00]",
        "mov rcx, [rdi + 0x68]", // rip: sysretq's target
        "mov r11, [rdi + 0x70]", // rflags: sysretq's RFLAGS
        "mov r10, [rdi + 0x30]",
        "mov rsp, [rdi + 0x78]", // rsp: the user stack (rdi still points at the frame)
        "mov rdi, [rdi + 0x58]", // last: spends the frame pointer
        "sysretq",
    );
}

// ---------------------------------------------------------------------------
// The x86_64 Linux ABI surface the minimal dispatcher serves: numbers, errno,
// and the `arch_prctl` requests. (`akuma-syscalls-linux` is the AArch64/`asm-
// generic` numbering — `write` 64, `exit` 93 — and the amd64 ABI is a
// different table; the full kernel translates per arch, and so does this.)
// ---------------------------------------------------------------------------

/// Syscall numbers, x86_64.
pub mod nr {
    /// `read(2)`.
    pub const READ: u64 = 0;
    /// `write(2)`.
    pub const WRITE: u64 = 1;
    /// `writev(2)` — musl's stdio flushes through this, even for a single
    /// buffer (the empty `wbase` head is one zero-length iovec).
    pub const WRITEV: u64 = 20;
    /// `mmap(2)`.
    pub const MMAP: u64 = 9;
    /// `mprotect(2)`.
    pub const MPROTECT: u64 = 10;
    /// `munmap(2)`.
    pub const MUNMAP: u64 = 11;
    /// `brk(2)`.
    pub const BRK: u64 = 12;
    /// `ioctl(2)`.
    pub const IOCTL: u64 = 16;
    /// `stat(2)` — x86_64 keeps the legacy number, and musl's `stat()` maps
    /// to it (not `newfstatat`); busybox `ls` stats its argument to learn
    /// whether it is a directory.
    pub const STAT: u64 = 4;
    /// `close(2)` — the fd-table counterpart to `openat`'s allocation.
    pub const CLOSE: u64 = 3;
    /// `rt_sigaction(2)` — accepted and recorded-not: this kernel has no
    /// signal delivery, so a registration has nothing to attach to. Returning
    /// success for a no-op is what lets musl's startup proceed.
    pub const RT_SIGACTION: u64 = 13;
    /// `rt_sigprocmask(2)` — same no-op contract as `RT_SIGACTION`.
    pub const RT_SIGPROCMASK: u64 = 14;
    /// `getpid(2)`.
    pub const GETPID: u64 = 39;
    /// `getuid(2)`.
    pub const GETUID: u64 = 102;
    /// `getgid(2)`.
    pub const GETGID: u64 = 104;
    /// `geteuid(2)`.
    pub const GETEUID: u64 = 107;
    /// `getegid(2)`.
    pub const GETEGID: u64 = 108;
    /// `exit(2)` — ends the calling thread; the only thread.
    pub const EXIT: u64 = 60;
    /// `arch_prctl(2)` — x86_64-specific: the FS/GS bases.
    pub const ARCH_PRCTL: u64 = 158;
    /// `set_tid_address(2)`.
    pub const SET_TID_ADDRESS: u64 = 218;
    /// `clock_gettime(2)` — zeroed: this kernel has no clock (the host
    /// tracks no wall time either), and the applets that call it only want
    /// *a* time, not a true one.
    pub const CLOCK_GETTIME: u64 = 228;
    /// `gettimeofday(2)` — same contract as `CLOCK_GETTIME`.
    pub const GETTIMEOFDAY: u64 = 96;
    /// `getdents64(2)` — the directory read `ls` walks a directory with.
    pub const GETDENTS64: u64 = 217;
    /// `openat(2)` — the one path in; `dirfd` must be `AT_FDCWD` (absolute
    /// paths only: this kernel has no working directory).
    pub const OPENAT: u64 = 257;
    /// `open(2)` — x86_64 keeps the legacy number, and musl's `open()` maps
    /// to it; same contract as `OPENAT` minus the dirfd.
    pub const OPEN: u64 = 2;
    /// `lstat(2)` — legacy number; musl's `lstat()` maps to it. Does not
    /// follow a final symlink.
    pub const LSTAT: u64 = 6;
    /// `fcntl(2)` — only the fd-flag commands busybox issues are served.
    pub const FCNTL: u64 = 72;
    /// `exit_group(2)` — musl's `exit(3)` calls this.
    pub const EXIT_GROUP: u64 = 231;

    // -- Stage 6, phase A: what apk.static needs to start (all x86_64
    // -- numbers; the aarch64 crate keeps the generic numbering).

    /// `fstat(2)` — x86_64's legacy number (aarch64 uses 80).
    pub const FSTAT: u64 = 5;
    /// `lseek(2)` — x86_64's legacy number (aarch64: 62).
    pub const LSEEK: u64 = 8;
    /// `uname(2)` (aarch64: 160).
    pub const UNAME: u64 = 63;
    /// `getcwd(2)` (aarch64: 17).
    pub const GETCWD: u64 = 79;
    /// `dup(2)` (aarch64: 23).
    pub const DUP: u64 = 32;
    /// `flock(2)` — stub: success, nothing locks (one task).
    pub const FLOCK: u64 = 73;
    /// `umask(2)` — stub: reports the conventional `0o022`.
    pub const UMASK: u64 = 95;
    /// `mremap(2)` — musl `realloc` on mmapped chunks.
    pub const MREMAP: u64 = 25;
    /// `statfs(2)` (aarch64: 43).
    pub const STATFS: u64 = 137;
    /// `fstatfs(2)` (aarch64: 44) — apk treats ENOSYS here as "forbidden".
    pub const FSTATFS: u64 = 138;
    /// `fchmod(2)` — stub: root's mode is the mode.
    pub const FCHMOD: u64 = 91;

    // -- Stage 6, phase C: unpack + commit (the *at family, x86_64).

    /// `mkdirat(2)` (aarch64: 34).
    pub const MKDIRAT: u64 = 258;
    /// `newfstatat(2)` — musl's stat()/fstatat() on x86_64 (aarch64: 79).
    pub const NEWFSTATAT: u64 = 262;
    /// `unlinkat(2)` (aarch64: 35).
    pub const UNLINKAT: u64 = 263;
    /// `renameat(2)` (aarch64: 38).
    pub const RENAMEAT: u64 = 264;
    /// `linkat(2)` — apk 3 prefers it for hard-linked package files.
    pub const LINKAT: u64 = 265;
    /// `symlinkat(2)` (aarch64: 36).
    pub const SYMLINKAT: u64 = 266;
    /// `readlinkat(2)` (aarch64: 78).
    pub const READLINKAT: u64 = 267;
    /// `fchmodat(2)` — stub like `FCHMOD`.
    pub const FCHMODAT: u64 = 268;
    /// `fchownat(2)` — stub: everything is root already.
    pub const FCHOWNAT: u64 = 260;
    /// `faccessat(2)` (aarch64: 48).
    pub const FACCESSAT: u64 = 269;

    // -- Stage 6 checklist: numbers the unhandled arm may still print, named
    // -- so a boot log reads as a plan. Most have no arm yet.

    /// `poll(2)` (ppoll is 271).
    pub const POLL: u64 = 7;
    /// `dup2(2)` (dup3 is 292).
    pub const DUP2: u64 = 33;
    /// `dup3(2)`.
    pub const DUP3: u64 = 292;
    /// `nanosleep` shares 35; `select(2)`.
    pub const SELECT_PSELECT6: u64 = 23;
    /// `sched_yield(2)`.
    pub const SCHED_YIELD: u64 = 24;
    /// `sendmsg(2)`.
    pub const SENDMSG: u64 = 46;
    /// `recvmsg(2)`.
    pub const RECVMSG: u64 = 47;
    /// `clone(2)` — phase D contingency (install scripts).
    pub const CLONE: u64 = 56;
    /// `execve(2)` — phase D contingency.
    pub const EXECVE: u64 = 59;
    /// `wait4(2)` — phase D contingency.
    pub const WAIT4: u64 = 61;
    /// `chdir(2)` (fchdir is 81).
    pub const CHDIR: u64 = 80;
    /// `fchdir(2)`.
    pub const FCHDIR: u64 = 81;
    /// `getrusage(2)`.
    pub const GETRUSAGE: u64 = 98;
    /// `sysinfo(2)`.
    pub const SYSINFO: u64 = 99;
    /// `sigaltstack(2)`.
    pub const SIGALTSTACK: u64 = 131;
    /// `pipe2(2)` — phase D contingency.
    pub const PIPE2: u64 = 293;
    /// `futex(2)`.
    pub const FUTEX: u64 = 202;
    /// `getsockname(2)`.
    pub const GETSOCKNAME: u64 = 51;
    /// `getpeername(2)`.
    pub const GETPEERNAME: u64 = 52;
    /// `setsockopt(2)`.
    pub const SETSOCKOPT: u64 = 54;
    /// `getsockopt(2)`.
    pub const GETSOCKOPT: u64 = 55;
    /// `set_robust_list(2)` — tolerated ENOSYS by sshd already.
    pub const SET_ROBUST_LIST: u64 = 273;
    /// `prlimit64(2)`.
    pub const PR_LIMIT64: u64 = 302;
    /// `kill(2)`.
    pub const KILL: u64 = 62;
    /// `utimensat(2)`.
    pub const UTIMENSAT: u64 = 88;

    // -- Stage 4: the socket family, the clock arms sshd's loop sleeps in,
    // -- and Akuma's private syscalls (0x1000 base on x86_64 — 300 is
    // -- `getcpu` here, so the AArch64 offsets cannot be reused; see
    // -- `libakuma`'s numbering note).

    /// `nanosleep(2)`.
    pub const NANOSLEEP: u64 = 35;
    /// `clock_nanosleep(2)`.
    pub const CLOCK_NANOSLEEP: u64 = 230;
    /// `socket(2)`.
    pub const SOCKET: u64 = 41;
    /// `connect(2)`.
    pub const CONNECT: u64 = 42;
    /// `accept(2)`.
    pub const ACCEPT: u64 = 43;
    /// `sendto(2)`.
    pub const SENDTO: u64 = 44;
    /// `recvfrom(2)`.
    pub const RECVFROM: u64 = 45;
    /// `shutdown(2)`.
    pub const SHUTDOWN: u64 = 48;
    /// `bind(2)`.
    pub const BIND: u64 = 49;
    /// `listen(2)`.
    pub const LISTEN: u64 = 50;
    /// `getppid(2)`.
    pub const GETPPID: u64 = 110;
    /// `gettid(2)`.
    pub const GETTID: u64 = 186;
    /// `getrandom(2)`.
    pub const GETRANDOM: u64 = 318;
    /// Akuma's private syscall base on x86_64.
    pub const AKUMA_PRIVATE_BASE: u64 = 0x1000;
    /// Akuma SPAWN: `spawn(path, argv, envp, stdin, stdin_len, flags)` —
    /// returns the packed `pid | stdout_fd << 32` word.
    pub const SPAWN: u64 = AKUMA_PRIVATE_BASE + 301;
    /// Akuma WAITPID: `waitpid(pid, &status)` — pid when reaped, 0 while
    /// running.
    pub const WAITPID: u64 = AKUMA_PRIVATE_BASE + 303;
    /// Akuma CLOSE_CHILD_STDIN: deliver EOF to a spawned child's stdin.
    pub const CLOSE_CHILD_STDIN: u64 = AKUMA_PRIVATE_BASE + 326;
    /// Akuma UPTIME: boot-relative time (libakuma's sshd/sh ask for it; the
    /// aarch64 offsets cannot be reused below 0x1000+301 on x86_64).
    pub const UPTIME: u64 = AKUMA_PRIVATE_BASE + 319;
}

/// `openat(2)` flag bits (x86_64) and `dirfd` special values.
pub mod open {
    /// `O_RDONLY` — 0, the only access mode a read-only root used to serve.
    pub const O_RDONLY: u64 = 0;
    /// `O_ACCMODE` mask: `O_RDONLY` is 0 — the only access mode the
    /// read-only-in-spirit root serves.
    pub const O_ACCMODE: u64 = 0b11;
    /// `O_WRONLY` — served once the root grew a write path (stage 6).
    pub const O_WRONLY: u64 = 0b01;
    /// `O_RDWR`.
    pub const O_RDWR: u64 = 0b10;
    /// `O_CREAT` — create when missing (stage 6: the root writes).
    pub const O_CREAT: u64 = 0x40;
    /// `O_TRUNC` — empty an existing file on open.
    pub const O_TRUNC: u64 = 0x200;
    /// `O_DIRECTORY` — the caller wants a directory; anything else is
    /// `ENOTDIR`.
    pub const O_DIRECTORY: u64 = 0x1_0000;
    /// `O_CLOEXEC` — recorded on the fd, meaningless until an exec family
    /// exists.
    pub const O_CLOEXEC: u64 = 0x8_0000;
    /// `AT_FDCWD`: the only `dirfd` this kernel serves (absolute paths only).
    pub const AT_FDCWD: u64 = (-100i64) as u64;
    /// `AT_SYMLINK_NOFOLLOW` — `newfstatat`'s lstat mode.
    pub const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
    /// `AT_REMOVEDIR` — `unlinkat` removing a directory.
    pub const AT_REMOVEDIR: u64 = 0x200;
    /// `AT_EMPTY_PATH` — operate on the `dirfd` itself.
    pub const AT_EMPTY_PATH: u64 = 0x1000;
}

/// errno values, negated on return.
pub mod err {
    /// Operation not permitted.
    pub const EPERM: u64 = 1;
    /// No such file or directory.
    pub const ENOENT: u64 = 2;
    /// I/O error (the filesystem refused a read the dispatcher asked for).
    pub const EIO: u64 = 5;
    /// No such device (mmap of something that is not anonymous or a device
    /// this kernel serves).
    pub const ENODEV: u64 = 19;
    /// Not a directory (`O_DIRECTORY` opened on a file; `getdents64` on an fd
    /// that is not a directory).
    pub const ENOTDIR: u64 = 20;
    /// Is a directory (`read(2)` on a directory fd).
    pub const EISDIR: u64 = 21;
    /// Permission denied (`openat` with a write access mode on a root this
    /// kernel does not write).
    pub const EACCES: u64 = 13;
    /// Bad file descriptor.
    pub const EBADF: u64 = 9;
    /// Out of memory.
    pub const ENOMEM: u64 = 12;
    /// Too many open files (the fd table's fixed slot ceiling).
    pub const EMFILE: u64 = 24;
    /// Bad address.
    pub const EFAULT: u64 = 14;
    /// Invalid argument.
    pub const EINVAL: u64 = 22;
    /// Inappropriate ioctl for device.
    pub const ENOTTY: u64 = 25;
    /// Function not implemented.
    pub const ENOSYS: u64 = 38;
    /// Address out of range (canonical forms): returned for a bad `arch_prctl`.
    pub const ERANGE: u64 = 34;
    /// Resource temporarily unavailable / operation would block (`O_NONBLOCK`
    /// on a socket with nothing to read or write; `try_accept` with an empty
    /// backlog).
    pub const EAGAIN: u64 = 11;
    /// No child processes (`waitpid` for a pid that is nobody's child).
    pub const ECHILD: u64 = 10;
    /// Address family not supported by the protocol (`socket(2)` for a
    /// family this stack does not serve — every family but `AF_INET`).
    pub const EAFNOSUPPORT: u64 = 47;
    /// Socket operation on a non-socket (an fd that is not a socket fd).
    pub const ENOTSOCK: u64 = 88;
    /// File exists (`O_CREAT`/`mkdirat` on a path that is already there).
    pub const EEXIST: u64 = 17;
    /// Illegal seek (`lseek` on a pipe/socket fd).
    pub const ESPIPE: u64 = 29;
    /// Directory not empty (`unlinkat AT_REMOVEDIR` on a full directory).
    pub const ENOTEMPTY: u64 = 39;
    /// No space left on the device.
    pub const ENOSPC: u64 = 28;
    /// Cross-device link (`rename`/`link` across mounts — one mount here).
    pub const EXDEV: u64 = 18;
}

/// `arch_prctl(2)` requests, x86_64.
pub mod arch {
    /// `ARCH_SET_FS` — set the FS base (the thread pointer on x86_64 musl).
    pub const ARCH_SET_FS: u64 = 0x1002;
    /// `ARCH_GET_FS` — read the FS base into `*addr`.
    pub const ARCH_GET_FS: u64 = 0x1003;
    /// `ARCH_SET_GS`.
    pub const ARCH_SET_GS: u64 = 0x1001;
    /// `ARCH_GET_GS`.
    pub const ARCH_GET_GS: u64 = 0x1004;
}

/// `mmap(2)` flags and `mprotect(2)` prots, x86_64 values.
pub mod mmap {
    /// `MAP_PRIVATE`.
    pub const MAP_PRIVATE: u64 = 0x02;
    /// `MAP_FIXED`.
    pub const MAP_FIXED: u64 = 0x10;
    /// `MAP_ANONYMOUS`.
    pub const MAP_ANONYMOUS: u64 = 0x20;
    /// `PROT_READ`.
    pub const PROT_READ: u64 = 0x1;
    /// `PROT_WRITE`.
    pub const PROT_WRITE: u64 = 0x2;
    /// `PROT_EXEC`.
    pub const PROT_EXEC: u64 = 0x4;
}
