//! Loading an ELF image for this target: `akuma-elf` does it.
//!
//! [`load`] and [`load_path`] call `akuma_elf::load_elf` /
//! `load_elf_eager_from_path`, which build the address space, map every
//! `PT_LOAD` and load the interpreter, then check the entry point against the
//! page tables the load wrote. The initial stack is `akuma_elf::attach_stack`,
//! the AArch64 kernel's builder (`Image::with_stack`): the same argv/envp/auxv
//! block, at [`crate::usermode::ELF_STACK_TOP`] rather than above the image.
//! This file built its own until 2026-11-12
//! (`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`, "One initial-stack builder").
//!
//! # Where things go
//!
//! | | this target | AArch64 |
//! |---|---|---|
//! | `PIE_BASE` | `0x1000_0000` | the same |
//! | `INTERP_BASE` | `0x3000_0000` | the same |
//! | stack top | [`crate::usermode::ELF_STACK_TOP`], the top of the lower half, as on Linux | `compute_stack_top(brk, has_interp)`, at most `0x40_0000_0000` |
//! | mmap window | `mm::MMAP_BASE` up to 1 MiB below the stack (`ProcessMemory::mmap_limit`) | the interpreter's end up to the stack |
//!
//! The stack stays at the top here because programs hint `mmap` addresses
//! above 256 GiB (Go's arenas start at `0xc0_0000_0000`), and a stack at
//! AArch64's place would sit in the middle of them.
//!
//! # Refusals
//!
//! * **The entry point** must be a non-zero user address in an executable
//!   page, read back from the page tables ([`load`]).
//! * A segment outside the lower half is refused by `akuma_mmu`'s x86 walk.
//! * W^X is `akuma-elf`'s: `SegProt` has two variants and `PF_X` wins, so a
//!   writable+executable segment loads read-execute.
//! * An argv/envp that does not fit the eager stack is refused by
//!   `akuma_elf::setup_linux_stack` (`E2BIG` from `execve`); [`MAX_ARGV`] and
//!   [`MAX_ENVP`] bound the entry counts before that.



use akuma_elf::ElfError;
use akuma_mmu::UserAddressSpace;

#[cfg(not(feature = "no-tests"))]
use crate::phys::phys_ptr;

const PAGE_SIZE: usize = 4096;

/// First address that is not userspace: PML4 slot 256 and up is the kernel's.
///
/// Only [`load`]'s entry-point check reads it now. The *segment* check that
/// used to is `akuma_mmu::USER_HALF_END`, enforced inside the walk.
const USER_VA_LIMIT: u64 = 0x0000_8000_0000_0000;

/// What a successful load produced.
///
/// The two facts about an `akuma_elf::LoadedElf` this target reads after the
/// stack is built: where ring 3 is entered (the *linker's* entry for a dynamic
/// image, which `LoadedElf` keeps in `interp`) and the page-aligned end of the
/// image, where `brk` starts.
pub struct LoadedImage {
    /// Where `enter_user_mode` jumps. `base + e_entry` for a static image, and
    /// the **dynamic linker's** entry for one with a `PT_INTERP` — the linker
    /// brings the program up and jumps to it itself.
    pub entry: u64,
    /// Page-aligned end of the highest `PT_LOAD`. Where a `brk` heap would
    /// start.
    pub end_va: u64,
}

/// Load `image` into a fresh address space through [`akuma_elf::load_elf`].
///
/// # Why this returns the address space rather than filling one
///
/// `load_elf` builds it: `A::new_space()` is the first thing it does, because
/// the image's own headers decide how many page tables it needs and the loader
/// is the only thing that has read them. The old signature took `&mut
/// UserAddressSpace` because it was the *caller* that made one. On failure the
/// space is dropped inside the crate and its destructor returns every frame the
/// half-finished load had taken — which is what `elf: rejected loads leak
/// nothing` checks on real hardware.
///
/// # The eager strategy, and why not the deferred one
///
/// [`akuma_elf::load_elf`] maps every page of every `PT_LOAD` up front.
/// `load_elf_from_path` would register demand-paged lazy regions instead, and
/// this target still cannot use it — but not for the reason once written
/// here: both file-reading hooks (`read_at`, wired in C2 slice 7; `resolve_file_id`,
/// wired 2026-09-17 once C1 step 4a gave this target a real mount table to key
/// on — `AKUMA_AMD64_BOX_SPAWN_EXT.md`) are real now, not `exec_runtime.rs`
/// stubs. What is still true is that this target's own `#PF` handler pages
/// from `Process::regions` (`akuma-mmap`), not from `akuma-exec`'s lazy-region
/// table — the two file-reading hooks were never the only thing blocking this,
/// and fixing them alone does not unblock it.
pub fn load(image: &[u8]) -> Result<(akuma_elf::LoadedElf<UserAddressSpace>, LoadedImage), &'static str> {
    let loaded = akuma_elf::load_elf::<UserAddressSpace>(image, None).map_err(|e| elf_err(&e))?;
    finish(loaded)
}

