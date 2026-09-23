#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libakuma::*;
use paws::{Edit, Flow, LineEditor, Mode, Stdout};

// The shell itself is the library (`lib.rs`), shared with sshd's `builtin-paws`
// in-process fallback; this binary is only its standalone host.

#[no_mangle]
pub extern "C" fn main() {
    let args_vec: Vec<String> = args().map(String::from).collect();

    // If -c is provided, execute the command and exit
    if args_vec.len() > 2 && args_vec[1] == "-c" {
        let code = match paws::execute_line(&args_vec[2], Mode::Standalone, &mut Stdout) {
            Flow::Continue(s) | Flow::Exit(s) => s,
            Flow::Reboot => 1, // standalone reboots in place; only a failure returns
        };
        exit(code);
    }

    run_shell();
}

fn run_shell() {
    paws::banner(&mut Stdout);
    let mut editor = LineEditor::new();
    let mut buf = [0u8; 1];

    loop {
        paws::prompt(&mut Stdout);
        let line = loop {
            let n = poll_input_event(u64::MAX, &mut buf);
            if n <= 0 {
                // Input closed: end the shell, as Ctrl-D on an empty line does.
                println("exit");
                exit(0);
            }
            match editor.feed(buf[0], &mut Stdout) {
                Edit::Pending => {}
                Edit::Line(line) => break Some(line),
                Edit::Interrupt => break None,
                Edit::Eof => exit(0),
            }
        };
        let Some(line) = line else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Flow::Exit(code) = paws::execute_line(trimmed, Mode::Standalone, &mut Stdout) {
            exit(code);
        }
    }
}
