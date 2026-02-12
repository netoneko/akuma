//! High-level repository operations
//!
//! Implements clone, fetch, and other Git commands.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{close, mkdir, open, open_flags, print, write_fd};

use crate::base64;
use crate::config::GitConfig;
use crate::error::{Error, Result};
use crate::http::Url;
use crate::pack_write;
use crate::protocol::ProtocolClient;
use crate::refs::RefManager;
use crate::sha1::{self, Sha1Hash};
use crate::store::ObjectStore;


/// Clone a repository from a URL
pub fn clone(url: &str) -> Result<()> {
    // Parse URL
    let parsed_url = Url::parse(url)?;
    
    print("scratch: connecting to ");
    print(&parsed_url.host);
    print("\n");

    // Create protocol client
    let mut client = ProtocolClient::new(parsed_url.clone());

    // Discover refs
    let (refs, caps) = client.discover_refs()?;
    
    if refs.is_empty() {
        return Err(Error::protocol("no refs found"));
    }

    print("scratch: found ");
    print_num(refs.len());
    print(" refs\n");

    // Find HEAD ref
    let head_ref = refs.iter()
        .find(|r| r.name == "HEAD")
        .or_else(|| refs.iter().find(|r| r.name == "refs/heads/main"))
        .or_else(|| refs.iter().find(|r| r.name == "refs/heads/master"))
        .ok_or_else(|| Error::protocol("no HEAD or main branch found"))?;

    print("scratch: HEAD -> ");
    print(&sha1::to_hex(&head_ref.sha));
    print("\n");

    // Collect all refs to fetch
    let wants: Vec<Sha1Hash> = refs.iter()
        .filter(|r| r.name != "HEAD") // Don't want HEAD directly, it's symbolic
        .map(|r| r.sha)
        .collect();

    // Deduplicate
    let mut unique_wants: Vec<Sha1Hash> = Vec::new();
    for want in wants {
        if !unique_wants.contains(&want) {
            unique_wants.push(want);
        }
    }

    print("scratch: requesting ");
    print_num(unique_wants.len());
    print(" objects\n");

    // Extract repo name from URL for directory
    let repo_name = extract_repo_name(&parsed_url.path);
    
    print("scratch: creating directory ");
    print(&repo_name);
    print("\n");

    // Create repository directory
    if mkdir(&repo_name) < 0 {
        // Directory might exist
    }

    // Initialize .git structure
    let git_dir = format!("{}/.git", repo_name);
    init_git_dir(&git_dir)?;

    // Write config
    write_config(&git_dir, url)?;

    // Create object store
    let store = ObjectStore::new(&git_dir);
    store.init()?;

    // Fetch and parse pack using streaming
    print("scratch: fetching and unpacking objects (streaming)\n");
    let object_count = client.fetch_pack_streaming(&unique_wants, &[], &caps, &git_dir)?;

    print("scratch: stored ");
    print_num(object_count as usize);
    print(" objects\n");

    // Create refs
    let ref_manager = RefManager::new(&git_dir);
    ref_manager.init()?;

    for remote_ref in &refs {
        if remote_ref.name == "HEAD" {
            continue;
        }
        
        // Create remote tracking ref
        if let Some(branch_name) = remote_ref.name.strip_prefix("refs/heads/") {
            ref_manager.write_remote_ref("origin", branch_name, &remote_ref.sha)?;
            // Also create local branch for the main branch
            if branch_name == "main" || branch_name == "master" {
                ref_manager.write_branch(branch_name, &remote_ref.sha)?;
            }
        } else if let Some(tag_name) = remote_ref.name.strip_prefix("refs/tags/") {
            ref_manager.write_tag(tag_name, &remote_ref.sha)?;
        }
    }

    // Find the default branch name
    let default_branch = refs.iter()
        .find(|r| r.name == "refs/heads/main")
        .map(|_| "main")
        .or_else(|| refs.iter().find(|r| r.name == "refs/heads/master").map(|_| "master"))
        .unwrap_or("main");

    // Set HEAD
    ref_manager.set_head_branch(default_branch)?;

    print("scratch: HEAD set to ");
    print(default_branch);
    print("\n");

    // Verify HEAD commit exists before checkout
    let head_commit_sha = ref_manager.resolve_head()?;
    print("scratch: HEAD commit: ");
    print(&crate::sha1::to_hex(&head_commit_sha));
    print("\n");

    if !store.exists(&head_commit_sha) {
        print("scratch: WARNING: HEAD commit object not on disk!\n");
    }

    // Checkout working tree
    print("scratch: checking out files\n");
    checkout_tree(&store, &head_commit_sha, &repo_name)?;

    print("scratch: done\n");
    Ok(())
}

