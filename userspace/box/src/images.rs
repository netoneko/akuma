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
    config_path, container_dir, container_upper, image_dir, layer_dir, layers_path, rootfs_dir,
    sanitize_name, IMAGES_BASE,
};
use boxlib::paths::{self, CONTAINERS_BASE, LAYERS_BASE};

/// Whether anything exists at `path` — file, directory or symlink.
pub fn path_exists(path: &str) -> bool {
    let path_c = format!("{}\0", path);
    libakuma::fstatat(-100, &path_c, 0).is_ok()
}

pub fn dir_exists(path: &str) -> bool {
    let path_c = format!("{}\0", path);
    match libakuma::fstatat(-100, &path_c, 0) {
        Ok(st) => st.st_mode & 0o170_000 == 0o040_000,
        Err(_) => false,
    }
}

pub fn ensure_base_dir() {
    mkdir_p(IMAGES_BASE);
    mkdir_p(LAYERS_BASE);
    mkdir_p(CONTAINERS_BASE);
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
