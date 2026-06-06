//! Pack file creation for push
//!
//! Creates pack files containing objects to send to remote.

use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::object::ObjectType;
use crate::sha1::{self, Sha1Hash};
use crate::store::ObjectStore;
use crate::zlib;

/// Pack file header magic
const PACK_MAGIC: &[u8; 4] = b"PACK";
/// Pack file version (we use version 2)
const PACK_VERSION: u32 = 2;

/// Create a pack file containing the specified objects
///
/// # Arguments
/// * `objects` - SHA-1 hashes of objects to include
/// * `store` - Object store to read objects from
///
/// # Returns
/// Complete pack file data (header + objects + trailer)
pub fn create_pack(objects: &[Sha1Hash], store: &ObjectStore) -> Result<Vec<u8>> {
    let mut pack = Vec::new();
    
    // Write header: "PACK" + version (4 bytes) + object count (4 bytes)
    pack.extend_from_slice(PACK_MAGIC);
    pack.extend_from_slice(&PACK_VERSION.to_be_bytes());
    pack.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    
    // Write each object
    for sha in objects {
        write_pack_object(&mut pack, sha, store)?;
    }
    
    // Write trailer: SHA-1 of everything so far
    let checksum = sha1::hash(&pack);
    pack.extend_from_slice(&checksum);
    
    Ok(pack)
}

/// Write a single object to the pack
fn write_pack_object(pack: &mut Vec<u8>, sha: &Sha1Hash, store: &ObjectStore) -> Result<()> {
    // Read object
    let (obj_type, content) = store.read_raw_content(sha)?;
    
    // Get pack type number
    let type_num = obj_type.pack_type();
    
    // Compress content
    let compressed = zlib::compress(&content);
    
    // Write type and size header
    // Format: variable-length encoding
    // First byte: MSB = continuation, bits 4-6 = type, bits 0-3 = size low bits
    // Subsequent bytes: MSB = continuation, bits 0-6 = size bits
    
    let size = content.len();
    
    // First byte: type (3 bits) + size low 4 bits
    let mut first_byte = (type_num << 4) | ((size & 0x0f) as u8);
    let mut remaining_size = size >> 4;
    
    if remaining_size > 0 {
        first_byte |= 0x80; // Set continuation bit
    }
    pack.push(first_byte);
    
    // Subsequent bytes for size
    while remaining_size > 0 {
        let mut byte = (remaining_size & 0x7f) as u8;
        remaining_size >>= 7;
        if remaining_size > 0 {
            byte |= 0x80; // Set continuation bit
        }
        pack.push(byte);
    }
    
    // Write compressed data
    pack.extend_from_slice(&compressed);
    
    Ok(())
}

/// Collect all objects reachable from a commit that aren't in the "have" set
///
/// Walks commit -> tree -> blobs/subtrees recursively
pub fn collect_objects_for_push(
    commit_sha: &Sha1Hash,
    have: &[Sha1Hash],
    store: &ObjectStore,
) -> Result<Vec<Sha1Hash>> {
    let mut objects = Vec::new();
    let mut visited: Vec<Sha1Hash> = Vec::new();
    
    collect_objects_recursive(commit_sha, have, store, &mut objects, &mut visited)?;
    
    Ok(objects)
}

fn collect_objects_recursive(
    sha: &Sha1Hash,
    have: &[Sha1Hash],
    store: &ObjectStore,
    objects: &mut Vec<Sha1Hash>,
    visited: &mut Vec<Sha1Hash>,
) -> Result<()> {
    // Skip if already in have set
    if have.contains(sha) {
        return Ok(());
    }
    
    // Skip if already visited
    if visited.contains(sha) {
        return Ok(());
    }
    visited.push(*sha);
    
    // Read object
    let obj = store.read(sha)?;
    
    // Add this object to the list
    objects.push(*sha);
    
    // Recurse into referenced objects
    match &obj {
        crate::object::Object::Commit(commit) => {
            // Add tree
            collect_objects_recursive(&commit.tree, have, store, objects, visited)?;
            // Don't recurse into parents - they should already be on remote
        }
        crate::object::Object::Tree(tree) => {
            // Add all entries (skip submodules — their objects live in another repo)
            for entry in &tree.entries {
                if entry.is_submodule() {
                    continue;
                }
                collect_objects_recursive(&entry.sha, have, store, objects, visited)?;
            }
        }
        crate::object::Object::Blob(_) => {
            // No references
        }
        crate::object::Object::Tag(tag) => {
            // Add tagged object
            collect_objects_recursive(&tag.object, have, store, objects, visited)?;
        }
    }
    
    Ok(())
}
