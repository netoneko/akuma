//! Git reference management
//!
//! Handles reading and writing refs (branches, tags, HEAD)

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{close, mkdir, open, open_flags, read_dir, read_fd, write_fd};

use crate::error::{Error, Result};
use crate::sha1::{self, Sha1Hash};

/// Reference manager
pub struct RefManager {
    git_dir: String,
}

impl RefManager {
    pub fn new(git_dir: &str) -> Self {
        Self {
            git_dir: String::from(git_dir),
        }
    }

    /// Initialize ref directories
    pub fn init(&self) -> Result<()> {
        let _ = mkdir(&format!("{}/refs", self.git_dir));
        let _ = mkdir(&format!("{}/refs/heads", self.git_dir));
        let _ = mkdir(&format!("{}/refs/tags", self.git_dir));
        let _ = mkdir(&format!("{}/refs/remotes", self.git_dir));
        Ok(())
    }

    /// Read HEAD reference
    pub fn read_head(&self) -> Result<String> {
        // Try uppercase first (standard), then lowercase (some filesystems)
        let path = format!("{}/HEAD", self.git_dir);
        match read_file_content(&path) {
            Ok(content) => Ok(content),
            Err(_) => {
                // Try lowercase as fallback
                let path_lower = format!("{}/head", self.git_dir);
                read_file_content(&path_lower)
            }
        }
    }

    /// Write HEAD reference
    pub fn write_head(&self, content: &str) -> Result<()> {
        let path = format!("{}/HEAD", self.git_dir);
        write_file_content(&path, content)
    }

    /// Set HEAD to point to a branch
    pub fn set_head_branch(&self, branch: &str) -> Result<()> {
        let content = format!("ref: refs/heads/{}\n", branch);
        self.write_head(&content)
    }

    /// Set HEAD to a detached commit
    pub fn set_head_detached(&self, sha: &Sha1Hash) -> Result<()> {
        let content = format!("{}\n", sha1::to_hex(sha));
        self.write_head(&content)
    }

    /// Resolve HEAD to a SHA (follows symbolic refs)
    pub fn resolve_head(&self) -> Result<Sha1Hash> {
        let head = self.read_head()?;
        let head = head.trim();
        
        if let Some(ref_path) = head.strip_prefix("ref: ") {
            // Symbolic ref
            self.resolve_ref(ref_path)
        } else {
            // Direct SHA
            sha1::from_hex(head)
                .ok_or_else(|| Error::invalid_object("invalid SHA in HEAD"))
        }
    }

    /// Resolve a reference to a SHA
    pub fn resolve_ref(&self, ref_path: &str) -> Result<Sha1Hash> {
        let path = format!("{}/{}", self.git_dir, ref_path);
        let content = read_file_content(&path)?;
        let sha_hex = content.trim();
        
        sha1::from_hex(sha_hex)
            .ok_or_else(|| Error::invalid_object("invalid SHA in ref"))
    }

    /// Read a branch ref
    pub fn read_branch(&self, name: &str) -> Result<Sha1Hash> {
        self.resolve_ref(&format!("refs/heads/{}", name))
    }

    /// Write a branch ref
    pub fn write_branch(&self, name: &str, sha: &Sha1Hash) -> Result<()> {
        let path = format!("{}/refs/heads/{}", self.git_dir, name);
        let content = format!("{}\n", sha1::to_hex(sha));
        write_file_content(&path, &content)
    }

    /// Delete a branch
    pub fn delete_branch_ref(&self, name: &str) -> Result<()> {
        let path = format!("{}/refs/heads/{}", self.git_dir, name);
        // We can't actually delete files yet, so we'll just check if it exists
        // and return an error if not
        let _ = self.read_branch(name)?;
        // TODO: Actually delete the file when libakuma supports it
        Err(Error::io("file deletion not yet supported"))
    }

    /// Read a tag ref
    pub fn read_tag(&self, name: &str) -> Result<Sha1Hash> {
        self.resolve_ref(&format!("refs/tags/{}", name))
    }

    /// Write a tag ref (lightweight tag)
    pub fn write_tag(&self, name: &str, sha: &Sha1Hash) -> Result<()> {
        let path = format!("{}/refs/tags/{}", self.git_dir, name);
        let content = format!("{}\n", sha1::to_hex(sha));
        write_file_content(&path, &content)
    }

