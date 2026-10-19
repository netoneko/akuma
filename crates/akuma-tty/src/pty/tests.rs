use super::*;

type Pair = PtyPair<u32>;

/// A pair with the slave opened, as `openpty` leaves it.
fn open_pair() -> Pair {
    let mut p = Pair::try_new().unwrap();
    p.set_locked(false);
    p.slave_open().unwrap();
    p
}

fn master_drain(p: &mut Pair) -> Vec<u8> {
    let mut out = vec![0u8; 64 * 1024];
    match p.master_read(&mut out) {
        MasterRead::Data(n) => out[..n].to_vec(),
        _ => Vec::new(),
    }
}

fn slave_read(p: &mut Pair, len: usize) -> SlaveRead {
    let mut out = vec![0u8; len];
    p.slave_read(&mut out, false)
}

fn slave_read_bytes(p: &mut Pair, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    match p.slave_read(&mut out, false) {
        SlaveRead::Data(n) => out[..n].to_vec(),
        other => panic!("expected data, got {other:?}"),
    }
}

fn raw(p: &mut Pair) {
    let mut t = p.termios();
    t.lflag &= !(flags::ICANON | flags::ECHO | flags::ISIG | flags::IEXTEN);
    t.iflag &= !flags::ICRNL;
    t.oflag &= !flags::OPOST;
    t.cc[cc::VMIN] = 1;
    t.cc[cc::VTIME] = 0;
    p.set_termios(t);
}

// ---- wire layouts -----------------------------------------------------------

#[test]
fn termios_wire_puts_cc_after_c_line() {
    let t = Termios::initial();
    let w = t.to_wire();
    assert_eq!(w.len(), 36);
    assert_eq!(w[16], 0, "c_line");
    assert_eq!(w[17 + cc::VINTR], 0x03);
    assert_eq!(w[17 + cc::VERASE], 0x7F);
    assert_eq!(w[17 + cc::VMIN], 1);
    assert_eq!(Termios::from_wire(&w), t);
}

#[test]
fn termios_initial_is_linux_std_plus_iutf8() {
    let t = Termios::initial();
    assert_eq!(t.iflag, 0o400 | 0o2000 | 0o40000);
    assert_eq!(t.oflag, 0o5);
    assert_eq!(t.lflag, 0o105073);
    assert_eq!(t.cflag, 0o277);
}

#[test]
fn winsize_round_trips() {
    let ws = Winsize { row: 50, col: 211, xpixel: 0, ypixel: 0 };
    assert_eq!(Winsize::from_wire(&ws.to_wire()), ws);
    assert_eq!(ws.to_wire()[..4], [50, 0, 211, 0]);
}

// ---- open, lock, hangup ------------------------------------------------------

#[test]
fn slave_open_needs_unlock_and_a_master() {
    let mut p = Pair::try_new().unwrap();
    assert_eq!(p.slave_open(), Err(SlaveOpenError::Locked));
    p.set_locked(false);
    assert!(p.slave_open().is_ok());
    assert!(p.master_close());
    assert_eq!(p.slave_open(), Err(SlaveOpenError::MasterGone));
}

#[test]
fn master_waits_for_a_slave_that_was_never_opened() {
    let mut p = Pair::try_new().unwrap();
    assert_eq!(p.master_read(&mut [0; 8]), MasterRead::WouldBlock);
    assert!(!p.master_poll().hup);
}

#[test]
fn master_reads_remaining_output_then_eio_after_slave_closes() {
    let mut p = open_pair();
    assert_eq!(p.slave_write(b"bye"), Ok(3));
    p.slave_close();
    assert!(p.master_poll().hup);
    assert!(p.master_poll().readable);
    assert_eq!(master_drain(&mut p), b"bye");
    assert_eq!(p.master_read(&mut [0; 8]), MasterRead::Hangup);
    assert_eq!(p.master_write(b"x"), Err(HungUp));
    assert!(!p.is_unreferenced(), "the master still holds it");
    p.master_close();
    assert!(p.is_unreferenced());
}

#[test]
fn slave_reads_eof_and_writes_eio_after_master_closes() {
    let mut p = open_pair();
    p.master_write(b"pending\n").unwrap();
    assert!(p.master_close(), "last master close is the hangup");
    assert_eq!(slave_read(&mut p, 16), SlaveRead::Hangup);
    assert_eq!(p.slave_write(b"x"), Err(HungUp));
    let poll = p.slave_poll();
    assert!(poll.hup && poll.readable && poll.writable);
}

