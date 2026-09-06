//! Host tests for the `/proc` wire formats.
//!
//! These pin the things that fail *silently*: a field count, a field position,
//! a truncation boundary and a trailing NUL. Every one of them is invisible to
//! the kernel that gets it wrong and visible only as `ps` printing nothing, or
//! printing a plausible wrong number.

extern crate std;

use std::string::{String, ToString};
use std::vec::Vec;

use super::*;

fn stat_of(pid: u32, ppid: u32, name: &str, state: ProcState, us: u64) -> ProcStat<'_> {
    ProcStat { pid, ppid, state, name, cpu_time_us: us }
}

fn render_stat_str(p: &ProcStat) -> String {
    let mut buf = [0u8; STAT_LINE_MAX];
    let n = render_pid_stat(p, &mut buf);
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

/// The whole reason the crate exists: `ps` finds `utime` by counting to field
/// 14, so the count and the positions are the contract.
#[test]
fn stat_line_has_the_44_fields_ps_counts_through() {
    let p = stat_of(42, 7, "/bin/busybox", ProcState::Running, 0);
    let line = render_stat_str(&p);
    let line = line.trim_end_matches('\n');
    let fields: Vec<&str> = line.split(' ').collect();
    assert_eq!(fields.len(), 44, "field count changed: {line}");
    assert_eq!(fields[0], "42", "field 1 is pid");
    assert_eq!(fields[1], "(busybox)", "field 2 is (comm)");
    assert_eq!(fields[2], "R", "field 3 is state");
    assert_eq!(fields[3], "7", "field 4 is ppid");
}

/// Field 14 is `utime`, in jiffies. This is the number `ps` prints as TIME, and
/// getting the *position* wrong is how it prints someone else's.
#[test]
fn utime_is_field_14_in_jiffies() {
    // 2.5 seconds of CPU = 250 jiffies at 100 Hz.
    let p = stat_of(1, 0, "sshd", ProcState::Running, 2_500_000);
    let line = render_stat_str(&p);
    let fields: Vec<&str> = line.trim_end().split(' ').collect();
    assert_eq!(fields[13], "250", "utime must be field 14 (0-indexed 13)");
    assert_eq!(fields[14], "0", "stime (field 15) is 0: no user/system split");
}

#[test]
fn sub_jiffy_cpu_time_reports_zero_not_a_rounded_up_tick() {
    let p = stat_of(1, 0, "sh", ProcState::Running, JIFFY_US - 1);
    assert_eq!(p.utime_jiffies(), 0);
}

/// `pgrp` and `session` are the pid: no process groups on either kernel, so
/// every process leads its own — the same answer `getpgrp`/`getsid` give.
#[test]
fn pgrp_and_session_report_the_pid_itself() {
    let p = stat_of(99, 1, "sh", ProcState::Running, 0);
    let line = render_stat_str(&p);
    let fields: Vec<&str> = line.trim_end().split(' ').collect();
    assert_eq!(fields[4], "99", "pgrp");
    assert_eq!(fields[5], "99", "session");
}

#[test]
fn state_chars_match_what_ps_expects() {
    for (state, want) in [
        (ProcState::Running, 'R'),
        (ProcState::Sleeping, 'S'),
        (ProcState::Zombie(0), 'Z'),
    ] {
        assert_eq!(state.stat_char(), want);
        let p = stat_of(1, 0, "x", state, 0);
        let line = render_stat_str(&p);
        assert_eq!(line.split(' ').nth(2).unwrap(), want.to_string());
    }
}

/// `comm` is the **basename**, not the path — `ps` prints this directly, and a
/// full path in the COMMAND column is how you notice.
#[test]
fn comm_is_the_basename() {
    assert_eq!(stat_of(1, 0, "/bin/busybox", ProcState::Running, 0).comm(), "busybox");
    assert_eq!(stat_of(1, 0, "busybox", ProcState::Running, 0).comm(), "busybox");
    assert_eq!(stat_of(1, 0, "/a/b/c/d", ProcState::Running, 0).comm(), "d");
}

#[test]
fn comm_truncates_to_15_bytes() {
    let p = stat_of(1, 0, "/bin/aaaaaaaaaaaaaaaaaaaaaaaaaaaa", ProcState::Running, 0);
    assert_eq!(p.comm().len(), COMM_LEN);
    assert_eq!(p.comm(), "aaaaaaaaaaaaaaa");
}

/// Truncation must land on a char boundary or the `&str` slice panics — a
/// kernel panic reachable from any program that names itself in UTF-8.
#[test]
fn comm_truncation_does_not_split_a_utf8_character() {
    // 'é' is two bytes: eight of them is 16 bytes, so the 15-byte cut lands
    // inside the eighth character.
    let name = "éééééééé";
    assert_eq!(name.len(), 16);
    let p = stat_of(1, 0, name, ProcState::Running, 0);
    let comm = p.comm();
    assert!(comm.len() <= COMM_LEN);
    assert_eq!(comm, "ééééééé", "trimmed to the boundary below 15");
}

/// A `comm` containing a space or a paren is what makes the 44-field split
/// ambiguous on real Linux; record what this renderer actually does rather than
/// pretending the case cannot arise.
#[test]
fn comm_with_a_space_is_a_known_ambiguity() {
    let p = stat_of(1, 0, "my prog", ProcState::Running, 0);
    let line = render_stat_str(&p);
    // Linux has the same property; `ps` scans for the *last* ')' rather than
    // splitting blindly. Pinned so a future change to quote it is deliberate.
    assert!(line.starts_with("1 (my prog) R "));
    assert_eq!(line.trim_end().split(' ').count(), 45, "the space adds a field");
}

#[test]
fn status_carries_the_lines_libcap_ng_reads() {
    let p = stat_of(3, 1, "/bin/sh", ProcState::Sleeping, 0);
    let mut buf = [0u8; STATUS_MAX];
    let n = render_status(&p, &mut buf);
    let text = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(text.starts_with("Name:\tsh\n"));
    assert!(text.contains("State:\tS (sleeping)\n"));
    assert!(text.contains("Pid:\t3\n"));
    assert!(text.contains("PPid:\t1\n"));
    for line in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        assert!(text.contains(line), "missing {line} — libcap-ng reads these");
    }
    assert!(text.contains(CAP_FULL_MASK));
    assert!(!text.contains("ExitCode:"), "only a zombie reports one");
}

