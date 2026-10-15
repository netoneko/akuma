//! The OCI image config as a value: read the parent's, change it the way
//! Dockerfile instructions do, write the child's — plus the manifest that names
//! it. Pure and host-tested; `build.rs` decides *what* to change.
//!
//! Only the fields `docker build` itself sets are modelled. A field a base image
//! carries that is not listed here (`container_config`, `Memory`, …) is dropped
//! on rewrite rather than copied blindly — the OCI spec makes all of them
//! optional, and a config this module wrote is complete for what it knows.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::json::{self, Seg, Value};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Healthcheck {
    /// `["NONE"]`, `["CMD", …]` or `["CMD-SHELL", "…"]`.
    pub test: Vec<String>,
    /// Nanoseconds; 0 = inherit.
    pub interval: i64,
    pub timeout: i64,
    pub start_period: i64,
    pub retries: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub created: String,
    pub created_by: String,
    pub empty_layer: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageConfig {
    pub architecture: String,
    pub os: String,
    pub created: String,
    pub env: Vec<String>,
    pub cmd: Vec<String>,
    pub entrypoint: Vec<String>,
    pub working_dir: String,
    pub user: String,
    pub exposed_ports: Vec<String>,
    pub volumes: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub stop_signal: String,
    pub shell: Vec<String>,
    pub healthcheck: Option<Healthcheck>,
    pub diff_ids: Vec<String>,
    pub history: Vec<HistoryEntry>,
}

#[cfg(target_arch = "aarch64")]
const NATIVE_ARCH: &str = "arm64";
#[cfg(not(target_arch = "aarch64"))]
const NATIVE_ARCH: &str = "amd64";

impl ImageConfig {
    /// `FROM scratch`: no layers, no config.
    pub fn scratch() -> Self {
        Self {
            architecture: String::from(NATIVE_ARCH),
            os: String::from("linux"),
            created: String::new(),
            env: Vec::new(),
            cmd: Vec::new(),
            entrypoint: Vec::new(),
            working_dir: String::new(),
            user: String::new(),
            exposed_ports: Vec::new(),
            volumes: Vec::new(),
            labels: BTreeMap::new(),
            stop_signal: String::new(),
            shell: Vec::new(),
            healthcheck: None,
            diff_ids: Vec::new(),
            history: Vec::new(),
        }
    }

    /// Read a config blob. Tolerant: anything missing stays at its default.
    pub fn parse(doc: &str) -> Self {
        let mut c = Self::scratch();
        if let Some(a) = json::string_at(doc, &["architecture"]) {
            c.architecture = a;
        }
        if let Some(o) = json::string_at(doc, &["os"]) {
            c.os = o;
        }
        c.created = json::string_at(doc, &["created"]).unwrap_or_default();
        c.env = json::strings_at(doc, &["config", "Env", "*"]);
        c.cmd = json::strings_at(doc, &["config", "Cmd", "*"]);
        c.entrypoint = json::strings_at(doc, &["config", "Entrypoint", "*"]);
        c.working_dir = json::string_at(doc, &["config", "WorkingDir"]).unwrap_or_default();
        c.user = json::string_at(doc, &["config", "User"]).unwrap_or_default();
        c.stop_signal = json::string_at(doc, &["config", "StopSignal"]).unwrap_or_default();
        c.shell = json::strings_at(doc, &["config", "Shell", "*"]);
        c.diff_ids = json::strings_at(doc, &["rootfs", "diff_ids", "*"]);

        let mut hc = Healthcheck::default();
        let mut have_hc = false;
        let mut hist: BTreeMap<usize, HistoryEntry> = BTreeMap::new();
        let _ = json::walk(doc, |path, value| {
            let segs = path.segments();
            match (segs, value) {
                ([Seg::Key(a), Seg::Key(b), Seg::Key(name)], Value::StartObject) if a == "config" => {
                    if b == "ExposedPorts" {
                        c.exposed_ports.push(name.clone());
                    } else if b == "Volumes" {
                        c.volumes.push(name.clone());
                    }
                }
                ([Seg::Key(a), Seg::Key(b), Seg::Key(name)], Value::Str(v)) if a == "config" && b == "Labels" => {
                    c.labels.insert(name.clone(), String::from(v));
                }
                ([Seg::Key(a), Seg::Key(b), Seg::Key(t), Seg::Index(_)], Value::Str(v))
                    if a == "config" && b == "Healthcheck" && t == "Test" =>
                {
                    have_hc = true;
                    hc.test.push(String::from(v));
                }
                ([Seg::Key(a), Seg::Key(b), Seg::Key(f)], Value::Int(n)) if a == "config" && b == "Healthcheck" => {
                    have_hc = true;
                    match f.as_str() {
                        "Interval" => hc.interval = n,
                        "Timeout" => hc.timeout = n,
                        "StartPeriod" => hc.start_period = n,
                        "Retries" => hc.retries = n,
                        _ => {}
                    }
                }
                ([Seg::Key(a), Seg::Index(i), Seg::Key(f)], v) if a == "history" => {
                    let e = hist.entry(*i).or_insert(HistoryEntry {
                        created: String::new(),
                        created_by: String::new(),
                        empty_layer: false,
                    });
                    match (f.as_str(), v) {
                        ("created", Value::Str(s)) => e.created = String::from(s),
                        ("created_by", Value::Str(s)) => e.created_by = String::from(s),
                        ("empty_layer", Value::Bool(b)) => e.empty_layer = b,
                        _ => {}
                    }
                }
                _ => {}
            }
        });
        if have_hc {
            c.healthcheck = Some(hc);
        }
        c.history = hist.into_values().collect();
        c
    }

