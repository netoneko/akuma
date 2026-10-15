//! `box save`, `box tag`, `box rmi` and the layer garbage collector.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libakuma::{close, exit, open, open_flags, print, println, read_fd};

use akuma_tar::write::{encode_header, padding_for, Kind, WriteEntry, END_OF_ARCHIVE};
use boxlib::imgconfig::{blob_entry, docker_manifest_json, index_json, manifest_json, with_default_tag};
use boxlib::layer::sha256_digest;
use boxlib::paths;

use crate::fsx::{self, CHUNK};
use crate::images;

struct TarSink {
    fd: i32,
    mtime: u64,
}

impl TarSink {
    fn emit(&self, path: &str, kind: Kind, size: u64) -> Result<(), String> {
        let mode = if kind == Kind::Directory { 0o755 } else { 0o644 };
        let e = WriteEntry { path, linkname: "", kind, size, mode, uid: 0, gid: 0, mtime: self.mtime };
        let mut hdr = Vec::new();
        encode_header(&e, &mut hdr);
        self.write(&hdr)
    }

    fn write(&self, data: &[u8]) -> Result<(), String> {
        if fsx::write_all(self.fd, data) {
            Ok(())
        } else {
            Err(String::from("write error"))
        }
    }

    fn dir(&self, path: &str) -> Result<(), String> {
        self.emit(path, Kind::Directory, 0)
    }

    fn bytes(&self, path: &str, data: &[u8]) -> Result<(), String> {
        self.emit(path, Kind::File, data.len() as u64)?;
        self.write(data)?;
        self.write(&alloc::vec![0u8; padding_for(data.len() as u64)])
    }

    fn file(&self, path: &str, src: &str, size: u64) -> Result<(), String> {
        self.emit(path, Kind::File, size)?;
        let fd = open(src, open_flags::O_RDONLY);
        if fd < 0 {
            return Err(format!("cannot open {src}"));
        }
        let mut buf = alloc::vec![0u8; CHUNK];
        let mut left = size;
        while left > 0 {
            let n = read_fd(fd, &mut buf[..left.min(CHUNK as u64) as usize]);
            if n <= 0 {
                close(fd);
                return Err(format!("{src} is shorter than its recorded size"));
            }
            self.write(&buf[..n as usize])?;
            left -= n as u64;
        }
        close(fd);
        self.write(&alloc::vec![0u8; padding_for(size)])
    }
}

/// `box save [-o file] <image>` — an OCI image layout (with Docker's legacy
/// `manifest.json` alongside), the format `docker load` reads.
pub fn cmd_save(args: libakuma::Args) -> ! {
    let argv: Vec<&str> = args.collect();
    let mut out: Option<&str> = None;
    let mut image: Option<&str> = None;
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "-o" || argv[i] == "--output" {
            i += 1;
            out = argv.get(i).copied();
        } else {
            image = Some(argv[i]);
        }
        i += 1;
    }
    let Some(image) = image else {
        print("Usage: box save [-o file.tar] <image>\n");
        exit(1);
    };
    match save(image, out) {
        Ok(()) => exit(0),
        Err(e) => {
            print("box save: ");
            println(&e);
            exit(1);
        }
    }
}

fn save(image: &str, out: Option<&str>) -> Result<(), String> {
    let store = images::sanitize_name(image);
    if !images::image_exists(&store) {
        return Err(format!("image '{image}' not found"));
    }
    let config = images::load_config(&store).ok_or("unreadable config")?;
    let layers = images::load_layers(&store);
    let mut sized: Vec<(String, u64)> = Vec::new();
    for d in &layers {
        let n = images::blob_size(d).ok_or_else(|| {
            format!("layer {d} has no stored blob (the image was pulled before blobs were kept) — re-pull it")
        })?;
        sized.push((d.clone(), n));
    }
    let config_digest = sha256_digest(config.as_bytes());
    let manifest = manifest_json(&config_digest, config.len() as u64, &sized);
    let manifest_digest = sha256_digest(manifest.as_bytes());
    let reference = with_default_tag(image);

    let fd = match out {
        Some(path) if path != "-" => {
            let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
            if fd < 0 {
                return Err(format!("cannot create {path}"));
            }
            fd
        }
        _ => 1,
    };
    let sink = TarSink { fd, mtime: fsx::now_secs() };
    let result = (|| -> Result<(), String> {
        sink.bytes("oci-layout", b"{\"imageLayoutVersion\":\"1.0.0\"}")?;
        sink.bytes("index.json", index_json(&manifest_digest, manifest.len() as u64, &reference).as_bytes())?;
        sink.bytes(
            "manifest.json",
            docker_manifest_json(&config_digest, &reference, &layers).as_bytes(),
        )?;
        sink.dir("blobs")?;
        sink.dir("blobs/sha256")?;
        sink.bytes(&blob_entry(&config_digest), config.as_bytes())?;
        sink.bytes(&blob_entry(&manifest_digest), manifest.as_bytes())?;
        for (d, n) in &sized {
            sink.file(&blob_entry(d), &images::blob_path(d), *n)?;
        }
        sink.write(&END_OF_ARCHIVE)
    })();
    if fd != 1 {
        close(fd);
    }
    result
}

