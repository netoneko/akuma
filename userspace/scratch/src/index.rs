//! Git index (staging area) management
//!
//! Implements a simple text-based index format for staging files before commit.
//! Format: `<mode> <sha> <path>` per line, with a header line.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{close, getcwd, open, open_flags, read_dir, read_fd, write_fd};

use crate::error::{Error, Result};
use crate::object::{Object, Tree, TreeEntry};
use crate::sha1::{self, Sha1Hash};
use crate::store::ObjectStore;

/// Convert a path to absolute (for file operations)
fn to_absolute(path: &str) -> String {
    if path.starts_with('/') {
        String::from(path)
    } else {
        let cwd = getcwd();
        if cwd == "/" {
            format!("/{}", path)
        } else {
            format!("{}/{}", cwd, path)
        }
    }
}

/// Convert an absolute path to relative (for storing in index)
/// The path is made relative to the repository root (cwd)
fn to_relative(path: &str) -> String {
    let cwd = getcwd();
    
    // If path starts with cwd, strip it
    if let Some(relative) = path.strip_prefix(&cwd) {
        let relative = relative.strip_prefix('/').unwrap_or(relative);
        if relative.is_empty() {
            String::from(".")
        } else {
            String::from(relative)
        }
    } else {
        // Path doesn't start with cwd, return as-is but strip leading /
        String::from(path.strip_prefix('/').unwrap_or(path))
    }
}

/// A single entry in the index
#[derive(Debug, Clone)]
pub struct IndexEntry {
    /// File mode (100644 for regular file, 100755 for executable, 040000 for directory)
    pub mode: u32,
    /// SHA-1 hash of the blob
    pub sha: Sha1Hash,
    /// Path relative to repository root
    pub path: String,
}

/// The staging index
#[derive(Debug, Clone, Default)]
pub struct Index {
    /// Entries in the index, keyed by path for quick lookup
    entries: BTreeMap<String, IndexEntry>,
}

impl Index {
    /// Create a new empty index
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Load the index from .git/index
    pub fn load(git_dir: &str) -> Result<Self> {
        let path = format!("{}/index", git_dir);
        let fd = open(&path, open_flags::O_RDONLY);
        
        if fd < 0 {
            // No index file - return empty index
            return Ok(Self::new());
        }

        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        
        loop {
            let n = read_fd(fd, &mut buf);
            if n <= 0 {
                break;
            }
            data.extend_from_slice(&buf[..n as usize]);
        }
        
        close(fd);

        let content = String::from_utf8(data)
            .map_err(|_| Error::io("index file not valid UTF-8"))?;

        Self::parse(&content)
    }

    /// Parse index content from string
    fn parse(content: &str) -> Result<Self> {
        let mut entries = BTreeMap::new();

        for line in content.lines() {
            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Parse: <mode> <sha> <path>
            let mut parts = line.splitn(3, ' ');
            
            let mode_str = parts.next()
                .ok_or_else(|| Error::io("invalid index entry: missing mode"))?;
            let sha_str = parts.next()
                .ok_or_else(|| Error::io("invalid index entry: missing sha"))?;
            let path = parts.next()
                .ok_or_else(|| Error::io("invalid index entry: missing path"))?;

            let mode = u32::from_str_radix(mode_str, 8)
                .map_err(|_| Error::io("invalid index entry: bad mode"))?;
            let sha = sha1::from_hex(sha_str)
                .ok_or_else(|| Error::io("invalid index entry: bad sha"))?;

            entries.insert(String::from(path), IndexEntry {
                mode,
                sha,
                path: String::from(path),
            });
        }

        Ok(Self { entries })
    }