#[test]
fn dup_of_master_delays_the_hangup() {
    let mut p = open_pair();
    p.master_ref();
    assert!(!p.master_close());
    assert!(!p.slave_hup());
    assert!(p.master_close());
}

// ---- canonical input ----------------------------------------------------------

#[test]
fn canonical_line_with_echo_and_icrnl() {
    let mut p = open_pair();
    let r = p.master_write(b"echo hi\r").unwrap();
    assert_eq!(r.accepted, 8);
    assert!(r.signals.is_empty());
    assert_eq!(master_drain(&mut p), b"echo hi\r\n");
    assert_eq!(slave_read_bytes(&mut p, 64), b"echo hi\n");
}

#[test]
fn canonical_read_blocks_until_a_line_ends() {
    let mut p = open_pair();
    p.master_write(b"abc").unwrap();
    assert_eq!(slave_read(&mut p, 64), SlaveRead::Block { timeout_ds: None });
    assert!(!p.slave_poll().readable);
    p.master_write(b"\n").unwrap();
    assert!(p.slave_poll().readable);
    assert_eq!(slave_read_bytes(&mut p, 64), b"abc\n");
}

#[test]
fn canonical_read_returns_one_line_at_a_time() {
    let mut p = open_pair();
    p.master_write(b"one\ntwo\n").unwrap();
    assert_eq!(slave_read_bytes(&mut p, 64), b"one\n");
    assert_eq!(slave_read_bytes(&mut p, 64), b"two\n");
}

#[test]
fn a_short_read_leaves_the_rest_of_the_line() {
    let mut p = open_pair();
    p.master_write(b"abcdef\n").unwrap();
    assert_eq!(slave_read_bytes(&mut p, 4), b"abcd");
    assert_eq!(slave_read_bytes(&mut p, 64), b"ef\n");
}

#[test]
fn erase_kill_and_word_erase() {
    let mut p = open_pair();
    p.master_write(b"ab\x7fc\n").unwrap();
    assert_eq!(slave_read_bytes(&mut p, 64), b"ac\n");
    assert_eq!(master_drain(&mut p), b"ab\x08 \x08c\r\n");

    p.master_write(b"junk\x15ok\n").unwrap();
    assert_eq!(slave_read_bytes(&mut p, 64), b"ok\n");

    p.master_write(b"ls -la  \x17\x17pwd\n").unwrap();
    assert_eq!(slave_read_bytes(&mut p, 64), b"pwd\n");
}

#[test]
fn erase_removes_a_whole_utf8_character() {
    let mut p = open_pair();
    p.master_write("aé\x7f\n".as_bytes()).unwrap();
    assert_eq!(slave_read_bytes(&mut p, 64), b"a\n");
}

#[test]
fn erase_of_an_echoed_control_char_rubs_out_two_columns() {
    let mut p = open_pair();
    p.master_write(b"\x01").unwrap();
    assert_eq!(master_drain(&mut p), b"^A");
    p.master_write(b"\x7f").unwrap();
    assert_eq!(master_drain(&mut p), b"\x08 \x08\x08 \x08");
}

#[test]
fn eof_on_an_empty_line_is_a_zero_read() {
    let mut p = open_pair();
    p.master_write(b"\x04").unwrap();
    assert!(p.slave_poll().readable);
    assert_eq!(slave_read(&mut p, 64), SlaveRead::Data(0));
    assert_eq!(slave_read(&mut p, 64), SlaveRead::Block { timeout_ds: None });
}

#[test]
fn eof_after_text_pushes_the_line_without_a_newline() {
    let mut p = open_pair();
    p.master_write(b"ab\x04").unwrap();
    assert_eq!(slave_read_bytes(&mut p, 64), b"ab");
    assert_eq!(slave_read(&mut p, 64), SlaveRead::Block { timeout_ds: None });
}

#[test]
fn literal_next_stores_a_control_char_as_data() {
    let mut p = open_pair();
    p.master_write(b"\x16\x03\n").unwrap();
    assert_eq!(slave_read_bytes(&mut p, 64), b"\x03\n");
}

