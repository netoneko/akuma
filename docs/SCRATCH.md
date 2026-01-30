# Scratch - Minimal Git Client for Akuma OS

Scratch is a minimal, `no_std` compatible Git client designed to run in the Akuma OS userspace. It implements the Git Smart HTTP protocol to clone repositories from GitHub and other Git servers.

## Overview

Scratch provides basic Git functionality without requiring the full Git binary or standard library. It's built from scratch (hence the name) to work within Akuma's constrained environment.

### Supported Commands

```bash
scratch clone <url>     # Clone a repository
scratch fetch           # Fetch updates from origin
scratch status          # Show current HEAD and branch
scratch branch          # List branches
scratch branch <name>   # Create a new branch
scratch tag             # List tags
scratch help            # Show help
```

### Key Features

- **No force push**: Force push is permanently disabled for safety
- **Streaming downloads**: Pack files are processed in chunks to minimize memory usage
- **HTTPS support**: Uses TLS 1.3 via libakuma-tls
- **GitHub compatible**: Tested with GitHub's Smart HTTP protocol

## Architecture

### Module Structure

```
scratch/
├── main.rs          # CLI entry point and command dispatch
├── error.rs         # Error types and handling
├── http.rs          # HTTP/HTTPS client with chunked encoding
├── stream.rs        # Streaming HTTP response processing
├── protocol.rs      # Git Smart HTTP protocol implementation
├── pktline.rs       # Git pkt-line framing protocol
├── pack.rs          # Pack file parser (in-memory, legacy)
├── pack_stream.rs   # Streaming pack parser
├── object.rs        # Git object types (blob, tree, commit, tag)
├── store.rs         # Object storage (.git/objects/)
├── refs.rs          # Reference management (.git/refs/)
├── repository.rs    # High-level repository operations
├── sha1.rs          # SHA-1 hashing wrapper
└── zlib.rs          # Zlib compression/decompression
```

### Data Flow for Clone

```
1. URL Parsing
   └─> Parse https://github.com/owner/repo.git

2. Reference Discovery
   └─> GET /info/refs?service=git-upload-pack
   └─> Parse pkt-line formatted ref list
   └─> Extract capabilities (side-band, ofs-delta, etc.)

3. Pack Negotiation
   └─> POST /git-upload-pack
   └─> Send "want" lines for desired refs
   └─> Receive pack file with all objects

4. Streaming Pack Processing
   └─> Read HTTP response in chunks (~4KB)
   └─> Decode chunked transfer encoding
   └─> Demultiplex sideband data
   └─> Parse pack objects one at a time
   └─> Decompress and write each object to .git/objects/
   └─> Handle delta objects (OFS_DELTA, REF_DELTA)

5. Reference Creation
   └─> Write remote-tracking refs to .git/refs/remotes/origin/
   └─> Create local branch for default branch
   └─> Set HEAD

6. Checkout
   └─> Read commit object to get tree SHA
   └─> Recursively checkout tree to working directory
```

### Memory Efficiency

The streaming architecture keeps memory usage low:

- **HTTP response**: Read in 4KB chunks, not buffered entirely
- **Chunked encoding**: Decoded on-the-fly
- **Pack parsing**: Objects written to disk immediately after decompression
- **Delta resolution**: Base objects read back from disk when needed

This allows cloning large repositories that wouldn't fit in memory.

## Git Protocol Details

### Pkt-Line Format

Git uses a simple framing protocol where each line is prefixed with a 4-character hex length:

```
0032want abc123... side-band-64k ofs-delta
0000                              # flush packet
```

Special packets:
- `0000` - Flush packet (end of section)
- `0001` - Delimiter packet
- `0002` - Response end packet

### Sideband Multiplexing

When `side-band` or `side-band-64k` capability is negotiated, pack data is multiplexed:

- Channel 1: Pack data
- Channel 2: Progress messages
- Channel 3: Error messages

### Object Types

| Type | ID | Description |
|------|-----|-------------|
| commit | 1 | Commit metadata and tree pointer |
| tree | 2 | Directory listing |
| blob | 3 | File content |
| tag | 4 | Annotated tag |
| ofs_delta | 6 | Delta against object at offset |
| ref_delta | 7 | Delta against object by SHA-1 |

### Delta Compression

Pack files use delta compression to reduce size:

- **OFS_DELTA**: Base object is at a relative offset earlier in the pack
- **REF_DELTA**: Base object is identified by SHA-1 (must exist in store)

Delta instructions:
- **Copy**: Copy bytes from base object at offset/length
- **Insert**: Insert literal bytes

## Dependencies

```toml
[dependencies]
libakuma = { path = "../libakuma" }      # Syscall wrappers
libakuma-tls = { path = "../libakuma-tls" }  # TLS 1.3 client
sha1_smol = "1.0"                        # SHA-1 hashing (no_std)
miniz_oxide = { version = "0.8", default-features = false, features = ["with-alloc"] }  # Zlib
```

## Limitations

- **HTTPS only**: HTTP without TLS is not fully implemented for streaming
- **No push**: Push functionality is not yet implemented
- **No merge**: Merge operations are not supported
- **No diff**: Cannot show diffs between commits
- **No SSH**: Only HTTP(S) transport is supported
- **No authentication**: Token/password auth not yet implemented

## Future Work

1. **Push support**: Implement git-receive-pack protocol
2. **Authentication**: Support for GitHub tokens
3. **Incremental fetch**: Better "have" negotiation for updates
4. **Shallow clones**: Support for `--depth` option
5. **Sparse checkout**: Only checkout specific paths

## Usage Examples

### Clone a Repository

```bash
scratch clone https://github.com/netoneko/akuma.git
```

Output:
```
scratch: cloning https://github.com/netoneko/akuma.git
scratch: connecting to github.com
scratch: fetching refs from /netoneko/akuma.git/info/refs?service=git-upload-pack
scratch: found 26 refs
scratch: HEAD -> fd85067078750f06dff2cd292f6f0312c26fb6e7
scratch: requesting 24 objects
scratch: fetching and unpacking objects (streaming)
scratch: pack version 2, 1543 objects
scratch: received 2 MB
scratch: stored 1543 objects
scratch: HEAD set to main
scratch: checking out files
scratch: done
```

### Check Status

```bash
cd akuma
scratch status
```

Output:
```
On branch main
HEAD: fd85067078750f06dff2cd292f6f0312c26fb6e7
```

### List Branches

```bash
scratch branch
```

Output:
```
* main
  safe-print
```

## Integration with Meow

Scratch is integrated with the Meow AI assistant. Meow can use Git commands through tool calls:

- `GitClone` - Clone a repository
- `GitFetch` - Fetch updates
- `GitPull` - Pull updates (fetch + merge)
- `GitPush` - Push changes (disabled for force push)
- `GitStatus` - Show current state
- `GitBranch` - Manage branches

See `MEOW.md` for more details on the AI assistant.
