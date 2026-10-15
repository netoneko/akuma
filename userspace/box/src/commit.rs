//! `box commit` and the layer-commit primitive `box build` is made of.
//!
//! [`commit_upper`] turns a directory of changes into an OCI layer in one pass:
//! walk in sorted order, write each entry as a tar header + payload, and feed the
//! bytes through [`boxlib::layer::LayerWriter`], which hashes the plain tar
//! (diff_id), deflates it, and hashes the result (digest) without ever holding
//! the layer in memory. The overlay's upper directory already *is* a layer diff
//! — deletions are `.wh.<name>` files, opaque directories carry
//! `.wh..wh..opq` — so those entries are emitted as the ordinary files they are.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use libakuma::{close, exit, mkdir_p, open, open_flags, print, println, read_fd};

use akuma_tar::write::{encode_header, padding_for, Kind, WriteEntry, END_OF_ARCHIVE};
use boxlib::imgconfig::{rfc3339, ImageConfig};
use boxlib::layer::{Finished, LayerWriter};

use crate::fsx::{self, CHUNK, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use crate::images;

/// A committed layer.
#[derive(Debug, Clone)]
pub struct Committed {
    /// `sha256:` of the compressed blob — the layer's directory/blob key.
    pub digest: String,
    /// `sha256:` of the plain tar — what the config's `diff_ids` lists.
    pub diff_id: String,
    pub size: u64,
}

type Seen = BTreeMap<(u64, u64), String>;

/// Stream the tree under `root` as tar into `w`.
fn walk<F: FnMut(&[u8])>(w: &mut LayerWriter<F>, root: &str, rel: &str, seen: &mut Seen) -> Result<(), String> {
    let dir = if rel.is_empty() { String::from(root) } else { format!("{root}/{rel}") };
    for (name, _) in fsx::list_sorted(&dir) {
        let rel_path = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let full = format!("{root}/{rel_path}");
        let st = fsx::lstat(&full).map_err(|e| format!("stat {full}: errno {e}"))?;
        let mut e = WriteEntry {
            path: &rel_path,
            linkname: "",
            kind: Kind::File,
            size: 0,
            mode: st.st_mode & 0o7777,
            uid: st.st_uid,
            gid: st.st_gid,
            mtime: st.st_mtime.max(0) as u64,
        };
        let mut hdr = Vec::new();
        match st.st_mode & S_IFMT {
            S_IFDIR => {
                e.kind = Kind::Directory;
                encode_header(&e, &mut hdr);
                w.write(&hdr);
                walk(w, root, &rel_path, seen)?;
            }
            S_IFLNK => {
                let target = libakuma::readlink(&full).ok_or_else(|| format!("readlink {full} failed"))?;
                e.kind = Kind::Symlink;
                e.linkname = &target;
                encode_header(&e, &mut hdr);
                w.write(&hdr);
            }
            S_IFREG => {
                let key = (st.st_dev, st.st_ino);
                if fsx::nlink(&st) > 1 {
                    if let Some(first) = seen.get(&key) {
                        e.kind = Kind::HardLink;
                        e.linkname = first;
                        encode_header(&e, &mut hdr);
                        w.write(&hdr);
                        continue;
                    }
                    seen.insert(key, rel_path.clone());
                }
                let size = st.st_size.max(0) as u64;
                e.size = size;
                encode_header(&e, &mut hdr);
                w.write(&hdr);
                stream_file(w, &full, size)?;
            }
            _ => {
                print("box: skipping special file ");
                println(&full);
            }
        }
    }
    Ok(())
}

/// Exactly `size` payload bytes (zero-filled if the file shrank under us,
/// truncated if it grew — the header already promised `size`), then padding.
fn stream_file<F: FnMut(&[u8])>(w: &mut LayerWriter<F>, path: &str, size: u64) -> Result<(), String> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(format!("open {path}: errno {}", -fd));
    }
    let mut buf = alloc::vec![0u8; CHUNK];
    let mut left = size;
    while left > 0 {
        let want = left.min(CHUNK as u64) as usize;
        let n = read_fd(fd, &mut buf[..want]);
        let got = if n > 0 {
            n as usize
        } else {
            buf[..want].fill(0);
            want
        };
        w.write(&buf[..got]);
        left -= got as u64;
    }
    close(fd);
    let pad = padding_for(size);
    if pad > 0 {
        w.write(&alloc::vec![0u8; pad]);
    }
    Ok(())
}