    /// Record a step in `history`; `layer` = it produced a diff.
    pub fn push_history(&mut self, created: &str, created_by: &str, layer: bool) {
        self.history.push(HistoryEntry {
            created: String::from(created),
            created_by: String::from(created_by),
            empty_layer: !layer,
        });
    }

    /// `ENV k=v`: replace an existing key in place, else append.
    pub fn set_env(&mut self, key: &str, value: &str) {
        let entry = format!("{key}={value}");
        match self.env.iter().position(|e| e.split('=').next() == Some(key)) {
            Some(i) => self.env[i] = entry,
            None => self.env.push(entry),
        }
    }

    pub fn to_json(&self) -> String {
        let mut o = String::from("{");
        o.push_str(&format!("\"architecture\":{},\"os\":{}", q(&self.architecture), q(&self.os)));
        if !self.created.is_empty() {
            o.push_str(&format!(",\"created\":{}", q(&self.created)));
        }
        o.push_str(",\"config\":{");
        let mut first = true;
        let mut field = |o: &mut String, text: String| {
            if !first {
                o.push(',');
            }
            first = false;
            o.push_str(&text);
        };
        if !self.user.is_empty() {
            field(&mut o, format!("\"User\":{}", q(&self.user)));
        }
        if !self.exposed_ports.is_empty() {
            field(&mut o, format!("\"ExposedPorts\":{}", key_set(&self.exposed_ports)));
        }
        if !self.env.is_empty() {
            field(&mut o, format!("\"Env\":{}", str_array(&self.env)));
        }
        if !self.entrypoint.is_empty() {
            field(&mut o, format!("\"Entrypoint\":{}", str_array(&self.entrypoint)));
        }
        if !self.cmd.is_empty() {
            field(&mut o, format!("\"Cmd\":{}", str_array(&self.cmd)));
        }
        if let Some(h) = &self.healthcheck {
            let mut t = format!("\"Healthcheck\":{{\"Test\":{}", str_array(&h.test));
            for (name, n) in [
                ("Interval", h.interval),
                ("Timeout", h.timeout),
                ("StartPeriod", h.start_period),
                ("Retries", h.retries),
            ] {
                if n != 0 {
                    t.push_str(&format!(",\"{name}\":{n}"));
                }
            }
            t.push('}');
            field(&mut o, t);
        }
        if !self.volumes.is_empty() {
            field(&mut o, format!("\"Volumes\":{}", key_set(&self.volumes)));
        }
        if !self.working_dir.is_empty() {
            field(&mut o, format!("\"WorkingDir\":{}", q(&self.working_dir)));
        }
        if !self.labels.is_empty() {
            let body: Vec<String> = self.labels.iter().map(|(k, v)| format!("{}:{}", q(k), q(v))).collect();
            field(&mut o, format!("\"Labels\":{{{}}}", body.join(",")));
        }
        if !self.stop_signal.is_empty() {
            field(&mut o, format!("\"StopSignal\":{}", q(&self.stop_signal)));
        }
        if !self.shell.is_empty() {
            field(&mut o, format!("\"Shell\":{}", str_array(&self.shell)));
        }
        o.push_str("},\"rootfs\":{\"type\":\"layers\",\"diff_ids\":");
        o.push_str(&str_array(&self.diff_ids));
        o.push_str("},\"history\":[");
        for (i, h) in self.history.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str(&format!("{{\"created\":{},\"created_by\":{}", q(&h.created), q(&h.created_by)));
            if h.empty_layer {
                o.push_str(",\"empty_layer\":true");
            }
            o.push('}');
        }
        o.push_str("]}");
        o
    }
}

