use boa::runtime::{Runtime, Source};
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    
    if args.len() < 2 {
        eprintln!("Usage: boa [script.js] or pipe JavaScript code through stdin");
        return;
    }

    // Try to read from file first
    let script_path = PathBuf::from(&args[1]);
    let source_code = if script_path.exists() {
        fs::read_to_string(script_path).expect("Failed to read script file")
    } else {
        // Fallback to reading from stdin
        println!("Reading JavaScript code from stdin...");
        let mut input = String::new();
        io::stdin().read_to_string(&mut input).expect("Failed to read from stdin");
        input
    };

    // Create and run the runtime
    let mut runtime = Runtime::new();
    let result = runtime.eval(Source::from_bytes(source_code.as_bytes()));

    match result {
        Ok(value) => println!("Result: {}", value),
        Err(e) => eprintln!("Error: {}", e),
    }
}