/// As [`load`], reading the image off the file at `path` a window at a time
/// ([`akuma_elf::load_elf_eager_from_path`]) instead of from a whole-file copy.
///
/// What `execve` and `sys_spawn` use since 2026-10-08. The copy they made
/// before (`fs::read_image`) put the executable's whole size on the kernel
/// heap: Chromium is 250 MB and re-execs itself, and its second copy did not
/// fit a 512 MiB heap (`docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md`).
/// The mapping stays eager; only where the bytes come from changed. Each read
/// goes through the `read_at` hook, which drops the BKL around it
/// (`exec_runtime::bkl_free_io`), so a slow disk still does not hold the lock.
///
/// `path` must already be symlink-resolved.
pub fn load_path(path: &str) -> Result<(akuma_elf::LoadedElf<UserAddressSpace>, LoadedImage), &'static str> {
    let loaded = akuma_elf::load_elf_eager_from_path::<UserAddressSpace>(path, None)
        .map_err(|e| elf_err(&e))?;
    finish(loaded)
}

/// The checks and the [`LoadedImage`] both loaders share.
fn finish(
    loaded: akuma_elf::LoadedElf<UserAddressSpace>,
) -> Result<(akuma_elf::LoadedElf<UserAddressSpace>, LoadedImage), &'static str> {
    // The cross target is the shared decision (akuma-elf-exec): the linker's
    // entry when an interpreter was mapped — ring 3 is entered in the linker,
    // which brings the program up itself — else the program's.
    let (entry, prog_entry) = {
        let prog_entry = loaded.entry_point as u64;
        let interp_entry = loaded.interp.as_ref().map_or(0, |i| i.entry_point as u64);
        (
            akuma_elf_exec::entry_to_cross(loaded.interp.is_some(), prog_entry, interp_entry),
            prog_entry,
        )
    };

    // The three refusals `place_image` made after placing an image, kept
    // because they cost no second parse — the answers are read back out of the
    // page tables the loader just wrote, which is what the hardware will do
    // rather than what the loader believes it did.
    if prog_entry == 0 || prog_entry >= USER_VA_LIMIT {
        return Err("entry point is not a user address");
    }
    if entry == 0 || entry >= USER_VA_LIMIT {
        return Err("interpreter entry point is not a user address");
    }
    if loaded
        .address_space
        .pte_prot(entry as usize & !(PAGE_SIZE - 1))
        .is_none_or(|(p, _cow)| !p.exec)
    {
        return Err("entry point is not in an executable segment");
    }

    let img = LoadedImage {
        entry,
        // The heap starts past the program, not past the linker: `brk` grows up
        // from the program image and the linker sits above it either way.
        // `LoadedElf::brk` is the highest `p_vaddr + p_memsz`, unrounded.
        end_va: align_up(loaded.brk as u64, PAGE_SIZE as u64),
    };
    Ok((loaded, img))
}

/// [`ElfError`] as one of this module's `&'static str`s.
///
/// A match rather than `Display`: the caller's error channel is a `&'static
/// str` all the way up to the boot suite and `execve`'s errno mapping, and
/// rendering through `format!` to get one would be a heap allocation on the
/// path that is reporting a failure. The arms that carry their own `&'static
/// str` pass it through, so a `MappingFailed("Out of memory for user page")`
/// still says which resource ran out.
pub fn elf_err(e: &ElfError) -> &'static str {
    match *e {
        ElfError::InvalidFormat(m) | ElfError::MappingFailed(m) => m,
        ElfError::InvalidMagic(_) => "not an ELF image",
        ElfError::WrongArchitecture => "not an x86-64 image",
        ElfError::NotExecutable => "not ET_EXEC or ET_DYN",
        ElfError::DynamicallyLinked => "dynamically linked and no interpreter could be loaded",
        ElfError::OutOfMemory => "out of memory loading an image",
        ElfError::AddressSpaceFailed => "no frame for a PML4",
    }
}

/// Round `v` up to the next multiple of `to`.
const fn align_up(v: u64, to: u64) -> u64 {
    v.div_ceil(to) * to
}

