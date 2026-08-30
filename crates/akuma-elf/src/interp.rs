//! Loading the dynamic linker (interpreter) into an address space that is
//! already being built for the main binary.
//!
//! The interpreter is always mapped eagerly — it runs immediately, so there is
//! nothing to gain by faulting it in — but *where its bytes come from* is a
//! build-profile decision, made in [`load_interp_for`]. That is the whole
//! source/mapping split: two profiles, two sources, one loader.

use alloc::collections::BTreeMap;
use alloc::string::String;

use elf::abi::{EM_AARCH64, PT_LOAD};

use akuma_mmu::UserAddressSpace;

use super::load::{apply_relocations, map_segment_eager};
use super::source::{ElfSource, parse_headers};
use super::types::{DEBUG_ELF_LOADING, ElfError, INTERP_BASE, InterpInfo};

/// Resolve a PT_INTERP path against an optional rootfs prefix.
///
/// Containers keep their interpreter under a prefix, so `/lib/ld-musl.so.1`
/// becomes `<prefix>/lib/ld-musl.so.1`.
fn resolve_interp_path(ipath: &str, prefix: Option<&str>) -> String {
    match prefix {
        Some(prefix) => {
            let mut p = String::from(prefix);
            if !p.ends_with('/') && !ipath.starts_with('/') {
                p.push('/');
            }
            p.push_str(ipath);
            p
        }
        None => String::from(ipath),
    }
}

/// Resolve and load the interpreter named by a binary's PT_INTERP.
///
/// The source is chosen by build profile, not by how the main binary was
/// loaded. On the size profile the heap seed is only 1 MB: slurping a ~600 KB
/// interpreter into the heap exhausts the seed before `PmmOomHandler` can grow
/// it, leaving no room for the process being spawned. `ElfSource::Path` reads
/// each PT_LOAD page with a single `read_at` (4 KB scratch buffer, freed
/// immediately), keeping peak heap use under 10 KB regardless of interpreter
/// size. Every other profile can afford the slurp and prefers it, because one
/// large sequential read beats hundreds of 4 KB ones.
pub(super) fn load_interp_for(
    ipath: &str,
    prefix: Option<&str>,
    address_space: &mut UserAddressSpace,
) -> Result<InterpInfo, ElfError> {
    let resolved = resolve_interp_path(ipath, prefix);
    if DEBUG_ELF_LOADING {
        log::debug!("[ELF] Loading interpreter: {}", resolved);
    }

    #[cfg(kernel_profile_extreme)]
    let interp_info = load_interpreter(ElfSource::Path(&resolved), address_space)?;

    #[cfg(not(kernel_profile_extreme))]
    let interp_info = {
        // M5c hold-shortening: drop the BKL around the dynamic-interpreter whole-file read
        // (a second ~1 MB read for dynamically-linked binaries), mirroring the main-binary
        // read in `do_execve`. Safe: `address_space` is a private, not-yet-installed AS,
        // and the read touches only VFS/block + the self-locked heap — no BKL state is
        // mutated during the window. Re-take BEFORE the `?` so an error return doesn't leave
        // the BKL dropped for the caller. No-op off shared-SMP (`bkl` calls compile away).
        let drop_bkl = (crate::vfs().exec_bkl_drop_enabled)();
        if drop_bkl {
            akuma_bkl::bkl::leave_kernel();
        }
        let read_res = (crate::vfs().read_file)(&resolved);
        if drop_bkl {
            akuma_bkl::bkl::enter_kernel();
        }
        let interp_data =
            read_res.map_err(|_| ElfError::InvalidFormat("Cannot read interpreter"))?;
        load_interpreter(ElfSource::Bytes(&interp_data), address_space)?
    };

    if DEBUG_ELF_LOADING {
        log::debug!(
            "[ELF] Interpreter loaded at base=0x{:x} entry=0x{:x}",
            interp_info.base_addr,
            interp_info.entry_point
        );
    }
    Ok(interp_info)
}

/// Map the interpreter at `INTERP_BASE` and relocate it so it can self-bootstrap.
fn load_interpreter(
    src: ElfSource<'_>,
    address_space: &mut UserAddressSpace,
) -> Result<InterpInfo, ElfError> {
    let headers = parse_headers(src)?;

    if headers.ehdr.e_machine != EM_AARCH64 {
        return Err(ElfError::WrongArchitecture);
    }
    if headers.segments().is_empty() {
        return Err(ElfError::InvalidFormat("Interpreter has no program headers"));
    }

    let base = INTERP_BASE;
    let entry_point = base + headers.ehdr.e_entry as usize;

    let mut mapped_pages: BTreeMap<usize, usize> = BTreeMap::new();
    for phdr in headers.segments().iter() {
        if phdr.p_type != PT_LOAD {
            continue;
        }
        map_segment_eager(src, address_space, base, &phdr, &mut mapped_pages)?;
    }

    // Both .rela.dyn (DT_RELA) and .rela.plt (DT_JMPREL) are covered — the pass
    // walks every SHT_RELA section.
    let applied = apply_relocations(src, &headers, base, address_space, &mapped_pages)?;

    if DEBUG_ELF_LOADING {
        log::debug!("[ELF] Interpreter: applied {} relocations", applied);
        log::debug!(
            "[ELF] Interpreter: entry=0x{:x} pages={}",
            entry_point,
            mapped_pages.len()
        );
    }

    Ok(InterpInfo {
        entry_point,
        base_addr: base,
    })
}
