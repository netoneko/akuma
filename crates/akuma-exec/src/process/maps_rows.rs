//! The `/proc/<pid>/maps` rows: what the region records say, plus what the
//! hardware says about the pages the loader placed.
//!
//! Moved verbatim from the amd64 kernel's `fd.rs` (PHASE-2-PROCFS, D27): the walk
//! reads only shared [`Process`](super::Process) facts, both kernels register it
//! (`VfsGlueHooks::pid_map_rows`), and a second copy in `akuma-runtime-tokio`
//! would have forked exactly the decisions this walk carries — the owner-walk,
//! the extent skip, the leaf runs, and the freed-tables re-check
//! (`docs/archive/PROC_MAPS_WALK_FREED_TABLES.md`). The *format* stays in
//! `akuma-procfs::render_maps_line`; this module only decides the rows.

use alloc::vec::Vec;

/// One `/proc/<pid>/maps` row: `(start, end, read, write, exec, private)`.
///
/// `akuma-vfs-glue` re-exports this alias, so its `VfsGlueHooks::pid_map_rows`
/// signature did not change when the walk moved here.
pub type MapRow = (usize, usize, bool, bool, bool, bool);

/// The rows `/proc/<pid>/maps` renders, ascending, for the process `pid`.
///
/// `None` when `pid` has no visible address space (the file is *absent* rather
/// than empty — `akuma-vfs-glue`'s rule).
///
/// This is the **metal** registration (the registered `Process` owns its
/// tables, `is_shared()` marks only a `CLONE_VM` member). The tokio kernels
/// register [`pid_map_rows_of_kernel_owned_roots`] — every `Process` there is
/// a borrowed view of a kernel-owned root, so the owner resolution this fn
/// starts from answers `None` for every process; see that fn's doc.
pub fn pid_map_rows(pid: u32) -> Option<Vec<MapRow>> {
    // Walk the address space's **owner**, not `pid`. A `/proc/<tid>/maps` read
    // for a `CLONE_VM` thread reaches the leader's page tables, but the thread's
    // own `address_space` lock is a different lock from the leader's — so the
    // leader's `execve` (which frees the old tables) or exit ran alongside the
    // walk, and the walk followed a frame field out of a freed-and-reused table
    // into an unmapped physmap address (`docs/archive/PROC_MAPS_WALK_FREED_TABLES.md`).
    // The leader also holds the regions; a member's list is empty.
    let (owner, l0) = crate::process::address_space_owner_of(pid)?;
    let mut rows = mmap_region_rows(owner)?;
    // Sorted because `collect_leaf_runs` binary-searches it: the region list
    // itself is in no particular order, and a linear "is this page already
    // covered?" test per leaf is the second half of the `mmap` blowup in
    // `docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §5 — reading a
    // 1500-region process's `maps` cost 1500 comparisons for every one of its
    // resident pages, which is why a 64 KB read of the file returned in under a
    // second and a 2 MB read of the same file never returned at all.
    let mut extents: Vec<(usize, usize)> = rows.iter().map(|r| (r.0, r.1)).collect();
    extents.sort_unstable();
    let leaves = crate::process::with_process(owner, |p| {
        let uas = p.address_space.lock();
        // `execve` swaps the space under this lock; if it did between the owner
        // lookup and here, the tables `l0` named are freed. Report no such file
        // rather than walk them.
        if p.address_space.is_shared() || p.address_space.l0_phys() != l0 {
            return None;
        }
        Some(collect_leaf_runs(&uas, &extents))
    })??;
    rows.extend(leaves);
    rows.sort_unstable_by_key(|r| r.0);
    Some(rows)
}

