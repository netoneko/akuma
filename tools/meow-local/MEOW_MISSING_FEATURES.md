# Features Missing from Meow (Unikernel Version)

This document lists features available in `meow-local` (native macOS/Linux version) that are
not yet implemented in `meow` (the unikernel version running in the Akuma kernel).

## Context Management Features

### 1. Context Window Query on Startup

**meow-local:** Queries the Ollama `/api/show` endpoint on startup to determine the model's
maximum context window size (`num_ctx` parameter).

**meow:** Does not query model info. Uses hardcoded defaults.

**Implementation Notes:**
- Requires HTTP client capability (already available via libakuma)
- Parse JSON response to extract `num_ctx` field
- Display context window size in startup banner

### 2. Token Count Display

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

### 3. CompactContext Tool

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

### 4. /tokens Command

**meow-local:** Provides a `/tokens` command to display current token usage statistics.

**meow:** Does not have this command.

## Other Feature Differences

### Shell Tool

**meow-local:** Has a sandboxed `Shell` tool for executing bash commands.

**meow:** Does not have shell execution capability (filesystem-only tools).

### Escape Key Handling

**meow-local:** Uses terminal raw mode for proper escape key detection to cancel requests.

**meow:** Has escape key detection but implementation differs due to kernel environment.

## Priority for Implementation

1. **High Priority:**
   - Token count display (improves user awareness)
   - CompactContext tool (prevents context overflow)

2. **Medium Priority:**
   - Context window query (nice-to-have, can use defaults)
   - /tokens command (debugging aid)

3. **Low Priority:**
   - Shell tool (security considerations in unikernel environment)