/// A JSON string literal.
pub fn q(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn str_array(items: &[String]) -> String {
    let body: Vec<String> = items.iter().map(|s| q(s)).collect();
    format!("[{}]", body.join(","))
}

/// `{"80/tcp":{},"443/tcp":{}}` — OCI's spelling of a set.
fn key_set(items: &[String]) -> String {
    let mut sorted: Vec<&String> = items.iter().collect();
    sorted.sort();
    sorted.dedup();
    let body: Vec<String> = sorted.iter().map(|s| format!("{}:{{}}", q(s))).collect();
    format!("{{{}}}", body.join(","))
}

pub const MEDIA_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const MEDIA_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const MEDIA_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

/// An OCI image manifest. `layers` are `(compressed digest, size)`, base-first.
pub fn manifest_json(config_digest: &str, config_size: u64, layers: &[(String, u64)]) -> String {
    let mut o = format!(
        "{{\"schemaVersion\":2,\"mediaType\":\"{MEDIA_MANIFEST}\",\"config\":{{\"mediaType\":\"{MEDIA_CONFIG}\",\"digest\":{},\"size\":{config_size}}},\"layers\":[",
        q(config_digest)
    );
    for (i, (d, n)) in layers.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push_str(&format!("{{\"mediaType\":\"{MEDIA_LAYER_GZIP}\",\"digest\":{},\"size\":{n}}}", q(d)));
    }
    o.push_str("]}");
    o
}

/// `name` → `name:latest`; a reference that already has a tag is unchanged.
/// (A `:` before the last `/` is a registry port, not a tag.)
pub fn with_default_tag(name: &str) -> String {
    let last = name.rsplit('/').next().unwrap_or(name);
    if last.contains(':') || last.contains('@') {
        String::from(name)
    } else {
        format!("{name}:latest")
    }
}

/// `blobs/sha256/<hex>` for `sha256:<hex>`.
pub fn blob_entry(digest: &str) -> String {
    format!("blobs/sha256/{}", digest.strip_prefix("sha256:").unwrap_or(digest))
}

/// The OCI layout's `index.json` for one image. `reference` is the full
/// `repo:tag`; the annotations are how Docker and containerd learn the name.
pub fn index_json(manifest_digest: &str, manifest_size: u64, reference: &str) -> String {
    let tag = reference.rsplit(':').next().unwrap_or("latest");
    format!(
        "{{\"schemaVersion\":2,\"mediaType\":\"application/vnd.oci.image.index.v1+json\",\"manifests\":[{{\"mediaType\":\"{MEDIA_MANIFEST}\",\"digest\":{},\"size\":{manifest_size},\"annotations\":{{\"io.containerd.image.name\":{},\"org.opencontainers.image.ref.name\":{}}}}}]}}",
        q(manifest_digest),
        q(reference),
        q(tag)
    )
}

/// The legacy `docker save` `manifest.json`, which every Docker version reads.
pub fn docker_manifest_json(config_digest: &str, reference: &str, layer_digests: &[String]) -> String {
    let layers: Vec<String> = layer_digests.iter().map(|d| q(&blob_entry(d))).collect();
    format!(
        "[{{\"Config\":{},\"RepoTags\":[{}],\"Layers\":[{}]}}]",
        q(&blob_entry(config_digest)),
        q(reference),
        layers.join(",")
    )
}