/// Fetch updates from origin
pub fn fetch() -> Result<()> {
    let git_dir = crate::git_dir();
    // Check if we're in a repo
    let refs = RefManager::new(&git_dir);
    let _ = refs.read_head()?; // This will fail if not a repo

    // Read remote URL from config
    let git_config = GitConfig::load()?;
    let remote_url_string = git_config.get_remote_url()
        .ok_or_else(|| Error::io("no remote URL in config"))?;
    let remote_url = &remote_url_string;
    let parsed_url = Url::parse(remote_url)?;

    print("scratch: fetching from ");
    print(remote_url);
    print("\n");

    // Create protocol client
    let mut client = ProtocolClient::new(parsed_url);

    // Discover refs
    let (remote_refs, caps) = client.discover_refs()?;

    // Collect local objects we have
    let store = ObjectStore::new(&git_dir);
    let mut haves: Vec<Sha1Hash> = Vec::new();

    // Get all local refs
    for (_, sha) in refs.list_branches_refs()? {
        if store.exists(&sha) {
            haves.push(sha);
        }
    }

    // Collect wants (refs we don't have)
    let mut wants: Vec<Sha1Hash> = Vec::new();
    for remote_ref in &remote_refs {
        if remote_ref.name == "HEAD" {
            continue;
        }
        if !store.exists(&remote_ref.sha) && !wants.contains(&remote_ref.sha) {
            wants.push(remote_ref.sha);
        }
    }

    if wants.is_empty() {
        print("scratch: already up to date\n");
        return Ok(());
    }

    print("scratch: requesting ");
    print_num(wants.len());
    print(" new objects\n");

    // Fetch and parse using streaming
    let object_count = client.fetch_pack_streaming(&wants, &haves, &caps, &git_dir)?;

    print("scratch: stored ");
    print_num(object_count as usize);
    print(" objects\n");

    // Update remote tracking refs
    for remote_ref in &remote_refs {
        if let Some(branch_name) = remote_ref.name.strip_prefix("refs/heads/") {
            refs.write_remote_ref("origin", branch_name, &remote_ref.sha)?;
        }
    }

    print("scratch: fetch complete\n");
    Ok(())
}

