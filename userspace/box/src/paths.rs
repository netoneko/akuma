//! Where `box` keeps things on disk, and the naming rules that get it there.
//!
//! ```text
//! /var/lib/box/images/<image>/oci-config.json   what to run
//! /var/lib/box/images/<image>/layers            digests, base-first
//! /var/lib/box/layers/sha256-<hex>/             extracted layer, shared
//! /var/lib/box/containers/<id>/upper/           one container's writes
//! /var/lib/box/containers/<id>/runtime/         resolv.conf + hosts, as the topmost lower
//! /var/lib/box/containers/<id>/image            store name of the image it was started from
//! /var/lib/box/blobs/sha256-<hex>               the compressed layer, exactly as the manifest names it
//! /var/lib/box/cache/<key>                      build cache: step key -> layer record
//! /var/lib/box/images/<image>/manifest.json     the manifest `box save` exports
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

/// Compressed layer blobs, keyed by digest — what `box save`/`push` export.
pub const BLOBS_BASE: &str = "/var/lib/box/blobs";
/// `box build` step cache.
pub const CACHE_BASE: &str = "/var/lib/box/cache";

pub fn blob_path(digest: &str) -> String {
    format!("{}/{}", BLOBS_BASE, digest.replace(':', "-"))
}

pub fn cache_path(key: &str) -> String {
    format!("{}/{}", CACHE_BASE, key)
}

pub fn manifest_path(name: &str) -> String {
    format!("{}/{}/manifest.json", IMAGES_BASE, name)
}

/// Per-container directory holding the files `box` injects (`/etc/resolv.conf`,
/// `/etc/hosts`). It is the overlay's topmost **lower**, not part of the upper,
/// so injected files never end up in a committed layer unless a process in the
/// container actually rewrote them (which copies them up).
pub fn container_runtime(id: &str) -> String {
    format!("{}/{}/runtime", CONTAINERS_BASE, id)
}

/// Records which image a container was started from, for `box commit`.
pub fn container_image_file(id: &str) -> String {
    format!("{}/{}/image", CONTAINERS_BASE, id)
}

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
/// Docker Hub's three spellings of itself (`docker.io`, `index.docker.io`,
/// `registry-1.docker.io`) and its implicit `library/` namespace are dropped, so
/// `busybox`, `library/busybox` and `docker.io/library/busybox` are one image.
/// **Every other registry host stays in the name**: `ghcr.io/me/app:1` and
/// `app:1` are different names, as in Docker.
///
/// Until 2026-10-16 any host containing a `.` was dropped, which made
/// `192.168.1.5:5000/app:1` — the name `box push` needs a tag for — the *same*
/// store directory as `app:1`, so `box rmi` of the registry name deleted the
/// image it was tagged from. (Layers and blobs are shared by digest either way;
/// what was being deduplicated was a config and a manifest.) An image stored
/// under a dropped-host name before then is not found under its new one:
/// `box pull` it again — the layers are already on disk, so only the manifest
/// and config are fetched.
///
/// Everything left is flattened into a single path component.
pub fn sanitize_name(image_str: &str) -> String {
    const HUB_HOSTS: [&str; 3] = ["docker.io", "index.docker.io", "registry-1.docker.io"];
    let mut s = image_str;
    // Same "looks like a host" test as `oci_ref::parse_image_ref`.
    let host = s.find('/').map(|pos| &s[..pos]).filter(|h| h.contains('.') || h.contains(':'));
    let on_hub = match host {
        None => true,
        Some(h) => HUB_HOSTS.contains(&h),
    };
    if let (Some(h), true) = (host, on_hub) {
        s = &s[h.len() + 1..];
    }
    if on_hub {
        if let Some(rest) = s.strip_prefix("library/") {
            s = rest;
        }
    }
    let mut out: String = s
        .chars()
        .map(|c| if c == '/' || c == ':' { '-' } else { c })
        .collect();
    // The result is a directory name under `IMAGES_BASE`. `.` and `..` (and
    // any dot-led name) are not image names Docker accepts, and `..` made
    // `box rmi ..` remove the whole of `/var/lib/box`. A leading dot becomes
    // `_`, and an empty name is `_` too, so the result is always one ordinary
    // path component.
    if out.starts_with('.') {
        out.replace_range(..1, "_");
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// The name `box run` gives a container it was not given one for:
/// `<store>-<stamp>`, with anything [`valid_container_name`] would refuse
/// mapped to `-` (a digest reference's `@`) and a `c` in front of a name that
/// does not start with a letter or digit (a store name sanitized from `.x`).
#[must_use]
pub fn default_container_name(store: &str, stamp: u64) -> String {
    let mut name: String = store
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '-' })
        .collect();
    if !name.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric) {
        name.insert(0, 'c');
    }
    name.truncate(100);
    alloc::format!("{name}-{stamp}")
}

