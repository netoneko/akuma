//! Git object types and parsing
//!
//! Git has four object types:
//! - Blob: File content
//! - Tree: Directory listing
//! - Commit: Snapshot with metadata
//! - Tag: Named reference with metadata

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::sha1::Sha1Hash;

/// Git object types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectType {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "blob" => Some(ObjectType::Blob),
            "tree" => Some(ObjectType::Tree),
            "commit" => Some(ObjectType::Commit),
            "tag" => Some(ObjectType::Tag),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ObjectType::Blob => "blob",
            ObjectType::Tree => "tree",
            ObjectType::Commit => "commit",
            ObjectType::Tag => "tag",
        }
    }

    /// Object type number used in pack files
    pub fn pack_type(&self) -> u8 {
        match self {
            ObjectType::Commit => 1,
            ObjectType::Tree => 2,
            ObjectType::Blob => 3,
            ObjectType::Tag => 4,
        }
    }

    pub fn from_pack_type(t: u8) -> Option<Self> {
        match t {
            1 => Some(ObjectType::Commit),
            2 => Some(ObjectType::Tree),
            3 => Some(ObjectType::Blob),
            4 => Some(ObjectType::Tag),
            _ => None,
        }
    }
}

/// A Git object (parsed)
#[derive(Debug, Clone)]
pub enum Object {
    Blob(Vec<u8>),
    Tree(Tree),
    Commit(Commit),
    Tag(Tag),
}

impl Object {
    /// Get the object type
    pub fn object_type(&self) -> ObjectType {
        match self {
            Object::Blob(_) => ObjectType::Blob,
            Object::Tree(_) => ObjectType::Tree,
            Object::Commit(_) => ObjectType::Commit,
            Object::Tag(_) => ObjectType::Tag,
        }
    }

    /// Parse a raw Git object (after decompression)
    ///
    /// Format: "{type} {size}\0{content}"
    pub fn parse(data: &[u8]) -> Result<Self> {
        // Find the null byte separating header from content
        let null_pos = data.iter().position(|&b| b == 0)
            .ok_or_else(|| Error::invalid_object("missing null byte in header"))?;

        let header = core::str::from_utf8(&data[..null_pos])
            .map_err(|_| Error::invalid_object("invalid UTF-8 in header"))?;

        // Parse "{type} {size}"
        let mut parts = header.split(' ');
        let type_str = parts.next()
            .ok_or_else(|| Error::invalid_object("missing type in header"))?;
        let size_str = parts.next()
            .ok_or_else(|| Error::invalid_object("missing size in header"))?;

        let obj_type = ObjectType::from_str(type_str)
            .ok_or_else(|| Error::invalid_object("unknown object type"))?;

        let size: usize = size_str.parse()
            .map_err(|_| Error::invalid_object("invalid size in header"))?;

        let content = &data[null_pos + 1..];
        if content.len() != size {
            return Err(Error::invalid_object("size mismatch"));
        }

        Self::parse_content(obj_type, content)
    }

    /// Parse object content (without header)
    pub fn parse_content(obj_type: ObjectType, content: &[u8]) -> Result<Self> {
        match obj_type {
            ObjectType::Blob => Ok(Object::Blob(content.to_vec())),
            ObjectType::Tree => Ok(Object::Tree(Tree::parse(content)?)),
            ObjectType::Commit => Ok(Object::Commit(Commit::parse(content)?)),
            ObjectType::Tag => Ok(Object::Tag(Tag::parse(content)?)),
        }
    }

    /// Serialize object to raw bytes (with header)
    pub fn serialize(&self) -> Vec<u8> {
        let content = self.serialize_content();
        let header = alloc::format!("{} {}\0", self.object_type().as_str(), content.len());
        
        let mut data = Vec::with_capacity(header.len() + content.len());
        data.extend_from_slice(header.as_bytes());
        data.extend_from_slice(&content);
        data
    }

    /// Serialize just the content (without header)
    pub fn serialize_content(&self) -> Vec<u8> {
        match self {
            Object::Blob(data) => data.clone(),
            Object::Tree(tree) => tree.serialize(),
            Object::Commit(commit) => commit.serialize(),
            Object::Tag(tag) => tag.serialize(),
        }
    }

    /// Get as blob content
    pub fn as_blob(&self) -> Result<&[u8]> {
        match self {
            Object::Blob(data) => Ok(data),
            _ => Err(Error::invalid_object("expected blob")),
        }
    }