#[test]
fn a_full_line_drops_bytes_but_can_still_end() {
    let mut p = open_pair();
    let long = vec![b'x'; MAX_CANON + 10];
    let r = p.master_write(&long).unwrap();
    assert_eq!(r.accepted, long.len(), "excess is consumed, not blocked on");
    p.master_write(b"\n").unwrap();
    let got = slave_read_bytes(&mut p, 2 * MAX_CANON);
    assert_eq!(got.len(), MAX_CANON + 1);
}

// ---- signals --------------------------------------------------------------------

#[test]
fn intr_raises_sigint_flushes_and_echoes() {
    let mut p = open_pair();
    p.master_write(b"half a li").unwrap();
    master_drain(&mut p);
    let r = p.master_write(b"\x03").unwrap();
    assert_eq!(r.accepted, 1);
    assert!(r.signals.contains(sig::SIGINT));
    assert_eq!(r.signals.iter().collect::<Vec<_>>(), vec![sig::SIGINT]);
    assert_eq!(master_drain(&mut p), b"^C");
    p.master_write(b"\n").unwrap();
    assert_eq!(slave_read_bytes(&mut p, 64), b"\n", "the half line was flushed");
}

#[test]
fn quit_and_susp_raise_their_signals() {
    let mut p = open_pair();
    assert!(p.master_write(b"\x1c").unwrap().signals.contains(sig::SIGQUIT));
    assert!(p.master_write(b"\x1a").unwrap().signals.contains(sig::SIGTSTP));
}

#[test]
fn without_isig_intr_is_data() {
    let mut p = open_pair();
    raw(&mut p);
    let r = p.master_write(b"\x03").unwrap();
    assert!(r.signals.is_empty());
    assert_eq!(slave_read_bytes(&mut p, 8), b"\x03");
}

#[test]
fn a_disabled_cc_slot_matches_nothing() {
    let mut p = open_pair();
    let mut t = p.termios();
    t.cc[cc::VINTR] = 0;
    p.set_termios(t);
    let r = p.master_write(b"\x00\n").unwrap();
    assert!(r.signals.is_empty());
}

// ---- raw input and VMIN/VTIME ----------------------------------------------------

#[test]
fn raw_bytes_are_readable_at_once_without_echo() {
    let mut p = open_pair();
    raw(&mut p);
    p.master_write(b"\x1b[A").unwrap();
    assert_eq!(master_drain(&mut p), b"");
    assert_eq!(slave_read_bytes(&mut p, 8), b"\x1b[A");
}

#[test]
fn vmin0_vtime0_returns_zero_immediately() {
    let mut p = open_pair();
    raw(&mut p);
    let mut t = p.termios();
    t.cc[cc::VMIN] = 0;
    p.set_termios(t);
    assert_eq!(slave_read(&mut p, 8), SlaveRead::Data(0));
}

#[test]
fn vmin0_vtime_arms_a_timer_then_returns_zero() {
    let mut p = open_pair();
    raw(&mut p);
    let mut t = p.termios();
    t.cc[cc::VMIN] = 0;
    t.cc[cc::VTIME] = 5;
    p.set_termios(t);
    assert_eq!(slave_read(&mut p, 8), SlaveRead::Block { timeout_ds: Some(5) });
    assert_eq!(p.slave_read(&mut [0; 8], true), SlaveRead::Data(0));
    p.master_write(b"k").unwrap();
    assert_eq!(slave_read_bytes(&mut p, 8), b"k");
}

#[test]
fn vmin_waits_for_that_many_bytes() {
    let mut p = open_pair();
    raw(&mut p);
    let mut t = p.termios();
    t.cc[cc::VMIN] = 3;
    p.set_termios(t);
    p.master_write(b"ab").unwrap();
    assert!(!p.slave_poll().readable);
    assert_eq!(slave_read(&mut p, 8), SlaveRead::Block { timeout_ds: None });
    // A smaller buffer lowers the bar: VMIN is capped at the read size.
    assert_eq!(slave_read_bytes(&mut p, 2), b"ab");
    p.master_write(b"abc").unwrap();
    assert!(p.slave_poll().readable);
    assert_eq!(slave_read_bytes(&mut p, 8), b"abc");
}

#[test]
fn vmin_and_vtime_time_out_between_bytes_only() {
    let mut p = open_pair();
    raw(&mut p);
    let mut t = p.termios();
    t.cc[cc::VMIN] = 4;
    t.cc[cc::VTIME] = 2;
    p.set_termios(t);
    assert_eq!(slave_read(&mut p, 8), SlaveRead::Block { timeout_ds: None }, "no byte, no timer");
    p.master_write(b"a").unwrap();
    assert_eq!(slave_read(&mut p, 8), SlaveRead::Block { timeout_ds: Some(2) });
    assert_eq!(p.slave_read(&mut [0; 8], true), SlaveRead::Data(1));
}

