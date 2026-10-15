//! `powersaved`'s pure half: the dim decision and the command line.
//!
//! The daemon's only input is `/proc/power`'s `idle_ms=` (console-input
//! inactivity, `amd64/src/input.rs`) and its only effect is a `dim=<percent>`
//! line to `/dev/power`. Everything between those — which idle duration maps
//! to which brightness, when a change is worth a write, what the argv meant —
//! is here, so a host test can exhaust it and the binary only carries
//! effects. The same shape `sshd`'s `wire.rs` and `box`'s `boxlib` chose.
//!
//! The policy is deliberately a **staircase, not a curve**: each stage names
//! an idle duration and the brightness to hold from there, the first stage
//! below the idle time wins, and anything above the first stage is full
//! brightness. Hysteresis is the restore rule: a keystroke is *instant* (the
//! next observation is above the first stage, so one write puts the light
//! back), while dimming only ever steps on the daemon's period boundary —
//! the person is already gone, so nobody watches the transition.

#![cfg_attr(not(test), no_std)]

/// Brightness when the keyboard is not idle, and what a restore writes.
pub const RESTORE_PCT: u8 = 100;

/// One step of the staircase: hold `pct` percent from `after_ms` of console
/// inactivity onward.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Stage {
    pub after_ms: u64,
    pub pct: u8,
}

/// The shipped defaults: visible soon, deep eventually, never quite black —
/// the framebuffer console's dim is colour scaling, and a screen someone has
/// to walk to must still say *something* from across the room. argv replaces
/// the whole staircase (`stage=after_ms:pct`, repeatable), so these are a
/// default and not policy.
pub const DEFAULT_STAGES: [Stage; 3] = [
    Stage { after_ms: 60_000, pct: 60 },
    Stage { after_ms: 180_000, pct: 30 },
    Stage { after_ms: 600_000, pct: 10 },
];

/// The brightness `idle_ms` of inactivity asks for: the **deepest** stage
/// that has already elapsed wins — the highest `after_ms` at or below the
/// idle time — or [`RESTORE_PCT`] above the first. The rule is
/// order-independent on purpose: [`parse_stages`] hands over a sorted
/// staircase so the common path is a straight scan, but a hand-built table
/// answers the same policy in the same way regardless of the order its
/// author wrote it down in. A tie keeps the later entry.
#[must_use]
pub fn target_pct(idle_ms: u64, stages: &[Stage]) -> u8 {
    let mut best_after: u64 = 0;
    let mut target = RESTORE_PCT;
    for s in stages {
        if idle_ms >= s.after_ms && (best_after == 0 || s.after_ms >= best_after) {
            best_after = s.after_ms;
            target = s.pct;
        }
    }
    target
}

/// The write-worthy outcome of one observation: `Some(pct)` only when the
/// factor in force no longer matches what the staircase asks for, `None` on
/// an ordinary quiet second. The daemon therefore writes to `/dev/power`
/// three times per idle cycle (down the steps) and once on the keystroke —
/// never once a second.
#[must_use]
pub fn step(current_pct: u8, idle_ms: u64, stages: &[Stage]) -> Option<u8> {
    let target = target_pct(idle_ms, stages);
    (target != current_pct).then_some(target)
}

/// The one fact the daemon reads, out of `/proc/power` or a `/dev/power`
/// snapshot: the value of `key=` in a `key=value` line file. `None` when the
/// kernel is too old to say — which the daemon treats as "do nothing" rather
/// than "dim blind".
#[must_use]
pub fn fact<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(key)
            && let Some(v) = rest.strip_prefix('=') {
            return Some(v);
        }
    }
    None
}

/// `after_ms:pct` — one argv stage. Sorted insertion is the caller's job
/// ([`parse_stages`]); this only refuses what cannot be a stage.
#[must_use]
pub fn parse_stage(arg: &str) -> Option<Stage> {
    let (ms, pct) = arg.split_once(':')?;
    let after_ms = parse_ms(ms)?;
    let pct: u8 = pct.parse().ok()?;
    (pct > 0 && pct <= 100).then_some(Stage { after_ms, pct })
}

/// A whole `stage=` list, kept sorted ascending by `after_ms` and capped at
/// `out.len()` stages — extra entries are refused rather than silently
/// dropped, because a staircase someone asked for that half-arrived is a
/// policy nobody can reason about. Returns `None` (keep the defaults) for an
/// empty list; the caller decides what a `None` from a *malformed* list means
/// (the daemon prints and exits: a typo in the boot config must not silently
/// dim on defaults).
#[must_use]
pub fn parse_stages(args: &[&str], out: &mut [Stage]) -> Option<usize> {
    if args.is_empty() {
        return None;
    }
    if args.len() > out.len() {
        return None;
    }
    let mut n = 0;
    for a in args {
        out[n] = parse_stage(a)?;
        n += 1;
    }
    out[..n].sort_unstable_by_key(|s| s.after_ms);
    Some(n)
}

