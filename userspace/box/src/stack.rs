//! The merged view of a layer stack, computed in userspace.
//!
//! `COPY --from=<stage>` has to read files out of a stage's *final* root — the
//! union of its layers — but a box may not mount, and box 0 has no overlay
//! mount of its own (`docs/archive/BOX_BUILD.md` open question 2). So the
//! builder resolves paths through the stack itself, using the same OCI rules the
//! kernel's `overlay_fs` applies: a `.wh.<name>` entry in layer L hides `<name>`
//! in every layer **below** L (not in L itself), and a `.wh..wh..opq` marker in
//! a directory of layer L hides that directory's contents in every layer below.
//!
//! Generic over [`LayerFs`] so the rules are host-tested against an in-memory
//! tree. The cost is a second implementation of overlay semantics, which can
//! drift from the kernel's; the tests below pin the cases that matter, and the
//! interop gate (`box save | docker load`) is what catches the rest.

use alloc::string::String;
use alloc::vec::Vec;

pub const WHITEOUT_PREFIX: &str = ".wh.";
pub const OPAQUE_MARKER: &str = ".wh..wh..opq";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Node {
    Dir,
    /// A regular file, symlink or anything else that is not a directory.
    Other,
}

/// One layer's own contents — no merging. `rel` is `/`-free of leading slash;
/// `""` is the layer's root.
pub trait LayerFs {
    /// What `rel` is in `layer` (0 = topmost), without following symlinks.
    fn node(&self, layer: usize, rel: &str) -> Option<Node>;
    /// Names inside the directory `rel` of `layer`, markers included.
    fn list(&self, layer: usize, rel: &str) -> Vec<String>;
}

/// Where a merged path lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    /// Topmost layer holding it — the one to read a file from.
    pub top: usize,
    pub node: Node,
    /// Last layer (inclusive) that contributes to it; for a directory, the
    /// layers `top..=floor` are merged.
    pub floor: usize,
}

pub struct Stack<'a, F: LayerFs> {
    fs: &'a F,
    layers: usize,
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        String::from(name)
    } else {
        alloc::format!("{dir}/{name}")
    }
}

fn split(rel: &str) -> (&str, &str) {
    match rel.rfind('/') {
        Some(i) => (&rel[..i], &rel[i + 1..]),
        None => ("", rel),
    }
}

impl<'a, F: LayerFs> Stack<'a, F> {
    pub fn new(fs: &'a F, layers: usize) -> Self {
        Self { fs, layers }
    }

    fn whiteout(&self, layer: usize, parent: &str, name: &str) -> bool {
        self.fs.node(layer, &join(parent, &alloc::format!("{WHITEOUT_PREFIX}{name}"))).is_some()
    }

    fn opaque(&self, layer: usize, dir: &str) -> bool {
        self.fs.node(layer, &join(dir, OPAQUE_MARKER)).is_some()
    }

    /// Resolve `rel` (no leading `/`, no `.`/`..`) in the merged view.
    pub fn resolve(&self, rel: &str) -> Option<Found> {
        if self.layers == 0 {
            return None;
        }
        if rel.is_empty() {
            return Some(Found { top: 0, node: Node::Dir, floor: self.layers - 1 });
        }
        let (parent, name) = split(rel);
        let p = self.resolve(parent)?;
        if p.node != Node::Dir {
            return None;
        }
        for l in p.top..=p.floor {
            if let Some(node) = self.fs.node(l, rel) {
                let mut floor = l;
                if node == Node::Dir && !self.opaque(l, rel) && !self.whiteout(l, parent, name) {
                    for l2 in l + 1..=p.floor {
                        match self.fs.node(l2, rel) {
                            Some(Node::Dir) => floor = l2,
                            Some(Node::Other) => break,
                            None => {}
                        }
                        if self.opaque(l2, rel) || self.whiteout(l2, parent, name) {
                            break;
                        }
                    }
                }
                return Some(Found { top: l, node, floor });
            }
            if self.whiteout(l, parent, name) {
                return None;
            }
        }
        None
    }

