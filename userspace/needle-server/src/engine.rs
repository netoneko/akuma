//! Needle inference engine — ported from needle-infer to no_std.
//!
//! Changes: std::path/io → libakuma::fs + custom error, eprintln → removed,
//! std::cmp::Reverse → core::cmp::Reverse, tests removed.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use needle_core::{
    config::FfnActivation,
    ffn::FfnWeights,
    layers::{DecoderLayer, EncoderLayer},
    model::NeedleModel,
    quant::QuantizedWeight,
    TransformerConfig,
};

use crate::constrained::{ConstrainedDecoder, ToolDef};
use crate::safetensors::{ParseError, SafeTensors};
use crate::tokenizer::{Vocabulary, EOS_ID, TOOLS_ID};

#[derive(Debug, Clone)]
pub struct InferenceResult {
    pub token_ids: Vec<u32>,
    pub text: String,
}

struct ContrastiveHead {
    hidden_kernel: Vec<f32>,
    hidden_bias: Vec<f32>,
    proj_kernel: Vec<f32>,
    hidden_dim: usize,
    pub contrastive_dim: usize,
}

impl ContrastiveHead {
    #[allow(clippy::needless_range_loop)]
    fn encode(&self, pooled: &[f32]) -> Vec<f32> {
        let d = pooled.len();
        let h = self.hidden_dim;
        let c = self.contrastive_dim;

        let mut hidden = vec![0.0f32; h];
        for j in 0..h {
            let mut acc = self.hidden_bias[j];
            for i in 0..d {
                acc += pooled[i] * self.hidden_kernel[i * h + j];
            }
            hidden[j] = acc.max(0.0);
        }

        let mut proj = vec![0.0f32; c];
        for j in 0..c {
            let mut acc = 0.0f32;
            for i in 0..h {
                acc += hidden[i] * self.proj_kernel[i * c + j];
            }
            proj[j] = acc;
        }

        let sq_sum: f32 = proj.iter().map(|x| x * x).sum();
        let norm = libm::sqrtf(sq_sum + 1e-12_f32);
        proj.iter_mut().for_each(|x| *x /= norm);
        proj
    }
}

pub struct NeedleEngine {
    model: NeedleModel,
    vocab: Vocabulary,
    max_gen_len: usize,
    contrastive: Option<ContrastiveHead>,
}

impl NeedleEngine {
    pub fn load(weights_path: &str, vocab_path: &str) -> Result<Self, ParseError> {
        let st = SafeTensors::load(weights_path)?;
        let vocab = Vocabulary::load_text(vocab_path)?;
        Self::from_parts(st, vocab)
    }

    pub fn from_bytes(weights_bytes: Vec<u8>, vocab_text: &str) -> Result<Self, ParseError> {
        let st = SafeTensors::from_bytes(weights_bytes)?;
        let vocab = Vocabulary::parse(vocab_text);
        Self::from_parts(st, vocab)
    }

    fn from_parts(st: SafeTensors, vocab: Vocabulary) -> Result<Self, ParseError> {
        let cfg = load_config_from_safetensors(&st);
        cfg.validate()
            .map_err(|e| ParseError::InvalidDataOwned(e.to_string()))?;
        let model = load_model(&st, &cfg)?;
        let contrastive = load_contrastive_head(&st, cfg.d_model);
        Ok(Self {
            model,
            vocab,
            max_gen_len: cfg.max_dec_len,
            contrastive,
        })
    }

    pub fn model(&self) -> &NeedleModel {
        &self.model
    }

    pub fn cfg(&self) -> &needle_core::TransformerConfig {
        &self.model.cfg
    }

    pub fn run(&self, query: &str, tools_json: &str) -> InferenceResult {
        self.run_impl(query, tools_json, |_, _| {})
    }

    pub fn run_stream<F>(&self, query: &str, tools_json: &str, on_token: F) -> InferenceResult
    where
        F: FnMut(u32, &str),
    {
        self.run_impl(query, tools_json, on_token)
    }

