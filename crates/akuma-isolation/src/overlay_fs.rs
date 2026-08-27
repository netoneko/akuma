//! Union filesystem over a stack of read-only lower layers and one writable
//! upper layer — the OCI image model, where a container's root is its image's
//! layers plus a private scratch directory.
//!
//! Layers are indexed **topmost-first**: index 0 is `upper`, index 1 is the last
//! layer the image applied, and the highest index is the image's base layer.
//! A name is served by the lowest index that has it.
//!
//! Deletions cannot be written to a read-only lower layer, so they are recorded
//! in the upper layer as **whiteouts**, using the names the OCI image-layer spec
//! already puts inside layer tarballs:
//!
//! - `.wh.<name>` in a directory hides `<name>` in every lower layer.
//! - `.wh..wh..opq` in a directory hides that directory's entire lower contents,
//!   while leaving the directory itself visible.
//!
//! Because those are the on-disk names the registry ships, an extracted layer is
//! usable as a lower layer with no rewriting: `box pull` untars each layer into
//! its own directory and this type interprets the markers at lookup time.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use akuma_vfs::{DirEntry, Filesystem, FsError, FsStats, Metadata};

/// Prefix marking a deleted entry, per the OCI image-layer spec.
const WHITEOUT_PREFIX: &str = ".wh.";
/// Marks a directory as hiding all lower-layer content, per the same spec.
const OPAQUE_MARKER: &str = ".wh..wh..opq";

/// Ceiling on lower layers.
///
/// Every lookup is O(components × layers) `exists` calls against real ext2, so a
/// pathological image cannot be allowed to make path resolution unbounded.
/// Docker's own limit is 127.
pub const MAX_LOWER_LAYERS: usize = 32;

/// The directories named by a `lowerdir=…,upperdir=…` mount option string.
///
/// Pure parse result — no filesystem is touched. The caller resolves each path
/// into a real `Filesystem` and decides what an unreadable one means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayOptions {
    /// Topmost-first, matching Linux's `lowerdir=` convention.
    pub lowerdirs: Vec<String>,
    pub upperdir: String,
}

/// Parse `lowerdir=/a:/b,upperdir=/c` — the same option string Linux's
/// overlayfs takes, minus `workdir` (nothing here needs a staging directory).
///
/// There is no escaping: `:` and `,` always separate, so a directory whose name
/// contains one cannot be expressed. Neither character is legal in a layer
/// digest or a container id, which is all this has to carry.  `workdir=` is
/// accepted and ignored so a Linux option string can be pasted in as-is.
///
/// # Errors
/// Returns a message when a key is unknown, a required key is missing, or a
/// path is empty or relative.
pub fn parse_options(data: &str) -> Result<OverlayOptions, &'static str> {
    let mut lowerdirs = Vec::new();
    let mut upperdir = String::new();

    for field in data.split(',') {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        let (key, value) = field.split_once('=').ok_or("malformed option (want key=value)")?;
        match key {
            "lowerdir" => {
                for dir in value.split(':') {
                    if dir.is_empty() {
                        return Err("empty lowerdir path");
                    }
                    if !dir.starts_with('/') {
                        return Err("lowerdir must be absolute");
                    }
                    lowerdirs.push(String::from(dir));
                }
            }
            "upperdir" => {
                if !upperdir.is_empty() {
                    return Err("duplicate upperdir");
                }
                if !value.starts_with('/') {
                    return Err("upperdir must be absolute");
                }
                upperdir = String::from(value);
            }
            "workdir" => {}
            _ => return Err("unknown overlay option"),
        }
    }

    if upperdir.is_empty() {
        return Err("missing upperdir");
    }
    if lowerdirs.is_empty() {
        return Err("missing lowerdir");
    }
    if lowerdirs.len() > MAX_LOWER_LAYERS {
        return Err("too many lower layers");
    }

    Ok(OverlayOptions { lowerdirs, upperdir })
}

/// A union of one writable upper layer over read-only lower layers.
pub struct OverlayFs {
    upper: Arc<dyn Filesystem>,
    /// Topmost lower layer first.
    lowers: Vec<Arc<dyn Filesystem>>,
}

/// Split an absolute path into its non-empty components.
fn components(path: &str) -> Vec<&str> {
    path.split('/').filter(|c| !c.is_empty() && *c != ".").collect()
}

