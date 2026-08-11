//! What a registry says an image is made of.
//!
//! An image is fetched in three hops: a **manifest list** (OCI calls it an
//! index) names one manifest per platform; the platform **manifest** names a
//! config blob and the layer blobs; the config blob says what to run. This
//! module owns the first two — deciding *which* digests to fetch. `oci.rs` does
//! the fetching, `spec.rs` reads the config.
//!
//! Everything here is a function from a JSON document to digests, so it is all
//! host-testable against captured registry responses.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::json::{self, Value};

/// The architecture Akuma runs. Registries spell it `arm64`; some older images
/// say `aarch64`.
const ARCH: [&str; 2] = ["arm64", "aarch64"];
const OS: &str = "linux";

/// A platform manifest: one config blob plus the layers, base-first.
#[derive(Debug, PartialEq, Eq)]
pub struct Manifest {
    pub config_digest: String,
    pub layer_digests: Vec<String>,
}

/// Whether this document names *other manifests* rather than layers.
///
/// Three signals, any of which is enough: the Docker media type, the OCI media
/// type, or the presence of a `manifests` array. The last one matters because
/// Docker Hub omits the top-level `mediaType` on some responses, and without it
/// a list would be parsed as a manifest with no layers.
pub fn is_manifest_list(doc: &str) -> bool {
    let media_type = json::string_at(doc, &["mediaType"]).unwrap_or_default();
    media_type.contains("manifest.list")
        || media_type.contains("image.index")
        || json::exists(doc, &["manifests"])
}

/// The digest of the `linux/arm64` entry in a manifest list.
///
/// Manifest lists also carry attestation entries whose platform is
/// `unknown/unknown`; matching on both architecture *and* os skips those.
pub fn select_platform_digest(doc: &str) -> Result<String, String> {
    // Fields are matched per array element: an entry's digest counts only when
    // that same entry's platform matched.
    let mut arch_ok: Option<usize> = None;
    let mut os_ok: Option<usize> = None;
    let mut digests: Vec<(usize, String)> = Vec::new();
    let mut matched: Option<String> = None;

    json::walk(doc, |path, value| {
        let Value::Str(s) = value else { return };
        if path.matches(&["manifests", "*", "platform", "architecture"]) && ARCH.contains(&s) {
            arch_ok = path.index_at(1);
        } else if path.matches(&["manifests", "*", "platform", "os"]) && s == OS {
            os_ok = path.index_at(1);
        } else if path.matches(&["manifests", "*", "digest"]) {
            if let Some(i) = path.index_at(1) {
                digests.push((i, s.to_string()));
            }
        }
        if matched.is_none() {
            if let (Some(a), Some(o)) = (arch_ok, os_ok) {
                if a == o {
                    matched = digests.iter().find(|(i, _)| *i == a).map(|(_, d)| d.clone());
                }
            }
        }
    })
    .map_err(|e| alloc::format!("malformed manifest list: {:?}", e))?;

    matched.ok_or_else(|| String::from("no linux/arm64 manifest found in manifest list"))
}

/// The config and layer digests of a platform manifest.
pub fn parse_manifest(doc: &str) -> Result<Manifest, String> {
    let config_digest = json::string_at(doc, &["config", "digest"])
        .ok_or_else(|| String::from("no digest in manifest config"))?;

    // Registries emit layers base-first and the order is what the overlay
    // stacking depends on; a walk preserves document order.
    let layer_digests = json::strings_at(doc, &["layers", "*", "digest"]);
    if layer_digests.is_empty() {
        return Err(String::from("no layers in manifest"));
    }

    Ok(Manifest {
        config_digest,
        layer_digests,
    })
}

