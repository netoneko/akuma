//! needle-server — Needle function-call inference HTTP server for Akuma OS.
//!
//! USAGE:
//!   needle-server [OPTIONS]
//!
//! OPTIONS:
//!   --port <PORT>        HTTP port [default: 8080]
//!   --weights <PATH>     Path to .safetensors weights (skips download)
//!   --vocab <PATH>       Path to vocabulary file (skips download)
//!   --weights-dir <DIR>  Download/cache dir [default: /data/needle]
//!   --download           Download missing weights from Cactus API
//!   --model <ID>         HuggingFace model ID [default: Abdalrahman/needle-rs-safetensors]
//!   --hf-token <TOKEN>   HuggingFace token for gated models
//!
//! EXAMPLES:
//!   needle-server --weights /data/needle.safetensors --vocab /data/vocab.txt
//!   needle-server --download --weights-dir /data/needle
//!   needle-server --download --port 9090

#![no_std]
#![no_main]

extern crate alloc;

mod api;
mod cactus;
mod constrained;
mod engine;
mod safetensors;
mod server;
mod tokenizer;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use engine::NeedleEngine;
use libakuma::net::{TcpListener};

struct Config {
    port: u16,
    weights: Option<String>,
    vocab: Option<String>,
    weights_dir: String,
    download: bool,
    model: String,
    hf_token: Option<String>,
}

impl Config {
    fn from_args() -> Self {
        let mut cfg = Config {
            port: 8080,
            weights: None,
            vocab: None,
            weights_dir: cactus::DEFAULT_WEIGHTS_DIR.into(),
            download: false,
            model: cactus::DEFAULT_MODEL.into(),
            hf_token: None,
        };

        let argc = libakuma::argc();
        let mut i = 1u32;
        while i < argc {
            let arg = libakuma::arg(i).unwrap_or("");
            match arg {
                "--port" => {
                    if let Some(v) = libakuma::arg(i + 1) {
                        cfg.port = v.parse().unwrap_or(8080);
                        i += 1;
                    }
                }
                "--weights" => {
                    if let Some(v) = libakuma::arg(i + 1) {
                        cfg.weights = Some(v.into());
                        i += 1;
                    }
                }
                "--vocab" => {
                    if let Some(v) = libakuma::arg(i + 1) {
                        cfg.vocab = Some(v.into());
                        i += 1;
                    }
                }
                "--weights-dir" => {
                    if let Some(v) = libakuma::arg(i + 1) {
                        cfg.weights_dir = v.into();
                        i += 1;
                    }
                }
                "--download" => cfg.download = true,
                "--model" => {
                    if let Some(v) = libakuma::arg(i + 1) {
                        cfg.model = v.into();
                        i += 1;
                    }
                }
                "--hf-token" => {
                    if let Some(v) = libakuma::arg(i + 1) {
                        cfg.hf_token = Some(v.into());
                        i += 1;
                    }
                }
                "--help" | "-h" => {
                    print_usage();
                    libakuma::exit(0);
                }
                _ => {}
            }
            i += 1;
        }
        cfg
    }
}

fn print_usage() {
    libakuma::println("needle-server [OPTIONS]");
    libakuma::println("");
    libakuma::println("OPTIONS:");
    libakuma::println("  --port <PORT>        HTTP port [default: 8080]");
    libakuma::println("  --weights <PATH>     Path to .safetensors weights (skips download)");
    libakuma::println("  --vocab <PATH>       Path to vocabulary file (skips download)");
    libakuma::println("  --weights-dir <DIR>  Download/cache dir [default: /data/needle]");
    libakuma::println("  --download           Download missing weights from Cactus API");
    libakuma::println("  --model <ID>         Model ID [default: Abdalrahman/needle-rs-safetensors]");
    libakuma::println("  --hf-token <TOKEN>   HuggingFace token for gated models");
    libakuma::println("");
    libakuma::println("EXAMPLES:");
    libakuma::println("  needle-server --weights /data/needle.safetensors --vocab /data/vocab.txt");
    libakuma::println("  needle-server --download --weights-dir /data/needle");
    libakuma::println("  needle-server --download --port 9090");
}

