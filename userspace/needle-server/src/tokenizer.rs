//! SentencePiece BPE tokenizer — ported from needle-infer to no_std.
//!
//! Changes from upstream: std::collections::BTreeMap → hashbrown::BTreeMap,
//! std::fs → libakuma::fs, tests removed.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::collections::BTreeMap;

pub const PAD_ID: u32 = 0;
pub const EOS_ID: u32 = 1;
pub const BOS_ID: u32 = 2;
pub const UNK_ID: u32 = 3;
pub const TOOL_CALL_ID: u32 = 4;
pub const TOOLS_ID: u32 = 5;

const SP_SPACE: char = '\u{2581}';

pub struct Vocabulary {
    pub id_to_piece: Vec<String>,
    pub piece_to_id: BTreeMap<String, u32>,
    scores: Vec<f32>,
    max_piece_bytes: usize,
}

impl Vocabulary {
    pub fn load_text(path: &str) -> Result<Self, i32> {
        let content = libakuma::fs::read_to_string(path)?;
        Ok(Self::parse(&content))
    }

    pub fn parse(content: &str) -> Self {
        let mut id_to_piece = Vec::new();
        let mut piece_to_id = BTreeMap::new();
        let mut scores: Vec<f32> = Vec::new();
        let mut max_piece_bytes = 0usize;
        let mut has_scores = false;

        for line in content.lines() {
            let id = id_to_piece.len() as u32;
            let (piece, score) = if let Some(tab) = line.find('\t') {
                has_scores = true;
                let p = &line[..tab];
                let s: f32 = line[tab + 1..].trim().parse().unwrap_or(0.0);
                (p.to_string(), s)
            } else {
                (line.to_string(), 0.0f32)
            };
            max_piece_bytes = max_piece_bytes.max(piece.len());
            piece_to_id.insert(piece.clone(), id);
            id_to_piece.push(piece);
            scores.push(score);
        }

        if !has_scores {
            scores.clear();
        }

        Self {
            id_to_piece,
            piece_to_id,
            scores,
            max_piece_bytes,
        }
    }

    pub fn encode(&self, text: &str) -> Vec<u32> {
        let text = text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        let normalized = normalize_sp(text);
        if self.scores.is_empty() {
            self.encode_greedy(normalized.as_bytes())
        } else {
            self.encode_bpe(&normalized)
        }
    }

    fn encode_bpe(&self, normalized: &str) -> Vec<u32> {
        let mut pieces: Vec<u32> = Vec::with_capacity(normalized.len());
        let mut piece_strings: Vec<String> = Vec::with_capacity(normalized.len());

        for c in normalized.chars() {
            let s = c.to_string();
            if let Some(&id) = self.piece_to_id.get(&s) {
                piece_strings.push(s);
                pieces.push(id);
            } else {
                for &b in s.as_bytes() {
                    let fb = byte_fallback_piece(b);
                    let id = self.piece_to_id.get(&fb).copied().unwrap_or(UNK_ID);
                    piece_strings.push(fb);
                    pieces.push(id);
                }
            }
        }

        loop {
            let mut best_score = f32::NEG_INFINITY;
            let mut best_i = usize::MAX;
            let mut best_id = 0u32;

            for i in 0..piece_strings.len().saturating_sub(1) {
                let mut merged = piece_strings[i].clone();
                merged.push_str(&piece_strings[i + 1]);
                if let Some(&id) = self.piece_to_id.get(&merged) {
                    let score = self.scores[id as usize];
                    if score > best_score {
                        best_score = score;
                        best_i = i;
                        best_id = id;
                    }
                }
            }

            if best_i == usize::MAX {
                break;
            }

            let merged_str = {
                let mut s = piece_strings[best_i].clone();
                s.push_str(&piece_strings[best_i + 1]);
                s
            };
            piece_strings[best_i] = merged_str;
            piece_strings.remove(best_i + 1);
            pieces[best_i] = best_id;
            pieces.remove(best_i + 1);
        }

        pieces
    }

    fn encode_greedy(&self, bytes: &[u8]) -> Vec<u32> {
        let n = bytes.len();
        let mut ids = Vec::with_capacity(n / 2 + 4);
        let mut pos = 0;

        while pos < n {
            let window_end = (pos + self.max_piece_bytes).min(n);
            let mut end = window_end;
            while end > pos && end < n && is_utf8_continuation(bytes[end]) {
                end -= 1;
            }

            let mut found = false;
            while end > pos {
                if let Ok(piece) = core::str::from_utf8(&bytes[pos..end]) {
                    if let Some(&id) = self.piece_to_id.get(piece) {
                        ids.push(id);
                        pos = end;
                        found = true;
                        break;
                    }
                }
                end -= 1;
                while end > pos && is_utf8_continuation(bytes[end]) {
                    end -= 1;
                }
            }

            if !found {
                let fallback = byte_fallback_piece(bytes[pos]);
                ids.push(self.piece_to_id.get(&fallback).copied().unwrap_or(UNK_ID));
                pos += 1;
            }
        }

        ids
    }

    pub fn decode_ids(&self, ids: &[u32]) -> String {
        let mut out = String::new();
        for &id in ids {
            if let Some(piece) = self.id_to_piece.get(id as usize) {
                out.push_str(&piece.replace(SP_SPACE, " "));
            }
        }
        out.trim_start().to_string()
    }

    pub fn piece_id(&self, piece: &str) -> Option<u32> {
        self.piece_to_id.get(piece).copied()
    }
}

fn normalize_sp(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + SP_SPACE.len_utf8());
    out.push(SP_SPACE);
    let mut prev_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(SP_SPACE);
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

#[inline(always)]
fn is_utf8_continuation(byte: u8) -> bool {
    (byte & 0xC0) == 0x80
}

fn byte_fallback_piece(byte: u8) -> String {
    format!("<0x{byte:02X}>")
}

pub fn to_snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev_underscore = false;
    let mut prev_upper = false;
    let chars: Vec<char> = name.chars().collect();
    let n = chars.len();
    for i in 0..n {
        let c = chars[i];
        if c.is_alphanumeric() || c == '_' {
            if c == '_' {
                if !prev_underscore && !out.is_empty() {
                    out.push('_');
                    prev_underscore = true;
                }
                prev_upper = false;
            } else if c.is_uppercase() {
                let prev_lower =
                    i > 0 && (chars[i - 1].is_lowercase() || chars[i - 1].is_ascii_digit());
                let next_lower = i + 1 < n && chars[i + 1].is_lowercase();
                if i > 0 && !prev_underscore && (prev_lower || (prev_upper && next_lower)) {
                    out.push('_');
                }
                out.push(c.to_ascii_lowercase());
                prev_underscore = false;
                prev_upper = true;
            } else {
                out.push(c);
                prev_underscore = false;
                prev_upper = false;
            }
        } else {
            if !prev_underscore && !out.is_empty() {
                out.push('_');
                prev_underscore = true;
            }
            prev_upper = false;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}
