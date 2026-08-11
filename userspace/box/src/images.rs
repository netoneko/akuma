use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libakuma::{open, close, read_fd, write_fd, open_flags, mkdir_p, read_dir};

const IMAGES_BASE: &str = "/var/lib/box/images";
/// Extracted layers, keyed by digest and shared across every image that
/// references them. An image directory holds only metadata.
const LAYERS_BASE: &str = "/var/lib/box/layers";
/// Where per-container writable upper directories live.
const CONTAINERS_BASE: &str = "/var/lib/box/containers";

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

pub fn sanitize_name(image_str: &str) -> String {
    let mut s = image_str;
    if let Some(pos) = s.find('/') {
        if s[..pos].contains('.') {
            s = &s[pos + 1..];
        }
    }
    if let Some(rest) = s.strip_prefix("library/") {
        s = rest;
    }
    s.replace('/', "-").replace(':', "-")
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

/// Record the image's layers, base-first, as the registry ordered them.
pub fn save_layers(name: &str, digests: &[String]) -> Result<(), String> {
    let mut body = String::new();
    for d in digests {
        body.push_str(d);
        body.push('\n');
    }
    let path = layers_path(name);
    let fd = open(&path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return Err(format!("failed to write {}", path));
    }
    write_fd(fd, body.as_bytes());
    close(fd);
    Ok(())
}

/// The image's layer digests, base-first.
pub fn load_layers(name: &str) -> Vec<String> {
    let path = layers_path(name);
    let fd = open(&path, open_flags::O_RDONLY);
    if fd < 0 {
        return Vec::new();
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

    core::str::from_utf8(&buf).map_or_else(
        |_| Vec::new(),
        |s| s.lines().filter(|l| !l.trim().is_empty()).map(String::from).collect(),
    )
}

/// The image's layer directories in overlay lookup order — **topmost-first**,
/// i.e. the reverse of the order the registry applies them.
pub fn overlay_lowerdirs(name: &str) -> Vec<String> {
    let mut dirs: Vec<String> = load_layers(name).iter().map(|d| layer_dir(d)).collect();
    dirs.reverse();
    dirs
}

pub fn save_config(name: &str, config_json: &str) -> Result<(), String> {
    let path = config_path(name);
    let fd = open(&path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return Err(format!("failed to write {}", path));
    }
    write_fd(fd, config_json.as_bytes());
    close(fd);
    Ok(())
}

pub fn load_config(name: &str) -> Option<String> {
    let path = config_path(name);
    let fd = open(&path, open_flags::O_RDONLY);
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

pub fn list_images() -> Vec<String> {
    let mut names = Vec::new();
    if let Some(entries) = read_dir(IMAGES_BASE) {
        for entry in entries {
            if entry.is_dir {
                let cfg = format!("{}/{}/oci-config.json", IMAGES_BASE, entry.name);
                let fd = open(&cfg, open_flags::O_RDONLY);
                if fd >= 0 {
                    close(fd);
                    names.push(String::from(entry.name));
                }
            }
        }
    }
    names
}

pub fn image_exists(name: &str) -> bool {
    let cfg = config_path(name);
    let fd = open(&cfg, open_flags::O_RDONLY);
    if fd >= 0 {
        close(fd);
        true
    } else {
        false
    }
}
