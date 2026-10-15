//! The on-disk image store: creating it, reading it, writing to it.
//!
//! Path and name rules live in `boxlib::paths` (host-tested); this file is the
//! I/O against them and is re-exported flat so callers say `images::layer_dir`
//! whether the work is a `format!` or a syscall.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libakuma::{open, close, read_fd, write_fd, open_flags, mkdir_p, read_dir};

pub use boxlib::paths::{
    config_path, container_dir, container_image_file, container_runtime, container_upper, image_dir, layer_dir, layers_path, rootfs_dir,
    sanitize_name, IMAGES_BASE,
};
use boxlib::paths::{self, BLOBS_BASE, CACHE_BASE, CONTAINERS_BASE, LAYERS_BASE};

/// Whether anything exists at `path` — file, directory or symlink.
///
/// Without following a final symlink: `/bin/cat` in an Alpine layer is
/// `-> /bin/busybox`, an absolute target that means the *container's* `/bin`,
/// so following it from the host finds nothing and a bare `CMD ["cat"]`
/// failed to resolve.
pub fn path_exists(path: &str) -> bool {
    crate::fsx::lstat(path).is_ok()
}

pub fn dir_exists(path: &str) -> bool {
    match libakuma::fstatat(-100, path, 0) {
        Ok(st) => st.st_mode & 0o170_000 == 0o040_000,
        Err(_) => false,
    }
}

pub fn ensure_base_dir() {
    mkdir_p(IMAGES_BASE);
    mkdir_p(LAYERS_BASE);
    mkdir_p(CONTAINERS_BASE);
    mkdir_p(BLOBS_BASE);
    mkdir_p(CACHE_BASE);
}

pub fn prepare_image_dir(name: &str) -> Result<(), String> {
    ensure_base_dir();
    let dir = image_dir(name);
    if !mkdir_p(&dir) {
        return Err(format!("failed to create {}", dir));
    }
    Ok(())
}

/// Read a whole file, or `None` if it cannot be opened or is not UTF-8.
fn read_text(path: &str) -> Option<String> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return None;
    }
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = read_fd(fd, &mut tmp);
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n as usize]);
    }
    close(fd);
    core::str::from_utf8(&buf).ok().map(String::from)
}

fn write_text(path: &str, body: &str) -> Result<(), String> {
    let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return Err(format!("failed to write {}", path));
    }
    write_fd(fd, body.as_bytes());
    close(fd);
    Ok(())
}

/// Record the image's layers, base-first, as the registry ordered them.
pub fn save_layers(name: &str, digests: &[String]) -> Result<(), String> {
    write_text(&layers_path(name), &paths::format_layers(digests))
}

/// The image's layer digests, base-first.
pub fn load_layers(name: &str) -> Vec<String> {
    read_text(&layers_path(name)).map_or_else(Vec::new, |body| paths::parse_layers(&body))
}

/// The image's layer directories in overlay lookup order — topmost-first.
pub fn overlay_lowerdirs(name: &str) -> Vec<String> {
    paths::lowerdirs(&load_layers(name))
}

pub fn save_config(name: &str, config_json: &str) -> Result<(), String> {
    write_text(&config_path(name), config_json)
}

pub fn load_config(name: &str) -> Option<String> {
    read_text(&config_path(name))
}

pub fn list_images() -> Vec<String> {
    let mut names = Vec::new();
    if let Some(entries) = read_dir(IMAGES_BASE) {
        for entry in entries {
            if entry.is_dir && image_exists(&entry.name) {
                names.push(entry.name.clone());
            }
        }
    }
    names
}

/// An image counts as present once its config is readable — that is the last
/// thing `box pull` writes, so a half-finished pull never looks complete.
pub fn image_exists(name: &str) -> bool {
    let fd = open(&config_path(name), open_flags::O_RDONLY);
    if fd >= 0 {
        close(fd);
        true
    } else {
        false
    }
}

// ── Building: blobs, manifests, writing an image out ──────────────────────

pub use boxlib::paths::{blob_path, manifest_path};

/// Size of a stored compressed blob, if it is there.
pub fn blob_size(digest: &str) -> Option<u64> {
    crate::fsx::stat(&blob_path(digest)).ok().map(|st| st.st_size as u64)
}

/// The compressed size of each of an image's layers, base-first, from the
/// manifest we wrote (or the blob on disk). `None` for a layer we cannot size —
/// an image pulled before blobs were kept.
pub fn layer_sizes(name: &str, digests: &[String]) -> Vec<Option<u64>> {
    let manifest = read_text(&manifest_path(name));
    digests
        .iter()
        .enumerate()
        .map(|(i, d)| {
            blob_size(d).or_else(|| {
                manifest
                    .as_deref()
                    .and_then(|m| boxlib::manifest::layer_size(m, i))
                    .and_then(|n| u64::try_from(n).ok())
            })
        })
        .collect()
}

/// Publish an image: layers file, config, manifest — config last, because that
/// is what makes [`image_exists`] true (see its doc).
pub fn write_image(
    name: &str,
    config: &boxlib::imgconfig::ImageConfig,
    layers: &[String],
    sizes: &[Option<u64>],
) -> Result<(), String> {
    prepare_image_dir(name)?;
    let config_json = config.to_json();
    let config_digest = boxlib::layer::sha256_digest(config_json.as_bytes());
    let with_sizes: Vec<(String, u64)> = layers
        .iter()
        .zip(sizes)
        .map(|(d, s)| (d.clone(), s.unwrap_or(0)))
        .collect();
    let manifest = boxlib::imgconfig::manifest_json(&config_digest, config_json.len() as u64, &with_sizes);
    save_layers(name, layers)?;
    write_text(&manifest_path(name), &manifest)?;
    save_config(name, &config_json)
}

/// Remove an image's metadata (config first, so it stops existing atomically).
pub fn remove_image_dir(name: &str) {
    let dir = image_dir(name);
    libakuma::unlink(&config_path(name));
    crate::fsx::remove_tree(&dir);
}
