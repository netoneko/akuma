use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libakuma::{open, close, read_fd, write_fd, open_flags, mkdir_p, read_dir};

const IMAGES_BASE: &str = "/var/lib/box/images";

pub fn image_dir(name: &str) -> String {
    format!("{}/{}", IMAGES_BASE, name)
}

pub fn rootfs_dir(name: &str) -> String {
    format!("{}/{}/rootfs", IMAGES_BASE, name)
}

pub fn config_path(name: &str) -> String {
    format!("{}/{}/oci-config.json", IMAGES_BASE, name)
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
}

pub fn prepare_image_dir(name: &str) -> Result<(), String> {
    ensure_base_dir();
    let rootfs = rootfs_dir(name);
    if !mkdir_p(&rootfs) {
        return Err(format!("failed to create {}", rootfs));
    }
    Ok(())
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
