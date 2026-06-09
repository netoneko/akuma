// SPDX-License-Identifier: MIT
// Ported from needle-rs (crates/needle-infer), MIT-licensed.
// Copyright (c) 2026 Abdalrahman Ibrahim — https://github.com/geekgineer/needle-rs
// See LICENSE in this directory for the full notice.

//! Grammar-constrained decoding — ported from needle-infer to no_std.
//!
//! Changes: std::collections::BTreeMap → hashbrown::BTreeMap,
//! std::cmp::Reverse → core::cmp::Reverse, tests removed.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;

use crate::tokenizer::to_snake_case;

#[derive(Default)]
struct TrieNode {
    children: BTreeMap<u8, usize>,
    is_terminal: bool,
}

pub struct Trie {
    nodes: Vec<TrieNode>,
}

impl Default for Trie {
    fn default() -> Self {
        Self::new()
    }
}

impl Trie {
    pub fn new() -> Self {
        Self { nodes: vec![TrieNode::default()] }
    }

    pub fn insert(&mut self, s: &[u8]) {
        let mut cur = 0;
        for &b in s {
            let next = self.nodes[cur].children.get(&b).copied();
            cur = match next {
                Some(n) => n,
                None => {
                    let n = self.nodes.len();
                    self.nodes.push(TrieNode::default());
                    self.nodes[cur].children.insert(b, n);
                    n
                }
            };
        }
        self.nodes[cur].is_terminal = true;
    }

    pub fn advance(&self, node: usize, bytes: &[u8]) -> Option<usize> {
        let mut cur = node;
        for &b in bytes {
            cur = *self.nodes[cur].children.get(&b)?;
        }
        Some(cur)
    }

    fn is_terminal(&self, node: usize) -> bool {
        self.nodes.get(node).is_some_and(|n| n.is_terminal)
    }

    fn child(&self, node: usize, b: u8) -> Option<usize> {
        self.nodes.get(node)?.children.get(&b).copied()
    }
}

fn check_token_valid(bytes: &[u8], trie: &Trie, start_node: usize) -> bool {
    let mut cur = start_node;
    for &b in bytes {
        if b == b'"' {
            return trie.is_terminal(cur);
        }
        match trie.child(cur, b) {
            Some(next) => cur = next,
            None => return false,
        }
    }
    true
}

fn build_mask_from_trie(
    trie: &Trie,
    node: usize,
    token_texts: &[Vec<u8>],
    vocab_size: usize,
) -> Vec<f32> {
    let mut mask = vec![-1e9f32; vocab_size];
    let mut any_allowed = false;
    for (id, text) in token_texts.iter().enumerate() {
        if id >= vocab_size { break; }
        if check_token_valid(text, trie, node) {
            mask[id] = 0.0;
            any_allowed = true;
        }
    }
    if !any_allowed {
        mask.fill(0.0);
    }
    mask
}

#[derive(Debug, Clone, PartialEq)]
pub enum JsonState {
    Free,
    InName,
    InArgKey,
}

const NAME_TRIGGER: &[u8] = b"\"name\":\"";
const ARGS_TRIGGER: &[u8] = b"\"arguments\":{";
const TAIL_LEN: usize = 13;

pub struct JsonStateMachine {
    pub state: JsonState,
    pub current_function: String,
    pub constrained_buf: Vec<u8>,
    in_arguments: bool,
    arguments_depth: usize,
    nesting_depth: usize,
    in_string: bool,
    prev_char_escape: bool,
    tail: Vec<u8>,
}

impl Default for JsonStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl JsonStateMachine {
    pub fn new() -> Self {
        Self {
            state: JsonState::Free,
            current_function: String::new(),
            constrained_buf: Vec::new(),
            in_arguments: false,
            arguments_depth: 0,
            nesting_depth: 0,
            in_string: false,
            prev_char_escape: false,
            tail: Vec::with_capacity(TAIL_LEN + 1),
        }
    }

