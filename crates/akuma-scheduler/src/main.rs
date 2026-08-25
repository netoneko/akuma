//! Runs the scenario matrix and prints it. `cargo run -p akuma-scheduler --bin
//! sched-sim --features cli --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)`.

use akuma_scheduler::scenarios::{adaptive_netpoll, shape, sweep, sweep_with};
use akuma_scheduler::{Config, NetpollPolicy, Report, SchedPolicy, Sim, WakePlacement};

fn row(label: &str, r: &[Report]) {
    print!("| {label:<34} |");
    for x in r {
        print!(" {:>8.0} |", x.iters_per_sec);
    }
    println!();
}

fn main() {
    println!("# Scheduler placement / netpoll wake simulation\n");
    println!("Model: 4 cores, 1 ms tick, one barrier-synchronous compute group of");
    println!("`-t N` threads + the netpoll thread. 10 s simulated per cell.");
    println!("Numbers are barrier iterations/s — a THROUGHPUT PROXY, not tok/s.\n");

    // --- calibration -------------------------------------------------
    println!("## Calibration against the hardware\n");
    println!("Hardware (CROSS_CORE_THREAD_COLLAPSE.md §4): peak at -t 3, -t 4 is 14.6x below it.\n");
    println!("| wake path                    | peak | -t 4 collapse |");
    println!("|------------------------------|-----:|--------------:|");
    for (name, w) in [
        ("immediate (SGI takes idle core)", WakePlacement::Immediate),
        ("next tick (waits out rotation)", WakePlacement::NextTick),
    ] {
        let (peak, ratio) = shape(&sweep_with(
            SchedPolicy::Immunity { ticks: 5 },
            NetpollPolicy::EveryTick,
            w,
        ));
        println!("| {name:<28} | {peak:>4} | {ratio:>12.1}x |");
    }
    println!("\nTick sensitivity: the tick is probe-selected from [1,2,3,5] ms and this");
    println!("host refuses WFI below ~2.5 ms, so 3 ms is the live value, not 1 ms.\n");
    println!("| tick | wake path  | -t 3 | -t 4 | collapse |");
    println!("|-----:|-----------|-----:|-----:|---------:|");
    for tick in [1_000_u64, 2_000, 3_000, 5_000] {
        for (nm, w) in [("immediate", WakePlacement::Immediate), ("next tick", WakePlacement::NextTick)] {
            let r: Vec<Report> = (1..=4)
                .map(|t| {
                    let mut c = Config::devbox(t);
                    c.tick_us = tick;
                    c.wake = w;
                    Sim::new(c).run()
                })
                .collect();
            let (_, ratio) = shape(&r);
            println!(
                "| {:>3} ms | {nm:<9} | {:>4.0} | {:>4.0} | {ratio:>7.1}x |",
                tick / 1000, r[2].iters_per_sec, r[3].iters_per_sec
            );
        }
    }
    println!("\nHow much wake latency would it take?\n");
    println!("| futex wake latency | -t 3 | -t 4 | collapse |");
    println!("|-------------------:|-----:|-----:|---------:|");
    for lat in [60_u64, 250, 500, 1_000, 2_000, 5_000] {
        let r: Vec<Report> = (1..=4)
            .map(|t| {
                let mut c = Config::devbox(t);
                c.wake_latency_us = lat;
                c.wake = WakePlacement::NextTick;
                Sim::new(c).run()
            })
            .collect();
        let (_, ratio) = shape(&r);
        println!(
            "| {lat:>15} us | {:>4.0} | {:>4.0} | {ratio:>7.1}x |",
            r[2].iters_per_sec, r[3].iters_per_sec
        );
    }
    println!();

    // --- placement policies ------------------------------------------
    println!("## Placement policy (netpoll unchanged: wakes every tick)\n");
    println!("| policy                             |     -t 1 |     -t 2 |     -t 3 |     -t 4 |");
    println!("|------------------------------------|---------:|---------:|---------:|---------:|");
    let rr = sweep(SchedPolicy::RoundRobin, NetpollPolicy::EveryTick);
    let imm = sweep(SchedPolicy::Immunity { ticks: 5 }, NetpollPolicy::EveryTick);
    let spr = sweep(SchedPolicy::Spread { starvation_us: 2_000, latency_starvation_us: 100 }, NetpollPolicy::EveryTick);
    let pin = sweep(SchedPolicy::Pinned, NetpollPolicy::EveryTick);
    row("round-robin (pre-fix)", &rr);
    row("immunity(5) — TODAY", &imm);
    row("spread governor", &spr);
    row("pinned (hard affinity)", &pin);
    println!();

    // --- does affinity survive traffic? -------------------------------
    println!("## Pinning vs spreading as network traffic arrives (-t 4)\n");
    println!("Throughput is only half the story: a policy can protect the compute");
    println!("group by starving netpoll. Both columns matter.\n");
    println!("| traffic pps |          policy | iters/s | mean pkt lat us | max pkt lat us |");
    println!("|------------:|----------------:|--------:|----------------:|---------------:|");
    for pps in [20_u64, 1_000, 5_000] {
        for (name, sched, np) in [
            ("pinned", SchedPolicy::Pinned, NetpollPolicy::EveryTick),
            ("immunity(5)", SchedPolicy::Immunity { ticks: 5 }, NetpollPolicy::EveryTick),
            ("spread", SchedPolicy::Spread { starvation_us: 2_000, latency_starvation_us: 100 }, NetpollPolicy::EveryTick),
            ("spread+adaptive", SchedPolicy::Spread { starvation_us: 2_000, latency_starvation_us: 100 }, adaptive_netpoll()),
        ] {
            let mut c = Config::devbox(4);
            c.traffic_pps = pps;
            c.sched = sched;
            c.netpoll = np;
            let r = Sim::new(c).run();
            println!(
                "| {pps:>11} | {name:>15} | {:>7.0} | {:>15.0} | {:>14} |",
                r.iters_per_sec, r.packet_latency_mean_us, r.packet_latency_max_us
            );
        }
    }
    println!();

    // --- netpoll policy ----------------------------------------------
    println!("## Netpoll wake policy (placement = today's immunity(5))\n");
    println!("| policy                             |     -t 1 |     -t 2 |     -t 3 |     -t 4 |");
    println!("|------------------------------------|---------:|---------:|---------:|---------:|");
    let ad = sweep(SchedPolicy::Immunity { ticks: 5 }, adaptive_netpoll());
    let both = sweep(SchedPolicy::Spread { starvation_us: 2_000, latency_starvation_us: 100 }, adaptive_netpoll());
    row("every tick — TODAY", &imm);
    row("traffic-adaptive (10 s window)", &ad);
    row("spread + traffic-adaptive", &both);
    println!();

    // --- stability ----------------------------------------------------
    println!("## Output stability at -t 4 (iteration interval)\n");
    println!("| policy                             | mean us | stddev | p99 us | preempt | parks |");
    println!("|------------------------------------|--------:|-------:|-------:|--------:|------:|");
    for (label, r) in [
        ("immunity(5) — TODAY", &imm[3]),
        ("pinned (hard affinity)", &pin[3]),
        ("spread governor", &spr[3]),
        ("traffic-adaptive netpoll", &ad[3]),
        ("spread + traffic-adaptive", &both[3]),
    ] {
        println!(
            "| {label:<34} | {:>7.0} | {:>6.0} | {:>6} | {:>7} | {:>5} |",
            r.iter_mean_us, r.iter_stddev_us, r.iter_p99_us, r.compute_preemptions, r.barrier_parks
        );
    }
    println!();

    // --- netpoll cost / latency ---------------------------------------
    println!("## Netpoll core cost and packet latency (-t 3)\n");
    println!("| traffic |            policy | core frac | wakes | mean lat us | max lat us |");
    println!("|--------:|------------------:|----------:|------:|------------:|-----------:|");
    for pps in [0_u64, 20, 1_000, 5_000] {
        for (name, np) in [("every tick", NetpollPolicy::EveryTick), ("adaptive", adaptive_netpoll())] {
            let mut cfg = Config::devbox(3);
            cfg.traffic_pps = pps;
            cfg.netpoll = np;
            let r = Sim::new(cfg).run();
            println!(
                "| {pps:>7} | {name:>17} | {:>9.3} | {:>5} | {:>11.0} | {:>10} |",
                r.netpoll_core_frac, r.netpoll_wakes, r.packet_latency_mean_us, r.packet_latency_max_us
            );
        }
    }
    println!();

    // --- the work-conservation bound ----------------------------------
    println!("## Is there room to win at all? (work conservation)\n");
    for (label, r) in [("immunity(5) — TODAY", &imm[3]), ("spread + adaptive", &both[3])] {
        println!(
            "-t 4 {label:<26} compute {:.3} + netpoll {:.3} = {:.3} of 4 cores",
            r.compute_core_frac * 4.0,
            r.netpoll_core_frac,
            r.compute_core_frac.mul_add(4.0, r.netpoll_core_frac)
        );
    }
}
