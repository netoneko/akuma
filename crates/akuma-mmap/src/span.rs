//! Page-span arithmetic: which pages a range touches, and how a byte range
//! splits across page boundaries.
//!
//! # Why this is here and not in the loader
//!
//! Extracted from `akuma-elf` on 2026-08-30. Every one of these was previously
//! computed inline, immediately adjacent to a raw pointer write — the ELF
//! segment loader recomputed a page span and a per-page copy window in the same
//! statement list as its `copy_nonoverlapping`, and `UserStack` recomputed
//! `frame_idx`/`offset`/`chunk_len` in three separate loops. Arithmetic in that
//! position is the worst kind to get wrong: an off-by-one becomes an
//! out-of-bounds *write*, not a wrong answer, and none of it could be tested
//! without booting a VM and running an ELF.
//!
//! It is arithmetic over `usize`, so it belongs in the crate that defines
//! [`PAGE_SIZE`](crate::PAGE_SIZE) and already does clip-and-split span math for
//! `MmapRegion`. Nothing here touches memory; the whole module is `const fn` and
//! a safe iterator, inside this crate's `#![forbid(unsafe_code)]`.

use crate::PAGE_SIZE;

/// One page-bounded slice of a byte range being written across frames.
///
/// A `Chunk` never crosses a page boundary — that is the invariant the callers
/// rely on to index a single frame per write, and
/// [`chunks_never_cross_a_page_boundary`](self) pins it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    /// Index of the page, counted from the iterator's `base_va`.
    pub page_index: usize,
    /// Byte offset of this chunk within its page.
    pub offset: usize,
    /// Length of this chunk in bytes. Always `> 0` and `<= PAGE_SIZE - offset`.
    pub len: usize,
    /// Offset of this chunk within the source buffer.
    pub src_offset: usize,
}

/// Splits `[start_va, start_va + len)` into per-page [`Chunk`]s, with page
/// indices counted from `base_va`.
///
/// `base_va` must be page-aligned and `<= start_va`; the loader's `base_va` is a
/// stack's bottom and `start_va` its current `sp`.
// Not `Copy`: an iterator that silently duplicates on assignment is a footgun —
// `for c in chunks` would consume a copy and leave the original unadvanced.
#[derive(Debug, Clone)]
pub struct PageChunks {
    base_va: usize,
    start_va: usize,
    len: usize,
    written: usize,
}

impl PageChunks {
    #[must_use]
    pub const fn new(base_va: usize, start_va: usize, len: usize) -> Self {
        Self { base_va, start_va, len, written: 0 }
    }
}

impl Iterator for PageChunks {
    type Item = Chunk;

    fn next(&mut self) -> Option<Chunk> {
        if self.written >= self.len {
            return None;
        }
        let va = self.start_va + self.written;
        let page_index = (va - self.base_va) / PAGE_SIZE;
        let offset = va % PAGE_SIZE;
        // `PAGE_SIZE - offset` is what stops a chunk crossing a page boundary.
        let len = min_usize(self.len - self.written, PAGE_SIZE - offset);
        let src_offset = self.written;
        self.written += len;
        Some(Chunk { page_index, offset, len, src_offset })
    }
}

const fn min_usize(a: usize, b: usize) -> usize {
    if a < b { a } else { b }
}

/// The page span a segment occupies: `(first page VA, page count)`.
///
/// `vaddr` need not be page-aligned — the first page starts at the page
/// containing it, and the count covers through the page containing the last
/// byte. A zero-length segment occupies **no** pages.
#[must_use]
pub const fn segment_span(vaddr: usize, memsz: usize) -> (usize, usize) {
    let start_page = vaddr & !(PAGE_SIZE - 1);
    if memsz == 0 {
        return (start_page, 0);
    }
    let end_page = (vaddr + memsz).div_ceil(PAGE_SIZE) * PAGE_SIZE;
    (start_page, (end_page - start_page) / PAGE_SIZE)
}