    /// Get as tree
    pub fn as_tree(&self) -> Result<&Tree> {
        match self {
            Object::Tree(tree) => Ok(tree),
            _ => Err(Error::invalid_object("expected tree")),
        }
    }

    /// Get as commit
    pub fn as_commit(&self) -> Result<&Commit> {
        match self {
            Object::Commit(commit) => Ok(commit),
            _ => Err(Error::invalid_object("expected commit")),
        }
    }
}

/// A tree entry (file or subdirectory)
#[derive(Debug, Clone)]
pub struct TreeEntry {
    /// File mode (100644 for file, 100755 for executable, 040000 for directory, etc.)
    pub mode: u32,
    /// Entry name
    pub name: String,
    /// SHA-1 hash of the blob or tree
    pub sha: Sha1Hash,
}

impl TreeEntry {
    /// Check if this entry is a directory (tree)
    pub fn is_dir(&self) -> bool {
        self.mode == 40000 || self.mode == 0o040000
    }

    /// Check if this entry is a regular file
    pub fn is_file(&self) -> bool {
        self.mode == 100644 || self.mode == 100755 || self.mode == 0o100644 || self.mode == 0o100755
    }

    /// Check if this entry is a submodule (gitlink)
    pub fn is_submodule(&self) -> bool {
        self.mode == 160000 || self.mode == 0o160000
    }
}

/// A Git tree (directory listing)
#[derive(Debug, Clone)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    /// Parse tree content
    ///
    /// Format: repeated entries of "{mode} {name}\0{20-byte sha}"
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut entries = Vec::new();
        let mut pos = 0;

        while pos < data.len() {
            // Find space after mode
            let space_pos = data[pos..].iter().position(|&b| b == b' ')
                .ok_or_else(|| Error::invalid_object("invalid tree entry: no space"))?;
            
            let mode_str = core::str::from_utf8(&data[pos..pos + space_pos])
                .map_err(|_| Error::invalid_object("invalid tree entry: mode not UTF-8"))?;
            let mode: u32 = u32::from_str_radix(mode_str, 8)
                .map_err(|_| Error::invalid_object("invalid tree entry: bad mode"))?;

            pos += space_pos + 1;

            // Find null after name
            let null_pos = data[pos..].iter().position(|&b| b == 0)
                .ok_or_else(|| Error::invalid_object("invalid tree entry: no null"))?;
            
            let name = core::str::from_utf8(&data[pos..pos + null_pos])
                .map_err(|_| Error::invalid_object("invalid tree entry: name not UTF-8"))?;

            pos += null_pos + 1;

            // Read 20-byte SHA
            if pos + 20 > data.len() {
                return Err(Error::invalid_object("invalid tree entry: truncated SHA"));
            }
            let mut sha = [0u8; 20];
            sha.copy_from_slice(&data[pos..pos + 20]);
            pos += 20;

            entries.push(TreeEntry {
                mode,
                name: String::from(name),
                sha,
            });
        }

        Ok(Tree { entries })
    }

    /// Serialize tree to bytes
    pub fn serialize(&self) -> Vec<u8> {
        let mut data = Vec::new();
        
        for entry in &self.entries {
            // Mode as octal string (no leading zeros except for the format)
            let mode_str = alloc::format!("{:o}", entry.mode);
            data.extend_from_slice(mode_str.as_bytes());
            data.push(b' ');
            data.extend_from_slice(entry.name.as_bytes());
            data.push(0);
            data.extend_from_slice(&entry.sha);
        }
        
        data
    }
}

/// A Git commit
#[derive(Debug, Clone)]
pub struct Commit {
    /// Tree SHA this commit points to
    pub tree: Sha1Hash,
    /// Parent commit SHAs (empty for root commit)
    pub parents: Vec<Sha1Hash>,
    /// Author line (name, email, timestamp)
    pub author: String,
    /// Committer line (name, email, timestamp)
    pub committer: String,
    /// Commit message
    pub message: String,
}