/// A layer's declared size in bytes, used to verify a download was not
/// truncated.
pub fn layer_size(doc: &str, index: usize) -> Option<i64> {
    let idx = alloc::format!("{}", index);
    json::number_at(doc, &["layers", &idx, "size"])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Docker Hub's response for `library/busybox:latest`, trimmed to the
    /// entries that matter — including the `unknown/unknown` attestation
    /// entries the real response carries.
    const MANIFEST_LIST: &str = r#"{
        "manifests": [
            {
                "digest": "sha256:amd64digest",
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "platform": {"architecture": "amd64", "os": "linux"},
                "size": 610
            },
            {
                "digest": "sha256:arm64digest",
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "platform": {"architecture": "arm64", "os": "linux"},
                "size": 610
            },
            {
                "digest": "sha256:attestation",
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "platform": {"architecture": "unknown", "os": "unknown"},
                "size": 840
            }
        ],
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "schemaVersion": 2
    }"#;

    const MANIFEST: &str = r#"{
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {
            "mediaType": "application/vnd.docker.container.image.v1+json",
            "size": 1471,
            "digest": "sha256:configdigest"
        },
        "layers": [
            {
                "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
                "size": 2295859,
                "digest": "sha256:layer0"
            },
            {
                "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
                "size": 128,
                "digest": "sha256:layer1"
            }
        ]
    }"#;

    #[test]
    fn recognises_a_manifest_list() {
        assert!(is_manifest_list(MANIFEST_LIST));
        assert!(!is_manifest_list(MANIFEST));
    }

    #[test]
    fn recognises_a_list_with_no_media_type() {
        // Docker Hub omits the top-level mediaType on some responses; the
        // `manifests` array is then the only signal, and missing it would parse
        // a list as a layerless manifest.
        let doc = r#"{"manifests":[{"digest":"sha256:a","platform":{"architecture":"arm64","os":"linux"}}]}"#;
        assert!(is_manifest_list(doc));
    }

    #[test]
    fn recognises_the_docker_media_type() {
        let doc = r#"{"mediaType":"application/vnd.docker.distribution.manifest.list.v2+json"}"#;
        assert!(is_manifest_list(doc));
    }

    #[test]
    fn selects_the_arm64_entry() {
        assert_eq!(
            select_platform_digest(MANIFEST_LIST).unwrap(),
            "sha256:arm64digest"
        );
    }

    #[test]
    fn accepts_aarch64_as_a_spelling_of_arm64() {
        let doc = r#"{"manifests":[
            {"digest":"sha256:a","platform":{"architecture":"aarch64","os":"linux"}}
        ]}"#;
        assert_eq!(select_platform_digest(doc).unwrap(), "sha256:a");
    }

    #[test]
    fn arch_and_os_must_belong_to_the_same_entry() {
        // The failure this guards against: an amd64/linux entry and an
        // arm64/plan9 entry between them satisfy "saw arm64" and "saw linux",
        // and a parser tracking them independently picks a digest that runs on
        // neither.
        let doc = r#"{"manifests":[
            {"digest":"sha256:amd","platform":{"architecture":"amd64","os":"linux"}},
            {"digest":"sha256:wrongos","platform":{"architecture":"arm64","os":"plan9"}}
        ]}"#;
        assert!(select_platform_digest(doc).is_err());
    }

    #[test]
    fn digest_before_platform_still_matches() {
        // Field order inside an entry is not guaranteed — OCI indexes usually
        // put `digest` first, Docker's often puts `platform` first.
        let doc = r#"{"manifests":[
            {"platform":{"architecture":"amd64","os":"linux"},"digest":"sha256:amd"},
            {"platform":{"architecture":"arm64","os":"linux"},"digest":"sha256:arm"}
        ]}"#;
        assert_eq!(select_platform_digest(doc).unwrap(), "sha256:arm");
    }

    #[test]
    fn skips_attestation_entries() {
        let doc = r#"{"manifests":[
            {"digest":"sha256:att","platform":{"architecture":"unknown","os":"unknown"}}
        ]}"#;
        assert!(select_platform_digest(doc).is_err());
    }

    #[test]
    fn reports_a_list_with_no_arm64() {
        let doc = r#"{"manifests":[
            {"digest":"sha256:amd","platform":{"architecture":"amd64","os":"linux"}}
        ]}"#;
        assert_eq!(
            select_platform_digest(doc).unwrap_err(),
            "no linux/arm64 manifest found in manifest list"
        );
    }

    #[test]
    fn parses_config_and_layers_in_order() {
        let m = parse_manifest(MANIFEST).unwrap();
        assert_eq!(m.config_digest, "sha256:configdigest");
        assert_eq!(m.layer_digests, ["sha256:layer0", "sha256:layer1"]);
    }

    #[test]
    fn config_digest_is_not_confused_with_a_layer_digest() {
        // Both objects have a `digest` member; only the path tells them apart.
        let m = parse_manifest(MANIFEST).unwrap();
        assert!(!m.layer_digests.contains(&m.config_digest));
    }

    #[test]
    fn rejects_a_manifest_with_no_layers() {
        let doc = r#"{"config":{"digest":"sha256:c"},"layers":[]}"#;
        assert_eq!(parse_manifest(doc).unwrap_err(), "no layers in manifest");
    }

    #[test]
    fn rejects_a_manifest_with_no_config() {
        let doc = r#"{"layers":[{"digest":"sha256:l"}]}"#;
        assert_eq!(
            parse_manifest(doc).unwrap_err(),
            "no digest in manifest config"
        );
    }

    #[test]
    fn reads_layer_sizes() {
        assert_eq!(layer_size(MANIFEST, 0), Some(2_295_859));
        assert_eq!(layer_size(MANIFEST, 1), Some(128));
        assert_eq!(layer_size(MANIFEST, 2), None);
    }
}
