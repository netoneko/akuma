//! Commit functionality for scratch
//!
//! Implements creating commits from staged changes or working directory.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{close, open, open_flags, print, read_dir, read_fd, time};

use crate::config::GitConfig;
use crate::error::{Error, Result};
use crate::index::Index;
use crate::object::{Commit, Object, Tree, TreeEntry};
use crate::refs::RefManager;
use crate::sha1::Sha1Hash;
use crate::store::ObjectStore;

/// Create a commit from staged changes (or working directory if index is empty)
///
/// # Arguments
/// * `message` - The commit message
/// * `author_name` - Optional author name (uses config or default if None)
/// * `author_email` - Optional author email (uses config or default if None)
/// * `amend` - If true, amend the last commit instead of creating a new one
pub fn create_commit(
    message: &str,
    author_name: Option<&str>,
    author_email: Option<&str>,
    amend: bool,
) -> Result<Sha1Hash> {
    let git_dir = crate::git_dir();
    let store = ObjectStore::new(&git_dir);
    let refs = RefManager::new(&git_dir);

    // Load config for user identity
    let config = GitConfig::load().unwrap_or_default();

    // Load index to check for staged files
    let mut index = Index::load(&git_dir).unwrap_or_default();

    // Determine parent commit(s)
    let parents = if amend {
        // For amend, use the parent(s) of the current HEAD
        if let Ok(head_sha) = refs.resolve_head() {
            let head_obj = store.read(&head_sha)?;
            let head_commit = head_obj.as_commit()?;
            head_commit.parents.clone()
        } else {
            Vec::new()
        }
    } else {
        // Normal commit: current HEAD is the parent
        refs.resolve_head().ok().map(|p| alloc::vec![p]).unwrap_or_default()
    };

    // Build tree from index if it has entries, otherwise from working directory
    let tree_sha = if index.is_empty() {
        // Fallback: commit all files (legacy behavior)
        print("[commit] No staged files, committing working directory\n");
        let repo_root = crate::repo_path(".");
        build_tree_from_directory(&repo_root, &store)?
    } else {
        // Build tree by merging staged files with parent tree
        // This is the correct Git behavior: staged changes are applied on top of parent
        print(&format!("[commit] {} staged file(s)\n", index.len()));
        for entry in index.entries() {
            print(&format!("[commit]   staged: {}\n", entry.path));
        }
        
        if let Some(parent_sha) = parents.first() {
            print(&format!("[commit] Merging with parent {}\n", crate::sha1::to_hex(parent_sha)));
            let parent_obj = store.read(parent_sha)?;
            let parent_commit = parent_obj.as_commit()?;
            
            // Show parent tree contents
            let parent_tree_obj = store.read(&parent_commit.tree)?;
            let parent_tree = parent_tree_obj.as_tree()?;
            print(&format!("[commit] Parent tree has {} entries:\n", parent_tree.entries.len()));
            for entry in &parent_tree.entries {
                let kind = if entry.mode == 0o040000 { "dir" } else { "file" };
                print(&format!("[commit]   {}: {} ({})\n", kind, entry.name, crate::sha1::to_hex(&entry.sha)));
            }
            
            let result_sha = merge_index_with_tree(&index, &parent_commit.tree, &store)?;
            
            // Show result tree contents
            let result_tree_obj = store.read(&result_sha)?;
            let result_tree = result_tree_obj.as_tree()?;
            print(&format!("[commit] Result tree has {} entries:\n", result_tree.entries.len()));
            for entry in &result_tree.entries {
                let kind = if entry.mode == 0o040000 { "dir" } else { "file" };
                print(&format!("[commit]   {}: {} ({})\n", kind, entry.name, crate::sha1::to_hex(&entry.sha)));
            }
            
            result_sha
        } else {
            // No parent (initial commit) - just use staged files
            print("[commit] Initial commit - no parent to merge with\n");
            index.build_tree(&store)?
        }
    };

    // Build author/committer lines (priority: argument > config > default)
    let name = author_name.unwrap_or_else(|| config.get_user_name());
    let email = author_email.unwrap_or_else(|| config.get_user_email());
    let timestamp = time();
    let author_line = format!("{} <{}> {} +0000", name, email, timestamp);
    let committer_line = author_line.clone();

    // Create commit object
    let commit = Commit {
        tree: tree_sha,
        parents,
        author: author_line,
        committer: committer_line,
        message: String::from(message),
    };

    let commit_obj = Object::Commit(commit);
    let commit_sha = store.write(&commit_obj)?;

    // Update current branch to point to new commit
    update_current_branch(&refs, &commit_sha)?;

    // Clear the index after successful commit
    if !index.is_empty() {
        index.clear();
        let _ = index.save(&git_dir);
    }

    Ok(commit_sha)
}

