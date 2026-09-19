//! `litter-hub --port 7700 --roster sherlock,hercules,zenigata,ressler`
//!
//! Thin CLI wrapper around `HubState` (see `lib.rs`) — all the logic worth
//! testing lives there and is exercised by `cargo test` without this binary
//! or a real socket loop at all.

use std::net::TcpListener;
use std::sync::Arc;

use litter_hub::HubState;

fn main() {
    let mut port: u16 = 7700;
    let mut roster: Vec<String> = Vec::new();

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                if let Some(p) = args.get(i) {
                    port = p.parse().unwrap_or_else(|_| {
                        eprintln!("litter-hub: invalid --port '{}'", p);
                        std::process::exit(1);
                    });
                }
            }
            "--roster" => {
                i += 1;
                if let Some(list) = args.get(i) {
                    roster = list.split(',').map(String::from).filter(|s| !s.is_empty()).collect();
                }
            }
            "--help" | "-h" => {
                println!("Usage: litter-hub --port <PORT> --roster <name1,name2,...>");
                return;
            }
            other => {
                eprintln!("litter-hub: unknown argument '{}'", other);
                std::process::exit(1);
            }
        }
        i += 1;
    }

    if roster.is_empty() {
        eprintln!("litter-hub: refusing to start with an empty --roster (ListPeers would have nothing to report)");
        std::process::exit(1);
    }
    for name in &roster {
        if !litter_wire::is_valid_name(name) {
            eprintln!("litter-hub: invalid agent name in --roster: '{}'", name);
            std::process::exit(1);
        }
    }

    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("litter-hub: failed to bind {}: {}", addr, e);
        std::process::exit(1);
    });
    println!("litter-hub: listening on {} (protocol v{}), roster: {:?}", addr, litter_wire::PROTOCOL_VERSION, roster);

    let state = Arc::new(HubState::new(roster));
    state.run_forever(listener);
}
