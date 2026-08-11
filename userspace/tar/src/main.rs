//! `tar` — the CLI over [`akuma_tar`]. Extraction only.
//!
//! The extraction itself lives in the library so that `box` can link it instead
//! of spawning this binary; this file is argument parsing and error reporting.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use akuma_tar::{ExtractOptions, extract_file};
use libakuma::{args, eprintln, exit};

#[unsafe(no_mangle)]
pub extern "C" fn main() {
    let args_vec: Vec<String> = args().map(String::from).collect();

    let mut extract = false;
    let mut opts = ExtractOptions::default();
    let mut archive_file: Option<String> = None;
    let mut target_dir = String::from(".");

    let mut i = 1;
    while i < args_vec.len() {
        let arg = &args_vec[i];
        if arg.starts_with('-') {
            let mut stop_bundle = false;
            for (char_idx, c) in arg.chars().skip(1).enumerate() {
                if stop_bundle {
                    break;
                }
                match c {
                    'z' => opts.gzip = true,
                    'x' => extract = true,
                    'v' => opts.verbose = true,
                    'f' => {
                        // Either bundled (-xfarchive.tar) or the next argument.
                        if char_idx + 2 < arg.len() {
                            archive_file = Some(String::from(&arg[char_idx + 2..]));
                        } else if i + 1 < args_vec.len() {
                            archive_file = Some(args_vec[i + 1].clone());
                            i += 1;
                        } else {
                            eprintln("tar: option requires an argument -- f");
                            exit(1);
                        }
                        stop_bundle = true;
                    }
                    'C' => {
                        if char_idx + 2 < arg.len() {
                            target_dir = String::from(&arg[char_idx + 2..]);
                        } else if i + 1 < args_vec.len() {
                            target_dir = args_vec[i + 1].clone();
                            i += 1;
                        } else {
                            eprintln("tar: option requires an argument -- C");
                            exit(1);
                        }
                        stop_bundle = true;
                    }
                    _ => {
                        eprintln(&format!("tar: invalid option -- '{c}'"));
                        exit(1);
                    }
                }
            }
        } else if archive_file.is_none() {
            archive_file = Some(arg.clone());
        } else {
            eprintln(&format!("tar: extra operand '{arg}'"));
            exit(1);
        }
        i += 1;
    }

    if !extract {
        eprintln("tar: only extraction (-x) is supported for now.");
        exit(1);
    }

    let Some(archive_path) = archive_file else {
        eprintln("tar: archive file not specified.");
        exit(1);
    };

    match extract_file(&archive_path, &target_dir, &opts) {
        Ok(_) => exit(0),
        Err(e) => {
            eprintln(&format!("tar: error: {}", e.describe()));
            exit(1);
        }
    }
}