/// Pull updates from origin (fetch + fast-forward merge)
pub fn pull() -> Result<()> {
    let git_dir = crate::git_dir();
    let refs = RefManager::new(&git_dir);
    let store = ObjectStore::new(&git_dir);

    // Get current branch name
    let head_content = refs.read_head()?;
    let head_trimmed = head_content.trim();
    let branch_name = head_trimmed
        .strip_prefix("ref: refs/heads/")
        .ok_or_else(|| Error::io("not on a branch (detached HEAD)"))?;

    print("scratch: pulling branch ");
    print(branch_name);
    print("\n");

    // Read remote URL from config
    let git_config = GitConfig::load()?;
    let remote_url_string = git_config.get_remote_url()
        .ok_or_else(|| Error::io("no remote URL in config"))?;
    let remote_url = &remote_url_string;
    let parsed_url = Url::parse(remote_url)?;

    print("scratch: fetching from ");
    print(remote_url);
    print("\n");

    // Create protocol client
    let mut client = ProtocolClient::new(parsed_url);

    // Discover refs
    let (remote_refs, caps) = client.discover_refs()?;

    // Find the remote ref for our branch
    let remote_ref_name = format!("refs/heads/{}", branch_name);
    let remote_sha = remote_refs.iter()
        .find(|r| r.name == remote_ref_name)
        .map(|r| r.sha)
        .ok_or_else(|| Error::ref_not_found(&format!("origin/{}", branch_name)))?;

    // Get local SHA
    let local_sha = refs.read_branch(branch_name)?;

    // Check if already up to date
    if local_sha == remote_sha {
        print("scratch: already up to date\n");
        return Ok(());
    }

    print("scratch: ");
    print(&sha1::to_hex(&local_sha)[..7]);
    print(" -> ");
    print(&sha1::to_hex(&remote_sha)[..7]);
    print("\n");

    // Collect local objects we have
    let mut haves: Vec<Sha1Hash> = Vec::new();
    for (_, sha) in refs.list_branches_refs()? {
        if store.exists(&sha) {
            haves.push(sha);
        }
    }

    // Check if we need to fetch new objects
    if !store.exists(&remote_sha) {
        // Collect wants (refs we don't have)
        let mut wants: Vec<Sha1Hash> = Vec::new();
        for remote_ref in &remote_refs {
            if remote_ref.name == "HEAD" {
                continue;
            }
            if !store.exists(&remote_ref.sha) && !wants.contains(&remote_ref.sha) {
                wants.push(remote_ref.sha);
            }
        }

        if !wants.is_empty() {
            print("scratch: requesting ");
            print_num(wants.len());
            print(" new objects\n");

            // Fetch and parse using streaming
            let object_count = client.fetch_pack_streaming(&wants, &haves, &caps, &git_dir)?;

            print("scratch: stored ");
            print_num(object_count as usize);
            print(" objects\n");
        }
    }

    // Update remote tracking ref
    refs.write_remote_ref("origin", branch_name, &remote_sha)?;

    // Fast-forward: update local branch to remote SHA
    // Note: This is a simple fast-forward, no merge support
    // TODO: Could verify that local_sha is an ancestor of remote_sha
    refs.write_branch(branch_name, &remote_sha)?;

    // Checkout the updated tree
    print("scratch: checking out files\n");
    let repo_root = crate::repo_path(".");
    checkout_tree(&store, &remote_sha, &repo_root)?;

    print("scratch: pull complete\n");
    Ok(())
}

/// Checkout a branch
pub fn checkout(branch_name: &str) -> Result<()> {
    let git_dir = crate::git_dir();
    let refs = RefManager::new(&git_dir);
    let store = ObjectStore::new(&git_dir);

    // Resolve branch to SHA
    let branch_sha = refs.read_branch(branch_name)?;

    // Update working directory
    let repo_root = crate::repo_path(".");
    checkout_tree(&store, &branch_sha, &repo_root)?;

    // Update HEAD to point to the branch
    refs.set_head_branch(branch_name)?;

    Ok(())
}