/// Encode `dir` as a layer blob into the blob store.
pub fn encode_layer(dir: &str) -> Result<Committed, String> {
    images::ensure_base_dir();
    let tmp = format!("{}/tmp-{}", boxlib::paths::BLOBS_BASE, libakuma::uptime());
    let fd = open(&tmp, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return Err(format!("cannot create {tmp}: errno {}", -fd));
    }
    let failed = Cell::new(false);
    let mut writer = LayerWriter::new(|bytes: &[u8]| {
        if !fsx::write_all(fd, bytes) {
            failed.set(true);
        }
    });
    let mut seen = Seen::new();
    let walked = walk(&mut writer, dir, "", &mut seen);
    writer.write(&END_OF_ARCHIVE);
    let Finished { diff_id, digest, compressed_size, .. } = writer.finish();
    close(fd);
    if let Err(e) = walked {
        libakuma::unlink(&tmp);
        return Err(e);
    }
    if failed.get() {
        libakuma::unlink(&tmp);
        return Err(String::from("write error while storing the layer blob (disk full?)"));
    }
    let blob = images::blob_path(&digest);
    if fsx::exists(&blob) {
        libakuma::unlink(&tmp); // identical content already stored
    } else if libakuma::rename(&tmp, &blob) < 0 {
        libakuma::unlink(&tmp);
        return Err(format!("cannot publish blob {blob}"));
    }
    Ok(Committed { digest, diff_id, size: compressed_size })
}

/// Commit `upper` and **consume** it: the directory is renamed into the layer
/// store (no copy), or deleted if that layer already exists.
pub fn commit_upper(upper: &str) -> Result<Committed, String> {
    let c = encode_layer(upper)?;
    let dest = images::layer_dir(&c.digest);
    if fsx::exists(&dest) {
        fsx::remove_tree(upper);
    } else if libakuma::rename(upper, &dest) < 0 {
        return Err(format!("cannot move {upper} to {dest}"));
    }
    Ok(c)
}

/// Commit `upper` and **keep** it — the container is still using the directory.
/// The layer is materialised by unpacking the blob just written.
pub fn commit_copy(upper: &str) -> Result<Committed, String> {
    let c = encode_layer(upper)?;
    let dest = images::layer_dir(&c.digest);
    if fsx::exists(&dest) {
        return Ok(c);
    }
    let staging = format!("{dest}.tmp");
    fsx::remove_tree(&staging);
    if !mkdir_p(&staging) {
        return Err(format!("cannot create {staging}"));
    }
    let opts = akuma_tar::ExtractOptions { gzip: true, verbose: false, max_bytes: 0 };
    akuma_tar::extract_file(&images::blob_path(&c.digest), &staging, &opts)
        .map_err(|e| format!("unpacking the new layer: {}", e.describe()))?;
    if libakuma::rename(&staging, &dest) < 0 {
        return Err(format!("cannot publish layer {dest}"));
    }
    Ok(c)
}

/// The config for `parent + layer`: diff_id and history appended.
pub fn child_config(parent: &ImageConfig, layer: Option<&Committed>, created_by: &str) -> ImageConfig {
    let now = rfc3339(fsx::now_secs());
    let mut c = parent.clone();
    c.created = now.clone();
    if let Some(l) = layer {
        c.diff_ids.push(l.diff_id.clone());
    }
    c.push_history(&now, created_by, layer.is_some());
    c
}

pub fn cmd_commit(args: libakuma::Args) -> ! {
    let argv: Vec<&str> = args.collect();
    let [container, image] = argv.as_slice() else {
        print("Usage: box commit <container> <image>\n");
        exit(1);
    };
    let Some(parent_store) = fsx::read_text(&images::container_image_file(container)) else {
        print("box commit: no container named '");
        print(container);
        print("' (or it was not started by `box run` of this version)\n");
        exit(1);
    };
    let parent_store = String::from(parent_store.trim());
    let parent = ImageConfig::parse(&images::load_config(&parent_store).unwrap_or_default());
    let mut layers = images::load_layers(&parent_store);
    let mut sizes = images::layer_sizes(&parent_store, &layers);

    let upper = images::container_upper(container);
    let committed = match commit_copy(&upper) {
        Ok(c) => c,
        Err(e) => {
            print("box commit: ");
            println(&e);
            exit(1);
        }
    };
    layers.push(committed.digest.clone());
    sizes.push(Some(committed.size));
    let config = child_config(&parent, Some(&committed), &format!("box commit {container}"));

    let store = images::sanitize_name(image);
    if let Err(e) = images::write_image(&store, &config, &layers, &sizes) {
        print("box commit: ");
        println(&e);
        exit(1);
    }
    print("Committed ");
    print(&store);
    print(" (layer ");
    print(&committed.digest[..committed.digest.len().min(19)]);
    print(")\n");
    exit(0);
}