    /// Delete a tag
    pub fn delete_tag_ref(&self, name: &str) -> Result<()> {
        let _ = self.read_tag(name)?;
        Err(Error::io("file deletion not yet supported"))
    }

    /// Write a remote-tracking ref
    pub fn write_remote_ref(&self, remote: &str, ref_name: &str, sha: &Sha1Hash) -> Result<()> {
        let dir = format!("{}/refs/remotes/{}", self.git_dir, remote);
        let _ = mkdir(&dir);
        
        let path = format!("{}/{}", dir, ref_name);
        let content = format!("{}\n", sha1::to_hex(sha));
        write_file_content(&path, &content)
    }

    /// List all branches
    pub fn list_branches_refs(&self) -> Result<Vec<(String, Sha1Hash)>> {
        let path = format!("{}/refs/heads", self.git_dir);
        list_refs_in_dir(&path)
    }

    /// List all tags
    pub fn list_tags_refs(&self) -> Result<Vec<(String, Sha1Hash)>> {
        let path = format!("{}/refs/tags", self.git_dir);
        list_refs_in_dir(&path)
    }
}

/// Read file content as string
fn read_file_content(path: &str) -> Result<String> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        // Check if this is HEAD not found (likely not a repo)
        if path.ends_with("/HEAD") || path == ".git/HEAD" {
            return Err(Error::not_a_repository());
        }
        return Err(Error::ref_not_found(path));
    }

    let mut data = Vec::new();
    let mut buf = [0u8; 256];
    
    loop {
        let n = read_fd(fd, &mut buf);
        if n <= 0 {
            break;
        }
        data.extend_from_slice(&buf[..n as usize]);
    }
    
    close(fd);
    
    String::from_utf8(data)
        .map_err(|_| Error::invalid_object("file not valid UTF-8"))
}

/// Write string content to file
fn write_file_content(path: &str, content: &str) -> Result<()> {
    let fd = open(path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return Err(Error::io("failed to create file"));
    }
    
    let written = write_fd(fd, content.as_bytes());
    close(fd);
    
    if written < 0 {
        return Err(Error::io("failed to write file"));
    }
    
    Ok(())
}

/// List refs in a directory
fn list_refs_in_dir(path: &str) -> Result<Vec<(String, Sha1Hash)>> {
    let mut refs = Vec::new();
    
    if let Some(entries) = read_dir(path) {
        for entry in entries {
            if entry.name.starts_with('.') {
                continue;
            }
            if entry.is_dir {
                continue; // TODO: Handle nested refs
            }
            
            let ref_path = format!("{}/{}", path, entry.name);
            if let Ok(content) = read_file_content(&ref_path) {
                if let Some(sha) = sha1::from_hex(content.trim()) {
                    refs.push((entry.name, sha));
                }
            }
        }
    }
    
    Ok(refs)
}

// ============================================================================
// Public API functions (used by main.rs)
// ============================================================================

/// List all branches in the current repository
pub fn list_branches() -> Result<Vec<(String, Sha1Hash)>> {
    let refs = RefManager::new(&crate::git_dir());
    refs.list_branches_refs()
}

/// Create a new branch at HEAD
pub fn create_branch(name: &str) -> Result<()> {
    let refs = RefManager::new(&crate::git_dir());
    let head_sha = refs.resolve_head()?;
    refs.write_branch(name, &head_sha)
}

/// Delete a branch
pub fn delete_branch(name: &str) -> Result<()> {
    let refs = RefManager::new(&crate::git_dir());
    refs.delete_branch_ref(name)
}

/// List all tags in the current repository
pub fn list_tags() -> Result<Vec<(String, Sha1Hash)>> {
    let refs = RefManager::new(&crate::git_dir());
    refs.list_tags_refs()
}

/// Create a new tag at HEAD
pub fn create_tag(name: &str) -> Result<()> {
    let refs = RefManager::new(&crate::git_dir());
    let head_sha = refs.resolve_head()?;
    refs.write_tag(name, &head_sha)
}

/// Delete a tag
pub fn delete_tag(name: &str) -> Result<()> {
    let refs = RefManager::new(&crate::git_dir());
    refs.delete_tag_ref(name)
}

/// Read HEAD
pub fn read_head() -> Result<String> {
    let refs = RefManager::new(&crate::git_dir());
    refs.read_head()
}