/// Merge staged index entries with a parent tree
/// 
/// This creates a new tree that contains:
/// - All entries from the parent tree that aren't overwritten by staged changes
/// - All staged entries (replacing any matching paths from parent)
fn merge_index_with_tree(index: &Index, parent_tree_sha: &Sha1Hash, store: &ObjectStore) -> Result<Sha1Hash> {
    // Load parent tree
    let parent_obj = store.read(parent_tree_sha)?;
    let parent_tree = parent_obj.as_tree()?;
    
    // Build a map of staged entries by their top-level component
    // e.g., "foo/bar.txt" -> top-level is "foo"
    let mut staged_top_level: BTreeMap<String, Vec<(String, &crate::index::IndexEntry)>> = BTreeMap::new();
    let mut staged_root_files: BTreeMap<String, &crate::index::IndexEntry> = BTreeMap::new();
    
    for entry in index.entries() {
        let path = entry.path.strip_prefix('/').unwrap_or(&entry.path);
        if path.is_empty() {
            continue;
        }
        
        if let Some(slash_pos) = path.find('/') {
            let top = &path[..slash_pos];
            let rest = &path[slash_pos + 1..];
            staged_top_level.entry(String::from(top))
                .or_default()
                .push((String::from(rest), entry));
        } else {
            staged_root_files.insert(String::from(path), entry);
        }
    }
    
    let mut new_entries: Vec<TreeEntry> = Vec::new();
    let mut processed_dirs: alloc::collections::BTreeSet<String> = alloc::collections::BTreeSet::new();
    
    // Process parent tree entries
    for parent_entry in &parent_tree.entries {
        if parent_entry.is_submodule() {
            // Submodule entries are preserved as-is — scratch does not manage submodules
            print("scratch: warning: preserving submodule entry: ");
            print(&parent_entry.name);
            print("\n");
            new_entries.push(parent_entry.clone());
        } else if parent_entry.mode == 0o040000 {
            // Directory - check if we have staged entries that modify it
            if let Some(staged_in_dir) = staged_top_level.get(&parent_entry.name) {
                // Recursively merge this subdirectory
                let new_subtree_sha = merge_subtree_with_staged(
                    &parent_entry.sha,
                    staged_in_dir,
                    store
                )?;
                new_entries.push(TreeEntry {
                    mode: 0o040000,
                    name: parent_entry.name.clone(),
                    sha: new_subtree_sha,
                });
                processed_dirs.insert(parent_entry.name.clone());
            } else {
                // No staged changes in this directory - keep as-is
                new_entries.push(parent_entry.clone());
            }
        } else {
            // File - check if it's overwritten by a staged file
            if let Some(staged_entry) = staged_root_files.get(&parent_entry.name) {
                // Replace with staged version
                new_entries.push(TreeEntry {
                    mode: staged_entry.mode,
                    name: parent_entry.name.clone(),
                    sha: staged_entry.sha,
                });
            } else {
                // Not staged - keep parent version
                new_entries.push(parent_entry.clone());
            }
        }
    }
    
    // Add new root-level files that weren't in parent
    for (name, staged_entry) in &staged_root_files {
        let exists = new_entries.iter().any(|e| &e.name == name);
        if !exists {
            new_entries.push(TreeEntry {
                mode: staged_entry.mode,
                name: name.clone(),
                sha: staged_entry.sha,
            });
        }
    }
    
    // Add new directories that weren't in parent
    for (dir_name, staged_entries) in &staged_top_level {
        if !processed_dirs.contains(dir_name) {
            // New directory - build tree from staged entries only
            let subtree_sha = build_tree_from_staged_entries(staged_entries, store)?;
            new_entries.push(TreeEntry {
                mode: 0o040000,
                name: dir_name.clone(),
                sha: subtree_sha,
            });
        }
    }
    
    // Sort entries (Git requires this)
    new_entries.sort_by(|a, b| {
        let a_name = if a.mode == 0o040000 { format!("{}/", a.name) } else { a.name.clone() };
        let b_name = if b.mode == 0o040000 { format!("{}/", b.name) } else { b.name.clone() };
        a_name.cmp(&b_name)
    });
    
    let tree = Tree { entries: new_entries };
    store.write(&Object::Tree(tree))
}

