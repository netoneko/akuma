//! Where `box` keeps things on disk, and the naming rules that get it there.
//!
//! ```text
//! /var/lib/box/images/<image>/oci-config.json   what to run
//! /var/lib/box/images/<image>/layers            digests, base-first
//! /var/lib/box/layers/sha256-<hex>/             extracted layer, shared
//! /var/lib/box/containers/<id>/upper/           one container's writes
//! ```
//!
//! Layers are keyed by digest rather than by image because a digest is what a
//! layer *is*: two images naming the same digest name byte-identical content,
//! so the extraction is done once and shared read-only. Only the container's
//! upper directory is writable.
//!
//! Pure string work — `images.rs` owns the I/O that acts on these paths.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const IMAGES_BASE: &str = "/var/lib/box/images";
/// Extracted layers, keyed by digest and shared across every image that
/// references them. An image directory holds only metadata.
pub const LAYERS_BASE: &str = "/var/lib/box/layers";
/// Where per-container writable upper directories live.
pub const CONTAINERS_BASE: &str = "/var/lib/box/containers";

pub fn image_dir(name: &str) -> String {
    format!("{}/{}", IMAGES_BASE, name)
}

pub fn rootfs_dir(name: &str) -> String {
    format!("{}/{}/rootfs", IMAGES_BASE, name)
}

pub fn config_path(name: &str) -> String {
    format!("{}/{}/oci-config.json", IMAGES_BASE, name)
}

pub fn layers_path(name: &str) -> String {
    format!("{}/{}/layers", IMAGES_BASE, name)
}

/// `sha256:abc…` → `/var/lib/box/layers/sha256-abc…`. A digest is already
/// filename-safe apart from its separator.
pub fn layer_dir(digest: &str) -> String {
    format!("{}/{}", LAYERS_BASE, digest.replace(':', "-"))
}

pub fn container_dir(id: &str) -> String {
    format!("{}/{}", CONTAINERS_BASE, id)
}

pub fn container_upper(id: &str) -> String {
    format!("{}/{}/upper", CONTAINERS_BASE, id)
}

/// The directory name an image reference is stored under.
///
/// Drops the registry host and Docker's implicit `library/` namespace, so
/// `busybox`, `library/busybox` and `docker.io/library/busybox` are one image
/// rather than three copies of it. Everything left is flattened into a single
/// path component.
pub fn sanitize_name(image_str: &str) -> String {
    let mut s = image_str;
    if let Some(pos) = s.find('/') {
        // Same "looks like a host" test as reference parsing, minus the port
        // case: `localhost:5000/img` keeps its host in the store name, which is
        // ugly but consistent — `pull` and `run` sanitize identically, so they
        // always agree on where the image lives.
        if s[..pos].contains('.') {
            s = &s[pos + 1..];
        }
    }
    if let Some(rest) = s.strip_prefix("library/") {
        s = rest;
    }
    s.chars()
        .map(|c| if c == '/' || c == ':' { '-' } else { c })
        .collect()
}

/// The `layers` file's contents: one digest per line, base-first, as the
/// registry ordered them.
pub fn format_layers(digests: &[String]) -> String {
    let mut body = String::new();
    for d in digests {
        body.push_str(d);
        body.push('\n');
    }
    body
}

/// Read back what [`format_layers`] wrote, ignoring blank lines.
pub fn parse_layers(body: &str) -> Vec<String> {
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect()
}

/// Layer directories in overlay lookup order — **topmost-first**, i.e. the
/// reverse of the order the registry applies them.
///
/// Getting this backwards is silent: the image still boots, but a file replaced
/// by a later layer resolves to the older version underneath it.
pub fn lowerdirs(digests: &[String]) -> Vec<String> {
    let mut dirs: Vec<String> = digests.iter().map(|d| layer_dir(d)).collect();
    dirs.reverse();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn image_paths_hang_off_the_image_directory() {
        assert_eq!(image_dir("busybox"), "/var/lib/box/images/busybox");
        assert_eq!(rootfs_dir("busybox"), "/var/lib/box/images/busybox/rootfs");
        assert_eq!(
            config_path("busybox"),
            "/var/lib/box/images/busybox/oci-config.json"
        );
        assert_eq!(layers_path("busybox"), "/var/lib/box/images/busybox/layers");
    }

    #[test]
    fn layer_directories_are_keyed_by_digest() {
        assert_eq!(
            layer_dir("sha256:b85757a5ca1a"),
            "/var/lib/box/layers/sha256-b85757a5ca1a"
        );
    }

    #[test]
    fn container_paths_are_separate_from_image_paths() {
        assert_eq!(container_dir("web"), "/var/lib/box/containers/web");
        assert_eq!(container_upper("web"), "/var/lib/box/containers/web/upper");
        // A container must never be able to write inside an image directory.
        assert!(!container_dir("web").starts_with(IMAGES_BASE));
    }

    #[test]
    fn equivalent_references_share_one_store_name() {
        assert_eq!(sanitize_name("busybox"), "busybox");
        assert_eq!(sanitize_name("library/busybox"), "busybox");
        assert_eq!(sanitize_name("docker.io/library/busybox"), "busybox");
    }

    #[test]
    fn tags_become_part_of_the_store_name() {
        assert_eq!(sanitize_name("alpine:3.19"), "alpine-3.19");
        assert_eq!(sanitize_name("docker.io/library/alpine:3.19"), "alpine-3.19");
    }

    #[test]
    fn user_and_registry_namespaces_are_flattened() {
        assert_eq!(sanitize_name("myuser/myapp:v1"), "myuser-myapp-v1");
        assert_eq!(sanitize_name("ghcr.io/owner/repo:v2"), "owner-repo-v2");
    }

    #[test]
    fn store_names_are_a_single_path_component() {
        for r in [
            "busybox",
            "myuser/myapp:v1",
            "ghcr.io/owner/repo:v2",
            "localhost:5000/img:dev",
        ] {
            let name = sanitize_name(r);
            assert!(!name.contains('/'), "{} → {}", r, name);
            assert!(!name.contains(':'), "{} → {}", r, name);
        }
    }

    #[test]
    fn layer_list_round_trips() {
        let digests = vec![String::from("sha256:a"), String::from("sha256:b")];
        assert_eq!(format_layers(&digests), "sha256:a\nsha256:b\n");
        assert_eq!(parse_layers(&format_layers(&digests)), digests);
    }

    #[test]
    fn layer_list_tolerates_blank_lines() {
        assert_eq!(parse_layers("sha256:a\n\n\nsha256:b\n"), ["sha256:a", "sha256:b"]);
        assert!(parse_layers("").is_empty());
        assert!(parse_layers("\n \n").is_empty());
    }

    #[test]
    fn overlay_order_is_the_reverse_of_registry_order() {
        // The registry lists base first; the overlay looks up topmost first.
        let digests = vec![
            String::from("sha256:base"),
            String::from("sha256:middle"),
            String::from("sha256:top"),
        ];
        assert_eq!(
            lowerdirs(&digests),
            [
                "/var/lib/box/layers/sha256-top",
                "/var/lib/box/layers/sha256-middle",
                "/var/lib/box/layers/sha256-base",
            ]
        );
    }

    #[test]
    fn a_single_layer_image_needs_no_reordering() {
        let digests = vec![String::from("sha256:only")];
        assert_eq!(lowerdirs(&digests), ["/var/lib/box/layers/sha256-only"]);
        assert!(lowerdirs(&[]).is_empty());
    }
}