    pub fn feed_byte(&mut self, b: u8) {
        match self.state {
            JsonState::InName => {
                if b == b'"' {
                    self.current_function =
                        String::from_utf8_lossy(&self.constrained_buf).into_owned();
                    self.constrained_buf.clear();
                    self.state = JsonState::Free;
                } else {
                    self.constrained_buf.push(b);
                }
                return;
            }
            JsonState::InArgKey => {
                if b == b'"' {
                    self.constrained_buf.clear();
                    self.state = JsonState::Free;
                } else {
                    self.constrained_buf.push(b);
                }
                return;
            }
            JsonState::Free => {}
        }

        self.push_tail(b);

        if self.in_string {
            if self.prev_char_escape {
                self.prev_char_escape = false;
                return;
            }
            if b == b'\\' {
                self.prev_char_escape = true;
                return;
            }
            if b == b'"' {
                self.in_string = false;
            }
            return;
        }

        if b == b'{' || b == b'[' {
            self.nesting_depth += 1;
        } else if b == b'}' || b == b']' {
            self.nesting_depth = self.nesting_depth.saturating_sub(1);
            if b == b'}' && self.in_arguments && self.nesting_depth < self.arguments_depth {
                self.in_arguments = false;
            }
            return;
        }

        if !self.in_arguments && self.tail.ends_with(NAME_TRIGGER) {
            self.state = JsonState::InName;
            self.constrained_buf.clear();
            return;
        }

        if self.tail.ends_with(ARGS_TRIGGER) {
            self.in_arguments = true;
            self.arguments_depth = self.nesting_depth;
            return;
        }

        if self.in_arguments
            && self.nesting_depth == self.arguments_depth
            && self.at_arg_key_start()
        {
            self.state = JsonState::InArgKey;
            self.constrained_buf.clear();
            return;
        }

        if b == b'"' && self.is_value_quote() {
            self.in_string = true;
        }
    }

    pub fn feed(&mut self, text: &[u8]) {
        for &b in text {
            self.feed_byte(b);
        }
    }

    fn push_tail(&mut self, b: u8) {
        self.tail.push(b);
        if self.tail.len() > TAIL_LEN {
            self.tail.drain(0..self.tail.len() - TAIL_LEN);
        }
    }

    fn at_arg_key_start(&self) -> bool {
        let n = self.tail.len();
        if n < 2 { return false; }
        (self.tail[n - 2] == b'{' || self.tail[n - 2] == b',') && self.tail[n - 1] == b'"'
    }

    fn is_value_quote(&self) -> bool {
        let n = self.tail.len();
        n >= 2 && self.tail[n - 2] == b':'
    }
}

pub struct ToolDef {
    pub name: String,
    pub snake_name: String,
    pub param_keys: Vec<String>,
}

impl ToolDef {
    pub fn from_json(json: &str) -> Vec<Self> {
        parse_tools_json(json)
    }
}

pub struct ConstrainedDecoder {
    name_trie: Trie,
    param_tries: BTreeMap<String, Trie>,
    sm: JsonStateMachine,
    token_texts: Vec<Vec<u8>>,
}

impl ConstrainedDecoder {
    pub fn new(tool_defs: &[ToolDef], token_bytes: Vec<(u32, Vec<u8>)>) -> Self {
        let mut name_trie = Trie::new();
        let mut param_tries = BTreeMap::new();

        for tool in tool_defs {
            name_trie.insert(tool.snake_name.as_bytes());
            let mut key_trie = Trie::new();
            for key in &tool.param_keys {
                key_trie.insert(key.as_bytes());
            }
            param_tries.insert(tool.snake_name.clone(), key_trie);
        }

        let max_id = token_bytes.iter().map(|(id, _)| *id as usize).max().unwrap_or(0);
        let mut token_texts = vec![Vec::new(); max_id + 1];
        for (id, bytes) in token_bytes {
            if (id as usize) <= max_id {
                token_texts[id as usize] = bytes;
            }
        }

        Self { name_trie, param_tries, sm: JsonStateMachine::new(), token_texts }
    }

    pub fn update(&mut self, token_id: u32) {
        if let Some(text) = self.token_texts.get(token_id as usize) {
            let text = text.clone();
            self.sm.feed(&text);
        }
    }

    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        self.sm.feed(bytes);
    }

    pub fn logit_mask(&self, vocab_size: usize) -> Vec<f32> {
        let texts = &self.token_texts;
        match &self.sm.state {
            JsonState::Free => vec![0.0f32; vocab_size],
            JsonState::InName => match self.name_trie.advance(0, &self.sm.constrained_buf) {
                Some(node) => build_mask_from_trie(&self.name_trie, node, texts, vocab_size),
                None => vec![0.0f32; vocab_size],
            },
            JsonState::InArgKey => match self.param_tries.get(&self.sm.current_function) {
                Some(trie) => match trie.advance(0, &self.sm.constrained_buf) {
                    Some(node) => build_mask_from_trie(trie, node, texts, vocab_size),
                    None => vec![0.0f32; vocab_size],
                },
                None => vec![0.0f32; vocab_size],
            },
        }
    }
}

fn parse_tools_json(json: &str) -> Vec<ToolDef> {
    let bytes = json.as_bytes();
    let mut tools = Vec::new();
    let mut i = 0;

    while i < bytes.len() && bytes[i] != b'[' && bytes[i] != b'{' { i += 1; }
    if i >= bytes.len() { return tools; }
    if bytes[i] == b'[' { i += 1; }

    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) { i += 1; }
        if i >= bytes.len() || bytes[i] == b']' { break; }
        if bytes[i] != b'{' { i += 1; continue; }

        let obj_end = json_find_matching(bytes, i, b'{', b'}');
        let obj_str = &json[i..obj_end + 1];
        if let Some(tool) = parse_single_tool(obj_str) { tools.push(tool); }
        i = obj_end + 1;
    }

    tools
}