impl Commit {
    /// Parse commit content
    pub fn parse(data: &[u8]) -> Result<Self> {
        let text = core::str::from_utf8(data)
            .map_err(|_| Error::invalid_object("commit not valid UTF-8"))?;

        let mut tree: Option<Sha1Hash> = None;
        let mut parents = Vec::new();
        let mut author = String::new();
        let mut committer = String::new();
        let mut in_message = false;
        let mut message = String::new();

        for line in text.lines() {
            if in_message {
                if !message.is_empty() {
                    message.push('\n');
                }
                message.push_str(line);
            } else if line.is_empty() {
                in_message = true;
            } else if let Some(rest) = line.strip_prefix("tree ") {
                tree = Some(crate::sha1::from_hex(rest)
                    .ok_or_else(|| Error::invalid_object("invalid tree SHA"))?);
            } else if let Some(rest) = line.strip_prefix("parent ") {
                parents.push(crate::sha1::from_hex(rest)
                    .ok_or_else(|| Error::invalid_object("invalid parent SHA"))?);
            } else if let Some(rest) = line.strip_prefix("author ") {
                author = String::from(rest);
            } else if let Some(rest) = line.strip_prefix("committer ") {
                committer = String::from(rest);
            }
            // Ignore other headers (gpgsig, etc.)
        }

        let tree = tree.ok_or_else(|| Error::invalid_object("commit missing tree"))?;

        Ok(Commit {
            tree,
            parents,
            author,
            committer,
            message,
        })
    }

    /// Serialize commit to bytes
    pub fn serialize(&self) -> Vec<u8> {
        let mut text = String::new();
        
        text.push_str("tree ");
        text.push_str(&crate::sha1::to_hex(&self.tree));
        text.push('\n');
        
        for parent in &self.parents {
            text.push_str("parent ");
            text.push_str(&crate::sha1::to_hex(parent));
            text.push('\n');
        }
        
        text.push_str("author ");
        text.push_str(&self.author);
        text.push('\n');
        
        text.push_str("committer ");
        text.push_str(&self.committer);
        text.push('\n');
        
        text.push('\n');
        text.push_str(&self.message);
        
        text.into_bytes()
    }
}

/// A Git tag (annotated)
#[derive(Debug, Clone)]
pub struct Tag {
    /// Object this tag points to
    pub object: Sha1Hash,
    /// Type of the tagged object
    pub object_type: ObjectType,
    /// Tag name
    pub tag: String,
    /// Tagger line
    pub tagger: String,
    /// Tag message
    pub message: String,
}

impl Tag {
    /// Parse tag content
    pub fn parse(data: &[u8]) -> Result<Self> {
        let text = core::str::from_utf8(data)
            .map_err(|_| Error::invalid_object("tag not valid UTF-8"))?;

        let mut object: Option<Sha1Hash> = None;
        let mut object_type: Option<ObjectType> = None;
        let mut tag = String::new();
        let mut tagger = String::new();
        let mut in_message = false;
        let mut message = String::new();

        for line in text.lines() {
            if in_message {
                if !message.is_empty() {
                    message.push('\n');
                }
                message.push_str(line);
            } else if line.is_empty() {
                in_message = true;
            } else if let Some(rest) = line.strip_prefix("object ") {
                object = Some(crate::sha1::from_hex(rest)
                    .ok_or_else(|| Error::invalid_object("invalid object SHA"))?);
            } else if let Some(rest) = line.strip_prefix("type ") {
                object_type = Some(ObjectType::from_str(rest)
                    .ok_or_else(|| Error::invalid_object("invalid object type in tag"))?);
            } else if let Some(rest) = line.strip_prefix("tag ") {
                tag = String::from(rest);
            } else if let Some(rest) = line.strip_prefix("tagger ") {
                tagger = String::from(rest);
            }
        }

        let object = object.ok_or_else(|| Error::invalid_object("tag missing object"))?;
        let object_type = object_type.ok_or_else(|| Error::invalid_object("tag missing type"))?;

        Ok(Tag {
            object,
            object_type,
            tag,
            tagger,
            message,
        })
    }

    /// Serialize tag to bytes
    pub fn serialize(&self) -> Vec<u8> {
        let mut text = String::new();
        
        text.push_str("object ");
        text.push_str(&crate::sha1::to_hex(&self.object));
        text.push('\n');
        
        text.push_str("type ");
        text.push_str(self.object_type.as_str());
        text.push('\n');
        
        text.push_str("tag ");
        text.push_str(&self.tag);
        text.push('\n');
        
        text.push_str("tagger ");
        text.push_str(&self.tagger);
        text.push('\n');
        
        text.push('\n');
        text.push_str(&self.message);
        
        text.into_bytes()
    }
}
