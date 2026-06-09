//! Cactus API client + HuggingFace CDN weight downloader.
//!
//! Flow:
//!   1. GET https://www.cactuscompute.com/api/models (no auth)
//!   2. Find entry where id matches --model arg
//!   3. Extract downloadUrl for needle.safetensors and vocab.txt
//!   4. download_file(url, dest) via libakuma-tls (with optional HF Bearer token)
//!
//! Falls back to direct HuggingFace URL if Cactus API doesn't list the model.

use alloc::format;
use alloc::string::{String, ToString};
use libakuma_tls::{download_file_with_headers, https_get, HttpHeaders};

pub const DEFAULT_MODEL: &str = "Abdalrahman/needle-rs-safetensors";
pub const DEFAULT_WEIGHTS_DIR: &str = "/data/needle";

pub struct DownloadConfig<'a> {
    pub model_id: &'a str,
    pub weights_dir: &'a str,
    pub hf_token: Option<&'a str>,
}

pub fn ensure_weights(cfg: &DownloadConfig) -> Result<(String, String), String> {
    let weights_path = format!("{}/needle.safetensors", cfg.weights_dir);
    let vocab_path = format!("{}/vocab.txt", cfg.weights_dir);

    if libakuma::fs::exists(&weights_path) && libakuma::fs::exists(&vocab_path) {
        return Ok((weights_path, vocab_path));
    }

    if libakuma::mkdir(cfg.weights_dir) < 0 && !libakuma::fs::exists(cfg.weights_dir) {
        return Err(format!("cannot create {}", cfg.weights_dir));
    }

    let (weights_url, vocab_url) = resolve_urls(cfg)?;

    let mut headers = HttpHeaders::new();
    if let Some(token) = cfg.hf_token {
        headers.bearer_auth(token);
    }

    libakuma::println(&format!("downloading weights from {weights_url}"));
    download_file_with_headers(&weights_url, &weights_path, &headers)
        .map_err(|e| format!("weights download failed: {e:?}"))?;

    libakuma::println(&format!("downloading vocab from {vocab_url}"));
    download_file_with_headers(&vocab_url, &vocab_path, &headers)
        .map_err(|e| format!("vocab download failed: {e:?}"))?;

    Ok((weights_path, vocab_path))
}

fn resolve_urls(cfg: &DownloadConfig) -> Result<(String, String), String> {
    if let Some(urls) = try_cactus_api(cfg.model_id) {
        return Ok(urls);
    }
    Ok(hf_urls(cfg.model_id))
}

fn try_cactus_api(model_id: &str) -> Option<(String, String)> {
    let headers = HttpHeaders::new();
    let body = https_get("https://www.cactuscompute.com/api/models", &headers).ok()?;
    let json = core::str::from_utf8(&body).ok()?;

    let model_id_lower = model_id.to_lowercase();
    if !json.contains(&model_id_lower) && !json.contains(model_id) {
        return None;
    }

    let weights_url = extract_download_url(json, "needle.safetensors")?;
    let vocab_url = extract_download_url(json, "vocab.txt")?;
    Some((weights_url, vocab_url))
}

fn extract_download_url(json: &str, filename: &str) -> Option<String> {
    let file_pos = json.find(filename)?;
    let search_start = if file_pos > 2048 { file_pos - 2048 } else { 0 };
    let window = &json[search_start..file_pos + filename.len() + 512];

    let dl_key = "\"downloadUrl\":\"";
    let key_pos = window.rfind(dl_key)?;
    let after = &window[key_pos + dl_key.len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

fn hf_urls(model_id: &str) -> (String, String) {
    let weights = format!(
        "https://huggingface.co/{}/resolve/main/needle.safetensors",
        model_id
    );
    let vocab = format!(
        "https://huggingface.co/{}/resolve/main/vocab.txt",
        model_id
    );
    (weights, vocab)
}
