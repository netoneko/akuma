//! Commit functionality for scratch
//!
//! Implements creating commits from working directory changes.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{close, open, open_flags, read_dir, read_fd, time};

use crate::config::GitConfig;
use crate::error::{Error, Result};
use crate::object::{Commit, Object, Tree, TreeEntry};
use crate::refs::RefManager;
use crate::sha1::{self, Sha1Hash};
use crate::store::ObjectStore;

/// Create a commit from the current working directory
///
/// # Arguments
/// * `message` - The commit message
/// * `author_name` - Optional author name (uses config or default if None)
/// * `author_email` - Optional author email (uses config or default if None)
pub fn create_commit(
    message: &str,
    author_name: Option<&str>,
    author_email: Option<&str>,
) -> Result<Sha1Hash> {
    let git_dir = crate::git_dir();
    let store = ObjectStore::new(&git_dir);
    let refs = RefManager::new(&git_dir);

    // Load config for user identity
    let config = GitConfig::load().unwrap_or_default();

    // Get current HEAD as parent
    let parent = refs.resolve_head().ok();

    // Build tree from working directory
    let repo_root = crate::repo_path(".");
    let tree_sha = build_tree_from_directory(&repo_root, &store)?;

    // Build author/committer lines (priority: argument > config > default)
    let name = author_name.unwrap_or_else(|| config.get_user_name());
    let email = author_email.unwrap_or_else(|| config.get_user_email());
    let timestamp = time();
    let author_line = format!("{} <{}> {} +0000", name, email, timestamp);
    let committer_line = author_line.clone();

    // Create commit object
    let commit = Commit {
        tree: tree_sha,
        parents: parent.map(|p| alloc::vec![p]).unwrap_or_default(),
        author: author_line,
        committer: committer_line,
        message: String::from(message),
    };

    let commit_obj = Object::Commit(commit);
    let commit_sha = store.write(&commit_obj)?;

    // Update current branch to point to new commit
    update_current_branch(&refs, &commit_sha)?;

    Ok(commit_sha)
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
