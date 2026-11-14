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
pub fn pid_map_rows(pid: u32) -> Option<Vec<MapRow>> {
    // Walk the address space's **owner**, not `pid`. A `/proc/<tid>/maps` read
    // for a `CLONE_VM` thread reaches the leader's page tables, but the thread's
    // own `address_space` lock is a different lock from the leader's — so the
    // leader's `execve` (which frees the old tables) or exit ran alongside the
    // walk, and the walk followed a frame field out of a freed-and-reused table
    // into an unmapped physmap address (`docs/archive/PROC_MAPS_WALK_FREED_TABLES.md`).
    // The leader also holds the regions; a member's list is empty.
    let (owner, l0) = crate::process::address_space_owner_of(pid)?;
    let regions = crate::process::with_process(owner, |p| {
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
    })?;
    let mut rows = regions;
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