#[test]
fn raw_input_backpressures_when_full() {
    let mut p = open_pair();
    raw(&mut p);
    let big = vec![b'z'; INPUT_CAPACITY + 100];
    assert_eq!(p.master_write(&big).unwrap().accepted, INPUT_CAPACITY);
    assert!(!p.master_poll().writable);
    assert_eq!(slave_read_bytes(&mut p, 100).len(), 100);
    assert!(p.master_poll().writable);
}

#[test]
fn canonical_line_that_does_not_fit_is_a_short_write() {
    let mut p = open_pair();
    raw(&mut p);
    let mut t = p.termios();
    p.master_write(&vec![b'q'; INPUT_CAPACITY - 2]).unwrap();
    t.lflag |= flags::ICANON;
    p.set_termios(t);
    // The queued raw bytes became one readable line; there is room for two
    // more entries, and a 5-byte line plus terminator needs six.
    let r = p.master_write(b"hello\n").unwrap();
    assert_eq!(r.accepted, 5, "the newline waits for room");
    assert_eq!(slave_read_bytes(&mut p, INPUT_CAPACITY).len(), INPUT_CAPACITY - 2);
    assert_eq!(p.master_write(b"\n").unwrap().accepted, 1);
    assert_eq!(slave_read_bytes(&mut p, 64), b"hello\n");
}

// ---- mode switches ----------------------------------------------------------------

#[test]
fn leaving_canonical_mode_makes_the_edit_line_readable() {
    let mut p = open_pair();
    p.master_write(b"\x04partial").unwrap();
    raw(&mut p);
    assert_eq!(slave_read_bytes(&mut p, 64), b"partial", "the EOF mark is dropped");
}

#[test]
fn entering_canonical_mode_makes_raw_bytes_one_line() {
    let mut p = open_pair();
    raw(&mut p);
    p.master_write(b"xyz").unwrap();
    let mut t = p.termios();
    t.lflag |= flags::ICANON;
    p.set_termios(t);
    assert!(p.slave_poll().readable);
    assert_eq!(slave_read_bytes(&mut p, 64), b"xyz");
}

// ---- output -------------------------------------------------------------------------

#[test]
fn output_maps_nl_to_crnl_under_opost_onlcr() {
    let mut p = open_pair();
    assert_eq!(p.slave_write(b"a\nb"), Ok(3));
    assert_eq!(master_drain(&mut p), b"a\r\nb");
    raw(&mut p);
    p.slave_write(b"a\nb").unwrap();
    assert_eq!(master_drain(&mut p), b"a\nb");
}

#[test]
fn output_backpressures_and_never_splits_crnl() {
    let mut p = open_pair();
    let n = p.slave_write(&vec![b'.'; OUTPUT_CAPACITY - 1]).unwrap();
    assert_eq!(n, OUTPUT_CAPACITY - 1);
    assert_eq!(p.slave_write(b"\n"), Ok(0), "\\r\\n needs two bytes");
    assert!(!p.slave_poll().writable);
    master_drain(&mut p);
    assert!(p.slave_poll().writable);
}

#[test]
fn echo_is_dropped_not_blocking_when_output_is_full() {
    let mut p = open_pair();
    p.slave_write(&vec![b'.'; OUTPUT_CAPACITY]).unwrap();
    let r = p.master_write(b"ls\n").unwrap();
    assert_eq!(r.accepted, 3);
    assert_eq!(slave_read_bytes(&mut p, 8), b"ls\n");
}

#[test]
fn queues_report_what_a_read_would_return() {
    let mut p = open_pair();
    p.master_write(b"ab\x04").unwrap();
    assert_eq!(p.input_queued(), 2, "the EOF mark is not a byte");
    p.slave_write(b"xyz").unwrap();
    assert_eq!(p.output_queued(), 3 + 2, "plus the echo of `ab`");
    p.flush(true, true);
    assert_eq!((p.input_queued(), p.output_queued()), (0, 0));
}

// ---- window size, waiters ------------------------------------------------------------

#[test]
fn winsize_change_is_reported_once() {
    let mut p = open_pair();
    let ws = Winsize { row: 40, col: 120, xpixel: 0, ypixel: 0 };
    assert!(p.set_winsize(ws));
    assert!(!p.set_winsize(ws));
    assert_eq!(p.winsize, ws);
}

