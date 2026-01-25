//! boa - JavaScript interpreter for akuma userspace
//!
//! A simple JavaScript file executor using the Boa engine.
//!
//! Usage: boa <script.js>
//!
//! Build: cargo build --target aarch64-unknown-linux-musl

use boa_engine::{Context, Source};
use std::env;
use std::fs;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("boa: JavaScript interpreter for akuma");
        eprintln!("Usage: boa <script.js>");
        process::exit(1);
    }

    let script_path = &args[1];

    // Read the script file
    let source_code = match fs::read_to_string(script_path) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error reading file '{}': {}", script_path, e);
            process::exit(1);
        }
    };

    // Create JavaScript context and evaluate
    let mut context = Context::default();

    match context.eval(Source::from_bytes(&source_code)) {
        Ok(result) => {
            // Try to display the result
            match result.to_string(&mut context) {
                Ok(s) => {
                    let output = s.to_std_string_escaped();
                    if output != "undefined" {
                        println!("{}", output);
                    }
                }
                Err(_) => {}
            }
        }
        Err(e) => {
            eprintln!("JavaScript Error: {e}");
            process::exit(1);
        }
    }
}