/// Recursively merge a subdirectory with staged entries
fn merge_subtree_with_staged(
    parent_tree_sha: &Sha1Hash,
    staged_entries: &[(String, &crate::index::IndexEntry)],
    store: &ObjectStore,
) -> Result<Sha1Hash> {
    let parent_obj = store.read(parent_tree_sha)?;
    let parent_tree = parent_obj.as_tree()?;
    
    // Group staged entries by their next path component
    let mut staged_subdirs: BTreeMap<String, Vec<(String, &crate::index::IndexEntry)>> = BTreeMap::new();
    let mut staged_files: BTreeMap<String, &crate::index::IndexEntry> = BTreeMap::new();
    
    for (path, entry) in staged_entries {
        if let Some(slash_pos) = path.find('/') {
            let top = &path[..slash_pos];
            let rest = &path[slash_pos + 1..];
            staged_subdirs.entry(String::from(top))
                .or_default()
                .push((String::from(rest), *entry));
        } else {
            staged_files.insert(path.clone(), *entry);
        }
    }
    
    let mut new_entries: Vec<TreeEntry> = Vec::new();
    let mut processed_dirs: alloc::collections::BTreeSet<String> = alloc::collections::BTreeSet::new();
    
    for parent_entry in &parent_tree.entries {
        if parent_entry.is_submodule() {
            // Preserve submodule entries unchanged
            new_entries.push(parent_entry.clone());
        } else if parent_entry.mode == 0o040000 {
            if let Some(staged_in_dir) = staged_subdirs.get(&parent_entry.name) {
                let new_subtree_sha = merge_subtree_with_staged(
                    &parent_entry.sha,
                    staged_in_dir,
                    store
                )?;
                new_entries.push(TreeEntry {
                    mode: 0o040000,
                    name: parent_entry.name.clone(),
                    sha: new_subtree_sha,
                });
                processed_dirs.insert(parent_entry.name.clone());
            } else {
                new_entries.push(parent_entry.clone());
            }
        } else {
            if let Some(staged_entry) = staged_files.get(&parent_entry.name) {
                new_entries.push(TreeEntry {
                    mode: staged_entry.mode,
                    name: parent_entry.name.clone(),
                    sha: staged_entry.sha,
                });
            } else {
                new_entries.push(parent_entry.clone());
            }
        }
    }
    
    // Add new files
    for (name, staged_entry) in &staged_files {
        let exists = new_entries.iter().any(|e| &e.name == name);
        if !exists {
            new_entries.push(TreeEntry {
                mode: staged_entry.mode,
                name: name.clone(),
                sha: staged_entry.sha,
            });
        }
    }
    
    // Add new subdirectories
    for (dir_name, entries) in &staged_subdirs {
        if !processed_dirs.contains(dir_name) {
            let subtree_sha = build_tree_from_staged_entries(entries, store)?;
            new_entries.push(TreeEntry {
                mode: 0o040000,
                name: dir_name.clone(),
                sha: subtree_sha,
            });
        }
    }
    
    new_entries.sort_by(|a, b| {
        let a_name = if a.mode == 0o040000 { format!("{}/", a.name) } else { a.name.clone() };
        let b_name = if b.mode == 0o040000 { format!("{}/", b.name) } else { b.name.clone() };
        a_name.cmp(&b_name)
    });
    
    let tree = Tree { entries: new_entries };
    store.write(&Object::Tree(tree))
}

