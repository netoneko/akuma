# Meow - LLM Chat Client

Meow is an LLM chat client that connects to a locally-hosted Ollama server. There are two versions:

1. **Userspace meow** (`userspace/meow/`) - Runs inside the Akuma kernel guest, limited tools
2. **meow-local** (`tools/meow-local/`) - Runs natively on the host, can edit Akuma source code

Both support tool calling for filesystem operations and streaming responses.

---

## meow-local (Native Host Version)

The native version runs on your host machine and can edit the Akuma kernel source code.

### Usage

```bash
# Build
cd tools/meow-local && cargo build --release

# Run from akuma directory (sandbox = current dir)
./tools/meow-local/target/release/meow-local

# Specify working directory
meow-local -C /path/to/akuma

# One-shot query
meow-local "explain the threading model"
```

### Code Editing Tools

| Tool | Args | Description |
|------|------|-------------|
| `FileReadLines` | `filename`, `start`, `end` | Read specific line ranges with line numbers |
| `CodeSearch` | `pattern`, `path`, `context` | Regex search across .rs files with context |
| `FileEdit` | `filename`, `old_text`, `new_text` | Search-and-replace (requires unique match) |

### All meow-local Tools

| Tool | Description |
|------|-------------|
| `FileRead` | Read entire file (max 1MB) |
| `FileWrite` | Create/overwrite file |
| `FileAppend` | Append to file |
| `FileExists` | Check if file exists |
| `FileList` | List directory contents |
| `FileDelete` | Delete file |
| `FolderCreate` | Create directory |
| `FileCopy` | Copy file |
| `FileMove` | Move file |
| `FileRename` | Rename file |
| `FileReadLines` | Read line range with numbers |
| `CodeSearch` | Grep-like regex search |
| `FileEdit` | Precise search-and-replace |
| `HttpFetch` | HTTP GET (no HTTPS) |
| `Shell` | Execute shell commands (sandboxed) |
| `CompactContext` | Summarize conversation to save tokens |

### Sandboxing

All file operations are sandboxed to the working directory. The `Shell` tool wraps commands to prevent escaping the sandbox via `cd`.

### Testing

```bash
cd tools/meow-local && cargo test
```

---

## Userspace meow (Guest Version)

Runs inside the Akuma kernel as a userspace binary. More limited than meow-local.

## Usage

```bash
meow                          # Interactive mode with default model
meow -m llama3.2              # Use specific model
meow "What is 2+2?"           # One-shot mode
meow -h                       # Show help
```

## Features

- **Streaming responses**: Displays LLM output token-by-token as it arrives
- **Tool calling**: LLM can execute filesystem and network operations
- **Progress indication**: Shows dots while waiting, elapsed time on first token
- **Network retry**: Automatic retry with exponential backoff on network errors
- **Memory limits**: Caps response size (16KB) and chat history (10 messages)

## Commands

| Command | Description |
|---------|-------------|
| `/help` | Show available commands |
| `/clear` | Clear chat history |
| `/model <name>` | Switch LLM model |
| `/exit` | Exit the chat |
| Ctrl+D | Exit on empty line |

## Available Tools

The LLM can invoke these tools via JSON commands:

### Filesystem Tools

| Tool | Args | Description |
|------|------|-------------|
| `FileRead` | `filename` | Read file contents (max 32KB) |
| `FileWrite` | `filename`, `content` | Write/create file |
| `FileAppend` | `filename`, `content` | Append to file |
| `FileExists` | `filename` | Check if file exists |
| `FileList` | `path` | List directory contents |
| `FileDelete` | `filename` | Delete a file |
| `FolderCreate` | `path` | Create directory |
| `FileCopy` | `source`, `destination` | Copy file |
| `FileMove` | `source`, `destination` | Move file |
| `FileRename` | `source_filename`, `destination_filename` | Rename file |

### Network Tools

| Tool | Args | Description |
|------|------|-------------|
| `HttpFetch` | `url` | HTTP GET request (max 16KB response) |

**Note**: `HttpFetch` only supports HTTP, not HTTPS. Userspace has no TLS stack.

## Network Architecture

```
┌─────────────────┐         ┌─────────────────┐
│  Akuma Guest    │         │  Host Machine   │
│                 │         │                 │
│  meow binary    │◄───────►│  Ollama API     │
│  (port 11434)   │  HTTP   │  localhost:11434│
│                 │         │                 │
│  10.0.2.15      │         │  10.0.2.2       │
└─────────────────┘         └─────────────────┘
     QEMU User-Mode Networking
```

- Guest connects to `10.0.2.2:11434` (QEMU's host gateway)
- Ollama must be running on the host machine
- Uses Ollama's `/api/chat` endpoint with `stream: true`

## SSL/TLS Status

| Component | TLS Support | Certificate Verification |
|-----------|-------------|-------------------------|
| Kernel `curl` | Yes (TLS 1.3) | Yes (use `-k` to skip) |
| Userspace `wget` | No | N/A |
| Userspace `meow` | No | N/A |

Userspace programs use libakuma's plain TCP sockets. TLS requires the kernel's async networking stack.

## Memory Constraints

To avoid OOM in the constrained environment:

- **Response buffer**: 16KB max per response
- **File operations**: 32KB max file size
- **Chat history**: 10 messages max (older trimmed)
- **Read buffer**: 1KB chunks

## Error Handling

- **Connection refused**: Ollama not running on host
- **Model not found (404)**: Requested model not available
- **Timeout**: Network latency, retry with backoff
- **Network errors**: Automatic retry (3 attempts, exponential backoff)

## Building

```bash
cd userspace
./build.sh  # Builds meow and copies to bootstrap/bin/
```

## Default Configuration

- **Model**: `deepseek-r1:32b`
- **Ollama endpoint**: `10.0.2.2:11434`
- **Persona**: Cyberpunk anime cat assistant
