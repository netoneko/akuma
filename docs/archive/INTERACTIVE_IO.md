# Interactive I/O for SSH Applications

This document describes the interactive I/O system that enables bidirectional communication between SSH clients and running processes, allowing truly interactive applications like chat clients.

## Overview

Akuma now supports interactive applications that need real-time bidirectional I/O:
- **Stdout streaming**: Process output is sent to SSH immediately as it's produced
- **Stdin forwarding**: User input from SSH is forwarded to the running process in real-time

This enables applications like `meow` (LLM chat client) that need to:
1. Display prompts and output incrementally
2. Read user input while running
3. Stream responses in real-time

## Architecture

### Data Flow

```
SSH Client                    Akuma Kernel                    User Process
    │                              │                               │
    │  SSH Channel Data ────────►  │                               │
    │  (user types)                │  ProcessChannel.write_stdin() │
    │                              │  ─────────────────────────────►│
    │                              │                               │
    │                              │  sys_read(STDIN)              │
    │                              │  ◄─────────────────────────────│
    │                              │  (reads from channel stdin)   │
    │                              │                               │
    │                              │  sys_write(STDOUT)            │
    │                              │  ─────────────────────────────►│
    │                              │  ProcessChannel.write()       │
    │                              │                               │
    │  SSH Channel Data ◄────────  │  channel.try_read()           │
    │  (output displayed)          │                               │
```

### Key Components

#### 1. ProcessChannel (src/process.rs)

Extended with bidirectional buffers:

```rust
pub struct ProcessChannel {
    /// Output buffer (process stdout → SSH)
    buffer: Spinlock<VecDeque<u8>>,
    /// Stdin buffer (SSH → process stdin)
    stdin_buffer: Spinlock<VecDeque<u8>>,
    // ... other fields
}

impl ProcessChannel {
    /// Write to stdin buffer (SSH → process)
    pub fn write_stdin(&self, data: &[u8]);
    
    /// Read from stdin buffer (process reads SSH input)
    pub fn read_stdin(&self, buf: &mut [u8]) -> usize;
}
```

#### 2. Syscall Integration (src/syscall.rs)

`sys_read` for stdin checks the ProcessChannel first:

```rust
FileDescriptor::Stdin => {
    // First try ProcessChannel's stdin buffer (interactive input)
    if let Some(channel) = crate::process::current_channel() {
        let bytes = channel.read_stdin(&mut temp_buf);
        if bytes > 0 {
            return bytes as u64;
        }
    }
    
    // Fall back to process stdin buffer (piped input)
    proc.read_stdin(&mut temp_buf)
}
```

#### 3. InteractiveRead Trait (src/shell/mod.rs)

Enables non-blocking reads for polling:

```rust
pub trait InteractiveRead: embedded_io_async::Read {
    /// Try to read with a very short timeout (10ms)
    /// Returns 0 if no data available (not EOF)
    fn try_read_interactive(&mut self, buf: &mut [u8]) 
        -> impl Future<Output = Result<usize, Self::Error>>;
}
```

#### 4. SshChannelStream (src/ssh/protocol.rs)

Implements `InteractiveRead` with short timeout:

```rust
impl InteractiveRead for SshChannelStream<'_> {
    async fn try_read_interactive(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        // Uses SSH_INTERACTIVE_READ_TIMEOUT (10ms)
        self.try_read_interactive(buf).await.map_err(|_| SshStreamError)
    }
}
```

#### 5. Interactive Execution (src/shell/mod.rs)

The `execute_external_interactive` function handles the polling loop:

```rust
pub async fn execute_external_interactive<S>(path, args, stdin, stream) -> Result<(), ShellError>
where
    S: InteractiveRead + embedded_io_async::Write,
{
    let (thread_id, channel) = spawn_process_with_channel(path, args, stdin)?;

    loop {
        // 1. Drain process stdout → SSH
        if let Some(data) = channel.try_read() {
            stream.write_all(&data).await;
            stream.flush().await;
        }

        // 2. Check for process exit
        if channel.has_exited() {
            break;
        }

        // 3. Poll SSH stdin → process (10ms timeout)
        match stream.try_read_interactive(&mut buf).await {
            Ok(n) if n > 0 => {
                channel.write_stdin(&buf[..n]);
            }
            _ => {}
        }

        YieldOnce::new().await;
    }
}
```

## Configuration

Interactive execution is enabled by default in `ShellContext`:

```rust
pub struct ShellContext {
    interactive_exec: bool,  // true by default
}
```

When enabled, simple external commands (no pipes, redirects, or builtins) use `execute_external_interactive` for full bidirectional I/O.

## Use Cases

### 1. Long-Running Output (hello)

```bash
$ ssh user@host
> hello 5 500
hello (1/5)    # Appears immediately
hello (2/5)    # Appears after 500ms
...
```

Output streams in real-time instead of buffering until completion.

### 2. Interactive Chat (meow)

```bash
$ ssh user@host
> meow
you> Hello!           # User types, forwarded to process stdin
                      # LLM response streams in real-time
assistant> Hi there!
you> _                # Waiting for next input
```

Full bidirectional communication enables interactive applications.

### 3. Commands with Stdin

```bash
$ ssh user@host
> myapp
Enter name: John      # Prompt appears immediately
Hello, John!          # Response after user input
```

## Timeouts

| Constant | Value | Purpose |
|----------|-------|---------|
| `SSH_READ_TIMEOUT` | 60s | Normal shell input reads |
| `SSH_INTERACTIVE_READ_TIMEOUT` | 10ms | Polling reads during process execution |

The 10ms timeout for interactive reads ensures:
- Quick polling between stdout/stdin checks
- Responsive output streaming
- Low latency for user input

## Limitations

1. **Single direction priority**: If the process produces output faster than the network can transmit, stdin polling may be delayed.

2. **No true async I/O**: The polling loop alternates between stdout and stdin rather than handling both truly concurrently.

3. **Buffering delays**: Due to the thread model (see SSH_STREAMING_ARCHITECTURE.md), there may be slight batching of output when thread 0 is busy.

## Related Documentation

- [SSH_STREAMING_ARCHITECTURE.md](SSH_STREAMING_ARCHITECTURE.md) - Output streaming and thread model
- [SSH.md](SSH.md) - SSH server implementation
- [PROCFS.md](PROCFS.md) - Alternative stdin/stdout access via /proc/<pid>/fd/

## Summary

The interactive I/O system enables:

| Feature | Before | After |
|---------|--------|-------|
| Stdout | Buffered until exit | Real-time streaming |
| Stdin | Pre-populated only | Interactive forwarding |
| Applications | Output-only | Fully interactive |

Key changes:
1. `ProcessChannel` extended with `stdin_buffer`
2. `sys_read` checks channel stdin for interactive input
3. `InteractiveRead` trait for non-blocking SSH reads
4. `execute_external_interactive` for bidirectional polling loop
5. Interactive mode enabled by default for external commands