/// Build a tree from staged entries only (for new directories)
fn build_tree_from_staged_entries(
    entries: &[(String, &crate::index::IndexEntry)],
    store: &ObjectStore,
) -> Result<Sha1Hash> {
    let mut subdirs: BTreeMap<String, Vec<(String, &crate::index::IndexEntry)>> = BTreeMap::new();
    let mut files: Vec<TreeEntry> = Vec::new();
    
    for (path, entry) in entries {
        if let Some(slash_pos) = path.find('/') {
            let top = &path[..slash_pos];
            let rest = &path[slash_pos + 1..];
            subdirs.entry(String::from(top))
                .or_default()
                .push((String::from(rest), *entry));
        } else {
            files.push(TreeEntry {
                mode: entry.mode,
                name: path.clone(),
                sha: entry.sha,
            });
        }
    }
    
    let mut tree_entries = files;
    
    for (dir_name, dir_entries) in subdirs {
        let subtree_sha = build_tree_from_staged_entries(&dir_entries, store)?;
        tree_entries.push(TreeEntry {
            mode: 0o040000,
            name: dir_name,
            sha: subtree_sha,
        });
    }
    
    tree_entries.sort_by(|a, b| {
        let a_name = if a.mode == 0o040000 { format!("{}/", a.name) } else { a.name.clone() };
        let b_name = if b.mode == 0o040000 { format!("{}/", b.name) } else { b.name.clone() };
        a_name.cmp(&b_name)
    });
    
    let tree = Tree { entries: tree_entries };
    store.write(&Object::Tree(tree))
}

/// Build a tree object from a directory
fn build_tree_from_directory(path: &str, store: &ObjectStore) -> Result<Sha1Hash> {
    let mut entries: Vec<TreeEntry> = Vec::new();

    let dir = read_dir(path).ok_or_else(|| Error::io("failed to read directory"))?;

    for entry in dir {
        // Skip hidden files and .git directory
        if entry.name.starts_with('.') {
            continue;
        }

        let entry_path = if path == "." {
            entry.name.clone()
        } else {
            format!("{}/{}", path, entry.name)
        };

        if entry.is_dir {
            // Recursively build tree for subdirectory
            let subtree_sha = build_tree_from_directory(&entry_path, store)?;
            entries.push(TreeEntry {
                mode: 0o040000, // Directory mode
                name: entry.name,
                sha: subtree_sha,
            });
        } else {
            // Read file and create blob
            let blob_sha = create_blob_from_file(&entry_path, store)?;
            entries.push(TreeEntry {
                mode: 0o100644, // Regular file mode
                name: entry.name,
                sha: blob_sha,
            });
        }
    }

    // Sort entries by name (Git requires this)
    entries.sort_by(|a, b| {
        // Git sorts directories as if they have a trailing slash
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
    let tree = Tree { entries };
    let tree_obj = Object::Tree(tree);
    store.write(&tree_obj)
}

/// Create a blob object from a file
fn create_blob_from_file(path: &str, store: &ObjectStore) -> Result<Sha1Hash> {
    let fd = open(path, open_flags::O_RDONLY);
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
    store.write(&blob_obj)
}

/// Update the current branch to point to a new commit
fn update_current_branch(refs: &RefManager, commit_sha: &Sha1Hash) -> Result<()> {
    let head = refs.read_head()?;
    let head = head.trim();

    if let Some(ref_path) = head.strip_prefix("ref: ") {
        // HEAD points to a branch - update it
        if let Some(branch_name) = ref_path.strip_prefix("refs/heads/") {
            refs.write_branch(branch_name, commit_sha)?;
        } else {
            return Err(Error::io("HEAD points to non-branch ref"));
        }
    } else {
        // Detached HEAD - update HEAD directly
        refs.set_head_detached(commit_sha)?;
    }

    Ok(())
}

/// Get the current branch name (if HEAD points to a branch)
pub fn current_branch() -> Result<Option<String>> {
    let refs = RefManager::new(&crate::git_dir());
    let head = refs.read_head()?;
    let head = head.trim();

    if let Some(ref_path) = head.strip_prefix("ref: ") {
        if let Some(branch_name) = ref_path.strip_prefix("refs/heads/") {
            return Ok(Some(String::from(branch_name)));
        }
    }

    Ok(None)
}
