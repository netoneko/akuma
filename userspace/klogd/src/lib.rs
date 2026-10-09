//! The pure half of `klogd`: which kernel-log lines are herd's, and how a
//! wrapped ring is merged into what was already kept.
//!
//! The kernel ring is 64 KiB and wraps within minutes on a busy box, taking the
//! boot's `[herd] Starting service` / `Failed to start` / `exited` lines with
//! it. Those are the lines that say why a service is not running, so they are
//! kept separately and only ever appended to.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec::Vec;

/// Upper bound on the kept herd text; past it nothing more is appended (the
/// boot's first lines are the ones worth having).
pub const ACC_CAP: usize = 256 * 1024;

/// Written between two chunks when the ring wrapped past everything already
/// kept, so a reader knows lines are missing rather than absent.
pub const GAP: &[u8] = b"--- gap: lines lost to ring wrap ---\n";

/// Append to `out` every line of `ring` that is herd's, each ending in `\n`.
/// The 20 s `Reloading config` line is dropped: it is most of what herd prints
/// and would crowd out the lifecycle lines.
pub fn herd_lines(ring: &[u8], out: &mut Vec<u8>) {
    for line in ring.split(|&b| b == b'\n') {
        if contains(line, b"[herd]") && !contains(line, b"Reloading") {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Merge a fresh snapshot `cur` (output of [`herd_lines`]) into `acc`.
///
/// `cur` is a window onto the same sequence `acc` was cut from, so its head may
/// repeat the tail of `acc`: the longest line-aligned prefix of `cur` that is a
/// suffix of `acc` is skipped, and the rest appended. No overlap with a
/// non-empty `acc` means the ring wrapped past it; [`GAP`] marks that.
/// Returns whether `acc` changed.
pub fn merge(acc: &mut Vec<u8>, cur: &[u8]) -> bool {
    if cur.is_empty() || acc.len() >= ACC_CAP {
        return false;
    }
    // Line ends in `cur`, longest prefix first.
    let mut ends: Vec<usize> = Vec::new();
    for (i, &b) in cur.iter().enumerate() {
        if b == b'\n' {
            ends.push(i + 1);
        }
    }
    let mut skip = 0;
    for &m in ends.iter().rev() {
        if acc.ends_with(&cur[..m]) {
            skip = m;
            break;
        }
    }
    if skip == 0 && !acc.is_empty() {
        acc.extend_from_slice(GAP);
    }
    let rest = &cur[skip..];
    if rest.is_empty() {
        return false;
    }
    let room = ACC_CAP - acc.len();
    acc.extend_from_slice(&rest[..rest.len().min(room)]);
    true
}

/// What a fresh ring snapshot `cur` adds to the previous one `prev`, for the
/// append-only full log (`klog-N.all`).
///
/// Only `cur` and `prev` up to their last `\n` are considered: a trailing
/// partial line is still being written and is picked up whole by the next pass. Of that, the
/// longest prefix ending at a line end that is also a suffix of `prev` is what
/// was already written; the rest is returned. `true` in the second field means
/// no prefix overlapped a non-empty `prev`: the ring wrapped past everything
/// between two passes and the caller should mark the gap. Byte-level, so a
/// ring whose head is torn mid-line still matches.
///
/// Returns a borrow of `cur`; allocates nothing.
pub fn new_tail<'a>(prev: &[u8], cur: &'a [u8]) -> (&'a [u8], bool) {
    let Some(last) = cur.iter().rposition(|&b| b == b'\n') else {
        return (&[], false);
    };
    let cur = &cur[..=last];
    // The same cut applies to `prev`: its partial last line was never written.
    let prev = match prev.iter().rposition(|&b| b == b'\n') {
        Some(i) => &prev[..=i],
        None => &[][..],
    };
    if prev.is_empty() {
        return (cur, false);
    }
    // Longest overlap first: scan line ends from the right.
    let mut m = cur.len();
    loop {
        if m <= prev.len() && prev.ends_with(&cur[..m]) {
            return (&cur[m..], false);
        }
        match cur[..m - 1].iter().rposition(|&b| b == b'\n') {
            Some(i) => m = i + 1,
            None => return (cur, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(ring: &str) -> Vec<u8> {
        let mut v = Vec::new();
        herd_lines(ring.as_bytes(), &mut v);
        v
    }

    #[test]
    fn keeps_herd_lines_drops_reload_and_others() {
        let got = lines("[rtw] x\n[herd] Starting service: sshd\n[herd] Reloading config...\n[SCHED] w\n[herd] Started sshd (pid=7)\n");
        assert_eq!(got, b"[herd] Starting service: sshd\n[herd] Started sshd (pid=7)\n");
    }

    #[test]
    fn torn_line_is_kept_whole() {
        let got = lines("[herd] Starting service: [SCHED] WARNING: yield_now\nsshd\n");
        assert_eq!(got, b"[herd] Starting service: [SCHED] WARNING: yield_now\n");
    }

    #[test]
    fn first_snapshot_is_taken_whole() {
        let mut acc = Vec::new();
        assert!(merge(&mut acc, b"a\nb\n"));
        assert_eq!(acc, b"a\nb\n");
    }

    #[test]
    fn identical_snapshot_changes_nothing() {
        let mut acc = b"a\nb\n".to_vec();
        assert!(!merge(&mut acc, b"a\nb\n"));
        assert_eq!(acc, b"a\nb\n");
    }

    #[test]
    fn growing_snapshot_appends_only_the_new_tail() {
        let mut acc = b"a\nb\n".to_vec();
        assert!(merge(&mut acc, b"a\nb\nc\n"));
        assert_eq!(acc, b"a\nb\nc\n");
    }

    #[test]
    fn wrapped_head_with_overlap_appends_the_tail() {
        // The ring lost "a"; it still holds "b" (already kept) and "c".
        let mut acc = b"a\nb\n".to_vec();
        assert!(merge(&mut acc, b"b\nc\n"));
        assert_eq!(acc, b"a\nb\nc\n");
    }

    #[test]
    fn wrap_past_everything_kept_marks_a_gap() {
        let mut acc = b"a\nb\n".to_vec();
        assert!(merge(&mut acc, b"x\ny\n"));
        let mut want = b"a\nb\n".to_vec();
        want.extend_from_slice(GAP);
        want.extend_from_slice(b"x\ny\n");
        assert_eq!(acc, want);
    }

    #[test]
    fn a_wrapped_ring_of_nothing_never_shrinks_what_was_kept() {
        let mut acc = b"a\nb\n".to_vec();
        assert!(!merge(&mut acc, b""));
        assert_eq!(acc, b"a\nb\n");
    }

    #[test]
    fn stops_at_the_cap() {
        let mut acc = alloc::vec![b'x'; ACC_CAP];
        assert!(!merge(&mut acc, b"a\n"));
        assert_eq!(acc.len(), ACC_CAP);
    }
    #[test]
    fn tail_first_pass_takes_complete_lines_only() {
        assert_eq!(new_tail(b"", b"a\nb\npart"), (&b"a\nb\n"[..], false));
    }

    #[test]
    fn tail_of_a_grown_ring_is_the_new_lines() {
        assert_eq!(new_tail(b"a\nb\npar", b"a\nb\npartial done\nc\n"), (&b"partial done\nc\n"[..], false));
    }

    #[test]
    fn tail_after_wrap_with_torn_head() {
        // The ring lost "aaa\nb" and now starts mid-line.
        assert_eq!(new_tail(b"aaa\nbbb\nccc\n", b"bb\nccc\nddd\n"), (&b"ddd\n"[..], false));
    }

    #[test]
    fn tail_unchanged_ring_is_empty() {
        assert_eq!(new_tail(b"a\nb\n", b"a\nb\n"), (&b""[..], false));
    }

    #[test]
    fn tail_wrapped_past_everything_reports_a_gap() {
        assert_eq!(new_tail(b"a\nb\n", b"x\ny\n"), (&b"x\ny\n"[..], true));
    }

    #[test]
    fn tail_repeated_lines_take_the_longest_overlap() {
        // "x\n" alone is also a suffix of prev; the whole "x\nx\n" must win.
        assert_eq!(new_tail(b"x\nx\n", b"x\nx\ny\n"), (&b"y\n"[..], false));
    }
}
