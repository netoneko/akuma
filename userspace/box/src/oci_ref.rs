//! Image references — `busybox`, `ghcr.io/owner/repo:sha-abc` — and the
//! registry URLs they name.
//!
//! Pure string work, split out of `oci.rs` so it can be host-tested; `oci.rs`
//! keeps the HTTPS side. The grammar implemented here is the subset of the
//! distribution spec that `box pull` accepts: `[registry[:port]/]name[:tag]`,
//! with no digest references (`name@sha256:…`) and no `latest`-is-not-a-tag
//! subtleties.

use alloc::format;
use alloc::string::String;

/// Docker Hub's v2 API host. `docker.io` is the name people type; it is not the
/// host that serves the registry API, so it is rewritten on the way in.
pub const DOCKER_HUB: &str = "registry-1.docker.io";

pub struct ImageRef {
    pub registry: String,
    pub name: String,
    pub tag: String,
}

/// Split a reference into registry, repository and tag, applying Docker's two
/// defaults: an unqualified name is a Docker Hub official image
/// (`library/<name>`), and a missing tag is `latest`.
///
/// The first path component is a registry only if it looks like a host — it
/// contains a `.` or a `:`. That is the same heuristic Docker uses, and it is
/// what keeps `myuser/myapp` a Hub repository rather than a host named
/// `myuser`.
pub fn parse_image_ref(s: &str) -> ImageRef {
    let (name_part, tag) = match s.rfind(':') {
        Some(pos) => {
            let after = &s[pos + 1..];
            // A colon before a `/` is a port, not a tag separator:
            // `localhost:5000/img` has no tag.
            if after.contains('/') {
                (s, "latest")
            } else {
                (&s[..pos], after)
            }
        }
        None => (s, "latest"),
    };

    let (registry, name) = if let Some(slash_pos) = name_part.find('/') {
        let first = &name_part[..slash_pos];
        if first.contains('.') || first.contains(':') {
            let reg = if first == "docker.io" { DOCKER_HUB } else { first };
            (String::from(reg), String::from(&name_part[slash_pos + 1..]))
        } else {
            (String::from(DOCKER_HUB), String::from(name_part))
        }
    } else {
        (String::from(DOCKER_HUB), format!("library/{}", name_part))
    };

    ImageRef {
        registry,
        name,
        tag: String::from(tag),
    }
}

impl ImageRef {
    /// Docker Hub is the only registry `box` authenticates against; anything
    /// else is pulled anonymously.
    pub fn needs_token(&self) -> bool {
        self.registry == DOCKER_HUB
    }

    /// Docker Hub's anonymous pull-token endpoint. Scoped to this repository
    /// only — the token a registry hands back is not reusable across images.
    pub fn token_url(&self) -> String {
        format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
            self.name
        )
    }

    /// `reference` is a tag *or* a digest — the v2 manifests endpoint takes
    /// either, which is how a manifest list is resolved to one platform.
    pub fn manifest_url(&self, reference: &str) -> String {
        format!(
            "https://{}/v2/{}/manifests/{}",
            self.registry, self.name, reference
        )
    }

    pub fn blob_url(&self, digest: &str) -> String {
        format!("https://{}/v2/{}/blobs/{}", self.registry, self.name, digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(s: &str) -> (String, String, String) {
        let r = parse_image_ref(s);
        (r.registry, r.name, r.tag)
    }

    #[test]
    fn bare_name_is_a_hub_official_image() {
        assert_eq!(
            parts("busybox"),
            (DOCKER_HUB.into(), "library/busybox".into(), "latest".into())
        );
    }

    #[test]
    fn tag_is_taken_from_the_last_colon() {
        assert_eq!(
            parts("ubuntu:22.04"),
            (DOCKER_HUB.into(), "library/ubuntu".into(), "22.04".into())
        );
    }

    #[test]
    fn user_namespace_stays_on_the_hub() {
        assert_eq!(
            parts("myuser/myapp:v1"),
            (DOCKER_HUB.into(), "myuser/myapp".into(), "v1".into())
        );
    }

    #[test]
    fn dotted_first_component_is_a_registry() {
        assert_eq!(
            parts("ghcr.io/owner/repo:sha-abc"),
            ("ghcr.io".into(), "owner/repo".into(), "sha-abc".into())
        );
    }

    #[test]
    fn registry_without_a_tag_defaults_to_latest() {
        assert_eq!(
            parts("ghcr.io/owner/repo"),
            ("ghcr.io".into(), "owner/repo".into(), "latest".into())
        );
    }

    #[test]
    fn docker_io_is_rewritten_to_the_api_host() {
        assert_eq!(
            parts("docker.io/library/alpine:3.19"),
            (DOCKER_HUB.into(), "library/alpine".into(), "3.19".into())
        );
    }

    #[test]
    fn port_is_not_mistaken_for_a_tag() {
        assert_eq!(
            parts("localhost:5000/myimage:dev"),
            ("localhost:5000".into(), "myimage".into(), "dev".into())
        );
        assert_eq!(
            parts("localhost:5000/myimage"),
            ("localhost:5000".into(), "myimage".into(), "latest".into())
        );
    }

    #[test]
    fn only_the_hub_gets_a_token() {
        assert!(parse_image_ref("busybox").needs_token());
        assert!(parse_image_ref("docker.io/library/busybox").needs_token());
        assert!(!parse_image_ref("ghcr.io/owner/repo").needs_token());
    }

    #[test]
    fn builds_registry_urls() {
        let r = parse_image_ref("busybox");
        assert_eq!(
            r.token_url(),
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/busybox:pull"
        );
        assert_eq!(
            r.manifest_url(&r.tag),
            "https://registry-1.docker.io/v2/library/busybox/manifests/latest"
        );
        assert_eq!(
            r.manifest_url("sha256:abc"),
            "https://registry-1.docker.io/v2/library/busybox/manifests/sha256:abc"
        );
        assert_eq!(
            r.blob_url("sha256:def"),
            "https://registry-1.docker.io/v2/library/busybox/blobs/sha256:def"
        );
    }

    #[test]
    fn third_party_registry_urls_keep_their_host() {
        let r = parse_image_ref("ghcr.io/owner/repo:v2");
        assert_eq!(
            r.manifest_url(&r.tag),
            "https://ghcr.io/v2/owner/repo/manifests/v2"
        );
    }
}