fn parse_single_tool(obj: &str) -> Option<ToolDef> {
    let name = json_extract_string(obj, "name")?;
    let snake_name = to_snake_case(&name);
    let param_keys = extract_param_keys(obj);
    Some(ToolDef { name, snake_name, param_keys })
}

fn extract_param_keys(tool_obj: &str) -> Vec<String> {
    let params = match json_extract_object(tool_obj, "parameters") {
        Some(p) => p,
        None => return Vec::new(),
    };
    if let Some(props) = json_extract_object(&params, "properties") {
        return json_top_level_keys(&props);
    }
    json_keys_with_object_values(&params)
}

fn json_find_matching(bytes: &[u8], start: usize, open: u8, close: u8) -> usize {
    let mut depth = 0usize;
    let mut i = start;
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
            if depth == 0 { return i; }
        }
        i += 1;
    }
    bytes.len().saturating_sub(1)
}

fn json_extract_string(obj: &str, field: &str) -> Option<String> {
    use alloc::format;
    let needle = format!("\"{}\":\"", field);
    let pos = obj.find(&needle)?;
    let after = &obj[pos + needle.len()..];
    let bytes = after.as_bytes();
    let mut i = 0;
    let mut escape = false;
    while i < bytes.len() {
        if escape { escape = false; }
        else if bytes[i] == b'\\' { escape = true; }
        else if bytes[i] == b'"' { return Some(after[..i].to_string()); }
        i += 1;
    }
    None
}

fn json_extract_object(obj: &str, field: &str) -> Option<String> {
    use alloc::format;
    let needle = format!("\"{}\":", field);
    let pos = obj.find(&needle)?;
    let after = obj[pos + needle.len()..].trim_start();
    let bytes = after.as_bytes();
    if bytes.is_empty() || bytes[0] != b'{' { return None; }
    let end = json_find_matching(bytes, 0, b'{', b'}');
    Some(after[..end + 1].to_string())
}

fn json_top_level_keys(obj: &str) -> Vec<String> {
    let bytes = obj.as_bytes();
    let mut keys = Vec::new();
    let mut i = 0;
    if i < bytes.len() && bytes[i] == b'{' { i += 1; }
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) { i += 1; }
        if i >= bytes.len() || bytes[i] == b'}' { break; }
        if bytes[i] != b'"' { i += 1; continue; }
        i += 1;
        let key_start = i;
        let mut escape = false;
        while i < bytes.len() {
            if escape { escape = false; }
            else if bytes[i] == b'\\' { escape = true; }
            else if bytes[i] == b'"' { break; }
            i += 1;
        }
        let key = obj[key_start..i].to_string();
        i += 1;
        while i < bytes.len() && bytes[i] != b':' { i += 1; }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; }
        if !key.is_empty() { keys.push(key); }
        i = json_skip_value(bytes, i);
    }
    keys
}

fn json_keys_with_object_values(obj: &str) -> Vec<String> {
    let bytes = obj.as_bytes();
    let mut keys = Vec::new();
    let mut i = 0;
    if i < bytes.len() && bytes[i] == b'{' { i += 1; }
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) { i += 1; }
        if i >= bytes.len() || bytes[i] == b'}' { break; }
        if bytes[i] != b'"' { i += 1; continue; }
        i += 1;
        let key_start = i;
        let mut escape = false;
        while i < bytes.len() {
            if escape { escape = false; }
            else if bytes[i] == b'\\' { escape = true; }
            else if bytes[i] == b'"' { break; }
            i += 1;
        }
        let key = obj[key_start..i].to_string();
        i += 1;
        while i < bytes.len() && bytes[i] != b':' { i += 1; }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; }
        if i < bytes.len() && bytes[i] == b'{' {
            if !key.is_empty() { keys.push(key); }
            let end = json_find_matching(bytes, i, b'{', b'}');
            i = end + 1;
        } else {
            i = json_skip_value(bytes, i);
        }
    }
    keys
}

fn json_skip_value(bytes: &[u8], i: usize) -> usize {
    let mut i = i;
    if i >= bytes.len() { return i; }
    match bytes[i] {
        b'{' => json_find_matching(bytes, i, b'{', b'}') + 1,
        b'[' => json_find_matching(bytes, i, b'[', b']') + 1,
        b'"' => {
            i += 1;
            let mut escape = false;
            while i < bytes.len() {
                if escape { escape = false; }
                else if bytes[i] == b'\\' { escape = true; }
                else if bytes[i] == b'"' { i += 1; return i; }
                i += 1;
            }
            i
        }
        _ => {
            while i < bytes.len() && bytes[i] != b',' && bytes[i] != b'}' && bytes[i] != b']' {
                i += 1;
            }
            i
        }
    }
}
