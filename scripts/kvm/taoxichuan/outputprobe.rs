use std::process::Command;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.get(1).map(String::as_str).unwrap_or("/usr/local/rust/bin/rustc");
    let rest: Vec<&str> = args.iter().skip(2).map(String::as_str).collect();
    let out = Command::new(prog).args(&rest).output();
    match out {
        Ok(o) => println!("OUTPUT-OK status={:?} stdout={}B stderr={}B", o.status.code(), o.stdout.len(), o.stderr.len()),
        Err(e) => println!("OUTPUT-ERR {e}"),
    }
}
