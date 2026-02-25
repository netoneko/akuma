# Pipe TTY Processing Fix

## Problem

Piped commands produced duplicate output. For example:

```
$ echo hello | grep e
hello

hello
```

The output appeared twice with a blank line in between instead of a single `hello`.

## Root Cause

The TTY line discipline in `sys_read` was applying interactive terminal processing (ECHO and ICRNL) to piped stdin data. In real Unix, stdin from a pipe is not a TTY, so no TTY processing should occur. Akuma routes all process I/O through `ProcessChannel` regardless of whether it originates from an interactive terminal or a pipe, so there was no distinction.

### What happened step by step

1. Built-in `echo hello` writes `hello\r\n` to the pipeline buffer
2. This becomes stdin for `grep e` via `ProcessChannel`
3. When `grep` reads stdin via `sys_read`:
   - **ICRNL** converted `\r` to `\n`, turning `hello\r\n` into `hello\n\n` (two lines)
   - **ECHO** wrote the transformed input back to the stdout channel: `hello\r\n\r\n`
4. `grep` matched "hello" and wrote its own output: `hello\r\n`
5. The output channel contained both echoed input and grep output: `hello\r\n\r\nhello\r\n`

### Secondary issue: double ONLCR translation

`sys_write` applied ONLCR (`\n` → `\r\n`) translation to process output before storing it in the `ProcessChannel`. Then `execute_external` applied the same translation when reading the buffered output for the final pipeline stage. This doubled the `\r` characters (harmless in terminals but incorrect).

## Fix

Two changes in `src/syscall.rs`:

### 1. Skip TTY line discipline for piped stdin (`sys_read`)

When `ProcessChannel::is_stdin_closed()` is true, skip ECHO and ICRNL processing. Piped processes have their stdin pre-loaded and immediately closed at spawn time, so this flag reliably distinguishes pipe input from interactive terminal input:

- **Pipe**: stdin written + closed before process starts → `is_stdin_closed() == true` on first read
- **Interactive**: stdin stays open, data arrives from SSH keyboard → `is_stdin_closed() == false`

### 2. Skip ONLCR for piped process stdout (`sys_write`)

When `ProcessChannel::is_stdin_closed()` is true, skip the `\n` → `\r\n` output translation. The pipeline handler (`execute_external`) already performs this translation for the final stage, so doing it in `sys_write` as well would double it.

## Affected Scenarios

- `echo hello | grep e` — no longer duplicates output
- Any pipeline where built-in commands pipe into external binaries
- Any external-to-external pipeline (ECHO was applied to all piped stdin)

## Design Note

The `is_stdin_closed()` check works because of how Akuma spawns pipeline processes:

```rust
// In spawn_process_with_channel_ext:
if let Some(data) = stdin {
    channel.write_stdin(data);
    channel.close_stdin();  // ← marks as pipe
}
```

Processes without stdin (`stdin.is_none()`) also get `close_stdin()` called, which correctly skips TTY processing for them too (they have no stdin data to echo anyway).