/// `(start, end, r, w, x, private)` for every recorded `mmap` region of `pid`,
/// in no particular order (the caller sorts).
fn mmap_region_rows(pid: u32) -> Option<Vec<MapRow>> {
    crate::process::with_process(pid, |p| {
        p.mmap_regions
            .lock()
            .iter()
            .map(|r| {
                let prot = r.recorded_prot().unwrap_or(akuma_mmap::Prot::RW_NO_EXEC);
                (
                    r.start_va,
                    r.start_va.saturating_add(r.len_bytes()),
                    !prot.is_none(),
                    prot.is_write(),
                    prot.is_exec(),
                    !r.shared_anon,
                )
            })
            .collect::<Vec<MapRow>>()
    })
}

/// [`pid_map_rows`] for a kernel whose registered `Process` holds only a
/// *borrowed view* of a root the kernel itself owns — the tokio kernels
/// (`disown_space`, `akuma-tokio-kernel-core/src/proc.rs`: `OWNED_ROOT` per
/// slot, a dead image's root parked rather than freed, the kernel freeing
/// roots at the reap). There `is_shared()` is the ownership *model*, not the
/// metal's `CLONE_VM` hazard, and [`pid_map_rows`]'s first step —
/// `address_space_owner_of`, whose answer for a shared process with no
/// non-shared owner is `None` — would mean "tables freed, do not walk" on the
/// metal but "kernel-owned" here; observed first-hand as a permanently absent
/// `/proc/self/maps` (PHASE-2-PROCFS gate run 2026-11-14: `MAPS RED errno=2`
/// beside a working `/proc/self/auxv` — same hook table, so the divergence was
/// the address-space model, not the registration).
///
/// So this walk resolves no owner: it reads the thread-group leader's recorded
/// regions (a member's list is empty; every view of the group names the
/// leader's root) and walks **`pid`'s own view** under its own lock,
/// re-checked by the same l0 compare the metal walk uses — a concurrent exec
/// swaps the view's root (`disown_space`, which like this walk runs under
/// `with_process`'s exclusive slot hold, so the swap cannot land inside a
/// walk), and the compare reports no such file for that read.
///
/// Residual, recorded: a sibling view's fault flips a PTE without this view's
/// lock (per-view locks over one root), so a run may miss a page placed
/// mid-walk — the staleness class the kernel's own reclaim sweep already
/// walks under (`try_evict_ro_page` takes its view's lock; sibling-view
/// faults are that path's residual too). The metal must not register this fn:
/// a metal `CLONE_VM` member's own lock excludes nothing, which is the
/// freed-tables hazard (`docs/archive/PROC_MAPS_WALK_FREED_TABLES.md`) the
/// owner resolution exists to avoid.
pub fn pid_map_rows_of_kernel_owned_roots(pid: u32) -> Option<Vec<MapRow>> {
    let (l0, tgid) =
        crate::process::with_process(pid, |p| (p.address_space.l0_phys(), p.tgid))?;
    let mut rows = mmap_region_rows(tgid)?;
    let mut extents: Vec<(usize, usize)> = rows.iter().map(|r| (r.0, r.1)).collect();
    extents.sort_unstable();
    let leaves = crate::process::with_process(pid, |p| {
        let uas = p.address_space.lock();
        if p.address_space.l0_phys() != l0 {
            return None;
        }
        Some(collect_leaf_runs(&uas, &extents))
    })??;
    rows.extend(leaves);
    rows.sort_unstable_by_key(|r| r.0);
    Some(rows)
}