/// Whether `name` may name a container (`box run --name`, `box commit`).
///
/// Docker's rule, `[a-zA-Z0-9][a-zA-Z0-9_.-]*`, capped at 128 bytes. The
/// name is a directory under `CONTAINERS_BASE`, and `box run --rm` deletes
/// that directory when the container ends: an unchecked `--name ../../..`
/// pointed the delete at `/`.
#[must_use]
pub fn valid_container_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
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
    fn build_store_paths() {
        assert_eq!(blob_path("sha256:ab"), "/var/lib/box/blobs/sha256-ab");
        assert_eq!(cache_path("k"), "/var/lib/box/cache/k");
        assert_eq!(manifest_path("x"), "/var/lib/box/images/x/manifest.json");
        assert_eq!(container_runtime("web"), "/var/lib/box/containers/web/runtime");
        assert_eq!(container_image_file("web"), "/var/lib/box/containers/web/image");
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
    fn user_namespaces_are_flattened_and_hub_aliases_collapse() {
        assert_eq!(sanitize_name("myuser/myapp:v1"), "myuser-myapp-v1");
        assert_eq!(sanitize_name("docker.io/myuser/myapp:v1"), "myuser-myapp-v1");
        assert_eq!(sanitize_name("registry-1.docker.io/library/busybox"), "busybox");
        assert_eq!(sanitize_name("index.docker.io/library/busybox"), "busybox");
    }

    /// The bug: `box tag app:1 <registry>/app:1` then `box rmi` of the registry
    /// name deleted `app:1`, because both were the store directory `app-1`.
    #[test]
    fn another_registry_is_a_different_image_from_the_hub_one() {
        assert_eq!(sanitize_name("ghcr.io/owner/repo:v2"), "ghcr.io-owner-repo-v2");
        assert_eq!(sanitize_name("192.168.1.5:5000/app:1"), "192.168.1.5-5000-app-1");
        assert_ne!(sanitize_name("192.168.1.5:5000/app:1"), sanitize_name("app:1"));
        assert_ne!(sanitize_name("ghcr.io/me/app:1"), sanitize_name("quay.io/me/app:1"));
        assert_ne!(sanitize_name("ghcr.io/me/app:1"), sanitize_name("me/app:1"));
        // `library/` is a Hub namespace; elsewhere it is just a path component.
        assert_eq!(sanitize_name("ghcr.io/library/x"), "ghcr.io-library-x");
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

    #[test]
    fn store_names_are_one_ordinary_component() {
        assert_eq!(sanitize_name(".."), "_.");
        assert_eq!(sanitize_name("."), "_");
        assert_eq!(sanitize_name(".hidden"), "_hidden");
        assert_eq!(sanitize_name(""), "_");
        assert_eq!(sanitize_name("../../x"), "_.-..-x");
        assert_eq!(sanitize_name("busybox"), "busybox");
    }

    #[test]
    fn container_names_follow_dockers_rule() {
        for ok in ["web", "busybox-123456", "a.b_c-d", "0"] {
            assert!(valid_container_name(ok), "{ok}");
        }
        for bad in ["", "..", ".", "../../..", "a/b", "-rm", "--rm", "_x", ".x", "a b", "é"] {
            assert!(!valid_container_name(bad), "{bad:?}");
        }
        assert!(!valid_container_name(&"a".repeat(129)));
    }

    #[test]
    fn default_container_names_are_valid() {
        assert_eq!(default_container_name("busybox", 7), "busybox-7");
        assert_eq!(default_container_name("alpine@sha256-ab", 1), "alpine-sha256-ab-1");
        assert_eq!(default_container_name("_hidden", 2), "c_hidden-2");
        for store in ["busybox", "alpine@sha256-ab", "_x", "", "ghcr.io-a-b-1.0", &"x".repeat(300)] {
            assert!(valid_container_name(&default_container_name(store, u64::MAX)), "{store}");
        }
    }
}