#[cfg(not(feature = "no-tests"))]
/// Read `dst.len()` bytes out of `space` at `va`, through the physmap.
///
/// Test-only: nothing in the running
/// kernel reads a *foreign* address space this way — a live one is read through
/// `uaccess` with `CR3` already naming it. The self-test that checks what the
/// stack builder actually wrote has no such luxury, since the space it is
/// inspecting has never been entered.
#[allow(unsafe_code)] // boot-suite probe; gated by `no-tests` (lib.rs)
pub fn read_user(space: &UserAddressSpace, va: u64, dst: &mut [u8]) -> bool {
    let mut done = 0usize;
    while done < dst.len() {
        let at = va + done as u64;
        let page = at & !(PAGE_SIZE as u64 - 1);
        let off = (at - page) as usize;
        let n = (PAGE_SIZE - off).min(dst.len() - done);
        let Some(pa) = space.translate(page as usize) else {
            return false;
        };
        // SAFETY: `pa` came from a walk of this space's own tables, so it is a
        // frame the space owns, and `off + n` is bounded by PAGE_SIZE.
        unsafe {
            core::ptr::copy_nonoverlapping(
                phys_ptr::<u8>(pa as u64).add(off),
                dst.as_mut_ptr().add(done),
                n,
            );
        }
        done += n;
    }
    true
}

/// The most argv entries the initial stack builder will place.
///
/// **Was 16 until 2026-09-12, and that cost a week of the wrong suspicion.**
/// A shell invoked as `sh -c "<cmd>"` needs three, which is what the original
/// bound was sized for; a *toolchain* does not. `rustc`'s call to its linker
/// passes about fifty arguments and `collect2`'s call to `ld` a similar number,
/// and the sixteenth onward were silently dropped — `sys_execve` truncated
/// rather than refusing. The linker therefore ran with a command line that was
/// valid, shorter, and missing its `-o` and its input files, so it reported
/// "no input files" and `cannot open output file a.out`, and every
/// investigation went looking at `collect2` and at path resolution
/// (`docs/archive/RUST_TOOLCHAIN_AMD64.md`, "Open after session 2", item 1).
///
/// The probe that settles it in one line, in the guest:
/// `busybox echo a b c … z` prints exactly fourteen letters — sixteen argv
/// entries counting `busybox` and `echo`.
///
/// **1024 since 2026-09-13, and 256 was not enough to build this kernel.**
/// `cargo`'s `rustc` invocation for one crate of the kernel's own graph is
/// **300 arguments** (measured: `akuma-syscalls-glue`, 12 570 bytes of command
/// line, longest single argument 156 bytes) — ~100 `--extern name=path`, ~40
/// `--allow=clippy::…`, the `--check-cfg` set. At 256 the `execve` was `E2BIG`
/// and the self-host build stopped there, with cargo reporting only
/// `could not compile` and no diagnostic, because rustc never ran to emit one.
///
/// Linux caps the *bytes* (`ARG_MAX`, ~2 MB) and never the count; this cap is a
/// sanity bound, so it is set well past what the workload needs rather than
/// just past it. The byte budget is `usermode::user_strv`'s.
pub const MAX_ARGV: usize = 1024;

/// The most envp entries the initial stack builder will place.
///
/// **256 since 2026-09-13, and the 64 it replaces was the wall every
/// proc-macro crate hit.** The note here used to say 64 "covers a login
/// shell's `PATH`/`HOME`/`TERM`/… and the dozen `CARGO_*`/`RUST*` variables a
/// build adds". A dozen is what a *rustc* invocation adds. A **build script**
/// gets the full `CARGO_CFG_*` / `CARGO_PKG_*` / `CARGO_FEATURE_*` set —
/// measured, 56 variables cargo sets itself — plus whatever the shell already
/// had, which lands a `cargo build` at 60-64 entries: right on the cap, so
/// whether a crate built at all depended on how many features it had and how
/// many variables the operator happened to export.
///
/// The failure was silent in the worst way. `execve` refuses rather than
/// truncating (correctly — silent argv truncation is what made every linker run
/// without its inputs, see [`MAX_ARGV`]), so the child got `E2BIG`, `_exit(1)`d
/// before running a line of its own code, and cargo reported
/// `process didn't exit successfully … (exit status: 1)` with **no stdout and
/// no stderr**, because the program had never started. `proc-macro2`'s build
/// script is the first one every derive macro needs, so `thiserror`, `serde`,
/// `zerocopy` and everything like them failed at exactly that line with nothing
/// to read (`docs/archive/RUST_TOOLCHAIN_AMD64.md` § session 5).
///
/// The one-line reproduction, in the guest: take any crate with a build script
/// that builds, `export` two more variables, and build it again.
///
/// Linux caps the *bytes* (`ARG_MAX`), not the entry count. 256 is four times
/// what a cargo build script needs and symmetric with [`MAX_ARGV`].
pub const MAX_ENVP: usize = 256;