    pub fn encode_contrastive(&self, text: &str) -> Option<Vec<f32>> {
        let head = self.contrastive.as_ref()?;
        let d = self.model.cfg.d_model;

        let token_ids = self.vocab.encode(text);
        if token_ids.is_empty() {
            return Some(vec![0.0f32; head.contrastive_dim]);
        }

        let mut enc_kv = self.model.make_enc_kv_caches(token_ids.len());
        let enc_hidden = self.model.encode(&token_ids, &mut enc_kv);

        let seq_len = token_ids.len();
        let mut pooled = vec![0.0f32; d];
        for t in 0..seq_len {
            for j in 0..d {
                pooled[j] += enc_hidden[t * d + j];
            }
        }
        let inv_n = 1.0 / seq_len as f32;
        pooled.iter_mut().for_each(|x| *x *= inv_n);

        Some(head.encode(&pooled))
    }

    pub fn contrastive_dim(&self) -> usize {
        self.contrastive.as_ref().map_or(0, |h| h.contrastive_dim)
    }

    pub fn retrieve_tools(
        &self,
        query: &str,
        tool_descriptions: &[&str],
        top_k: usize,
    ) -> Vec<(usize, f32)> {
        let q_emb = match self.encode_contrastive(query) {
            Some(e) => e,
            None => return Vec::new(),
        };

        let mut scores: Vec<(usize, f32)> = tool_descriptions
            .iter()
            .enumerate()
            .filter_map(|(i, desc)| {
                let t_emb = self.encode_contrastive(desc)?;
                let score: f32 = q_emb.iter().zip(t_emb.iter()).map(|(a, b)| a * b).sum();
                Some((i, score))
            })
            .collect();

        scores.sort_by(|(_, a), (_, b)| b.total_cmp(a));
        scores.truncate(top_k);
        scores
    }

    fn run_impl<F>(&self, query: &str, tools_json: &str, mut on_token: F) -> InferenceResult
    where
        F: FnMut(u32, &str),
    {
        let tool_defs = ToolDef::from_json(tools_json);
        let normalized_tools = normalize_tools_json(tools_json, &tool_defs);
        let compact_tools = compact_json(&normalized_tools);

        let query_ids = self.vocab.encode(query);
        let tools_ids = self.vocab.encode(&compact_tools);

        if query_ids.is_empty() && tools_ids.is_empty() {
            return InferenceResult { token_ids: Vec::new(), text: String::new() };
        }

        let max_enc = self.model.cfg.max_enc_len;
        if max_enc == 0 {
            return InferenceResult { token_ids: Vec::new(), text: String::new() };
        }
        let q_len = query_ids.len().min(max_enc.saturating_sub(2));
        let remaining = max_enc.saturating_sub(q_len + 1);
        let t_len = tools_ids.len().min(remaining);

        let mut enc_input = Vec::with_capacity(q_len + 1 + t_len);
        enc_input.extend_from_slice(&query_ids[..q_len]);
        enc_input.push(TOOLS_ID);
        enc_input.extend_from_slice(&tools_ids[..t_len]);

        let enc_len = enc_input.len();
        let mut enc_kv = self.model.make_enc_kv_caches(enc_len);
        let mut dec_kv = self.model.make_dec_kv_caches();
        self.model.encode(&enc_input, &mut enc_kv);

        let token_bytes: Vec<(u32, Vec<u8>)> = self
            .vocab
            .id_to_piece
            .iter()
            .enumerate()
            .map(|(i, piece)| (i as u32, piece.replace('\u{2581}', " ").into_bytes()))
            .collect();
        let mut constrained = ConstrainedDecoder::new(&tool_defs, token_bytes);

        let mut output_ids = Vec::with_capacity(64);
        let mut current_token = EOS_ID;
        let mut logits = vec![0.0f32; self.model.cfg.vocab_size];

        for _step in 0..self.max_gen_len {
            self.model.decode_step(current_token, &enc_kv, &mut dec_kv, &mut logits);

            let mask = constrained.logit_mask(self.model.cfg.vocab_size);
            for (l, &m) in logits.iter_mut().zip(mask.iter()) {
                *l += m;
            }

            let next_token = logits
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.total_cmp(b))
                .map(|(i, _)| i as u32)
                .unwrap_or(EOS_ID);

            if next_token == EOS_ID { break; }

            let piece = self
                .vocab
                .id_to_piece
                .get(next_token as usize)
                .map(|p| p.replace('\u{2581}', " "))
                .unwrap_or_default();
            on_token(next_token, &piece);

            output_ids.push(next_token);
            current_token = next_token;
            constrained.update(next_token);
        }

