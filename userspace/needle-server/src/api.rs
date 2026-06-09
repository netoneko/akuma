//! Manual JSON parsing for /v1/route and /v1/retrieve requests/responses.
//! No serde — hand-rolled for size.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

// ── Request types ─────────────────────────────────────────────────────────────

pub struct RouteRequest {
    pub query: String,
    pub tools_json: String,
    pub stream: bool,
}

pub struct CompletionsRequest {
    pub query: String,
    pub tools_json: String,
}

pub struct RetrieveRequest {
    pub query: String,
    pub tools: Vec<String>,
    pub top_k: usize,
}

// ── Parsers ───────────────────────────────────────────────────────────────────

pub fn parse_route_request(body: &str) -> Option<RouteRequest> {
    let query = json_extract_string(body, "query")?;
    let tools_json = json_extract_raw(body, "tools")?;
    let stream = body.contains("\"stream\":true");
    Some(RouteRequest { query, tools_json, stream })
}

/// Parse an OpenAI-style chat completions request.
/// Extracts the last user message as the query, and the tools array as raw JSON.
pub fn parse_completions_request(body: &str) -> Option<CompletionsRequest> {
    let query = extract_last_user_message(body)?;
    let tools_json = json_extract_raw(body, "tools").unwrap_or_else(|| String::from("[]"));
    Some(CompletionsRequest { query, tools_json })
}

pub fn parse_retrieve_request(body: &str) -> Option<RetrieveRequest> {
    let query = json_extract_string(body, "query")?;
    let tools_raw = json_extract_raw(body, "tools")?;
    let tools = parse_string_array(&tools_raw);
    let top_k = json_extract_usize(body, "top_k").unwrap_or(5);
    Some(RetrieveRequest { query, tools, top_k })
}

// ── Response builders ─────────────────────────────────────────────────────────

pub fn write_route_response(buf: &mut Vec<u8>, tool_call_json: &str, latency_ms: u64) {
    let s = alloc::format!(
        "{{\"tool_call\":{},\"latency_ms\":{}}}",
        tool_call_json,
        latency_ms
    );
    buf.extend_from_slice(s.as_bytes());
}

pub fn write_stream_token(buf: &mut Vec<u8>, token: &str) {
    buf.extend_from_slice(b"{\"token\":\"");
    write_json_str(buf, token);
    buf.extend_from_slice(b"\"}\n");
}

pub fn write_stream_done(buf: &mut Vec<u8>, tool_call_json: &str) {
    let s = alloc::format!("{{\"done\":true,\"tool_call\":{}}}\n", tool_call_json);
    buf.extend_from_slice(s.as_bytes());
}

pub fn write_retrieve_response(buf: &mut Vec<u8>, results: &[(&str, f32)]) {
    buf.extend_from_slice(b"{\"results\":[");
    for (i, (name, score)) in results.iter().enumerate() {
        if i > 0 { buf.push(b','); }
        buf.extend_from_slice(b"{\"name\":\"");
        write_json_str(buf, name);
        let score_s = alloc::format!("\",\"score\":{:.4}}}", score);
        buf.extend_from_slice(score_s.as_bytes());
    }
    buf.extend_from_slice(b"]}");
}

pub fn write_health_response(buf: &mut Vec<u8>, loaded: bool) {
    let s = if loaded {
        "{\"status\":\"ok\",\"model\":\"needle\",\"loaded\":true}"
    } else {
        "{\"status\":\"loading\",\"model\":\"needle\",\"loaded\":false}"
    };
    buf.extend_from_slice(s.as_bytes());
}