/// Present user leaves outside every extent in `skip`, coalesced into runs.
///
/// `skip` must be **sorted by start**, which [`covered_by`] relies on; the extents
/// in it never overlap (regions do not — `MAP_FIXED` unmaps its range before it
/// records one, and every other placement comes from `find_free_va`).
///
/// `for_each_user_leaf` visits in ascending VA order (it walks each level's
/// indices upward), which is what makes a single-pass coalesce correct rather
/// than a sort-then-merge.
///
/// A run is not a VMA: two adjacent loader mappings with identical permissions
/// merge into one line. That is what the hardware says, which is the only
/// record this target keeps for pages the loader placed.
#[cfg(target_arch = "x86_64")]
fn collect_leaf_runs(uas: &akuma_mmu::UserAddressSpace, skip: &[(usize, usize)]) -> Vec<MapRow> {
    let mut rows: Vec<MapRow> = Vec::new();
    let mut run: Option<MapRow> = None;
    uas.for_each_user_leaf(|leaf| {
        let (va, prot) = (leaf.va, leaf.prot);
        if !prot.user || covered_by(skip, va) {
            // Flush across a gap a region already covers, so a mapping either
            // side of it is not merged through it.
            if let Some(r) = run.take() {
                rows.push(r);
            }
            return;
        }
        // Every present leaf is readable — x86 has no read-disable bit, so `r`
        // is not a fact the PTE can carry differently.
        let (w, x) = (prot.write, prot.exec);
        match run {
            Some(ref mut r) if r.1 == va && r.3 == w && r.4 == x => r.1 = va + 4096,
            _ => {
                if let Some(r) = run.take() {
                    rows.push(r);
                }
                run = Some((va, va + 4096, true, w, x, true));
            }
        }
    });
    if let Some(r) = run.take() {
        rows.push(r);
    }
    rows
}

/// The AArch64 `UserAddressSpace` has no whole-space leaf walk (`for_each_user_leaf` is the amd64 struct's; the walk
/// was written and tested only there, PHASE-2-PROCFS), so this target reports the region rows alone — what its
/// `/proc/<pid>/maps` showed before the walk moved into this crate. Gating, not porting: porting the walk is its own
/// piece of work with its own tests.
#[cfg(not(target_arch = "x86_64"))]
fn collect_leaf_runs(_uas: &akuma_mmu::UserAddressSpace, _skip: &[(usize, usize)]) -> Vec<MapRow> {
    Vec::new()
}

/// Does any extent in the sorted, non-overlapping `skip` contain `va`?
///
/// `partition_point` lands one past the last extent starting at or below `va`,
/// so the only extent that can contain it is the one before that — the reason
/// this is O(log n) rather than the linear `any` it replaced. `pub` because the
/// amd64 kernel's boot self-test suite (its `maps_skip_lookup_check`) still
/// exercises it where this crate cannot host-test (kernel-only deps).
pub fn covered_by(skip: &[(usize, usize)], va: usize) -> bool {
    match skip.partition_point(|&(s, _)| s <= va).checked_sub(1) {
        Some(i) => va < skip[i].1,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::covered_by;

    /// [`covered_by`] must answer exactly what the linear scan it replaced
    /// answered (moved with it from the amd64 kernel's `fd.rs`).
    ///
    /// The swap is invisible when it is wrong. A `/proc/<pid>/maps` that loses a
    /// hit prints a page twice — once from its region and once as a leaf run —
    /// and one that gains a hit silently drops a run; both render a
    /// plausible-looking file, and the only reader that would notice is a person
    /// reading it. So the search is checked against its own predecessor as an
    /// oracle, page by page across the shapes the real extent list has: an
    /// extent at the very first address, two abutting extents, a one-page hole,
    /// a wide one, and the space either side of the whole list.
    #[test]
    fn maps_skip_lookup_agrees_with_linear_scan() {
        const PG: usize = 4096;
        let base = 0x1_0000_0000usize;
        // Out of order on the way in, because the caller is what sorts: this is
        // the case that says the sort is load-bearing rather than incidental.
        let mut skip = [
            (base + 8 * PG, base + 12 * PG),
            (base, base + PG),
            (base + 2 * PG, base + 3 * PG),
            (base + 3 * PG, base + 4 * PG),
        ];
        skip.sort_unstable();
        for i in 0..20usize {
            let va = (base - 2 * PG) + i * PG;
            let oracle = skip.iter().any(|&(s, e)| va >= s && va < e);
            assert_eq!(covered_by(&skip, va), oracle, "va={va:#x}");
        }
    }

    #[test]
    fn empty_skip_list_covers_nothing() {
        let base = 0x1_0000_0000usize;
        assert!(!covered_by(&[], base));
    }
}