        let raw = self.vocab.decode_ids(&output_ids);

        let tool_call_piece = self
            .vocab
            .id_to_piece
            .get(crate::tokenizer::TOOL_CALL_ID as usize)
            .map(|p| p.replace('\u{2581}', " "))
            .unwrap_or_default();
        let stripped = if !tool_call_piece.is_empty() {
            raw.strip_prefix(&tool_call_piece).unwrap_or(&raw)
        } else {
            &raw
        };

        let text = restore_tool_names(stripped, &tool_defs);

        InferenceResult { token_ids: output_ids, text }
    }
}

fn normalize_tools_json(json: &str, tool_defs: &[ToolDef]) -> String {
    let renames: Vec<(&str, &str)> = tool_defs
        .iter()
        .filter(|t| t.name != t.snake_name)
        .map(|t| (t.name.as_str(), t.snake_name.as_str()))
        .collect();

    if renames.is_empty() { return json.to_string(); }

    let bytes = json.as_bytes();
    let mut out = String::with_capacity(json.len() + 16);
    let mut i = 0;
    let mut last_flush = 0;

    while i < bytes.len() {
        if bytes[i] == b'"' && bytes[i..].starts_with(b"\"name\"") {
            out.push_str(&json[last_flush..i]);
            out.push_str("\"name\"");
            i += 6;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; }
            if i < bytes.len() && bytes[i] == b':' { out.push(':'); i += 1; }
            while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; }
            if i < bytes.len() && bytes[i] == b'"' {
                out.push('"');
                i += 1;
                let val_start = i;
                while i < bytes.len() {
                    if bytes[i] == b'\\' { i += 2; continue; }
                    if bytes[i] == b'"' { break; }
                    i += 1;
                }
                let val = &json[val_start..i];
                let replacement = renames.iter().find(|(orig, _)| *orig == val)
                    .map(|(_, snake)| *snake).unwrap_or(val);
                out.push_str(replacement);
                if i < bytes.len() { out.push('"'); i += 1; }
            }
            last_flush = i;
        } else {
            i += 1;
        }
    }

    out.push_str(&json[last_flush..]);
    out
}

fn compact_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    let mut in_string = false;
    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if ch == '\\' {
                if let Some(escaped) = chars.next() { out.push(escaped); }
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
            out.push('"');
        } else if !ch.is_ascii_whitespace() {
            out.push(ch);
        }
    }
    out
}

fn restore_tool_names(text: &str, tool_defs: &[ToolDef]) -> String {
    let mut renames: Vec<(&str, &str)> = tool_defs
        .iter()
        .filter(|t| t.name != t.snake_name)
        .map(|t| (t.snake_name.as_str(), t.name.as_str()))
        .collect();
    renames.sort_by_key(|(a, _): &(&str, &str)| core::cmp::Reverse(a.len()));

    if renames.is_empty() { return text.to_string(); }

    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut last_flush = 0;

    while i < bytes.len() {
        if bytes[i] == b'"' && bytes[i..].starts_with(b"\"name\"") {
            out.push_str(&text[last_flush..i]);
            out.push_str("\"name\"");
            i += 6;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; }
            if i < bytes.len() && bytes[i] == b':' { out.push(':'); i += 1; }
            while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; }
            if i < bytes.len() && bytes[i] == b'"' {
                out.push('"');
                i += 1;
                let val_start = i;
                while i < bytes.len() {
                    if bytes[i] == b'\\' { i += 2; continue; }
                    if bytes[i] == b'"' { break; }
                    i += 1;
                }
                let val = &text[val_start..i];
                let replacement = renames.iter().find(|(snake, _)| *snake == val)
                    .map(|(_, orig)| *orig).unwrap_or(val);
                out.push_str(replacement);
                if i < bytes.len() { out.push('"'); i += 1; }
            }
            last_flush = i;
        } else {
            i += 1;
        }
    }

    out.push_str(&text[last_flush..]);
    out
}

