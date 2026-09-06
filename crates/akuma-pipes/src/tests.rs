//! Every test here pins a rule that cost a live debugging session on the
//! aarch64 kernel. None of them could be written while the table lived inside
//! `akuma-syscalls-glue`, which does not build off the target.

use super::*;
use alloc::vec;
use alloc::vec::Vec;

/// A wake token that records nothing but its own identity.
type T = PipeTable<u32>;

fn fired(w: Wakes<u32>) -> Vec<usize> {
    w.drain().map(|(tid, _)| tid).collect()
}

#[test]
fn a_new_pipe_has_one_reader_and_one_writer() {
    // Not zero: `pipe(2)` hands out two descriptors and the pipe is live from
    // that moment. Starting at zero reports EOF before the writer has run.
    let mut t = T::new();
    let id = t.create();
    assert_eq!(t.counts(id), Some((1, 1)));
    assert!(t.exists(id));
}

#[test]
fn write_then_read_round_trips() {
    let mut t = T::new();
    let id = t.create();
    assert_eq!(t.write(id, b"hello").0, WriteOutcome::Wrote(5));
    let mut buf = [0u8; 8];
    let (r, _) = t.read(id, &mut buf);
    assert_eq!((r.bytes, r.eof), (5, false));
    assert_eq!(&buf[..5], b"hello");
}

#[test]
fn an_empty_pipe_with_a_live_writer_is_not_eof() {
    // The distinction the whole blocking path rests on: "nothing yet" and
    // "nothing ever" are different answers to an empty buffer.
    let mut t = T::new();
    let id = t.create();
    let mut buf = [0u8; 4];
    let (r, _) = t.read(id, &mut buf);
    assert_eq!((r.bytes, r.eof), (0, false));
}

#[test]
fn eof_arrives_only_when_the_last_writer_goes() {
    let mut t = T::new();
    let id = t.create();
    t.clone_ref(id, true); // a second writer — a fork, say
    assert_eq!(t.counts(id), Some((1, 2)));

    t.close_write(id);
    let mut buf = [0u8; 4];
    assert!(!t.read(id, &mut buf).0.eof, "one writer left: not EOF");

    t.close_write(id);
    assert!(t.read(id, &mut buf).0.eof, "last writer gone: EOF");
}

#[test]
fn buffered_bytes_survive_the_last_writer() {
    // EOF is "drained *and* no writers", not "no writers". A reader must still
    // get what was written before the writer left.
    let mut t = T::new();
    let id = t.create();
    t.write(id, b"tail");
    t.close_write(id);
    let mut buf = [0u8; 8];
    let (r, _) = t.read(id, &mut buf);
    assert_eq!((r.bytes, r.eof), (4, false));
    assert!(t.read(id, &mut buf).0.eof);
}

#[test]
fn writing_with_no_readers_is_a_broken_pipe() {
    let mut t = T::new();
    let id = t.create();
    t.close_read(id);
    assert_eq!(t.write(id, b"x").0, WriteOutcome::BrokenPipe);
}

#[test]
fn a_missing_pipe_reads_eof_and_writes_nosuchpipe() {
    // Two different answers on purpose: EOF so a reader never blocks on an id
    // nobody holds, and `NoSuchPipe` (not `BrokenPipe`) so a writer does not
    // raise SIGPIPE for a pipe that never existed.
    let mut t = T::new();
    let mut buf = [0u8; 4];
    assert!(t.read(999, &mut buf).0.eof);
    assert_eq!(t.write(999, b"x").0, WriteOutcome::NoSuchPipe);
}

#[test]
fn the_pipe_is_destroyed_only_when_both_ends_are_gone() {
    let mut t = T::new();
    let id = t.create();
    assert!(!t.close_write(id).0.destroyed, "reader still holds it");
    assert!(t.exists(id));
    assert!(t.close_read(id).0.destroyed);
    assert!(!t.exists(id));
}

#[test]
fn a_full_buffer_takes_a_short_write_then_nothing() {
    // `Wrote(0)` is not success-with-nothing-to-do. A caller that treats it as
    // success silently drops the data, which desyncs a framed protocol.
    let mut t = T::with_capacity(8);
    let id = t.create();
    assert_eq!(t.write(id, b"abcdefghij").0, WriteOutcome::Wrote(8));
    assert_eq!(t.write(id, b"more").0, WriteOutcome::Wrote(0));
    assert_eq!(t.buffered(id), 8);
}

// ---------------------------------------------------------------- wakes ----

#[test]
fn a_write_wakes_every_waiter() {
    let mut t = T::new();
    let id = t.create();
    t.add_poller(id, 7, 70);
    t.add_poller(id, 9, 90);
    let (_, w) = t.write(id, b"x");
    assert_eq!(fired(w), vec![7, 9]);
    assert_eq!(t.poller_count(id), 0, "the set is drained, not copied");
}

