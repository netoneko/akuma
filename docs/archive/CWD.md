# Current Working Directory in Akuma

This document explains how the current working directory (cwd) system works in Akuma, which differs from traditional Unix systems.

## Overview

Akuma implements cwd through:
1. **ProcessInfo page** - A kernel-mapped page at `0x1000` containing process metadata including cwd
2. **chdir syscall** - Updates the process's cwd at runtime
3. **Inheritance** - Child processes inherit cwd from their parent via spawn

## ProcessInfo Structure

The kernel and userspace share a `ProcessInfo` structure mapped at address `0x1000`:

```rust
#[repr(C)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub argc: u32,
    pub argv_len: u32,
    pub cwd_len: u32,           // Length of cwd string
    pub _reserved: u32,
    pub cwd_data: [u8; 256],    // Current working directory (null-terminated)
    pub argv_data: [u8; 744],   // Command line arguments
}
// Total: 1024 bytes
```

The kernel writes this page before a process starts; userspace can only read it (mapped read-only). The `chdir` syscall updates both the kernel's `Process.cwd` field AND the ProcessInfo page.

## Kernel Shell → Userspace Process

When the Akuma shell (which runs in kernel context) spawns a userspace process:

1. Shell maintains its own `ShellContext.cwd` (updated by `cd` command)
2. When spawning a process, shell passes cwd to `spawn_process_with_channel_cwd()`
3. Kernel sets `Process.cwd` to the provided value
4. Before process starts, `prepare_for_execution()` writes cwd to ProcessInfo page
5. Userspace process can read cwd via `libakuma::getcwd()`

```
┌─────────────────────────────────────────────────────────────┐
│  Kernel Shell                                               │
│  ┌─────────────────────┐                                    │
│  │ ShellContext        │                                    │
│  │   cwd: "/meow"      │                                    │
│  └──────────┬──────────┘                                    │
│             │ cd /meow                                      │
│             ▼                                               │
│  ┌─────────────────────┐                                    │
│  │ spawn_process_with_ │                                    │
│  │ channel_cwd(        │                                    │
│  │   path, args, stdin,│                                    │
│  │   cwd: "/meow"      │◄─── Shell passes its cwd           │
│  │ )                   │                                    │
│  └──────────┬──────────┘                                    │
│             │                                               │
│             ▼                                               │
│  ┌─────────────────────┐     ┌──────────────────────┐       │
│  │ Process             │     │ ProcessInfo (0x1000) │       │
│  │   cwd: "/meow"      │────▶│   cwd_data: "/meow"  │       │
│  └─────────────────────┘     └──────────────────────┘       │
│                                         │                   │
│                                         │ mapped read-only  │
└─────────────────────────────────────────┼───────────────────┘
                                          │
                                          ▼
┌─────────────────────────────────────────────────────────────┐
│  Userspace (scratch)                                        │
│  ┌─────────────────────┐                                    │
│  │ libakuma::getcwd()  │◄─── Reads from ProcessInfo         │
│  │   → "/meow"         │                                    │
│  └─────────────────────┘                                    │
└─────────────────────────────────────────────────────────────┘
```

## Userspace Process → Child Process (via SPAWN syscall)

When a userspace process (like meow) spawns a child (like scratch):

1. Parent calls `libakuma::chdir()` to update its own cwd
2. This invokes the `CHDIR` syscall which updates both `Process.cwd` and ProcessInfo
3. Parent calls `libakuma::spawn()` which invokes the `SPAWN` syscall
4. Kernel's `sys_spawn` reads parent's `Process.cwd` and passes to child
5. Child inherits parent's cwd automatically