/// Push a branch to origin
/// 
/// If `branch` is Some, push that branch. Otherwise, push the current branch from HEAD.
pub fn push(token: Option<&str>, branch: Option<&str>) -> Result<()> {
    let git_dir = crate::git_dir();
    let refs = RefManager::new(&git_dir);
    let store = ObjectStore::new(&git_dir);

    // Determine the branch name - either from argument or from HEAD
    let head_content;
    let branch_name = match branch {
        Some(name) => name,
        None => {
            // Get current branch name from HEAD
            head_content = refs.read_head()?;
            let head_trimmed = head_content.trim();
            head_trimmed
                .strip_prefix("ref: refs/heads/")
                .ok_or_else(|| Error::io("not on a branch (detached HEAD)"))?
        }
    };

    print("scratch: pushing branch ");
    print(branch_name);
    print("\n");

    // Get local commit SHA
    let local_sha = refs.read_branch(branch_name)?;

    // Load git config (includes remote URL and optional credential token)
    let git_config = GitConfig::load()?;
    let remote_url_string = git_config.get_remote_url()
        .ok_or_else(|| Error::io("no remote URL in config"))?;
    let remote_url = &remote_url_string;
    let parsed_url = Url::parse(remote_url)?;

    // Use token from argument, or fall back to config
    let config_token = git_config.get_credential_token();
    let effective_token = token.or(config_token.as_deref());
    
    // Create auth header if token available
    let auth = effective_token.map(|t| base64::basic_auth("git", t));
    let auth_ref = auth.as_deref();

    // Create protocol client
    let mut client = ProtocolClient::new(parsed_url);

    // Discover remote refs
    let (remote_refs, caps) = client.discover_refs_for_push(auth_ref)?;

    // Find remote ref for this branch
    let ref_name = format!("refs/heads/{}", branch_name);
    let old_sha = remote_refs
        .iter()
        .find(|r| r.name == ref_name)
        .map(|r| r.sha)
        .unwrap_or([0u8; 20]); // Zero SHA for new branch

    // Check if already up to date
    if old_sha == local_sha {
        print("scratch: already up to date\n");
        return Ok(());
    }

    // Check for non-fast-forward (if old_sha is not zero and not an ancestor)
    // For now, we'll let the server reject non-fast-forward pushes

    print("scratch: ");
    print(&sha1::to_hex(&old_sha)[..7]);
    print(" -> ");
    print(&sha1::to_hex(&local_sha)[..7]);
    print("\n");

    // Collect objects to push
    // Get list of objects remote already has
    let have: Vec<Sha1Hash> = remote_refs.iter().map(|r| r.sha).collect();
    
    let objects = pack_write::collect_objects_for_push(&local_sha, &have, &store)?;

    print("scratch: packing ");
    print_num(objects.len());
    print(" objects\n");

    // Create pack file
    let pack_data = pack_write::create_pack(&objects, &store)?;

    print("scratch: pack size ");
    print_num(pack_data.len());
    print(" bytes\n");

    // Push to remote
    client.push_pack(&old_sha, &local_sha, &ref_name, &pack_data, &caps, auth_ref)?;

    // Update remote tracking ref
    refs.write_remote_ref("origin", branch_name, &local_sha)?;

    Ok(())
}

/// Initialize the .git directory structure
fn init_git_dir(git_dir: &str) -> Result<()> {
    let _ = mkdir(git_dir);
    let _ = mkdir(&format!("{}/objects", git_dir));
    let _ = mkdir(&format!("{}/objects/pack", git_dir));
    let _ = mkdir(&format!("{}/objects/info", git_dir));
    let _ = mkdir(&format!("{}/refs", git_dir));
    let _ = mkdir(&format!("{}/refs/heads", git_dir));
    let _ = mkdir(&format!("{}/refs/tags", git_dir));
    let _ = mkdir(&format!("{}/refs/remotes", git_dir));
    let _ = mkdir(&format!("{}/refs/remotes/origin", git_dir));
    Ok(())
}

/// Write repository config
fn write_config(git_dir: &str, remote_url: &str) -> Result<()> {
    let config_content = format!(
        "[core]\n\
         \trepositoryformatversion = 0\n\
         \tfilemode = true\n\
         \tbare = false\n\
         [remote \"origin\"]\n\
         \turl = {}\n\
         \tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        remote_url
    );

    let path = format!("{}/config", git_dir);
    let fd = open(&path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        return Err(Error::io("failed to create config"));
    }
    
    let _ = write_fd(fd, config_content.as_bytes());
    close(fd);
    Ok(())
}

/// Extract repository name from URL path
fn extract_repo_name(path: &str) -> String {
    // Remove .git suffix
    let path = path.strip_suffix(".git").unwrap_or(path);
    
    // Get last component
    let name = path.rsplit('/').next().unwrap_or("repo");
    
    if name.is_empty() {
        String::from("repo")
    } else {
        String::from(name)
    }
}