    /// Save the index to .git/index
    pub fn save(&self, git_dir: &str) -> Result<()> {
        let path = format!("{}/index", git_dir);
        let fd = open(&path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
        
        if fd < 0 {
            return Err(Error::io("failed to create index file"));
        }

        // Write header
        let header = b"# scratch index v1\n";
        write_fd(fd, header);

        // Write entries (sorted by path due to BTreeMap)
        for entry in self.entries.values() {
            let line = format!("{:o} {} {}\n", entry.mode, sha1::to_hex(&entry.sha), entry.path);
            write_fd(fd, line.as_bytes());
        }

        close(fd);
        Ok(())
    }

    /// Clear the index (remove all entries)
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Check if the index is empty
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get the number of entries
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Add a file to the index
    /// 
    /// This creates a blob object for the file and adds it to the index.
    /// The path can be relative or absolute; it will be converted appropriately.
    /// Returns 1 if the file was successfully staged.
    pub fn add_file(&mut self, path: &str, store: &ObjectStore) -> Result<usize> {
        // Convert to absolute path for file operations
        let abs_path = to_absolute(path);
        
        // DEBUG
        libakuma::print("DEBUG add_file: path=");
        libakuma::print(path);
        libakuma::print(" abs_path=");
        libakuma::print(&abs_path);
        libakuma::print(" cwd=");
        libakuma::print(&getcwd());
        libakuma::print("\n");
        
        // Read file content
        let fd = open(&abs_path, open_flags::O_RDONLY);
        if fd < 0 {
            return Err(Error::io("failed to open file"));
        }

        let mut content = Vec::new();
        let mut buf = [0u8; 4096];

        loop {
            let n = read_fd(fd, &mut buf);
            if n <= 0 {
                break;
            }
            content.extend_from_slice(&buf[..n as usize]);
        }

        close(fd);

        // Create blob object
        let blob_obj = Object::Blob(content);
        let sha = store.write(&blob_obj)?;

        // Convert to relative path for storing in index
        let relative_path = to_relative(&abs_path);
        
        // DEBUG
        libakuma::print("DEBUG add_file: relative_path=");
        libakuma::print(&relative_path);
        libakuma::print("\n");
        
        // Skip "." path
        if relative_path == "." {
            libakuma::print("DEBUG add_file: skipping . path\n");
            return Ok(0);
        }

        // Add to index
        self.entries.insert(relative_path.clone(), IndexEntry {
            mode: 0o100644, // Regular file
            sha,
            path: relative_path,
        });

        Ok(1)
    }

    /// Add a directory recursively to the index
    /// Returns the number of files staged.
    pub fn add_directory(&mut self, path: &str, store: &ObjectStore) -> Result<usize> {
        // Convert to absolute path for directory operations
        let abs_path = to_absolute(path);
        
        let dir = read_dir(&abs_path).ok_or_else(|| Error::io("failed to read directory"))?;

        let mut count = 0;
        for entry in dir {
            // Skip hidden files and .git directory
            if entry.name.starts_with('.') {
                continue;
            }

            let entry_path = format!("{}/{}", abs_path, entry.name);

            if entry.is_dir {
                // Recursively add directory
                count += self.add_directory(&entry_path, store)?;
            } else {
                // Add file
                count += self.add_file(&entry_path, store)?;
            }
        }

        Ok(count)
    }

    /// Add a path (file or directory) to the index
    /// Returns the number of files staged.
    pub fn add_path(&mut self, path: &str, store: &ObjectStore) -> Result<usize> {
        // Convert to absolute path for checking
        let abs_path = to_absolute(path);        
        let is_dir = read_dir(&abs_path).is_some();
        
        // Check if path is a directory or file
        if is_dir {
            self.add_directory(&abs_path, store)
        } else {
            self.add_file(&abs_path, store)
        }
    }

    /// Build a tree object from the index entries
    /// 
    /// Converts the flat list of paths into a nested tree structure.
    pub fn build_tree(&self, store: &ObjectStore) -> Result<Sha1Hash> {
        // Group entries by their top-level directory
        let mut root_entries: Vec<TreeEntry> = Vec::new();
        let mut subdirs: BTreeMap<String, Vec<&IndexEntry>> = BTreeMap::new();

        for entry in self.entries.values() {
            // Normalize path: strip leading slash if present
            let path = entry.path.strip_prefix('/').unwrap_or(&entry.path);
            
            // Skip entries with empty paths
            if path.is_empty() {
                continue;
            }
            
            if let Some(slash_pos) = path.find('/') {
                // Entry is in a subdirectory
                let dir_name = &path[..slash_pos];
                
                // Skip if directory name is empty (malformed path like "/foo")
                if dir_name.is_empty() {
                    continue;
                }
                
                subdirs.entry(String::from(dir_name))
                    .or_insert_with(Vec::new)
                    .push(entry);
            } else {
                // Entry is at root level - skip if name is empty
                if path.is_empty() {
                    continue;
                }
                root_entries.push(TreeEntry {
                    mode: entry.mode,
                    name: String::from(path),
                    sha: entry.sha,
                });
            }
        }

        // Recursively build trees for subdirectories
        for (dir_name, dir_entries) in subdirs {
            let subtree_sha = self.build_subtree(&dir_name, &dir_entries, store)?;
            root_entries.push(TreeEntry {
                mode: 0o040000, // Directory
                name: dir_name,
                sha: subtree_sha,
            });
        }

        // Sort entries (Git requires this)
        root_entries.sort_by(|a, b| {
            let a_name = if a.mode == 0o040000 {
                format!("{}/", a.name)
            } else {
                a.name.clone()
            };
            let b_name = if b.mode == 0o040000 {
                format!("{}/", b.name)
            } else {
                b.name.clone()
            };
            a_name.cmp(&b_name)
        });

        // Create tree object
        let tree = Tree { entries: root_entries };
        let tree_obj = Object::Tree(tree);
        store.write(&tree_obj)
    }

    /// Build a subtree from a subset of entries
    fn build_subtree(&self, prefix: &str, entries: &[&IndexEntry], store: &ObjectStore) -> Result<Sha1Hash> {
        let prefix_with_slash = format!("{}/", prefix);
        
        let mut tree_entries: Vec<TreeEntry> = Vec::new();
        let mut subdirs: BTreeMap<String, Vec<&IndexEntry>> = BTreeMap::new();

        for entry in entries {
            // Normalize the entry path first (strip leading slash)
            let entry_path = entry.path.strip_prefix('/').unwrap_or(&entry.path);
            
            // Strip the prefix from the path
            let relative_path = entry_path.strip_prefix(&prefix_with_slash)
                .or_else(|| entry_path.strip_prefix(prefix).and_then(|p| p.strip_prefix('/')))
                .unwrap_or(entry_path);
            
            // Skip empty paths
            if relative_path.is_empty() {
                continue;
            }

            if let Some(slash_pos) = relative_path.find('/') {
                // Entry is in a deeper subdirectory
                let dir_name = &relative_path[..slash_pos];
                
                // Skip if directory name is empty
                if dir_name.is_empty() {
                    continue;
                }
                
                subdirs.entry(String::from(dir_name))
                    .or_insert_with(Vec::new)
                    .push(*entry);
            } else {
                // Entry is directly in this directory - skip if name is empty
                if relative_path.is_empty() {
                    continue;
                }
                tree_entries.push(TreeEntry {
                    mode: entry.mode,
                    name: String::from(relative_path),
                    sha: entry.sha,
                });
            }
        }

        // Recursively build trees for subdirectories
        for (dir_name, dir_entries) in subdirs {
            let full_prefix = format!("{}/{}", prefix, dir_name);
            let subtree_sha = self.build_subtree(&full_prefix, &dir_entries, store)?;
            tree_entries.push(TreeEntry {
                mode: 0o040000,
                name: dir_name,
                sha: subtree_sha,
            });
        }

        // Sort entries
        tree_entries.sort_by(|a, b| {
            let a_name = if a.mode == 0o040000 {
                format!("{}/", a.name)
            } else {
                a.name.clone()
            };
            let b_name = if b.mode == 0o040000 {
                format!("{}/", b.name)
            } else {
                b.name.clone()
            };
            a_name.cmp(&b_name)
        });

        let tree = Tree { entries: tree_entries };
        let tree_obj = Object::Tree(tree);
        store.write(&tree_obj)
    }

    /// Get all entries (for iteration)
    pub fn entries(&self) -> impl Iterator<Item = &IndexEntry> {
        self.entries.values()
    }
}