#[no_mangle]
pub extern "C" fn main() {
    let cfg = Config::from_args();

    // Resolve weight paths
    let (weights_path, vocab_path) = if let (Some(w), Some(v)) = (&cfg.weights, &cfg.vocab) {
        (w.clone(), v.clone())
    } else if cfg.download {
        let dl_cfg = cactus::DownloadConfig {
            model_id: &cfg.model,
            weights_dir: &cfg.weights_dir,
            hf_token: cfg.hf_token.as_deref(),
        };
        match cactus::ensure_weights(&dl_cfg) {
            Ok(paths) => paths,
            Err(e) => {
                libakuma::eprintln(&format!("download failed: {e}"));
                libakuma::exit(1);
            }
        }
    } else {
        libakuma::eprintln("no weights specified — use --weights/--vocab or --download");
        print_usage();
        libakuma::exit(1);
    };

    libakuma::println(&format!("loading model from {weights_path}"));
    let engine = match NeedleEngine::load(&weights_path, &vocab_path) {
        Ok(e) => e,
        Err(err) => {
            libakuma::eprintln(&format!("model load failed: {err:?}"));
            libakuma::exit(1);
        }
    };
    libakuma::println("model loaded");

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            libakuma::eprintln(&format!("bind failed: {e:?}"));
            libakuma::exit(1);
        }
    };
    libakuma::println(&format!("needle-server listening on {}", cfg.port));

    loop {
        match listener.accept() {
            Ok((stream, _addr)) => handle_connection(&engine, stream),
            Err(_) => libakuma::sleep_ms(1),
        }
    }
}

fn handle_connection(engine: &NeedleEngine, stream: libakuma::net::TcpStream) {
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    if !server::read_request(&stream, &mut buf) {
        return;
    }
    let text = match core::str::from_utf8(&buf) {
        Ok(s) => s,
        Err(_) => {
            server::send_bad_request(&stream, "invalid UTF-8");
            return;
        }
    };
    let req = match server::parse_request(text) {
        Some(r) => r,
        None => {
            server::send_bad_request(&stream, "malformed request");
            return;
        }
    };

    match (req.method, req.path) {
        ("GET", "/health") | ("GET", "/health/") => {
            let mut body = Vec::new();
            api::write_health_response(&mut body, true);
            server::send_json(&stream, 200, "OK", &body);
        }

        ("GET", "/openapi.json") => {
            server::send_not_implemented(&stream);
        }

        ("POST", "/v1/route") | ("POST", "/v1/route/") => {
            handle_route(engine, &stream, req.body);
        }

        ("POST", "/v1/retrieve") | ("POST", "/v1/retrieve/") => {
            handle_retrieve(engine, &stream, req.body);
        }

        _ => {
            server::send_not_found(&stream);
        }
    }
}

fn handle_route(engine: &NeedleEngine, stream: &libakuma::net::TcpStream, body: &str) {
    let req = match api::parse_route_request(body) {
        Some(r) => r,
        None => {
            server::send_bad_request(stream, "missing query or tools");
            return;
        }
    };

    let start_ms = libakuma::time();

    if req.stream {
        server::send_chunked_start(stream);
        let mut chunks: Vec<u8> = Vec::new();
        let result = engine.run_stream(&req.query, &req.tools_json, |_, piece| {
            chunks.clear();
            api::write_stream_token(&mut chunks, piece);
            server::send_chunk(stream, &chunks);
        });
        let latency_ms = libakuma::time().saturating_sub(start_ms);
        let _ = latency_ms; // streaming response doesn't include latency
        let mut done_buf: Vec<u8> = Vec::new();
        api::write_stream_done(&mut done_buf, &result.text);
        server::send_chunk(stream, &done_buf);
    } else {
        let result = engine.run(&req.query, &req.tools_json);
        let latency_ms = libakuma::time().saturating_sub(start_ms);
        let mut body = Vec::new();
        api::write_route_response(&mut body, &result.text, latency_ms);
        server::send_json(stream, 200, "OK", &body);
    }
}

fn handle_retrieve(engine: &NeedleEngine, stream: &libakuma::net::TcpStream, body: &str) {
    let req = match api::parse_retrieve_request(body) {
        Some(r) => r,
        None => {
            server::send_bad_request(stream, "missing query or tools");
            return;
        }
    };

    if engine.contrastive_dim() == 0 {
        server::send_bad_request(stream, "model has no contrastive head");
        return;
    }

    let tool_refs: Vec<&str> = req.tools.iter().map(|s| s.as_str()).collect();
    let scores = engine.retrieve_tools(&req.query, &tool_refs, req.top_k);

    let named: Vec<(&str, f32)> = scores
        .iter()
        .map(|(idx, score)| (tool_refs[*idx], *score))
        .collect();

    let mut resp_body = Vec::new();
    api::write_retrieve_response(&mut resp_body, &named);
    server::send_json(stream, 200, "OK", &resp_body);
}