/// `/a/b` → (`/a`, `b`). The parent of a top-level entry is `/`.
fn split_parent(path: &str) -> (String, String) {
    let comps = components(path);
    match comps.split_last() {
        None => (String::from("/"), String::new()),
        Some((name, parents)) => {
            let mut parent = String::from("/");
            for (i, c) in parents.iter().enumerate() {
                if i > 0 {
                    parent.push('/');
                }
                parent.push_str(c);
            }
            (parent, String::from(*name))
        }
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn whiteout_of(path: &str) -> String {
    let (parent, name) = split_parent(path);
    join(&parent, &format!("{WHITEOUT_PREFIX}{name}"))
}

/// Where a lookup landed: which layer serves the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Found {
    /// 0 = upper, 1.. = lowers.
    layer: usize,
}

impl Found {
    const fn is_upper(self) -> bool {
        self.layer == 0
    }
}

impl OverlayFs {
    /// `lowers` are ordered **topmost-first** — the layer applied last comes
    /// first, matching the lookup order. `box run` reverses the image's layer
    /// list (which is base-first) before calling this.
    ///
    /// Every layer, `upper` included, must be backed by the same underlying
    /// filesystem, because [`Filesystem::read_at_by_inode`] is forwarded on the
    /// assumption that an inode number identifies a file across all of them.
    /// See that method for what goes wrong otherwise.
    #[must_use]
    pub fn new(upper: Arc<dyn Filesystem>, mut lowers: Vec<Arc<dyn Filesystem>>) -> Self {
        lowers.truncate(MAX_LOWER_LAYERS);
        Self { upper, lowers }
    }

    fn layer_count(&self) -> usize {
        self.lowers.len() + 1
    }

    fn layer(&self, idx: usize) -> &Arc<dyn Filesystem> {
        if idx == 0 { &self.upper } else { &self.lowers[idx - 1] }
    }

    /// Resolve `path` to the layer that serves it, walking one component at a
    /// time so that a whiteout or an opaque marker on an *ancestor* hides the
    /// whole subtree beneath it.
    ///
    /// Within a single layer the entry is checked before its whiteout: a
    /// re-created file lives alongside the whiteout that deleted it (nothing
    /// unlinks the marker on the way back up), and the newer entry must win.
    fn find(&self, path: &str) -> Option<Found> {
        let comps = components(path);
        if comps.is_empty() {
            return Some(Found { layer: 0 });
        }

        // Layers at or past this index are hidden by an opaque ancestor.
        let mut cutoff = self.layer_count();
        let mut prefix = String::from("/");

        for (i, comp) in comps.iter().enumerate() {
            let parent = prefix.clone();
            prefix = join(&parent, comp);
            let wh = join(&parent, &format!("{WHITEOUT_PREFIX}{comp}"));

            let mut found = None;
            for l in 0..cutoff {
                let fs = self.layer(l);
                if fs.exists(&prefix) {
                    found = Some(l);
                    break;
                }
                if fs.exists(&wh) {
                    // Deleted at this layer; everything below is hidden.
                    return None;
                }
            }
            let l = found?;

            let last = i + 1 == comps.len();
            if last {
                return Some(Found { layer: l });
            }

            // Descending further. A plain file at this component shadows any
            // lower-layer *directory* of the same name — the path cannot
            // continue through it, and must not be allowed to resume in a layer
            // the file is hiding.
            let fs = self.layer(l);
            if !fs.metadata(&prefix).is_ok_and(|m| m.is_dir) {
                if !fs.is_symlink(&prefix) {
                    return None;
                }
                // A symlinked prefix stays within its own layer: whatever it
                // points at is that layer's business, and the underlying
                // filesystem resolves it.
                cutoff = l + 1;
            }

            // An opaque directory cuts off everything below it.
            if fs.exists(&join(&prefix, OPAQUE_MARKER)) {
                cutoff = l + 1;
            }
        }

        None
    }

    /// The layers whose contents merge to form directory `path`, topmost-first.
    /// Empty when the directory is not visible at all.
    fn dir_layers(&self, path: &str) -> Vec<usize> {
        let mut out = Vec::new();

        let Some(found) = self.find(path) else {
            return out;
        };
        // A non-directory shadows any lower directory of the same name.
        if !self.layer(found.layer).metadata(path).is_ok_and(|m| m.is_dir) {
            return out;
        }

        for l in found.layer..self.layer_count() {
            let fs = self.layer(l);
            if l > found.layer {
                // A whiteout below the winning layer cannot exist (find would
                // have stopped), but a non-directory or a missing entry ends the
                // merge just as it does in the walk above.
                if !fs.exists(path) {
                    continue;
                }
                if !fs.metadata(path).is_ok_and(|m| m.is_dir) {
                    break;
                }
            }
            out.push(l);
            if fs.exists(&join(path, OPAQUE_MARKER)) {
                break;
            }
        }
        out
    }

    /// Recreate `path`'s ancestor directories in the upper layer so a file can
    /// be written there. Missing intermediate directories are created empty;
    /// their lower-layer contents keep showing through the merge.
    fn copy_up_parents(&self, path: &str) -> Result<(), FsError> {
        let (parent, _) = split_parent(path);
        let comps = components(&parent);
        let mut prefix = String::from("/");
        for comp in comps {
            prefix = join(&prefix, comp);
            if !self.upper.exists(&prefix) {
                self.upper.create_dir(&prefix)?;
            }
        }
        Ok(())
    }

    /// Materialize `path` in the upper layer so it can be modified in place.
    /// A file already in upper is left alone; one served by a lower layer is
    /// copied whole. Returns `NotFound` if the path is not visible.
    fn copy_up(&self, path: &str) -> Result<(), FsError> {
        let found = self.find(path).ok_or(FsError::NotFound)?;
        if found.is_upper() {
            return Ok(());
        }

        let src = self.layer(found.layer);
        let meta = src.metadata(path)?;
        self.copy_up_parents(path)?;

        if meta.is_dir {
            if !self.upper.exists(path) {
                self.upper.create_dir(path)?;
            }
            return Ok(());
        }

        if src.is_symlink(path) {
            let target = src.read_symlink(path)?;
            return self.upper.create_symlink(path, &target);
        }

        let data = src.read_file(path)?;
        self.upper.write_file(path, &data)?;
        if meta.mode != 0 {
            // Best-effort: an upper layer that cannot chmod still holds the data.
            let _ = self.upper.chmod(path, meta.mode);
        }
        Ok(())
    }

    /// Drop the whiteout shadowing `path`, if any — called when the name comes
    /// back, so the merged view stops hiding lower layers that never went away.
    fn clear_whiteout(&self, path: &str) {
        let wh = whiteout_of(path);
        if self.upper.exists(&wh) {
            let _ = self.upper.remove_file(&wh);
        }
    }

    /// Record a deletion that cannot be performed on a read-only lower layer.
    fn write_whiteout(&self, path: &str) -> Result<(), FsError> {
        self.copy_up_parents(path)?;
        self.upper.write_file(&whiteout_of(path), &[])
    }

    /// Prepare the upper layer to receive a brand-new entry at `path`.
    fn prepare_create(&self, path: &str) -> Result<(), FsError> {
        self.copy_up_parents(path)?;
        self.clear_whiteout(path);
        Ok(())
    }

    /// The layer serving `path`, for read-only delegation.
    fn read_layer(&self, path: &str) -> Result<&Arc<dyn Filesystem>, FsError> {
        let found = self.find(path).ok_or(FsError::NotFound)?;
        Ok(self.layer(found.layer))
    }
}

impl Filesystem for OverlayFs {
    fn name(&self) -> &'static str {
        "overlay"
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let layers = self.dir_layers(path);
        if layers.is_empty() {
            return Err(FsError::NotFound);
        }

        let mut out: Vec<DirEntry> = Vec::new();
        // Names already decided by a higher layer — whether they were emitted or
        // whited out. Either way a lower layer must not re-introduce them.
        let mut seen: Vec<String> = Vec::new();

        for l in layers {
            let Ok(entries) = self.layer(l).read_dir(path) else {
                continue;
            };
            for e in entries {
                if e.name == OPAQUE_MARKER {
                    continue;
                }
                if let Some(hidden) = e.name.strip_prefix(WHITEOUT_PREFIX) {
                    let hidden = String::from(hidden);
                    if !seen.contains(&hidden) {
                        seen.push(hidden);
                    }
                    continue;
                }
                if seen.contains(&e.name) {
                    continue;
                }
                seen.push(e.name.clone());
                out.push(e);
            }
        }
        Ok(out)
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> {
        self.read_layer(path)?.read_file(path)
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        self.prepare_create(path)?;
        self.upper.write_file(path, data)
    }

    fn read_at(&self, path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        self.read_layer(path)?.read_at(path, offset, buf)
    }

    fn write_at(&self, path: &str, offset: usize, data: &[u8]) -> Result<usize, FsError> {
        if self.find(path).is_some() {
            self.copy_up(path)?;
        } else {
            // Nothing to copy up, and a layer may refuse to write into a file it
            // has never heard of — create it empty first.
            self.prepare_create(path)?;
            self.upper.write_file(path, &[])?;
        }
        self.upper.write_at(path, offset, data)
    }

    fn create_dir(&self, path: &str) -> Result<(), FsError> {
        if self.find(path).is_some() {
            return Err(FsError::AlreadyExists);
        }
        self.prepare_create(path)?;
        self.upper.create_dir(path)
    }

    fn remove_file(&self, path: &str) -> Result<(), FsError> {
        let found = self.find(path).ok_or(FsError::NotFound)?;
        if found.is_upper() {
            self.upper.remove_file(path)?;
            // A lower layer may still carry the name — hide it now that the
            // upper copy is gone.
            if self.find(path).is_some() {
                self.write_whiteout(path)?;
            }
            return Ok(());
        }
        self.write_whiteout(path)
    }

    fn remove_dir(&self, path: &str) -> Result<(), FsError> {
        let found = self.find(path).ok_or(FsError::NotFound)?;
        if !self.read_dir(path)?.is_empty() {
            return Err(FsError::DirectoryNotEmpty);
        }
        if found.is_upper() {
            self.upper.remove_dir(path)?;
            if self.find(path).is_some() {
                self.write_whiteout(path)?;
            }
            return Ok(());
        }
        self.write_whiteout(path)
    }

    fn exists(&self, path: &str) -> bool {
        self.find(path).is_some()
    }

    fn metadata(&self, path: &str) -> Result<Metadata, FsError> {
        self.read_layer(path)?.metadata(path)
    }

    fn create_symlink(&self, link_path: &str, target: &str) -> Result<(), FsError> {
        self.prepare_create(link_path)?;
        self.upper.create_symlink(link_path, target)
    }

    fn read_symlink(&self, path: &str) -> Result<String, FsError> {
        self.read_layer(path)?.read_symlink(path)
    }

    fn is_symlink(&self, path: &str) -> bool {
        self.read_layer(path).is_ok_and(|fs| fs.is_symlink(path))
    }

    fn chmod(&self, path: &str, mode: u32) -> Result<(), FsError> {
        self.copy_up(path)?;
        self.upper.chmod(path, mode)
    }

    fn truncate(&self, path: &str, length: u64) -> Result<(), FsError> {
        self.copy_up(path)?;
        self.upper.truncate(path, length)
    }

    fn fallocate(&self, path: &str, mode: i32, offset: u64, len: u64) -> Result<(), FsError> {
        self.copy_up(path)?;
        self.upper.fallocate(path, mode, offset, len)
    }

    /// Files only. Renaming a directory that exists in a lower layer means
    /// copying its whole subtree up before the rename and whiting out every
    /// lower name underneath it; until a caller needs that, refusing is better
    /// than half-doing it.
    fn rename(&self, old_path: &str, new_path: &str) -> Result<(), FsError> {
        let found = self.find(old_path).ok_or(FsError::NotFound)?;
        if self.layer(found.layer).metadata(old_path).is_ok_and(|m| m.is_dir) {
            return Err(FsError::NotSupported);
        }

        self.copy_up(old_path)?;
        self.prepare_create(new_path)?;
        self.upper.rename(old_path, new_path)?;

        // The source name is gone from upper but may still exist below.
        if self.find(old_path).is_some() {
            self.write_whiteout(old_path)?;
        }
        Ok(())
    }

    fn stats(&self) -> Result<FsStats, FsError> {
        self.upper.stats()
    }

    fn sync(&self) -> Result<(), FsError> {
        self.upper.sync()
    }

    fn resolve_inode(&self, path: &str) -> Result<u32, FsError> {
        self.read_layer(path)?.resolve_inode(path)
    }

    /// Forwarded to whichever layer recognizes the inode.
    ///
    /// The kernel's file page cache is keyed on inode alone
    /// (`src/file_page_cache.rs`), so this is only sound when every layer sits
    /// on the same underlying filesystem and inode numbers are therefore
    /// globally unique — which is why [`OverlayFs::new`] requires it. A layer
    /// that synthesizes inode numbers (`MemoryFilesystem` hashes the path)
    /// would collide with real ones and hand the page-fault path the contents
    /// of an unrelated file.
    fn read_at_by_inode(&self, inode: u32, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        // The upper layer's error is what propagates when no layer serves the
        // inode, rather than a blanket `NotFound`. Every layer sits on the same
        // underlying filesystem (see above), so a *refusal* — `NotAFile` for a
        // directory, which `read(2)` on a directory fd now reaches — is the same
        // refusal from every layer, and reporting it as "no such file" would
        // turn an `EISDIR` into an `ENOENT` for anything inside a box.
        let mut upper_err = FsError::NotFound;
        for l in 0..self.layer_count() {
            match self.layer(l).read_at_by_inode(inode, offset, buf) {
                Ok(n) => return Ok(n),
                Err(e) if l == 0 => upper_err = e,
                Err(_) => {}
            }
        }
        Err(upper_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use akuma_vfs::MemoryFilesystem;

    /// Build a layer. A trailing `/` makes a directory; anything else is a file
    /// whose contents are its second field. Parent directories are created
    /// automatically, so a layer reads like the tar it came from.
    fn layer(entries: &[(&str, &str)]) -> Arc<dyn Filesystem> {
        let fs = MemoryFilesystem::new();
        for (path, contents) in entries {
            let dir_path = if path.ends_with('/') {
                String::from(path.trim_end_matches('/'))
            } else {
                split_parent(path).0
            };
            let mut prefix = String::from("/");
            for comp in components(&dir_path) {
                prefix = join(&prefix, comp);
                if !fs.exists(&prefix) {
                    fs.create_dir(&prefix).unwrap();
                }
            }
            if !path.ends_with('/') {
                fs.write_file(path, contents.as_bytes()).unwrap();
            }
        }
        Arc::new(fs)
    }

    /// Upper plus lowers, topmost-first.
    fn overlay(upper: &[(&str, &str)], lowers: &[&[(&str, &str)]]) -> OverlayFs {
        OverlayFs::new(layer(upper), lowers.iter().map(|l| layer(l)).collect())
    }

    fn read(fs: &OverlayFs, path: &str) -> String {
        String::from_utf8(fs.read_file(path).unwrap()).unwrap()
    }

    fn names(fs: &OverlayFs, path: &str) -> Vec<String> {
        let mut v: Vec<String> = fs.read_dir(path).unwrap().into_iter().map(|e| e.name).collect();
        v.sort();
        v
    }

    #[test]
    fn upper_shadows_lower() {
        let fs = overlay(&[("/etc/hosts", "upper")], &[&[("/etc/hosts", "lower")]]);
        assert_eq!(read(&fs, "/etc/hosts"), "upper");
    }

    #[test]
    fn lower_shows_through_where_upper_is_empty() {
        let fs = overlay(&[], &[&[("/bin/busybox", "elf")]]);
        assert_eq!(read(&fs, "/bin/busybox"), "elf");
        assert!(fs.exists("/bin"));
        assert!(!fs.exists("/bin/missing"));
    }

    #[test]
    fn topmost_lower_wins() {
        let fs = overlay(
            &[],
            &[&[("/etc/issue", "top")], &[("/etc/issue", "middle")], &[("/etc/issue", "base")]],
        );
        assert_eq!(read(&fs, "/etc/issue"), "top");
    }

    #[test]
    fn whiteout_in_upper_hides_lower_file() {
        let fs = overlay(&[("/etc/.wh.passwd", "")], &[&[("/etc/passwd", "root:x:0:0")]]);
        assert!(!fs.exists("/etc/passwd"));
        assert_eq!(fs.read_file("/etc/passwd"), Err(FsError::NotFound));
        assert_eq!(names(&fs, "/etc"), Vec::<String>::new());
    }

    /// Whiteouts ship inside image layers, so a lower layer must be able to
    /// delete a file from a still-lower one.
    #[test]
    fn whiteout_in_a_lower_layer_hides_the_layer_below_it() {
        let fs = overlay(
            &[],
            &[&[("/usr/.wh.doomed", "")], &[("/usr/doomed", "gone"), ("/usr/kept", "here")]],
        );
        assert!(!fs.exists("/usr/doomed"));
        assert_eq!(read(&fs, "/usr/kept"), "here");
    }

    #[test]
    fn whiteout_on_a_directory_hides_the_whole_subtree() {
        let fs = overlay(
            &[("/.wh.opt", "")],
            &[&[("/opt/app/bin/run", "x"), ("/etc/keep", "y")]],
        );
        assert!(!fs.exists("/opt"));
        assert!(!fs.exists("/opt/app/bin/run"));
        assert_eq!(read(&fs, "/etc/keep"), "y");
    }

    #[test]
    fn opaque_dir_hides_lower_contents_but_not_the_dir() {
        let fs = overlay(
            &[("/var/log/.wh..wh..opq", ""), ("/var/log/new", "fresh")],
            &[&[("/var/log/old", "stale"), ("/var/spool/mail", "m")]],
        );
        assert!(fs.exists("/var/log"));
        assert_eq!(read(&fs, "/var/log/new"), "fresh");
        assert!(!fs.exists("/var/log/old"));
        // Only the marked directory is cut off.
        assert_eq!(read(&fs, "/var/spool/mail"), "m");
        assert_eq!(names(&fs, "/var/log"), alloc::vec![String::from("new")]);
    }

    #[test]
    fn read_dir_merges_layers_and_dedupes() {
        let fs = overlay(
            &[("/etc/hosts", "upper")],
            &[&[("/etc/hosts", "lower"), ("/etc/resolv.conf", "ns")], &[("/etc/passwd", "p")]],
        );
        assert_eq!(
            names(&fs, "/etc"),
            alloc::vec![
                String::from("hosts"),
                String::from("passwd"),
                String::from("resolv.conf")
            ]
        );
    }

    #[test]
    fn read_dir_of_a_missing_dir_is_not_found() {
        let fs = overlay(&[], &[&[("/etc/hosts", "x")]]);
        assert_eq!(fs.read_dir("/nope").unwrap_err(), FsError::NotFound);
    }

    #[test]
    fn write_copies_up_and_leaves_the_layer_untouched() {
        let lower = layer(&[("/etc/hosts", "127.0.0.1 localhost")]);
        let upper = layer(&[]);
        let fs = OverlayFs::new(upper.clone(), alloc::vec![lower.clone()]);

        fs.write_file("/etc/hosts", b"127.0.0.1 box").unwrap();

        assert_eq!(read(&fs, "/etc/hosts"), "127.0.0.1 box");
        assert_eq!(upper.read_file("/etc/hosts").unwrap(), b"127.0.0.1 box");
        assert_eq!(lower.read_file("/etc/hosts").unwrap(), b"127.0.0.1 localhost");
    }

    #[test]
    fn write_at_copies_the_lower_contents_up_first() {
        let lower = layer(&[("/data/f", "AAAAAAAA")]);
        let fs = OverlayFs::new(layer(&[]), alloc::vec![lower.clone()]);

        fs.write_at("/data/f", 2, b"BB").unwrap();

        assert_eq!(read(&fs, "/data/f"), "AABBAAAA");
        assert_eq!(lower.read_file("/data/f").unwrap(), b"AAAAAAAA");
    }

    #[test]
    fn write_creates_missing_parent_dirs_in_upper() {
        let upper = layer(&[]);
        let fs = OverlayFs::new(upper.clone(), alloc::vec![layer(&[("/a/b/c/keep", "k")])]);

        fs.write_file("/a/b/c/new", b"n").unwrap();

        assert!(upper.exists("/a/b/c"));
        assert_eq!(read(&fs, "/a/b/c/new"), "n");
        // The copy-up of the parents did not shadow the lower layer's contents.
        assert_eq!(read(&fs, "/a/b/c/keep"), "k");
    }

    #[test]
    fn write_at_on_a_brand_new_path_creates_it() {
        let fs = overlay(&[], &[]);
        fs.write_at("/tmp/x", 0, b"hi").unwrap();
        assert_eq!(read(&fs, "/tmp/x"), "hi");
    }

    #[test]
    fn removing_a_lower_file_writes_a_whiteout() {
        let lower = layer(&[("/bin/rm-me", "x"), ("/bin/keep", "y")]);
        let upper = layer(&[]);
        let fs = OverlayFs::new(upper.clone(), alloc::vec![lower.clone()]);

        fs.remove_file("/bin/rm-me").unwrap();

        assert!(!fs.exists("/bin/rm-me"));
        assert!(upper.exists("/bin/.wh.rm-me"));
        assert!(lower.exists("/bin/rm-me"));
        assert_eq!(names(&fs, "/bin"), alloc::vec![String::from("keep")]);
    }

    #[test]
    fn removing_a_copied_up_file_still_hides_the_lower_one() {
        let fs = overlay(&[("/etc/hosts", "upper")], &[&[("/etc/hosts", "lower")]]);
        fs.remove_file("/etc/hosts").unwrap();
        assert!(!fs.exists("/etc/hosts"));
    }

    #[test]
    fn removing_an_upper_only_file_leaves_no_whiteout() {
        let upper = layer(&[("/tmp/scratch", "x")]);
        let fs = OverlayFs::new(upper.clone(), alloc::vec![layer(&[("/tmp/", "")])]);

        fs.remove_file("/tmp/scratch").unwrap();

        assert!(!fs.exists("/tmp/scratch"));
        assert!(!upper.exists("/tmp/.wh.scratch"));
    }

    #[test]
    fn removing_a_missing_file_is_not_found() {
        let fs = overlay(&[], &[&[("/etc/hosts", "x")]]);
        assert_eq!(fs.remove_file("/etc/nope").unwrap_err(), FsError::NotFound);
    }

    #[test]
    fn recreating_a_deleted_file_clears_its_whiteout() {
        let upper = layer(&[]);
        let fs = OverlayFs::new(upper.clone(), alloc::vec![layer(&[("/etc/hosts", "lower")])]);

        fs.remove_file("/etc/hosts").unwrap();
        assert!(!fs.exists("/etc/hosts"));

        fs.write_file("/etc/hosts", b"reborn").unwrap();

        assert_eq!(read(&fs, "/etc/hosts"), "reborn");
        assert!(!upper.exists("/etc/.wh.hosts"));
        assert_eq!(names(&fs, "/etc"), alloc::vec![String::from("hosts")]);
    }

    #[test]
    fn create_dir_over_an_existing_name_conflicts() {
        let fs = overlay(&[], &[&[("/var/lib/", "")]]);
        assert_eq!(fs.create_dir("/var/lib").unwrap_err(), FsError::AlreadyExists);
        fs.create_dir("/var/run").unwrap();
        assert!(fs.exists("/var/run"));
    }

    #[test]
    fn remove_dir_refuses_a_dir_that_is_only_non_empty_below() {
        let fs = overlay(&[("/opt/", "")], &[&[("/opt/thing", "t")]]);
        assert_eq!(fs.remove_dir("/opt").unwrap_err(), FsError::DirectoryNotEmpty);
    }

    #[test]
    fn remove_dir_whiteouts_an_empty_lower_dir() {
        let upper = layer(&[]);
        let fs = OverlayFs::new(upper.clone(), alloc::vec![layer(&[("/empty/", "")])]);

        fs.remove_dir("/empty").unwrap();

        assert!(!fs.exists("/empty"));
        assert!(upper.exists("/.wh.empty"));
    }

    #[test]
    fn rename_copies_up_and_whiteouts_the_source() {
        let lower = layer(&[("/etc/hosts", "content")]);
        let upper = layer(&[]);
        let fs = OverlayFs::new(upper.clone(), alloc::vec![lower.clone()]);

        fs.rename("/etc/hosts", "/etc/hosts.bak").unwrap();

        assert_eq!(read(&fs, "/etc/hosts.bak"), "content");
        assert!(!fs.exists("/etc/hosts"));
        assert!(upper.exists("/etc/.wh.hosts"));
        assert!(lower.exists("/etc/hosts"));
    }

    #[test]
    fn renaming_a_directory_is_refused_rather_than_half_done() {
        let fs = overlay(&[], &[&[("/opt/app/f", "x")]]);
        assert_eq!(fs.rename("/opt/app", "/opt/app2").unwrap_err(), FsError::NotSupported);
        assert_eq!(read(&fs, "/opt/app/f"), "x");
    }

    #[test]
    fn metadata_comes_from_the_winning_layer() {
        let fs = overlay(&[("/etc/hosts", "12345")], &[&[("/etc/hosts", "1")]]);
        assert_eq!(fs.metadata("/etc/hosts").unwrap().size, 5);
        assert!(fs.metadata("/etc").unwrap().is_dir);
        assert_eq!(fs.metadata("/etc/nope").unwrap_err(), FsError::NotFound);
    }

    #[test]
    fn a_file_shadows_a_lower_directory_of_the_same_name() {
        let fs = overlay(&[("/opt", "now a file")], &[&[("/opt/app/f", "x")]]);
        assert_eq!(read(&fs, "/opt"), "now a file");
        assert!(!fs.exists("/opt/app/f"));
        assert_eq!(fs.read_dir("/opt").unwrap_err(), FsError::NotFound);
    }

    #[test]
    fn lower_layers_are_capped() {
        let lowers: Vec<Arc<dyn Filesystem>> =
            (0..MAX_LOWER_LAYERS + 8).map(|_| layer(&[])).collect();
        let fs = OverlayFs::new(layer(&[]), lowers);
        assert_eq!(fs.layer_count(), MAX_LOWER_LAYERS + 1);
    }

    /// The shape `box run` actually hands this type: a base image layer, a
    /// second layer that edits one file, deletes another and blanks out a
    /// directory, and an empty container scratch layer on top.
    #[test]
    fn a_three_layer_image_merges_the_way_the_registry_intended() {
        let scratch = layer(&[]);
        let fs = OverlayFs::new(
            scratch.clone(),
            alloc::vec![
                layer(&[
                    ("/etc/issue", "v2"),
                    ("/etc/.wh.legacy.conf", ""),
                    ("/var/cache/.wh..wh..opq", ""),
                ]),
                layer(&[
                    ("/bin/busybox", "elf"),
                    ("/etc/issue", "v1"),
                    ("/etc/legacy.conf", "old"),
                    ("/var/cache/stale", "junk"),
                ]),
            ],
        );

        assert_eq!(read(&fs, "/bin/busybox"), "elf");
        assert_eq!(read(&fs, "/etc/issue"), "v2");
        assert!(!fs.exists("/etc/legacy.conf"));
        assert!(fs.exists("/var/cache"));
        assert!(!fs.exists("/var/cache/stale"));
        assert_eq!(names(&fs, "/etc"), alloc::vec![String::from("issue")]);

        // A container write lands in scratch and nowhere else.
        fs.write_file("/etc/issue", b"mine").unwrap();
        assert_eq!(read(&fs, "/etc/issue"), "mine");
        assert_eq!(scratch.read_file("/etc/issue").unwrap(), b"mine");
    }

    #[test]
    fn options_parse_in_linux_form() {
        let o = parse_options("lowerdir=/l/2:/l/1,upperdir=/c/7/upper").unwrap();
        assert_eq!(o.upperdir, "/c/7/upper");
        assert_eq!(o.lowerdirs, alloc::vec![String::from("/l/2"), String::from("/l/1")]);
    }

    #[test]
    fn options_accept_any_field_order_and_ignore_workdir() {
        let o = parse_options("upperdir=/u,workdir=/w,lowerdir=/l").unwrap();
        assert_eq!(o.upperdir, "/u");
        assert_eq!(o.lowerdirs, alloc::vec![String::from("/l")]);
    }

    #[test]
    fn options_reject_what_cannot_be_mounted() {
        for bad in [
            "",
            "lowerdir=/l",
            "upperdir=/u",
            "upperdir=/u,lowerdir=",
            "upperdir=/u,lowerdir=relative",
            "upperdir=relative,lowerdir=/l",
            "upperdir=/u,lowerdir=/a::/b",
            "upperdir=/u,upperdir=/u2,lowerdir=/l",
            "upperdir=/u,lowerdir=/l,rootcontext=x",
            "upperdir",
        ] {
            assert!(parse_options(bad).is_err(), "should have been rejected: {bad:?}");
        }
    }

    #[test]
    fn options_cap_the_layer_count() {
        let mut opt = String::from("upperdir=/u,lowerdir=/l0");
        for i in 1..=MAX_LOWER_LAYERS {
            opt.push_str(&format!(":/l{i}"));
        }
        assert!(parse_options(&opt).is_err());
    }

    #[test]
    fn root_is_always_present() {
        let fs = overlay(&[], &[]);
        assert!(fs.exists("/"));
        assert_eq!(fs.name(), "overlay");
    }
}
