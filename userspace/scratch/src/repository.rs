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

    // Checkout working tree
    print("scratch: checking out files\n");
    let head_commit_sha = ref_manager.resolve_head()?;
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
    let remote_url = git_config.remote_url.as_ref()
        .ok_or_else(|| Error::io("no remote URL in config"))?;
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

/// Push current branch to origin
pub fn push(token: Option<&str>) -> Result<()> {
    let git_dir = crate::git_dir();
    let refs = RefManager::new(&git_dir);
    let store = ObjectStore::new(&git_dir);

    // Get current branch name
    let head = refs.read_head()?;
    let head = head.trim();
    
    let branch_name = head
        .strip_prefix("ref: refs/heads/")
        .ok_or_else(|| Error::io("not on a branch (detached HEAD)"))?;

    print("scratch: pushing branch ");
    print(branch_name);
    print("\n");

    // Get local commit SHA
    let local_sha = refs.read_branch(branch_name)?;

    // Load git config (includes remote URL and optional credential token)
    let git_config = GitConfig::load()?;
    let remote_url = git_config.remote_url.as_ref()
        .ok_or_else(|| Error::io("no remote URL in config"))?;
    let parsed_url = Url::parse(remote_url)?;

    // Use token from argument, or fall back to config
    let effective_token = token.or(git_config.credential_token.as_deref());
    
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
    let commit_obj = store.read(commit_sha)?;
    let commit = commit_obj.as_commit()?;

    // Checkout tree
    checkout_tree_recursive(store, &commit.tree, dest)
}

/// Recursively checkout a tree
fn checkout_tree_recursive(store: &ObjectStore, tree_sha: &Sha1Hash, dest: &str) -> Result<()> {
    let tree_obj = store.read(tree_sha)?;
    let tree = tree_obj.as_tree()?;

    for entry in &tree.entries {
        let path = format!("{}/{}", dest, entry.name);

        if entry.is_dir() {
            // Create directory and recurse
            let _ = mkdir(&path);
            checkout_tree_recursive(store, &entry.sha, &path)?;
        } else {
            // Write file
            let blob_obj = store.read(&entry.sha)?;
            let content = blob_obj.as_blob()?;

            let fd = open(&path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
            if fd >= 0 {
                let _ = write_fd(fd, content);
                close(fd);
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