/// Write an OpenAI-compatible chat completion response.
/// `tool_call_json` is what the engine produced: `{"name":"...","arguments":{...}}`.
/// If it looks like a valid tool call, emit finish_reason=tool_calls; otherwise
/// emit the text as content with finish_reason=stop.
pub fn write_completions_response(buf: &mut Vec<u8>, tool_call_json: &str) {
    let name = json_extract_string(tool_call_json, "name");
    let args_raw = json_extract_raw(tool_call_json, "arguments");

    if let (Some(name), Some(args_raw)) = (name, args_raw) {
        // Tool call response
        let args_escaped = json_stringify(&args_raw);
        buf.extend_from_slice(b"{\"id\":\"chatcmpl-needle\",\"object\":\"chat.completion\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":null,\"tool_calls\":[{\"id\":\"call_0\",\"type\":\"function\",\"function\":{\"name\":\"");
        write_json_str(buf, &name);
        buf.extend_from_slice(b"\",\"arguments\":\"");
        write_json_str(buf, &args_escaped);
        buf.extend_from_slice(b"\"}}]},\"finish_reason\":\"tool_calls\"}]}");
    } else {
        // Plain text response
        buf.extend_from_slice(b"{\"id\":\"chatcmpl-needle\",\"object\":\"chat.completion\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"");
        write_json_str(buf, tool_call_json);
        buf.extend_from_slice(b"\"},\"finish_reason\":\"stop\"}]}");
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn write_json_str(buf: &mut Vec<u8>, s: &str) {
    for b in s.bytes() {
        match b {
            b'"' => { buf.push(b'\\'); buf.push(b'"'); }
            b'\\' => { buf.push(b'\\'); buf.push(b'\\'); }
            b'\n' => { buf.push(b'\\'); buf.push(b'n'); }
            b'\r' => { buf.push(b'\\'); buf.push(b'r'); }
            _ => buf.push(b),
        }
    }
}

fn json_extract_string(obj: &str, field: &str) -> Option<String> {
    let needle = alloc::format!("\"{}\":", field);
    let pos = obj.find(&needle)?;
    let after = obj[pos + needle.len()..].trim_start();
    if !after.starts_with('"') { return None; }
    let after = &after[1..];
    let bytes = after.as_bytes();
    let mut i = 0;
    let mut escape = false;
    let mut out = String::new();
    while i < bytes.len() {
        if escape {
            match bytes[i] {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                c => { out.push('\\'); out.push(c as char); }
            }
            escape = false;
        } else if bytes[i] == b'\\' {
            escape = true;
        } else if bytes[i] == b'"' {
            return Some(out);
        } else {
            out.push(bytes[i] as char);
        }
        i += 1;
    }
    None
}

fn json_extract_raw(obj: &str, field: &str) -> Option<String> {
    let needle = alloc::format!("\"{}\":", field);
    let pos = obj.find(&needle)?;
    let after = obj[pos + needle.len()..].trim_start();
    let bytes = after.as_bytes();
    if bytes.is_empty() { return None; }
    let (open, close) = match bytes[0] {
        b'[' => (b'[', b']'),
        b'{' => (b'{', b'}'),
        _ => return None,
    };
    let mut depth = 0usize;
    let mut i = 0;
    let mut in_str = false;
    let mut escape = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if escape { escape = false; }
            else if b == b'\\' { escape = true; }
            else if b == b'"' { in_str = false; }
        } else if b == b'"' { in_str = true; }
        else if b == open { depth += 1; }
        else if b == close {
            depth = depth.saturating_sub(1);
            if depth == 0 { return Some(after[..i + 1].to_string()); }
        }
        i += 1;
    }
    None
}

fn json_extract_usize(obj: &str, field: &str) -> Option<usize> {
    let needle = alloc::format!("\"{}\":", field);
    let pos = obj.find(&needle)?;
    let after = obj[pos + needle.len()..].trim_start();
    let end = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
    after[..end].parse().ok()
}

/// Extract the content of the last message with role "user" from a messages array.
fn extract_last_user_message(body: &str) -> Option<String> {
    let messages_raw = json_extract_raw(body, "messages")?;
    let bytes = messages_raw.as_bytes();
    let mut last_user_content: Option<String> = None;
    let mut i = 0;
    // Walk through each {...} object in the array
    while i < bytes.len() {
        if bytes[i] != b'{' { i += 1; continue; }
        // Find matching closing brace
        let mut depth = 0usize;
        let mut in_str = false;
        let mut escape = false;
        let start = i;
        while i < bytes.len() {
            let b = bytes[i];
            if in_str {
                if escape { escape = false; }
                else if b == b'\\' { escape = true; }
                else if b == b'"' { in_str = false; }
            } else if b == b'"' { in_str = true; }
            else if b == b'{' { depth += 1; }
            else if b == b'}' {
                depth = depth.saturating_sub(1);
                if depth == 0 { i += 1; break; }
            }
            i += 1;
        }
        let obj = core::str::from_utf8(&bytes[start..i]).unwrap_or("");
        if let Some(role) = json_extract_string(obj, "role") {
            if role == "user" {
                last_user_content = json_extract_string(obj, "content");
            }
        }
    }
    last_user_content
}

/// Escape a raw JSON value as a JSON string (for embedding in "arguments" field).
fn json_stringify(raw: &str) -> String {
    let mut out = String::new();
    for b in raw.bytes() {
        match b {
            b'"' => { out.push('\\'); out.push('"'); }
            b'\\' => { out.push('\\'); out.push('\\'); }
            b'\n' => { out.push('\\'); out.push('n'); }
            b'\r' => { out.push('\\'); out.push('r'); }
            _ => out.push(b as char),
        }
    }
    out
}

fn parse_string_array(arr: &str) -> Vec<String> {
    let mut result = Vec::new();
    let arr = arr.trim();
    if !arr.starts_with('[') { return result; }
    let inner = &arr[1..];
    let bytes = inner.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) { i += 1; }
        if i >= bytes.len() || bytes[i] == b']' { break; }
        if bytes[i] == b'"' {
            i += 1;
            let start = i;
            let mut escape = false;
            while i < bytes.len() {
                if escape { escape = false; }
                else if bytes[i] == b'\\' { escape = true; }
                else if bytes[i] == b'"' { break; }
                i += 1;
            }
            result.push(inner[start..i].to_string());
            i += 1;
        } else {
            i += 1;
        }
    }
    result
}