/// `60000` — a millisecond count for `after_ms=` and `period_ms=`.
#[must_use]
pub fn parse_ms(arg: &str) -> Option<u64> {
    arg.parse::<u64>().ok().filter(|v| *v > 0)
}

#[cfg(test)]
mod tests {
    use super::{fact, parse_ms, parse_stage, parse_stages, step, target_pct, Stage, DEFAULT_STAGES, RESTORE_PCT};

    #[test]
    fn staircase_by_elapsed_time() {
        assert_eq!(target_pct(0, &DEFAULT_STAGES), RESTORE_PCT);
        assert_eq!(target_pct(59_999, &DEFAULT_STAGES), RESTORE_PCT);
        assert_eq!(target_pct(60_000, &DEFAULT_STAGES), 60);
        assert_eq!(target_pct(179_999, &DEFAULT_STAGES), 60);
        assert_eq!(target_pct(180_000, &DEFAULT_STAGES), 30);
        assert_eq!(target_pct(599_999, &DEFAULT_STAGES), 30);
        assert_eq!(target_pct(600_000, &DEFAULT_STAGES), 10);
        assert_eq!(target_pct(u64::MAX, &DEFAULT_STAGES), 10);
    }

    #[test]
    fn unsorted_stages_still_answer_the_last_elapsed_one() {
        // parse_stages sorts; but the decision itself must not depend on the
        // sort for the *last matching* rule to stay the meaning.
        let stages = [Stage { after_ms: 180_000, pct: 30 }, Stage { after_ms: 60_000, pct: 60 }];
        assert_eq!(target_pct(61_000, &stages), 60);
        assert_eq!(target_pct(181_000, &stages), 30);
    }

    #[test]
    fn writes_only_on_change() {
        let mut current = RESTORE_PCT;
        // the long walk down: three writes
        assert_eq!(step(current, 61_000, &DEFAULT_STAGES), Some(60));
        current = 60;
        assert_eq!(step(current, 61_000, &DEFAULT_STAGES), None);
        assert_eq!(step(current, 200_000, &DEFAULT_STAGES), Some(30));
        current = 30;
        assert_eq!(step(current, 700_000, &DEFAULT_STAGES), Some(10));
        current = 10;
        assert_eq!(step(current, 900_000, &DEFAULT_STAGES), None);
        // the keystroke: one write straight back
        assert_eq!(step(current, 500, &DEFAULT_STAGES), Some(RESTORE_PCT));
    }

    #[test]
    fn facts_read_key_value_lines() {
        let p = "source=ec\nac=0\nidle_ms=61234\ndim=40\n";
        assert_eq!(fact(p, "idle_ms"), Some("61234"));
        assert_eq!(fact(p, "dim"), Some("40"));
        assert_eq!(fact(p, "power_mw"), None);
        // no prefix accidents: "id" must not read "idle_ms"
        assert_eq!(fact(p, "id"), None);
        assert_eq!(fact("source=none", "idle_ms"), None);
    }

    #[test]
    fn stage_args_parse_and_sort() {
        assert_eq!(parse_stage("180000:30"), Some(Stage { after_ms: 180_000, pct: 30 }));
        assert_eq!(parse_stage("60000:60"), Some(Stage { after_ms: 60_000, pct: 60 }));
        assert_eq!(parse_stage("0:40"), None);
        assert_eq!(parse_stage("60000:0"), None);
        assert_eq!(parse_stage("60000:101"), None);
        assert_eq!(parse_stage(":"), None);
        assert_eq!(parse_stage("60:40"), Some(Stage { after_ms: 60, pct: 40 }));

        let mut out = [Stage { after_ms: 0, pct: 0 }; 8];
        let n = parse_stages(&["600000:10", "60000:60", "180000:30"], &mut out).unwrap();
        assert_eq!(n, 3);
        assert_eq!(target_pct(200_000, &out[..n]), 30);
        // a list longer than the buffer is refused, not truncated
        assert_eq!(parse_stages(&["1:1", "2:2", "3:3", "4:4", "5:5", "6:6", "7:7", "8:8", "9:9"], &mut out), None);
        // an empty list means "keep the defaults"
        assert_eq!(parse_stages(&[], &mut out), None);
        // one malformed entry poisons the whole list
        assert_eq!(parse_stages(&["60000:60", "zap"], &mut out), None);
    }

    #[test]
    fn ms_args_are_positive() {
        assert_eq!(parse_ms("1000"), Some(1000));
        assert_eq!(parse_ms("0"), None);
        assert_eq!(parse_ms("-5"), None);
        assert_eq!(parse_ms(""), None);
    }
}