```
┌─────────────────────────────────────────────────────────────┐
│  Userspace (meow)                                           │
│  ┌─────────────────────┐                                    │
│  │ libakuma::chdir(    │◄─── Cd tool calls this             │
│  │   "/meow"           │                                    │
│  │ )                   │                                    │
│  └──────────┬──────────┘                                    │
│             │ CHDIR syscall                                 │
└─────────────┼───────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────┐
│  Kernel                                                     │
│  ┌─────────────────────┐     ┌──────────────────────┐       │
│  │ sys_chdir()         │     │ meow's ProcessInfo   │       │
│  │   Process.cwd =     │────▶│   cwd_data: "/meow"  │       │
│  │     "/meow"         │     └──────────────────────┘       │
│  └─────────────────────┘                                    │
└─────────────────────────────────────────────────────────────┘

              ... later, meow spawns scratch ...

┌─────────────────────────────────────────────────────────────┐
│  Userspace (meow)                                           │
│  ┌─────────────────────┐                                    │
│  │ libakuma::spawn(    │◄─── GitStatus tool calls this      │
│  │   "/bin/scratch",   │                                    │
│  │   args              │                                    │
│  │ )                   │                                    │
│  └──────────┬──────────┘                                    │
│             │ SPAWN syscall                                 │
└─────────────┼───────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────┐
│  Kernel                                                     │
│  ┌─────────────────────────────────────────────────────┐    │
│  │ sys_spawn()                                         │    │
│  │   parent_cwd = current_process().cwd  // "/meow"    │    │
│  │   spawn_process_with_channel_cwd(..., parent_cwd)   │    │
│  └──────────┬──────────────────────────────────────────┘    │
│             │                                               │
│             ▼                                               │
│  ┌─────────────────────┐     ┌──────────────────────┐       │
│  │ scratch's Process   │     │ scratch's ProcessInfo│       │
│  │   cwd: "/meow"      │────▶│   cwd_data: "/meow"  │       │
│  └─────────────────────┘     └──────────────────────┘       │
└─────────────────────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────┐
│  Userspace (scratch)                                        │
│  ┌─────────────────────┐                                    │
│  │ libakuma::getcwd()  │◄─── Inherited from parent!         │
│  │   → "/meow"         │                                    │
│  └─────────────────────┘                                    │
└─────────────────────────────────────────────────────────────┘
```

## libakuma API

### getcwd()

Returns the current working directory as a static string slice.

```rust
pub fn getcwd() -> &'static str
```

Reads directly from the ProcessInfo page at `0x1000`. Returns "/" if cwd is not set.

### chdir()

Changes the current working directory.

```rust
pub fn chdir(path: &str) -> i32
```

Returns 0 on success, negative errno on failure:
- `-ENOENT` (-2): Directory does not exist
- `-EINVAL` (-22): Invalid path

This syscall updates both:
1. The kernel's `Process.cwd` field (for inheritance to children)
2. The ProcessInfo page (for `getcwd()` to return the new value)

## Syscalls

### CHDIR (306)

```
Arguments:
  x0: path_ptr - Pointer to path string
  x1: path_len - Length of path string

Returns:
  0 on success
  Negative errno on failure
```

### SPAWN (301)

When spawning a child process, the kernel automatically:
1. Reads the parent's `Process.cwd`
2. Passes it to the child via `spawn_process_with_channel_cwd()`
3. Child's ProcessInfo.cwd is set before the child starts

## Use Cases

### Kernel Shell

```
akuma:/> cd /meow
akuma:/meow> scratch status    # scratch inherits cwd="/meow"
```

The shell's `cd` command updates `ShellContext.cwd`. When spawning scratch, the shell explicitly passes its cwd.

### Meow (AI Chat Client)

```
meow> Cd /repo        # Calls libakuma::chdir("/repo")
meow> GitStatus       # Spawns scratch, which inherits cwd="/repo"
```

Meow's `Cd` tool calls `libakuma::chdir()` to update its own cwd. When it spawns scratch via the `SPAWN` syscall, the kernel automatically inherits meow's cwd to scratch.

### Scratch (Git Client)

Scratch uses `libakuma::getcwd()` to determine the repository location:

```rust
pub fn git_dir() -> String {
    let cwd = getcwd();
    if cwd == "/" {
        String::from("/.git")
    } else {
        format!("{}/.git", cwd)
    }
}
```

## Sandboxing in Meow

Meow implements file operation sandboxing based on cwd:

1. All file tools resolve paths relative to cwd
2. Paths that escape cwd (via `..`) are rejected
3. Absolute paths outside cwd are rejected

```rust
fn resolve_path(path: &str) -> Option<String> {
    let cwd = get_working_dir();
    
    if path.starts_with('/') {
        // Absolute path must be within cwd
        if path.starts_with(&format!("{}/", cwd)) || cwd == "/" {
            return Some(path.to_string());
        }
        return None; // Escape attempt
    }
    
    // Resolve relative path, reject ".." escapes
    // ...
}
```

## Limitations

1. **Max path length**: 255 bytes (ProcessInfo.cwd_data is 256 bytes including null terminator)
2. **No automatic resolution**: Paths with `..` are not automatically resolved by the kernel
3. **Read-only ProcessInfo**: Userspace cannot directly modify cwd; must use chdir syscall

## Files

| File | Description |
|------|-------------|
| `src/process.rs` | ProcessInfo struct, Process.cwd, spawn_process_with_channel_cwd |
| `src/syscall.rs` | CHDIR syscall implementation |
| `src/shell/mod.rs` | ShellContext.cwd, passes cwd to spawned processes |
| `userspace/libakuma/src/lib.rs` | getcwd(), chdir() functions |
| `userspace/meow/src/tools.rs` | Cd tool, sandboxing via resolve_path() |
| `userspace/scratch/src/main.rs` | git_dir(), repo_path() using getcwd() |