#[test]
fn a_zombie_reports_its_exit_code_in_status() {
    let p = stat_of(3, 1, "sh", ProcState::Zombie(7), 0);
    let mut buf = [0u8; STATUS_MAX];
    let n = render_status(&p, &mut buf);
    let text = core::str::from_utf8(&buf[..n]).unwrap();
    assert!(text.contains("State:\tZ (zombie)\n"));
    assert!(text.ends_with("ExitCode:\t7\n"));
}

#[test]
fn status_fits_in_its_declared_maximum() {
    let p = stat_of(u32::MAX, u32::MAX, "aaaaaaaaaaaaaaaaaaaaaaaa", ProcState::Zombie(-1), 0);
    let mut buf = [0u8; STATUS_MAX];
    let n = render_status(&p, &mut buf);
    assert!(n < STATUS_MAX, "STATUS_MAX must not be a truncation point");
}

#[test]
fn stat_line_fits_in_its_declared_maximum() {
    let p = stat_of(u32::MAX, u32::MAX, "aaaaaaaaaaaaaaaaaaaaaa", ProcState::Running, u64::MAX);
    let mut buf = [0u8; STAT_LINE_MAX];
    let n = render_pid_stat(&p, &mut buf);
    assert!(n < STAT_LINE_MAX, "STAT_LINE_MAX must not be a truncation point");
}

/// Every element is NUL-terminated **including the last** — `ps` splits on NUL,
/// so a missing final one merges the last argument into the reader's buffer.
#[test]
fn cmdline_nul_terminates_every_element() {
    let p = stat_of(1, 0, "/bin/sh", ProcState::Running, 0);
    let args: [&[u8]; 3] = [b"sh", b"-c", b"echo hi"];
    let mut buf = [0u8; 64];
    let n = render_cmdline(&p, args, &mut buf);
    assert_eq!(&buf[..n], b"sh\0-c\0echo hi\0");
}

/// An empty argv falls back to the name rather than emitting nothing: a truly
/// empty `cmdline` is how Linux marks a kernel thread, and `ps` then brackets
/// the name.
#[test]
fn cmdline_falls_back_to_the_name_when_argv_is_empty() {
    let p = stat_of(1, 0, "/bin/sshd", ProcState::Running, 0);
    let args: [&[u8]; 0] = [];
    let mut buf = [0u8; 64];
    let n = render_cmdline(&p, args, &mut buf);
    assert_eq!(&buf[..n], b"/bin/sshd\0");
}

#[test]
fn cmdline_truncates_rather_than_overflowing() {
    let p = stat_of(1, 0, "x", ProcState::Running, 0);
    let args: [&[u8]; 2] = [b"aaaaaaaa", b"bbbbbbbb"];
    let mut buf = [0u8; 4];
    let n = render_cmdline(&p, args, &mut buf);
    assert_eq!(n, 4);
    assert_eq!(&buf[..n], b"aaaa");
}

#[test]
fn a_zero_length_buffer_writes_nothing_rather_than_panicking() {
    let p = stat_of(1, 0, "x", ProcState::Running, 0);
    let args: [&[u8]; 1] = [b"x"];
    assert_eq!(render_cmdline(&p, args, &mut []), 0);
    assert_eq!(render_pid_stat(&p, &mut []), 0);
    assert_eq!(render_status(&p, &mut []), 0);
}