/// Where a segment's file bytes land in one of its pages.
///
/// Returns `(offset within the page, offset within the segment's file bytes,
/// length)`, or `None` when the page holds no file-backed bytes at all — either
/// it starts past `filesz` (pure `.bss`, and the frame arrived zeroed) or the
/// window is empty.
///
/// The two asymmetric cases are the ones worth reading twice: a page **before**
/// `vaddr` (an unaligned segment's first page) has its copy start part-way in,
/// while a page **after** `vaddr` starts at offset 0 but part-way through the
/// file bytes.
#[must_use]
pub const fn segment_page_copy(
    page_va: usize,
    vaddr: usize,
    filesz: usize,
) -> Option<(usize, usize, usize)> {
    let src_offset = page_va.saturating_sub(vaddr);
    if src_offset >= filesz {
        return None;
    }
    let dst_offset = vaddr.saturating_sub(page_va);
    if dst_offset >= PAGE_SIZE {
        return None;
    }
    let len = min_usize(PAGE_SIZE - dst_offset, filesz - src_offset);
    if len == 0 {
        return None;
    }
    Some((dst_offset, src_offset, len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    // ---- segment_span --------------------------------------------------

    #[test]
    fn span_of_an_aligned_single_page() {
        assert_eq!(segment_span(0x1000, PAGE_SIZE), (0x1000, 1));
    }

    #[test]
    fn span_of_an_unaligned_segment_covers_both_pages() {
        // One byte before a boundary, two bytes long -> two pages.
        assert_eq!(segment_span(0x1fff, 2), (0x1000, 2));
    }

    /// A zero-length segment occupies no pages.
    ///
    /// **This is a deliberate behaviour change, and the old behaviour was
    /// incoherent.** The pre-extraction expression was
    /// `end_page = (vaddr + memsz + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)`, which for
    /// `memsz == 0` gives:
    ///
    /// - page-aligned `vaddr` (`0x1000`): `end_page == start_page` -> **0** pages
    /// - unaligned `vaddr` (`0x1abc`): `end_page == 0x2000` -> **1** page
    ///
    /// The same empty segment mapped a frame or not depending only on its
    /// alignment, which nothing could have intended. It is harmless either way (a
    /// `PT_LOAD` with `p_memsz == 0` also has `p_filesz == 0`, so nothing is
    /// copied and the frame would just be an unreferenced zero page), but an
    /// arbitrary difference is exactly what an extracted, tested version should
    /// settle rather than preserve. Zero pages for zero bytes, both ways.
    #[test]
    fn a_zero_length_segment_occupies_no_pages() {
        assert_eq!(segment_span(0x1000, 0), (0x1000, 0));
        assert_eq!(segment_span(0x1abc, 0), (0x1000, 0));
    }

    #[test]
    fn span_is_exact_at_page_multiples() {
        assert_eq!(segment_span(0x2000, 3 * PAGE_SIZE), (0x2000, 3));
        // One byte more needs a fourth page.
        assert_eq!(segment_span(0x2000, 3 * PAGE_SIZE + 1), (0x2000, 4));
    }

    #[test]
    fn span_pages_always_cover_the_last_byte() {
        for vaddr in [0x1000usize, 0x1001, 0x1fff, 0x2abc] {
            for memsz in [1usize, 7, PAGE_SIZE - 1, PAGE_SIZE, PAGE_SIZE + 1, 9000] {
                let (start, pages) = segment_span(vaddr, memsz);
                let last_byte = vaddr + memsz - 1;
                assert!(start <= vaddr, "start {start:#x} > vaddr {vaddr:#x}");
                assert!(
                    last_byte < start + pages * PAGE_SIZE,
                    "vaddr={vaddr:#x} memsz={memsz} leaves byte {last_byte:#x} unmapped"
                );
            }
        }
    }

    // ---- segment_page_copy ---------------------------------------------

    #[test]
    fn a_page_entirely_inside_the_file_bytes_copies_a_full_page() {
        // Segment at 0x1000, 3 pages of file bytes; the middle page is full.
        assert_eq!(
            segment_page_copy(0x2000, 0x1000, 3 * PAGE_SIZE),
            Some((0, PAGE_SIZE, PAGE_SIZE))
        );
    }

    /// An unaligned segment's first page: the copy starts part-way into the page
    /// and the source starts at 0.
    #[test]
    fn the_page_before_an_unaligned_vaddr_starts_part_way_in() {
        let vaddr = 0x1100;
        let (dst, src, len) = segment_page_copy(0x1000, vaddr, 0x800).unwrap();
        assert_eq!(dst, 0x100, "copy must start at vaddr within the page");
        assert_eq!(src, 0, "the first page takes the segment's first bytes");
        assert_eq!(len, 0x800);
    }

    #[test]
    fn a_page_starting_past_filesz_is_bss_and_copies_nothing() {
        // filesz one page, so the second page is pure .bss.
        assert_eq!(segment_page_copy(0x2000, 0x1000, PAGE_SIZE), None);
    }

    /// The page straddling the end of the file bytes copies only the file part;
    /// the rest is `.bss` and stays as the zeroed frame provides.
    #[test]
    fn the_page_straddling_filesz_copies_only_the_file_part() {
        let (dst, src, len) = segment_page_copy(0x2000, 0x1000, PAGE_SIZE + 100).unwrap();
        assert_eq!((dst, src), (0, PAGE_SIZE));
        assert_eq!(len, 100, "must not copy past filesz into .bss");
    }

    #[test]
    fn a_segment_with_no_file_bytes_copies_nothing_anywhere() {
        for page in 0..4usize {
            assert_eq!(segment_page_copy(0x1000 + page * PAGE_SIZE, 0x1000, 0), None);
        }
    }

    /// The copy window must never run off the end of the page. This is the
    /// invariant that makes the caller's single-frame write sound.
    #[test]
    fn a_copy_window_never_leaves_its_page() {
        for vaddr in [0x1000usize, 0x1100, 0x1fff] {
            for filesz in [1usize, 100, PAGE_SIZE - 1, PAGE_SIZE, PAGE_SIZE + 1, 9000] {
                let (start, pages) = segment_span(vaddr, filesz);
                for i in 0..pages {
                    if let Some((dst, src, len)) =
                        segment_page_copy(start + i * PAGE_SIZE, vaddr, filesz)
                    {
                        assert!(dst + len <= PAGE_SIZE, "vaddr={vaddr:#x} filesz={filesz} page={i}");
                        assert!(src + len <= filesz, "read past filesz");
                    }
                }
            }
        }
    }

    /// Across a whole segment, the copy windows must cover **exactly** the file
    /// bytes: every byte once, none twice, none missed.
    #[test]
    fn the_windows_tile_the_file_bytes_exactly_once() {
        for vaddr in [0x1000usize, 0x1100, 0x1ffe] {
            for filesz in [1usize, 100, PAGE_SIZE, PAGE_SIZE + 1, 3 * PAGE_SIZE + 7] {
                let (start, pages) = segment_span(vaddr, filesz);
                let mut covered = 0usize;
                let mut expect_src = 0usize;
                for i in 0..pages {
                    if let Some((_, src, len)) =
                        segment_page_copy(start + i * PAGE_SIZE, vaddr, filesz)
                    {
                        assert_eq!(src, expect_src, "gap or overlap at page {i}");
                        expect_src += len;
                        covered += len;
                    }
                }
                assert_eq!(covered, filesz, "vaddr={vaddr:#x} filesz={filesz} not fully copied");
            }
        }
    }

    // ---- PageChunks ----------------------------------------------------

    #[test]
    fn a_zero_length_write_produces_no_chunks() {
        assert_eq!(PageChunks::new(0x1000, 0x1000, 0).count(), 0);
    }

    #[test]
    fn a_write_inside_one_page_is_a_single_chunk() {
        let c: Vec<_> = PageChunks::new(0x1000, 0x1010, 16).collect();
        assert_eq!(c, [Chunk { page_index: 0, offset: 0x10, len: 16, src_offset: 0 }]);
    }

    #[test]
    fn a_write_across_a_boundary_splits_at_the_page_edge() {
        // Start 8 bytes before the end of page 0, write 16.
        let c: Vec<_> = PageChunks::new(0x1000, 0x1000 + PAGE_SIZE - 8, 16).collect();
        assert_eq!(
            c,
            [
                Chunk { page_index: 0, offset: PAGE_SIZE - 8, len: 8, src_offset: 0 },
                Chunk { page_index: 1, offset: 0, len: 8, src_offset: 8 },
            ]
        );
    }

    #[test]
    fn a_multi_page_write_yields_full_interior_pages() {
        let c: Vec<_> = PageChunks::new(0x1000, 0x1000, 3 * PAGE_SIZE).collect();
        assert_eq!(c.len(), 3);
        for (i, ch) in c.iter().enumerate() {
            assert_eq!(ch.page_index, i);
            assert_eq!(ch.offset, 0);
            assert_eq!(ch.len, PAGE_SIZE);
        }
    }

    /// **No chunk may cross a page boundary.** Callers index one frame per chunk
    /// and write `len` bytes at `offset`; a chunk that crossed would write off
    /// the end of a frame and into whatever the next physical frame happens to
    /// be — not the next *virtual* page.
    #[test]
    fn chunks_never_cross_a_page_boundary() {
        for start in [0x1000usize, 0x1001, 0x1fff, 0x1000 + PAGE_SIZE - 1] {
            for len in [1usize, 8, PAGE_SIZE - 1, PAGE_SIZE, PAGE_SIZE + 1, 5 * PAGE_SIZE + 3] {
                for ch in PageChunks::new(0x1000, start, len) {
                    assert!(ch.len > 0, "empty chunk");
                    assert!(
                        ch.offset + ch.len <= PAGE_SIZE,
                        "chunk {ch:?} crosses a page boundary (start={start:#x} len={len})"
                    );
                }
            }
        }
    }

    /// The chunks must tile the source buffer exactly: contiguous `src_offset`s
    /// summing to `len`, and page indices that advance by one.
    #[test]
    fn chunks_tile_the_source_exactly_once() {
        for start in [0x1000usize, 0x1234, 0x1fff] {
            for len in [1usize, 100, PAGE_SIZE, 2 * PAGE_SIZE + 5] {
                let mut expect_src = 0usize;
                let mut prev_page: Option<usize> = None;
                for ch in PageChunks::new(0x1000, start, len) {
                    assert_eq!(ch.src_offset, expect_src, "gap/overlap in source");
                    if let Some(p) = prev_page {
                        assert_eq!(ch.page_index, p + 1, "page indices must advance by one");
                    }
                    prev_page = Some(ch.page_index);
                    expect_src += ch.len;
                }
                assert_eq!(expect_src, len, "start={start:#x} len={len} not fully covered");
            }
        }
    }

    /// The page index is relative to `base_va`, not to the write's start — the
    /// stack indexes `frames[]` from its bottom while writing near its top.
    #[test]
    fn page_index_is_relative_to_base_not_to_the_write_start() {
        let base = 0x1000;
        let start = base + 5 * PAGE_SIZE + 16;
        let c: Vec<_> = PageChunks::new(base, start, 4).collect();
        assert_eq!(c[0].page_index, 5);
        assert_eq!(c[0].offset, 16);
    }
}