/// `box tag <source> <target>` — a second name for the same layers.
pub fn cmd_tag(args: libakuma::Args) -> ! {
    let argv: Vec<&str> = args.collect();
    let [src, dst] = argv.as_slice() else {
        print("Usage: box tag <source> <target>\n");
        exit(1);
    };
    let (s, d) = (images::sanitize_name(src), images::sanitize_name(dst));
    let Some(config) = images::load_config(&s) else {
        print("box tag: no such image: ");
        println(src);
        exit(1);
    };
    let layers = images::load_layers(&s);
    if let Err(e) = images::prepare_image_dir(&d)
        .and_then(|()| images::save_layers(&d, &layers))
        .and_then(|()| {
            if let Some(m) = fsx::read_file(&images::manifest_path(&s)) {
                fsx::write_file(&images::manifest_path(&d), &m)?;
            }
            images::save_config(&d, &config)
        })
    {
        print("box tag: ");
        println(&e);
        exit(1);
    }
    exit(0);
}

/// `box rmi <image>…` — drop the names, then collect layers nothing uses.
pub fn cmd_rmi(args: libakuma::Args) -> ! {
    let argv: Vec<&str> = args.collect();
    if argv.is_empty() {
        print("Usage: box rmi <image>...\n");
        exit(1);
    }
    let mut status = 0;
    for a in &argv {
        let store = images::sanitize_name(a);
        if images::image_exists(&store) {
            images::remove_image_dir(&store);
            print("Untagged: ");
            println(a);
        } else {
            print("box rmi: no such image: ");
            println(a);
            status = 1;
        }
    }
    gc();
    exit(status);
}

/// `box prune` — collect unreferenced layers, blobs and cache entries.
pub fn cmd_prune() -> ! {
    gc();
    exit(0);
}

/// Remove every layer directory and blob no image lists, then every cache entry
/// pointing at one that is gone. Build-cache intermediates are unreferenced by
/// design, so this also discards the cache they backed. Not safe to run while a
/// `box build` is in flight.
pub fn gc() {
    let mut referenced: Vec<String> = Vec::new();
    for name in images::list_images() {
        referenced.extend(images::load_layers(&name));
    }
    let keep = |entry: &str| referenced.iter().any(|d| d.replace(':', "-") == entry);

    let (mut layers, mut blobs, mut cache) = (0, 0, 0);
    for (name, _) in fsx::list_sorted(paths::LAYERS_BASE) {
        if !keep(&name) {
            fsx::remove_tree(&format!("{}/{}", paths::LAYERS_BASE, name));
            layers += 1;
        }
    }
    for (name, _) in fsx::list_sorted(paths::BLOBS_BASE) {
        if !name.starts_with("tmp-") && !keep(&name) {
            libakuma::unlink(&format!("{}/{}", paths::BLOBS_BASE, name));
            blobs += 1;
        }
    }
    for (name, _) in fsx::list_sorted(paths::CACHE_BASE) {
        let path = format!("{}/{}", paths::CACHE_BASE, name);
        let live = fsx::read_text(&path)
            .and_then(|r| r.split_whitespace().next().map(String::from))
            .is_some_and(|d| fsx::exists(&images::layer_dir(&d)));
        if !live {
            libakuma::unlink(&path);
            cache += 1;
        }
    }
    print("Removed ");
    libakuma::print_dec(layers);
    print(" layers, ");
    libakuma::print_dec(blobs);
    print(" blobs, ");
    libakuma::print_dec(cache);
    println(" cache entries");
}