fn load_contrastive_head(st: &SafeTensors, d_model: usize) -> Option<ContrastiveHead> {
    let proj_kernel = st.get_f32("contrastive_proj_kernel")?;
    let hidden_kernel = st.get_f32("contrastive_hidden_kernel")?;
    let hidden_bias = st.get_f32("contrastive_hidden_bias")?;

    let hidden_dim = hidden_bias.len();
    if hidden_dim == 0
        || hidden_kernel.len() != d_model * hidden_dim
        || proj_kernel.len() % hidden_dim != 0
    {
        return None;
    }
    let contrastive_dim = proj_kernel.len() / hidden_dim;
    Some(ContrastiveHead { hidden_kernel, hidden_bias, proj_kernel, hidden_dim, contrastive_dim })
}

fn load_config_from_safetensors(st: &SafeTensors) -> TransformerConfig {
    let d = TransformerConfig::default();

    let usize_field = |key: &str, fallback: usize| -> usize {
        st.get_metadata(key).and_then(|s| s.parse().ok()).unwrap_or(fallback)
    };
    let f32_field = |key: &str, fallback: f32| -> f32 {
        st.get_metadata(key).and_then(|s| s.parse().ok()).unwrap_or(fallback)
    };
    let bool_field = |key: &str, fallback: bool| -> bool {
        st.get_metadata(key)
            .map(|s| matches!(s, "True" | "true" | "1"))
            .unwrap_or(fallback)
    };

    let max_enc_len = st
        .get_metadata("max_enc_len")
        .and_then(|s| s.parse().ok())
        .or_else(|| st.get_metadata("max_seq_len").and_then(|s| s.parse().ok()))
        .unwrap_or(d.max_enc_len);

    let activation = st
        .get_metadata("activation")
        .map(FfnActivation::parse)
        .unwrap_or(d.activation.clone());

    TransformerConfig {
        d_model: usize_field("d_model", d.d_model),
        num_heads: usize_field("num_heads", d.num_heads),
        num_kv_heads: usize_field("num_kv_heads", d.num_kv_heads),
        num_layers: usize_field("num_encoder_layers", d.num_layers),
        num_dec_layers: usize_field("num_decoder_layers", d.num_dec_layers),
        vocab_size: usize_field("vocab_size", d.vocab_size),
        max_enc_len,
        max_dec_len: usize_field("max_dec_len", d.max_dec_len),
        ffn_dim: usize_field("ffn_dim", d.ffn_dim),
        no_feedforward: bool_field("no_feedforward", d.no_feedforward),
        activation,
        rope_theta: f32_field("rope_theta", d.rope_theta),
        ..d
    }
}

fn load_model(st: &SafeTensors, cfg: &TransformerConfig) -> Result<NeedleModel, ParseError> {
    let d = cfg.d_model;
    let v = cfg.vocab_size;

    let embedding = st.get_f32("embedding")
        .ok_or(ParseError::InvalidData("missing embedding tensor"))?;
    if embedding.len() != v * d {
        return Err(ParseError::InvalidDataOwned(
            format!("embedding size mismatch: got {}, expected {}", embedding.len(), v * d)
        ));
    }

    let encoder_layers = (0..cfg.num_layers).map(|i| load_encoder_layer(st, cfg, i)).collect();
    let decoder_layers = (0..cfg.num_dec_layers).map(|i| load_decoder_layer(st, cfg, i)).collect();

    let encoder_final_norm = st.get_f32("encoder_final_norm")
        .unwrap_or_else(|| vec![0.0f32; d]);
    let decoder_final_norm = st.get_f32("decoder_final_norm")
        .unwrap_or_else(|| vec![0.0f32; d]);

    Ok(NeedleModel::new(
        cfg.clone(),
        embedding,
        encoder_layers,
        decoder_layers,
        encoder_final_norm,
        decoder_final_norm,
    ))
}

fn load_encoder_layer(st: &SafeTensors, cfg: &TransformerConfig, i: usize) -> EncoderLayer {
    let prefix = format!("encoder.{i}");
    let (ffn, ffn_gate, ffn_norm, ffn_activation) = if cfg.no_feedforward {
        (None, 0.0, None, None)
    } else {
        let f = load_ffn_weights(st, cfg, &format!("{prefix}.ffn"));
        let g = load_scalar(st, &format!("{prefix}.ffn_gate"));
        let n = load_vec(st, &format!("{prefix}.ffn_norm"), cfg.d_model);
        (Some(f), g, Some(n), Some(cfg.activation.clone()))
    };
    EncoderLayer {
        self_attn: load_attn_weights(st, cfg, &format!("{prefix}.self_attn")),
        self_attn_gate: load_scalar(st, &format!("{prefix}.self_attn_gate")),
        norm: load_vec(st, &format!("{prefix}.norm"), cfg.d_model),
        ffn,
        ffn_gate,
        ffn_norm,
        ffn_activation,
    }
}

