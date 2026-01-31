# Features Comparison: meow-local vs meow (Unikernel)

This document compares features between `meow-local` (native macOS/Linux version) and
`meow` (the unikernel version running in the Akuma kernel).

**Status: Most features have been backported to the unikernel version.**

## Multi-Provider Support

### 1. Provider Configuration System

**meow-local:** Supports multiple AI providers with persistent configuration in `~/.config/meow/config.toml`.

Commands:
- `meow-local init` - Interactive provider setup wizard
- `meow-local init --list` - List configured providers
- `meow-local init --delete <name>` - Remove a provider
- `--provider <name>` flag to select provider at runtime

Supported API types:
- Ollama (native format)
- OpenAI-compatible (works with OpenAI, Groq, and other compatible APIs)

**meow:** Only supports a single hardcoded Ollama endpoint.

**Implementation Notes:**
- Store config in filesystem (ext2 or memfs)
- TOML parsing would need a no_std compatible parser or simpler format
- API key storage for OpenAI-compatible providers

### 2. HTTPS/TLS Support

**meow-local:** Uses `native-tls` crate for secure HTTPS connections to providers like api.openai.com.

**meow:** HTTP only. No TLS implementation in the kernel.

**Implementation Notes:**
- Would require a no_std TLS implementation (e.g., rustls with ring)
- Significant complexity and binary size increase
- Alternative: proxy through a local HTTP-to-HTTPS gateway

### 3. Provider/Model Switching at Runtime

**meow-local:** Interactive commands for switching providers and models:
- `/provider` - Show current provider
- `/provider list` - List and switch providers with model selection
- `/model list` - Fetch and list available models from current provider

**meow:** Static model configuration.

### 4. OpenAI Streaming Format

**meow-local:** Supports both Ollama NDJSON and OpenAI SSE streaming formats:
- Ollama: `{"message":{"content":"..."}, "done":true/false}`
- OpenAI: `data: {"choices":[{"delta":{"content":"..."}}]}`

**meow:** Only Ollama format.

## Context Management Features

### 5. Context Window Query on Startup

**meow-local:** Queries the Ollama `/api/show` endpoint on startup to determine the model's
maximum context window size (`num_ctx` parameter).

**meow:** Does not query model info. Uses hardcoded defaults.

**Implementation Notes:**
- Requires HTTP client capability (already available via libakuma)
- Parse JSON response to extract `num_ctx` field
- Display context window size in startup banner

### 6. Token Count Display

**meow-local:** Displays current estimated token usage in the prompt:
```
[5k/32k] (=^･ω･^=) > 
```
The format shows `[current_tokens/limit_tokens]` before each prompt.

**meow:** Does not track or display token usage.

**Implementation Notes:**
- Token estimation: ~4 characters per token (rough approximation)
- Calculate total tokens across all messages in history
- Update display before each prompt

### 7. CompactContext Tool

**meow-local:** Provides a `CompactContext` tool that allows the LLM to compress conversation
history when token count approaches the limit (32k tokens by default).

Tool specification:
```json
{
  "command": {
    "tool": "CompactContext",
    "args": {
      "summary": "A comprehensive summary of the conversation so far..."
    }
  }
}
```

**meow:** Does not have context compaction capability.

**Implementation Notes:**
- The tool replaces conversation history with:
  1. System prompt
  2. A summary message containing the LLM-generated summary
  3. An acknowledgment message from the assistant
- Significantly reduces token count while preserving conversation context
- LLM can proactively use this when it notices high token count

### 8. /tokens Command

**meow-local:** Provides a `/tokens` command to display current token usage statistics.

**meow:** Does not have this command.

## Code Editing Tools

### 9. FileReadLines Tool

**meow-local:** Read specific line ranges from a file with line numbers.

```json
{
  "command": {
    "tool": "FileReadLines",
    "args": {"filename": "path/to/file", "start": 100, "end": 150}
  }
}
```

**meow:** Only has `FileRead` for entire file contents.

**Implementation Notes:**
- Simple to implement with line-by-line file reading
- Useful for navigating large files without loading everything

### 10. CodeSearch Tool

**meow-local:** Grep-like regex search across `.rs` files with context lines.

```json
{
  "command": {
    "tool": "CodeSearch",
    "args": {"pattern": "fn.*async", "path": "src/", "context": 2}
  }
}
```

**meow:** No code search capability.

**Implementation Notes:**
- Requires regex crate (already available in kernel)
- Recursive directory traversal with file extension filtering
- Implemented in `code_search.rs` module

### 11. FileEdit Tool

**meow-local:** Precise search-and-replace editing that requires unique match.

```json
{
  "command": {
    "tool": "FileEdit",
    "args": {"filename": "path/to/file", "old_text": "exact text", "new_text": "replacement"}
  }
}
```

**meow:** No search-and-replace tool.

**Implementation Notes:**
- Fails if text not found or multiple matches (prevents accidents)
- Reports line numbers of matches for disambiguation
- Returns diff-like output showing changes

## Shell & System Features

### 12. Shell Tool

**meow-local:** Has a sandboxed `Shell` tool for executing bash commands.

```json
{
  "command": {
    "tool": "Shell",
    "args": {"cmd": "cargo build --release"}
  }
}
```

Security features:
- Commands run within sandbox directory
- `cd` command intercepted to prevent sandbox escape
- Dangerous command patterns blocked

**meow:** Does not have shell execution capability (filesystem-only tools).

### 13. Working Directory Option

**meow-local:** Supports `-C` / `--directory` flag to set working directory.

```bash
meow-local -C /path/to/project
```

**meow:** Always uses current directory.

### 14. Escape Key Handling

**meow-local:** Uses background thread with `poll()` for escape key detection to cancel requests.

**meow:** Has escape key detection but implementation differs due to kernel environment.

## Implementation Status

| Feature | meow-local | meow (unikernel) | Notes |
|---------|------------|------------------|-------|
| Multi-provider config | Yes | Yes | Config stored at `/etc/meow/config` |
| HTTPS/TLS support | Yes (native-tls) | Yes (libakuma-tls) | TLS 1.3, NoVerify mode |
| OpenAI streaming format | Yes | Yes | SSE parsing implemented |
| Provider commands | Yes | Yes | `/provider`, `/model list` |
| Token count display | Yes | Yes | Shows `[5k/32k]` in prompt |
| /tokens command | Yes | Yes | Shows current usage |
| CompactContext tool | Yes | Yes | Compresses conversation history |
| FileReadLines tool | Yes | Yes | Read specific line ranges |
| CodeSearch tool | Yes | Yes | Simple string matching (no regex) |
| FileEdit tool | Yes | Yes | Search-and-replace with unique match |
| Shell tool | Yes (sandboxed) | Yes | Spawns binaries directly |
| meow init | Yes (interactive) | Yes (displays config) | Manual config editing required |

### Differences

1. **CodeSearch**: meow-local uses full regex via the `regex` crate.
   The unikernel version uses simple string matching to avoid the dependency.

2. **Shell Tool**: meow-local uses a sandboxed bash wrapper.
   The unikernel version spawns binaries directly (no shell interpretation).

3. **Interactive Init**: meow-local has a fully interactive provider setup wizard.
   The unikernel version displays current config and requires manual editing.

4. **TLS**: meow-local uses native-tls with certificate verification.
   The unikernel version uses embedded-tls in NoVerify mode (like `curl -k`).
