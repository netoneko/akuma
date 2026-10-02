extern crate std;
use super::*;

const A: u64 = 1;
const B: u64 = 2;

#[test]
fn range_resolution() {
    assert_eq!(range(10, 5), Some((10, 15)));
    assert_eq!(range(10, 0), Some((10, u64::MAX)), "len 0 is to EOF");
    assert_eq!(range(10, -4), Some((6, 10)), "negative len locks the bytes before");
    assert_eq!(range(-1, 5), None, "negative start");
    assert_eq!(range(2, -5), None, "negative len reaching before 0");
}

#[test]
fn write_excludes_other_owner_but_not_self() {
    let mut t = Table::new();
    assert!(t.try_lock("f", A, 10, Kind::Write, 0, 10).is_ok());
    assert!(t.try_lock("f", A, 10, Kind::Write, 0, 10).is_ok(), "own lock never conflicts");
    let b = t.try_lock("f", B, 20, Kind::Write, 5, 6).unwrap_err();
    assert_eq!((b.owner, b.pid), (A, 10));
    assert!(t.try_lock("f", B, 20, Kind::Write, 10, 20).is_ok(), "disjoint ranges coexist");
}

#[test]
fn readers_share_writers_do_not() {
    let mut t = Table::new();
    assert!(t.try_lock("f", A, 1, Kind::Read, 0, 100).is_ok());
    assert!(t.try_lock("f", B, 2, Kind::Read, 0, 100).is_ok());
    // A cannot upgrade while B reads.
    assert!(t.try_lock("f", A, 1, Kind::Write, 0, 100).is_err());
    t.release_file("f", B);
    assert!(t.try_lock("f", A, 1, Kind::Write, 0, 100).is_ok(), "upgrade once alone");
}

#[test]
fn files_are_independent() {
    let mut t = Table::new();
    assert!(t.try_lock("db", A, 1, Kind::Write, 0, 1).is_ok());
    assert!(t.try_lock("db-shm", B, 2, Kind::Write, 0, 1).is_ok());
}

#[test]
fn unlock_middle_splits() {
    let mut t = Table::new();
    t.try_lock("f", A, 1, Kind::Write, 0, 100).unwrap();
    t.unlock("f", A, 40, 60);
    let h = t.held("f", A);
    assert_eq!(h.len(), 2);
    assert_eq!((h[0].start, h[0].end, h[1].start, h[1].end), (0, 40, 60, 100));
    assert!(t.try_lock("f", B, 2, Kind::Write, 40, 60).is_ok(), "the hole is free");
    assert!(t.try_lock("f", B, 2, Kind::Write, 0, 40).is_err());
}

#[test]
fn downgrade_part_of_a_lock() {
    let mut t = Table::new();
    t.try_lock("f", A, 1, Kind::Write, 0, 100).unwrap();
    t.try_lock("f", A, 1, Kind::Read, 0, 50).unwrap();
    assert!(t.try_lock("f", B, 2, Kind::Read, 0, 50).is_ok(), "downgraded half is shareable");
    assert!(t.try_lock("f", B, 2, Kind::Read, 50, 100).is_err(), "write half still excludes");
}

#[test]
fn adjacent_same_kind_merge_so_cycles_do_not_grow() {
    let mut t = Table::new();
    for i in 0..50u64 {
        t.try_lock("f", A, 1, Kind::Write, i, i + 1).unwrap();
    }
    assert_eq!(t.held("f", A).len(), 1);
    assert_eq!((t.held("f", A)[0].start, t.held("f", A)[0].end), (0, 50));
}

#[test]
fn to_eof_locks_everything_after() {
    let mut t = Table::new();
    t.try_lock("f", A, 1, Kind::Write, 100, u64::MAX).unwrap();
    assert!(t.try_lock("f", B, 2, Kind::Read, 1 << 40, (1 << 40) + 1).is_err());
    assert!(t.try_lock("f", B, 2, Kind::Read, 0, 100).is_ok());
}

#[test]
fn close_releases_whole_file_and_exit_releases_all() {
    let mut t = Table::new();
    t.try_lock("a", A, 1, Kind::Write, 0, 10).unwrap();
    t.try_lock("a", A, 1, Kind::Read, 20, 30).unwrap();
    t.try_lock("b", A, 1, Kind::Write, 0, 10).unwrap();
    t.release_file("a", A);
    assert!(!t.is_locked("a") && t.is_locked("b"));
    t.release_owner(A);
    assert!(!t.is_locked("b"));
}

#[test]
fn getlk_reports_blocker_without_taking_anything() {
    let mut t = Table::new();
    t.try_lock("f", A, 7, Kind::Write, 4, 8).unwrap();
    let c = t.conflict("f", B, Kind::Read, 0, 100).unwrap();
    assert_eq!((c.pid, c.start, c.end, c.kind), (7, 4, 8, Kind::Write));
    assert!(t.conflict("f", A, Kind::Write, 0, 100).is_none(), "own locks are not blockers");
    assert!(t.held("f", B).is_empty());
}

/// SQLite's WAL-index pattern: each connection takes a shared lock on one byte
/// to say "I am reading", and `recovery` needs the exclusive lock on all of
/// them. The writer must be refused while any reader holds its byte.
#[test]
fn sqlite_wal_reader_blocks_exclusive_sweep() {
    let mut t = Table::new();
    let base = 120u64; // WAL_READ_LOCK region in -shm
    t.try_lock("x-shm", A, 1, Kind::Read, base + 3, base + 4).unwrap();
    assert!(t.try_lock("x-shm", B, 2, Kind::Write, base, base + 5).is_err());
    t.unlock("x-shm", A, base + 3, base + 4);
    assert!(t.try_lock("x-shm", B, 2, Kind::Write, base, base + 5).is_ok());
}
