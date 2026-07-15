# Shell Environment Variables

The built-in kernel shell supports environment variables that are stored per-session and passed to child processes.

## Commands

### export

Set or list environment variables.

```
export                  # List all exported variables
export KEY=VALUE        # Set a variable
export A=1 B=2          # Set multiple variables
```

### set

List or set shell variables. In this shell all variables are exported, so `set` and `export` behave identically for assignment.

```
set                     # List all variables (KEY=VALUE format)
set KEY=VALUE           # Set a variable
```

### unset

Remove one or more environment variables.

```
unset KEY               # Remove a single variable
unset A B C             # Remove multiple variables
```

### env

Print all environment variables in `KEY=VALUE` format (one per line).

```
env
```

## Variable Expansion

The shell expands variables in command lines before parsing pipelines, chains, and redirection.

| Syntax | Meaning |
|--------|---------|
| `$VAR` | Replaced with the value of `VAR` |
| `${VAR}` | Replaced with the value of `VAR` (braced form) |
| `$$` | Literal `$` character |
| `~` | Expanded to `$HOME` at word boundaries |

Single-quoted regions (`'...'`) suppress expansion:

```
echo '$HOME'            # Prints literal $HOME
echo $HOME              # Prints /
echo ~/docs             # Prints /docs
```

## Default Variables

New shell sessions start with these defaults (sourced from `process::DEFAULT_ENV`):

| Variable | Default | Description |
|----------|---------|-------------|
| `PATH` | `/usr/bin:/bin` | Executable search path |
| `HOME` | `/` | Home directory |
| `TERM` | `xterm` | Terminal type |
| `PWD` | `/` | Current working directory |

`PWD` is automatically kept in sync with the shell's current directory -- running `cd /foo` updates both the internal cwd and the `PWD` environment variable.

## Passing to Child Processes

When the shell spawns an external binary, all environment variables are passed to the child process. The flow is:

```
ShellContext.env (BTreeMap)
  → ctx.env_as_vec()        converts to Vec<String> of "KEY=VALUE"
  → spawn_process_with_channel_cwd(..., env, ...)
    → Process::from_elf(..., env, ...)
      → elf_loader::setup_linux_stack(stack, args, env, auxv)
        → envp pointers on the user stack (Linux AArch64 ABI)
```

Child processes see the variables through the standard `envp` mechanism -- musl's `getenv()` or libakuma's `env()` function.

If no environment is provided to the process spawner (e.g. by non-shell callers), the same defaults from `process::DEFAULT_ENV` are used as a fallback.

## Implementation

### Storage

Environment variables live in `ShellContext.env`, a `BTreeMap<String, String>` (`BTreeMap` is used instead of `HashMap` to avoid needing a hasher in `no_std`). Each SSH session gets its own `ShellContext`, so variables are isolated between sessions.

### DEFAULT_ENV Constant

The default environment is defined once in `src/process.rs`:

```rust
pub const DEFAULT_ENV: &[&str] = &[
    "PATH=/usr/bin:/bin",
    "HOME=/",
    "TERM=xterm",
];
```

This constant is used by both the shell (to populate initial env) and the process spawner (as a fallback when no env is provided).

### Variable Expansion

`expand_variables()` in `src/shell/mod.rs` runs before command chain parsing. It walks the byte slice character-by-character, tracking single-quote state, and replaces `$VAR` / `${VAR}` tokens with values from `ShellContext.env`.

## Files

| File | Description |
|------|-------------|
| `src/process.rs` | `DEFAULT_ENV` constant, env parameter on `exec_async_cwd` / `exec_streaming_cwd` |
| `src/shell/mod.rs` | `ShellContext.env` field, `expand_variables()`, env threading through execution paths |
| `src/shell/commands/builtin.rs` | `ExportCommand`, `SetCommand`, `UnsetCommand`, `EnvCommand` |
| `src/shell/commands/mod.rs` | Command registration |

## Related Documentation

- [CWD.md](CWD.md) -- Current working directory system (PWD is part of env)
- [KILL_COMMAND.md](KILL_COMMAND.md) -- Another built-in command reference
