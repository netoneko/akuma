#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod api;
pub mod constrained;
pub mod engine;
pub mod safetensors;
pub mod tokenizer;

#[cfg(test)]
mod tests {
    use std::path::Path;
    use super::engine::NeedleEngine;

    const WEIGHTS: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../bootstrap/models/needle.safetensors"
    );
    const VOCAB: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../bootstrap/models/vocab.txt"
    );

    fn load_engine() -> Option<NeedleEngine> {
        if !Path::new(WEIGHTS).exists() || !Path::new(VOCAB).exists() {
            return None;
        }
        let weights = std::fs::read(WEIGHTS).ok()?;
        let vocab = std::fs::read_to_string(VOCAB).ok()?;
        NeedleEngine::from_bytes(weights, &vocab).ok()
    }

    #[test]
    fn health_response() {
        let mut buf = alloc::vec::Vec::new();
        super::api::write_health_response(&mut buf, true);
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.contains("\"loaded\":true"));
        assert!(s.contains("\"status\":\"ok\""));
    }

    #[test]
    fn parse_completions_request() {
        let body = r#"{"model":"x","messages":[{"role":"user","content":"hello"}],"tools":[]}"#;
        let req = super::api::parse_completions_request(body).expect("parse failed");
        assert_eq!(req.query, "hello");
    }

    #[test]
    fn weather_route() {
        let engine = match load_engine() {
            Some(e) => e,
            None => {
                eprintln!("skipping weather_route: model weights not found at {WEIGHTS}");
                return;
            }
        };

        let tools = r#"[{"name":"get_weather","description":"Get current weather for a location","parameters":{"type":"object","properties":{"location":{"type":"string"}},"required":["location"]}}]"#;
        let result = engine.run("What's the weather in Paris?", tools);
        eprintln!("weather_route result: {}", result.text);
        assert!(!result.text.is_empty(), "expected non-empty inference output");
    }

    #[test]
    fn no_tools_route() {
        let engine = match load_engine() {
            Some(e) => e,
            None => {
                eprintln!("skipping no_tools_route: model weights not found");
                return;
            }
        };

        let result = engine.run("hello", "[]");
        eprintln!("no_tools_route result: {:?}", result.text);
    }

    #[test]
    fn git_clone_route() {
        let engine = match load_engine() {
            Some(e) => e,
            None => {
                eprintln!("skipping git_clone_route: model weights not found");
                return;
            }
        };

        let tools = r#"[
          {
            "name": "git_clone",
            "description": "Clone a git repository to a local directory",
            "parameters": {
              "type": "object",
              "properties": {
                "url":  {"type": "string", "description": "Repository URL to clone"},
                "path": {"type": "string", "description": "Local destination directory"}
              },
              "required": ["url"]
            }
          }
        ]"#;

        let query = "need to clone git repo https://github.com/netoneko/akuma-playground.git";
        let result = engine.run(query, tools);
        eprintln!("git_clone_route result: {}", result.text);

        assert!(
            result.text.contains("git_clone"),
            "expected tool call to git_clone, got: {}",
            result.text
        );
        assert!(
            result.text.contains("akuma-playground"),
            "expected URL with akuma-playground in arguments, got: {}",
            result.text
        );
    }

    #[test]
    fn completions_response_shape() {
        let mut buf = alloc::vec::Vec::new();
        super::api::write_completions_response(
            &mut buf,
            r#"{"name":"get_weather","arguments":{"location":"Paris"}}"#,
        );
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.contains("\"finish_reason\":\"tool_calls\""));
        assert!(s.contains("get_weather"));
    }
}