#[test]
fn waiters_fill_replace_and_drain() {
    let mut p = open_pair();
    p.take_wakes();
    for tid in 0..MAX_WAITERS {
        assert!(p.register_waiter(tid, tid as u32));
    }
    assert!(!p.register_waiter(99, 0), "full");
    assert!(p.register_waiter(3, 33), "re-registering replaces");
    assert!(p.take_wakes().is_empty(), "registering is not a change");
    p.master_write(b"x").unwrap();
    let mut fired = Vec::new();
    p.take_wakes().fire(|tid, w| fired.push((tid, w)));
    assert_eq!(fired.len(), MAX_WAITERS);
    assert!(fired.contains(&(3, 33)));
    assert!(p.take_wakes().is_empty());
    assert!(p.register_waiter(99, 0));
}

/// The bug the first kernel boot found: a blocked reader registers and the
/// same locked section then handed it its own wake, so it spun instead of
/// sleeping. Only a real change releases waiters.
#[test]
fn a_reader_that_finds_nothing_stays_registered() {
    let mut p = open_pair();
    p.take_wakes();
    assert_eq!(slave_read(&mut p, 8), SlaveRead::Block { timeout_ds: None });
    assert!(p.register_waiter(7, 7));
    assert!(p.take_wakes().is_empty(), "nothing changed");
    assert_eq!(p.master_read(&mut [0; 8]), MasterRead::WouldBlock);
    let _ = p.slave_poll();
    assert!(p.take_wakes().is_empty(), "looking is not a change");
    p.master_write(b"line\n").unwrap();
    let mut woke = Vec::new();
    p.take_wakes().fire(|tid, _| woke.push(tid));
    assert_eq!(woke, vec![7]);
}

#[test]
fn draining_releases_a_blocked_writer() {
    let mut p = open_pair();
    p.slave_write(&vec![b'.'; OUTPUT_CAPACITY]).unwrap();
    p.take_wakes();
    assert!(p.register_waiter(5, 5));
    assert!(p.take_wakes().is_empty());
    master_drain(&mut p);
    assert!(!p.take_wakes().is_empty());
}

#[test]
fn ring_wraps_and_reads_across_the_seam() {
    let mut r: Ring<u8> = Ring::try_new(4).unwrap();
    assert!(Ring::<u8>::try_new(0).is_none());
    for b in 1..=3 {
        assert!(r.push(b));
    }
    assert_eq!(r.pop(), Some(1));
    assert!(r.push(4) && r.push(5));
    assert!(!r.push(6));
    let mut out = [0u8; 8];
    assert_eq!(r.read_into(&mut out), 4);
    assert_eq!(&out[..4], &[2, 3, 4, 5]);
    assert!(r.is_empty());
}

/// What the Akuma `ssh` client asks for (`SET_TERMINAL_ATTRIBUTES` raw): keys
/// reach the reader at once, byte for byte, with no echo — arrow keys and a
/// lone Esc included. In cooked mode the same bytes sit in the line buffer
/// and are echoed back as `^[[A`.
#[test]
fn raw_slave_gets_keys_unedited_and_unechoed() {
    let mut p = open_pair();
    let mut t = p.termios();
    t.make_raw();
    p.set_termios(t);
    p.master_write(b"\x1b[A").unwrap();
    p.master_write(b"\r").unwrap();
    p.master_write(b"\x7f").unwrap();
    p.master_write(b"\x1b").unwrap();
    let mut out = [0u8; 16];
    assert_eq!(p.slave_read(&mut out, false), SlaveRead::Data(6));
    assert_eq!(&out[..6], b"\x1b[A\r\x7f\x1b");
    assert!(master_drain(&mut p).is_empty(), "raw: nothing echoed");
}

#[test]
fn cooked_slave_buffers_and_echoes_the_same_keys() {
    let mut p = open_pair();
    p.master_write(b"\x1b[A").unwrap();
    assert_eq!(slave_read(&mut p, 16), SlaveRead::Block { timeout_ds: None });
    assert_eq!(master_drain(&mut p), b"^[[A");
}

#[test]
fn make_cooked_undoes_make_raw() {
    let mut t = Termios::initial();
    t.make_raw();
    t.make_cooked();
    assert_eq!(t, Termios::initial());
}