#[test]
fn a_write_that_accepted_nothing_wakes_nobody() {
    // Nothing changed, so there is nothing for a waiter to re-test; waking
    // them is a spin.
    let mut t = T::with_capacity(4);
    let id = t.create();
    t.write(id, b"abcd");
    t.add_poller(id, 7, 70);
    let (outcome, w) = t.write(id, b"e");
    assert_eq!(outcome, WriteOutcome::Wrote(0));
    assert!(w.is_empty());
    assert_eq!(t.poller_count(id), 1, "the waiter stays registered");
}

#[test]
fn a_read_wakes_waiters_because_it_made_room() {
    let mut t = T::with_capacity(4);
    let id = t.create();
    t.write(id, b"abcd");
    t.add_poller(id, 3, 30);
    let mut buf = [0u8; 2];
    let (_, w) = t.read(id, &mut buf);
    assert_eq!(fired(w), vec![3]);
}

#[test]
fn a_read_that_found_nothing_wakes_nobody() {
    let mut t = T::new();
    let id = t.create();
    t.add_poller(id, 3, 30);
    let mut buf = [0u8; 4];
    let (_, w) = t.read(id, &mut buf);
    assert!(w.is_empty());
    assert_eq!(t.poller_count(id), 1);
}

#[test]
fn losing_the_last_writer_wakes_blocked_readers() {
    let mut t = T::new();
    let id = t.create();
    t.add_poller(id, 5, 50);
    let (_, w) = t.close_write(id);
    assert_eq!(fired(w), vec![5], "EOF is an event");
}

#[test]
fn losing_the_last_reader_wakes_blocked_writers() {
    // `busybox yes | busybox head -n 1`: `yes` fills the buffer and parks,
    // `head` reads one line and exits, and the last-reader close lands while
    // `yes` is asleep. Without this wake it never retries, never sees
    // `BrokenPipe`, never gets SIGPIPE, and sleeps forever. Invisible until
    // pipes were capped — an uncapped pipe never blocked a writer.
    let mut t = T::with_capacity(4);
    let id = t.create();
    t.write(id, b"abcd");
    assert!(!t.check_set_writer(id, 11, 110), "full: the writer must block");
    let (_, w) = t.close_read(id);
    assert_eq!(fired(w), vec![11]);
    assert_eq!(t.write(id, b"x").0, WriteOutcome::BrokenPipe, "the retry sees it");
}

#[test]
fn a_non_final_close_wakes_nobody() {
    let mut t = T::new();
    let id = t.create();
    t.clone_ref(id, true);
    t.add_poller(id, 5, 50);
    let (_, w) = t.close_write(id);
    assert!(w.is_empty(), "a writer remains; nothing changed for the reader");
}

// ------------------------------------------------- check-and-register ------

#[test]
fn check_set_reader_registers_only_when_it_would_block() {
    let mut t = T::new();
    let id = t.create();
    // Nothing to read and a live writer: block, and be registered for it.
    assert!(!t.check_set_reader(id, 4, 40));
    assert!(t.is_poller_registered(id, 4));

    // Data present: do not block, do not register.
    let id2 = t.create();
    t.write(id2, b"x");
    assert!(t.check_set_reader(id2, 4, 40));
    assert!(!t.is_poller_registered(id2, 4));

    // EOF: do not block.
    let id3 = t.create();
    t.close_write(id3);
    assert!(t.check_set_reader(id3, 4, 40));
    assert!(!t.is_poller_registered(id3, 4));
}

#[test]
fn check_set_writer_registers_only_when_full_and_readable() {
    let mut t = T::with_capacity(2);
    let id = t.create();
    assert!(t.check_set_writer(id, 4, 40), "room: no need to block");
    t.write(id, b"ab");
    assert!(!t.check_set_writer(id, 4, 40), "full: block");
    assert!(t.is_poller_registered(id, 4));

    // Full but broken: do not block — the caller must proceed to get EPIPE.
    let id2 = t.create();
    t.write(id2, b"ab");
    t.close_read(id2);
    assert!(t.check_set_writer(id2, 4, 40));
}

#[test]
fn check_set_on_a_missing_pipe_never_blocks() {
    let mut t = T::new();
    assert!(t.check_set_reader(404, 1, 10));
    assert!(t.check_set_writer(404, 1, 10));
}

#[test]
fn registering_the_same_thread_twice_keeps_one_entry() {
    // Keyed by tid for dedup: a thread that polls in a loop must not grow the
    // set without bound.
    let mut t = T::new();
    let id = t.create();
    t.add_poller(id, 8, 80);
    t.add_poller(id, 8, 81);
    assert_eq!(t.poller_count(id), 1);
}

#[test]
fn readable_and_writable_answer_for_a_missing_pipe_without_blocking() {
    let mut t = T::new();
    assert!(t.readable(404), "a gone pipe reads EOF, which is ready");
    assert!(t.writable(404), "so the writer proceeds and gets NoSuchPipe");
    let id = t.create();
    assert!(!t.readable(id));
    assert!(t.writable(id));
}

#[test]
fn ids_are_not_reused_while_a_pipe_is_live() {
    let mut t = T::new();
    let a = t.create();
    let b = t.create();
    assert_ne!(a, b);
    t.close_write(a);
    t.close_read(a);
    assert_ne!(t.create(), a, "a destroyed id is not handed straight back");
}
