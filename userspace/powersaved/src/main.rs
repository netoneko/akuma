//! `powersaved` — the power-saving daemon.
//!
//! Once a second: read `/proc/power`, take its `idle_ms=` (console-input
//! inactivity; a keystroke anywhere — keyboard, serial, USB HID — resets it
//! in the kernel), and if the staircase of idle thresholds now asks for a
//! different brightness than the one in force, write `dim=<percent>` to
//! `/dev/power` and say so. The kernel applies it to the framebuffer console
//! and repaints; a keystroke observes as activity, the staircase answers
//! `100`, and the light is back on the next one-second lap.
//!
//! The kernel log records nothing; this daemon's stdout (herd drains it to
//! `/var/log/herd/`) is the decision log — one line per change, nothing on
//! quiet seconds, the same discipline `[IDLESTAT]` keeps.
//!
//! argv: `after_ms=<n>` moves the first step, `stage=<ms>:<pct>` (repeatable)
//! replaces the whole staircase, `period_ms=<n>` the poll period. A malformed
//! argv prints and exits non-zero: a typo in a boot config must not silently
//! dim on defaults. Allocation: a handful of small `String`s per second on a
//! userspace budget — nothing on the kernel's.

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![cfg_attr(test, allow(dead_code))]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use powersaved::{fact, parse_ms, parse_stage, parse_stages, step, Stage, DEFAULT_STAGES, RESTORE_PCT};

use libakuma::{fs, print, sleep_ms};

const PROC_POWER: &str = "/proc/power";
const DEV_POWER: &str = "/dev/power";

/// The daemon's configuration, from argv over the shipped defaults.
struct Config {
    stages: Vec<Stage>,
    period_ms: u64,
}

/// `key=value` argv, akuma command-line style (`ecram=0x…`, `idlesleep=<n>`).
fn apply_arg(cfg: &mut Config, a: &str) -> Result<(), String> {
    if let Some(v) = a.strip_prefix("stage=") {
        let s = parse_stage(v).ok_or_else(|| format!("bad stage: {a}"))?;
        cfg.stages.clear();
        cfg.stages.push(s);
        return Ok(());
    }
    if let Some(v) = a.strip_prefix("after_ms=") {
        let ms = parse_ms(v).ok_or_else(|| format!("bad after_ms: {a}"))?;
        // The one knob most people want: move the first step, keep the shape.
        cfg.stages = DEFAULT_STAGES.to_vec();
        cfg.stages[0] = Stage { after_ms: ms, pct: cfg.stages[0].pct };
        return Ok(());
    }
    if let Some(v) = a.strip_prefix("period_ms=") {
        cfg.period_ms = parse_ms(v).ok_or_else(|| format!("bad period_ms: {a}"))?;
        return Ok(());
    }
    Err(format!("unknown argument: {a}"))
}

#[unsafe(no_mangle)]
pub extern "C" fn main() {
    print("[powersaved] rev ");
    print(libakuma::GIT_REV);
    print("\n");

    // Collect `stage=` args first so the staircase is replaced wholesale and
    // sorted once; the single-knob flags then edit the result.
    let mut stage_args: Vec<&str> = Vec::new();
    let mut others: Vec<&str> = Vec::new();
    for i in 1..libakuma::argc() {
        match libakuma::arg(i) {
            Some(a) if a.starts_with("stage=") => stage_args.push(a),
            Some(a) => others.push(a),
            None => {}
        }
    }
    let mut cfg = Config { stages: Vec::new(), period_ms: 1000 };
    {
        let mut buf = [Stage { after_ms: 0, pct: 0 }; 8];
        match parse_stages(&stage_args, &mut buf) {
            None => cfg.stages = DEFAULT_STAGES.to_vec(),
            Some(n) => cfg.stages = buf[..n].to_vec(),
        }
    }
    for a in others {
        if let Err(e) = apply_arg(&mut cfg, a) {
            print("[powersaved] ");
            print(&e);
            print("\n");
            libakuma::exit(2);
        }
    }
    // Sync with what the kernel already holds (a daemon restart must not
    // double-step), then loop. `/dev/power` read = the two kernel facts.
    let mut current: u8 = match fs::read_to_string(DEV_POWER) {
        Ok(s) => fact(&s, "dim").and_then(|v| v.parse().ok()).unwrap_or(RESTORE_PCT),
        Err(_) => {
            print("[powersaved] no /dev/power — kernel too old? facts only, no dim\n");
            RESTORE_PCT
        }
    };
    print("[powersaved] watching idle, dim starts at ");
    print(&format!("{current}\n"));

    loop {
        if let Ok(s) = fs::read_to_string(PROC_POWER) {
            let idle_ms: u64 = fact(&s, "idle_ms").and_then(|v| v.parse().ok()).unwrap_or(0);
            // The kernel's own dim, from the same read: something else may have
            // changed it (a manual `dim=` write, another tool), and trusting the
            // cached value would then never write the restore — the screen would
            // stay dark under a person typing. Missing = keep what we have.
            if let Some(kernel_dim) = fact(&s, "dim").and_then(|v| v.parse::<u8>().ok())
                && kernel_dim != current
            {
                print(&format!("[powersaved] kernel dim is {kernel_dim}, not {current}; resyncing\n"));
                current = kernel_dim;
            }
            if let Some(target) = step(current, idle_ms, &cfg.stages) {
                let line = format!("dim={target}\n");
                match fs::write(DEV_POWER, line.as_bytes()) {
                    Ok(()) => {
                        print(&format!(
                            "[powersaved] idle_ms={idle_ms} dim {current} -> {target}\n"
                        ));
                        current = target;
                    }
                    Err(e) => {
                        // One line per failed write, not one per second: the
                        // retry is the loop, and a silent failure is worse.
                        print(&format!("[powersaved] dim write failed errno={e}\n"));
                    }
                }
            }
        }
        sleep_ms(cfg.period_ms);
    }
}