/// RFC 3339 UTC, whole seconds: `2026-10-15T12:34:56Z`.
pub fn rfc3339(epoch: u64) -> String {
    let days = (epoch / 86_400) as i64;
    let rem = epoch % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn sample() -> ImageConfig {
        let mut c = ImageConfig::scratch();
        c.created = rfc3339(1_700_000_000);
        c.env = vec!["PATH=/bin".into(), "A=\"quoted\"".into()];
        c.cmd = vec!["/bin/sh".into(), "-c".into(), "echo \"hi\"\n".into()];
        c.entrypoint = vec!["/entry".into()];
        c.working_dir = "/work".into();
        c.user = "app:app".into();
        c.exposed_ports = vec!["8080/tcp".into(), "80/tcp".into()];
        c.volumes = vec!["/data".into()];
        c.labels.insert("k".into(), "v w".into());
        c.stop_signal = "SIGTERM".into();
        c.shell = vec!["/bin/bash".into(), "-c".into()];
        c.healthcheck = Some(Healthcheck {
            test: vec!["CMD-SHELL".into(), "true".into()],
            interval: 30_000_000_000,
            timeout: 0,
            start_period: 0,
            retries: 3,
        });
        c.diff_ids = vec!["sha256:aa".into(), "sha256:bb".into()];
        c.push_history("2023-11-14T22:13:20Z", "/bin/sh -c apk add x", true);
        c.push_history("2023-11-14T22:13:21Z", "ENV A=1", false);
        c
    }

    #[test]
    fn config_round_trips() {
        let c = sample();
        let mut back = ImageConfig::parse(&c.to_json());
        // Ports are written sorted; compare as sets.
        back.exposed_ports.sort();
        let mut want = c.clone();
        want.exposed_ports.sort();
        assert_eq!(back, want);
    }

    #[test]
    fn output_is_what_box_run_reads() {
        let json = sample().to_json();
        let p = crate::spec::image_process_from_config(&json);
        assert_eq!(p.entrypoint, ["/entry"]);
        assert_eq!(p.working_dir, "/work");
        assert_eq!(p.user, "app:app");
        assert_eq!(p.env[0], "PATH=/bin");
    }

    #[test]
    fn empty_fields_are_omitted_not_blank() {
        let json = ImageConfig::scratch().to_json();
        assert!(!json.contains("WorkingDir") && !json.contains("User") && !json.contains("Env"));
        assert!(json.contains("\"diff_ids\":[]") && json.contains("\"history\":[]"));
    }

    #[test]
    fn env_replaces_in_place() {
        let mut c = ImageConfig::scratch();
        c.set_env("A", "1");
        c.set_env("B", "2");
        c.set_env("A", "3");
        assert_eq!(c.env, ["A=3", "B=2"]);
        // A key that is a prefix of another must not match it.
        c.set_env("AB", "x");
        assert_eq!(c.env, ["A=3", "B=2", "AB=x"]);
    }

    #[test]
    fn manifest_names_config_and_layers() {
        let m = manifest_json("sha256:cc", 10, &[("sha256:l1".into(), 5), ("sha256:l2".into(), 6)]);
        assert_eq!(json::string_at(&m, &["config", "digest"]).unwrap(), "sha256:cc");
        assert_eq!(json::strings_at(&m, &["layers", "*", "digest"]), ["sha256:l1", "sha256:l2"]);
        assert_eq!(json::number_at(&m, &["layers", "1", "size"]), Some(6));
        // And the pull side's own reader agrees.
        let parsed = crate::manifest::parse_manifest(&m).unwrap();
        assert_eq!(parsed.layer_digests, ["sha256:l1", "sha256:l2"]);
    }

    #[test]
    fn save_documents() {
        assert_eq!(with_default_tag("app"), "app:latest");
        assert_eq!(with_default_tag("app:2"), "app:2");
        assert_eq!(with_default_tag("localhost:5000/app"), "localhost:5000/app:latest");
        assert_eq!(blob_entry("sha256:ab"), "blobs/sha256/ab");
        let idx = index_json("sha256:mm", 7, "app:2");
        assert_eq!(json::string_at(&idx, &["manifests", "0", "digest"]).unwrap(), "sha256:mm");
        assert_eq!(
            json::string_at(&idx, &["manifests", "0", "annotations", "org.opencontainers.image.ref.name"]).unwrap(),
            "2"
        );
        let m = docker_manifest_json("sha256:cc", "app:2", &["sha256:l1".into()]);
        assert_eq!(json::string_at(&m, &["0", "Config"]).unwrap(), "blobs/sha256/cc");
        assert_eq!(json::strings_at(&m, &["0", "Layers", "*"]), ["blobs/sha256/l1"]);
        assert_eq!(json::string_at(&m, &["0", "RepoTags", "0"]).unwrap(), "app:2");
    }

    #[test]
    fn rfc3339_known_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn json_escaping() {
        assert_eq!(q("a\"b\\c\n\u{1}"), "\"a\\\"b\\\\c\\n\\u0001\"");
    }
}
