# Meow Tool Execution

Meow provides tools that the LLM can invoke to interact with the filesystem, network, and shell. Tools are invoked via JSON commands embedded in the LLM's response.

## Tool Command Format

The LLM emits tool calls as JSON blocks:

```json
{
  "command": {
    "tool": "FileRead",
    "args": {
      "filename": "/etc/meow/config"
    }
  }
}
```

Commands can appear in fenced code blocks (` ```json `) or inline as raw JSON starting with `{"command"`.

## Current Limitations

### Single Command Per Response

**Only the first tool command in a response is executed.**

The `find_and_execute_tool` function searches for the first JSON block containing a tool command, executes it, and returns. If the LLM emits multiple tool calls in a single response, only the first one runs.

Example problematic response from LLM:
```
I'll read both files for you.

```json
{"command": {"tool": "FileRead", "args": {"filename": "file1.txt"}}}
```

```json
{"command": {"tool": "FileRead", "args": {"filename": "file2.txt"}}}
```
```

Result: Only `file1.txt` is read; the second command is ignored.

### Workaround

The LLM should issue one tool call per response and wait for the result before issuing the next command. The system prompt should instruct the model to work sequentially.

### Future Improvement

To support multiple commands per response:
1. Loop `find_and_execute_tool` on remaining text until no commands found
2. Or modify the function to find all commands and return a `Vec<ToolResult>`

## Available Tools

### File Operations

| Tool | Description | Args |
|------|-------------|------|
| `FileRead` | Read entire file | `filename` |
| `FileReadLines` | Read specific line range | `filename`, `start`, `end` |
| `FileWrite` | Write/create file | `filename`, `content` |
| `FileAppend` | Append to file | `filename`, `content` |
| `FileEdit` | Search-and-replace (unique match required) | `filename`, `old_text`, `new_text` |
| `FileExists` | Check if file exists | `filename` |
| `FileList` | List directory contents | `path` |
| `FileCopy` | Copy file | `source`, `destination` |
| `FileMove` | Move file | `source`, `destination` |
| `FolderCreate` | Create directory | `path` |

### Navigation

| Tool | Description | Args |
|------|-------------|------|
| `Cd` | Change working directory | `path` |
| `Pwd` | Print working directory | (none) |

### Code Tools

| Tool | Description | Args |
|------|-------------|------|
| `CodeSearch` | Regex search across .rs files | `pattern`, `path`, `context` |

### Network

| Tool | Description | Args |
|------|-------------|------|
| `HttpFetch` | HTTP/HTTPS GET request | `url` |

### Git (via scratch)

| Tool | Description | Args |
|------|-------------|------|
| `GitClone` | Clone repository | `url` |
| `GitStatus` | Show repo status | (none) |
| `GitAdd` | Stage files | `path` |
| `GitCommit` | Create commit | `message`, `amend` |
| `GitPush` | Push to remote (force disabled) | `force` |
| `GitPull` | Fetch + merge | (none) |
| `GitFetch` | Fetch from remote | (none) |
| `GitBranch` | List/create/delete branches | `name`, `delete` |
| `GitCheckout` | Switch branches | `branch` |
| `GitLog` | Show commit history | `count`, `oneline` |
| `GitTag` | List/create/delete tags | `name`, `delete` |
| `GitConfig` | Get/set config | `key`, `value` |

### Shell

| Tool | Description | Args |
|------|-------------|------|
| `Shell` | Execute arbitrary command | `cmd` |

## Sandbox

All file operations are sandboxed to the working directory (set at startup or via `Cd`). Paths outside the sandbox are denied.

## Size Limits

- `FileRead`: 32KB max
- `FileReadLines`: 128KB max
- `HttpFetch`: 64KB max
- `Shell`: 30 second timeout