    /// The merged names inside directory `rel`, sorted, markers removed.
    pub fn list(&self, rel: &str) -> Vec<String> {
        let Some(d) = self.resolve(rel) else { return Vec::new() };
        if d.node != Node::Dir {
            return Vec::new();
        }
        let mut hidden: Vec<String> = Vec::new();
        let mut names: Vec<String> = Vec::new();
        for l in d.top..=d.floor {
            if self.fs.node(l, rel) != Some(Node::Dir) {
                continue;
            }
            let entries = self.fs.list(l, rel);
            for n in &entries {
                if n.starts_with(WHITEOUT_PREFIX) {
                    continue;
                }
                if !hidden.contains(n) && !names.contains(n) {
                    names.push(n.clone());
                }
            }
            // Whiteouts in this layer hide the layers below it only.
            for n in &entries {
                if let Some(target) = n.strip_prefix(WHITEOUT_PREFIX) {
                    if n != OPAQUE_MARKER {
                        hidden.push(String::from(target));
                    }
                }
            }
        }
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::vec;

    /// layer -> path -> node. Directories are listed implicitly by prefix.
    #[derive(Default)]
    struct Mem(Vec<BTreeMap<String, Node>>);

    impl Mem {
        fn layer(mut self, entries: &[(&str, Node)]) -> Self {
            let mut m = BTreeMap::new();
            for (p, n) in entries {
                m.insert(String::from(*p), *n);
            }
            self.0.push(m);
            self
        }
    }

    impl LayerFs for Mem {
        fn node(&self, layer: usize, rel: &str) -> Option<Node> {
            if rel.is_empty() {
                return Some(Node::Dir);
            }
            self.0[layer].get(rel).copied()
        }
        fn list(&self, layer: usize, rel: &str) -> Vec<String> {
            let prefix = if rel.is_empty() { String::new() } else { alloc::format!("{rel}/") };
            self.0[layer]
                .keys()
                .filter_map(|k| k.strip_prefix(&prefix))
                .filter(|r| !r.contains('/'))
                .map(String::from)
                .collect()
        }
    }

    use Node::{Dir as D, Other as F};

    #[test]
    fn upper_layers_shadow_lower_ones() {
        let fs = Mem::default()
            .layer(&[("a", F)]) // layer 0 = top
            .layer(&[("a", F), ("b", F)]);
        let s = Stack::new(&fs, 2);
        assert_eq!(s.resolve("a").unwrap().top, 0);
        assert_eq!(s.resolve("b").unwrap().top, 1);
        assert!(s.resolve("c").is_none());
    }

    #[test]
    fn a_whiteout_hides_lower_layers_only() {
        let fs = Mem::default()
            .layer(&[(".wh.gone", F), ("same", F), (".wh.same", F)])
            .layer(&[("gone", F), ("same", F), ("kept", F)]);
        let s = Stack::new(&fs, 2);
        assert!(s.resolve("gone").is_none());
        assert_eq!(s.resolve("same").unwrap().top, 0, "an entry beats its own layer's whiteout");
        assert!(s.resolve("kept").is_some());
        assert_eq!(s.list(""), ["kept", "same"]);
    }

    #[test]
    fn directories_merge_across_layers() {
        let fs = Mem::default()
            .layer(&[("etc", D), ("etc/new", F)])
            .layer(&[("etc", D), ("etc/old", F)]);
        let s = Stack::new(&fs, 2);
        let d = s.resolve("etc").unwrap();
        assert_eq!((d.top, d.floor), (0, 1));
        assert_eq!(s.list("etc"), ["new", "old"]);
        assert!(s.resolve("etc/old").is_some());
    }

    #[test]
    fn an_opaque_directory_hides_everything_below() {
        let fs = Mem::default()
            .layer(&[("var", D), ("var/.wh..wh..opq", F), ("var/fresh", F)])
            .layer(&[("var", D), ("var/stale", F)]);
        let s = Stack::new(&fs, 2);
        assert_eq!(s.resolve("var").unwrap().floor, 0);
        assert!(s.resolve("var/stale").is_none());
        assert_eq!(s.list("var"), ["fresh"]);
    }

    #[test]
    fn a_whiteout_of_a_directory_hides_its_contents() {
        let fs = Mem::default()
            .layer(&[(".wh.opt", F)])
            .layer(&[("opt", D), ("opt/x", F)]);
        let s = Stack::new(&fs, 2);
        assert!(s.resolve("opt").is_none());
        assert!(s.resolve("opt/x").is_none());
    }

    #[test]
    fn a_file_replacing_a_directory_hides_the_old_children() {
        let fs = Mem::default().layer(&[("d", F)]).layer(&[("d", D), ("d/x", F)]);
        let s = Stack::new(&fs, 2);
        assert_eq!(s.resolve("d").unwrap().node, F);
        assert!(s.resolve("d/x").is_none());
    }

    #[test]
    fn a_directory_replacing_a_file_does_not_inherit_it() {
        let fs = Mem::default().layer(&[("d", D), ("d/x", F)]).layer(&[("d", F)]);
        let s = Stack::new(&fs, 2);
        let d = s.resolve("d").unwrap();
        assert_eq!((d.node, d.floor), (D, 0));
        assert_eq!(s.list("d"), ["x"]);
    }

    #[test]
    fn deep_whiteout_inside_a_merged_directory() {
        let fs = Mem::default()
            .layer(&[("usr", D), ("usr/bin", D), ("usr/bin/.wh.old", F), ("usr/bin/new", F)])
            .layer(&[("usr", D), ("usr/bin", D), ("usr/bin/old", F), ("usr/bin/keep", F)]);
        let s = Stack::new(&fs, 2);
        assert_eq!(s.list("usr/bin"), ["keep", "new"]);
        assert!(s.resolve("usr/bin/old").is_none());
    }

    #[test]
    fn an_empty_stack_has_no_root() {
        let fs = Mem::default();
        assert!(Stack::new(&fs, 0).resolve("").is_none());
        assert_eq!(Stack::new(&fs, 0).list(""), Vec::<String>::new());
        let _ = vec![0];
    }
}