/// Checkout the tree for a commit
fn checkout_tree(store: &ObjectStore, commit_sha: &Sha1Hash, dest: &str) -> Result<()> {
    // Read commit
    let commit_obj = match store.read(commit_sha) {
        Ok(obj) => obj,
        Err(e) => {
            print("scratch: checkout: commit ");
            print(&crate::sha1::to_hex(commit_sha));
            print(": ");
            print(e.message());
            print("\n");
            return Err(e);
        }
    };
    let commit = commit_obj.as_commit()?;

    // Checkout tree with progress tracking
    let mut file_count: usize = 0;
    checkout_tree_recursive(store, &commit.tree, dest, &mut file_count)?;
    print("\nscratch: checked out ");
    print_num(file_count);
    print(" files\n");
    Ok(())
}

/// Recursively checkout a tree
fn checkout_tree_recursive(store: &ObjectStore, tree_sha: &Sha1Hash, dest: &str, file_count: &mut usize) -> Result<()> {
    let tree_obj = match store.read(tree_sha) {
        Ok(obj) => obj,
        Err(e) => {
            print("scratch: checkout: tree ");
            print(&crate::sha1::to_hex(tree_sha));
            print(" in ");
            print(dest);
            print(": ");
            print(e.message());
            print("\n");
            return Err(e);
        }
    };
    let tree = tree_obj.as_tree()?;

    for entry in &tree.entries {
        let path = format!("{}/{}", dest, entry.name);

        if entry.is_submodule() {
            // Submodules reference commits in external repos — skip checkout
            print("scratch: skipping submodule ");
            print(&entry.name);
            print("\n");
            continue;
        } else if entry.is_dir() {
            // Create directory and recurse
            let _ = mkdir(&path);
            checkout_tree_recursive(store, &entry.sha, &path, file_count)?;
        } else {
            // Write file
            
            // Warn BEFORE reading/decompressing the full content
            let mut file_size = 0;
            if let Ok((_obj_type, size)) = store.read_info(&entry.sha) {
                file_size = size;
                if size > 300 * 1024 {
                    print("\nscratch: warning: checking out large file ");
                    print(&entry.name);
                    print(" (");
                    print_num(size / 1024);
                    print(" KB)");
                    if size > 5 * 1024 * 1024 {
                        print(" - WARNING: MASSIVE FILE");
                    }
                    print("\n");
                }
            }

            // Now perform the streaming read/write
            let fd = open(&path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
            if fd >= 0 {
                let mut total_written = 0;
                let mut last_dot = 0;
                let chunk_size = 64 * 1024;

                // Print an initial dot to show we've started the decompression/write process
                if file_size >= chunk_size {
                    print(".");
                }

                let result = store.read_to_callback(&entry.sha, |chunk| {
                    let n = write_fd(fd, chunk);
                    if n < 0 {
                        return Err(Error::io("write failed"));
                    }
                    total_written += n as usize;
                    
                    // Progress dots for large files
                    if file_size >= chunk_size {
                        // We check >= last_dot + chunk_size to handle potential small chunks from decompressor
                        if total_written >= last_dot + chunk_size {
                            print(".");
                            last_dot = total_written;
                        }
                    }
                    Ok(())
                });

                close(fd);
                
                if result.is_err() {
                    print("\nscratch: checkout: failed to stream ");
                    print(&entry.name);
                    print("\n");
                }
            } else {
                print("scratch: checkout: failed to open ");
                print(&entry.name);
                print("\n");
            }

            *file_count += 1;
            if *file_count % 50 == 0 {
                print(".");
            }
        }
    }

    Ok(())
}

fn print_num(n: usize) {
    if n == 0 {
        print("0");
        return;
    }

    let mut buf = [0u8; 20];
    let mut i = 0;
    let mut num = n;

    while num > 0 {
        buf[i] = b'0' + (num % 10) as u8;
        num /= 10;
        i += 1;
    }

    while i > 0 {
        i -= 1;
        let s = [buf[i]];
        if let Ok(s) = core::str::from_utf8(&s) {
            print(s);
        }
    }
}