fn load_decoder_layer(st: &SafeTensors, cfg: &TransformerConfig, i: usize) -> DecoderLayer {
    let prefix = format!("decoder.{i}");
    let (ffn, ffn_gate, ffn_norm, ffn_activation) = if cfg.no_feedforward {
        (None, 0.0, None, None)
    } else {
        let f = load_ffn_weights(st, cfg, &format!("{prefix}.ffn"));
        let g = load_scalar(st, &format!("{prefix}.ffn_gate"));
        let n = load_vec(st, &format!("{prefix}.ffn_norm"), cfg.d_model);
        (Some(f), g, Some(n), Some(cfg.activation.clone()))
    };
    DecoderLayer {
        self_attn: load_attn_weights(st, cfg, &format!("{prefix}.self_attn")),
        self_attn_gate: load_scalar(st, &format!("{prefix}.self_attn_gate")),
        self_attn_norm: load_vec(st, &format!("{prefix}.self_attn_norm"), cfg.d_model),
        cross_attn: load_attn_weights(st, cfg, &format!("{prefix}.cross_attn")),
        cross_attn_gate: load_scalar(st, &format!("{prefix}.cross_attn_gate")),
        cross_attn_norm: load_vec(st, &format!("{prefix}.cross_attn_norm"), cfg.d_model),
        ffn,
        ffn_gate,
        ffn_norm,
        ffn_activation,
    }
}

fn load_ffn_weights(st: &SafeTensors, cfg: &TransformerConfig, prefix: &str) -> FfnWeights {
    let d = cfg.d_model;
    let ff = cfg.ffn_dim;
    FfnWeights {
        gate_proj: load_quant(st, &format!("{prefix}.gate_proj"), d, ff),
        up_proj: load_quant(st, &format!("{prefix}.up_proj"), d, ff),
        down_proj: load_quant(st, &format!("{prefix}.down_proj"), ff, d),
    }
}

fn load_attn_weights(
    st: &SafeTensors,
    cfg: &TransformerConfig,
    prefix: &str,
) -> needle_core::attn::AttnWeights {
    let d = cfg.d_model;
    let h = cfg.num_heads;
    let kv_h = cfg.num_kv_heads;
    let hd = cfg.head_dim();
    needle_core::attn::AttnWeights {
        wq: load_quant(st, &format!("{prefix}.wq"), d, h * hd),
        wk: load_quant(st, &format!("{prefix}.wk"), d, kv_h * hd),
        wv: load_quant(st, &format!("{prefix}.wv"), d, kv_h * hd),
        wo: load_quant(st, &format!("{prefix}.wo"), h * hd, d),
        q_norm: load_vec(st, &format!("{prefix}.q_norm"), hd),
        k_norm: load_vec(st, &format!("{prefix}.k_norm"), hd),
    }
}

fn load_quant(st: &SafeTensors, name: &str, in_feat: usize, out_feat: usize) -> QuantizedWeight {
    let scale_name = format!("{name}.scale");
    if let (Some(raw), Some(scales)) = (st.get_raw(name), st.get_f32(&scale_name)) {
        if scales.len() % out_feat == 0 {
            let num_groups = scales.len() / out_feat;
            return QuantizedWeight {
                data: raw.to_vec(),
                scales,
                in_feat,
                out_feat,
                num_groups,
            };
        }
    }
    let w = st.get_f32(name).unwrap_or_else(|| vec![0.0f32; in_feat * out_feat]);
    QuantizedWeight::quantize(&w, in_feat, out_feat)
}

fn load_vec(st: &SafeTensors, name: &str, len: usize) -> Vec<f32> {
    st.get_f32(name).unwrap_or_else(|| vec![0.0f32; len])
}

fn load_scalar(st: &SafeTensors, name: &str) -> f32 {
    st.get_f32(name).and_then(|v| v.first().copied()).unwrap_or(0.0)
}
