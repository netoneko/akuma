# In-kernel shell

> **REMOVED 2026-08-10 (branch `trim-fat-sshd`).** The in-kernel shell (`src/shell/`, `crates/akuma-shell` consumers) was deleted from the
> tree along with the built-in SSH server that was its only front end. This
> document is kept verbatim below as the historical record of how it worked;
> it no longer describes anything in `src/`. See
> [`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md) for what replaced it
> (a userspace shell over the userspace `/bin/sshd` — busybox `ash`, or `userspace/paws` on the 4 MB demo).

The built-in interactive shell exposed over the kernel's own SSH server
(`:2222`) — distinct from userspace `dash`/`paws`, which run as ordinary ELF
processes. Source: `src/shell/mod.rs` + `src/shell/commands/`. The command
trait, registry, parser, and interactive-execution machinery are extracted
into the host-testable `crates/akuma-shell` crate; `src/shell/` is the kernel
adapter (the `KernelShellBackend`) plus the concrete command implementations.

> **Stability: C (active risk).** The heaviest-churned file in this batch:
> touched as recently as Jul 3 2026 (`optional smoltcp, almost done`,
> feature-gating the interactive/pty paths) and Jun 25 (`fix pty in built-in
> shell`). Both June/July touches are still inside the repo's active-churn
> window as of this writing (Jul 15). Expect surprises around the
> `smoltcp`-gated interactive paths specifically; the buffered/pipeline
> execution paths are older and steadier.

## Command dispatch model

`Command` (from `akuma_shell`) is the trait every builtin implements:
`name()`, `description()`, `usage()`, and an async
`execute(args, stdin, stdout, ctx) -> Result<(), ShellError>` returning a
boxed, pinned future (`src/shell/commands/builtin.rs:24-51` for `echo` as the
simplest example). `CommandRegistry` holds `&'static dyn Command` references,
registered once in `create_default_registry()`
(`src/shell/commands/mod.rs:30-79`).

`KernelShellBackend` (`src/shell/mod.rs:64-112`) implements the crate's
`ShellBackend` trait, wiring the extracted shell logic to kernel-specific
subsystems: `find_executable` searches `/usr/bin` then `/bin` via
`async_fs::exists`/`list_dir` (`:123-142`) for anything not registered as a
builtin (`builtins_first()` is gated by `crate::config::SSH_BUILT_INS_FIRST`);
`execute_buffered`/`execute_streaming` hand off to
`akuma_exec::process::exec_*` to actually run an external ELF binary as a
child process; `write_file`/`append_file` go through `crate::async_fs` (see
[`async-fs.md`](async-fs.md)).

Three execution shapes exist, all built on the same registry:

- **Buffered** (`execute_external`, `:144-186`) — runs to completion,
  collects output, used for piped/non-interactive commands.
- **Streaming** (`execute_external_streaming`, `:188-213`) — streams output
  as it's produced, still non-interactive.
- **Interactive** (`execute_external_interactive`,
  `#[cfg(feature = "smoltcp")]`, `:215-353`) — the built-in `:2222` SSH shell
  spawning a real pty-backed child (`spawn_process_with_channel_ext(...,
  pty=true)`), forwarding stdin, handling Ctrl-C (`0x03` → `channel.
  set_interrupted()`), and distinguishing client-stdin-EOF (deliver EOF,
  keep streaming output) from client-disconnect (deliver EOF, stop
  streaming) — see the inline comments at `:253-275` for the reasoning.

The extracted crate also provides pipeline (`|`), chaining (`;`, `&&`), and
redirection (`>`, `>>`) parsing (`parse_pipeline`, tested at
`src/shell/mod.rs:424-454`) and `expand_variables`/`translate_input_keys` for
the interactive path.

## Notable built-in commands

Registered in `create_default_registry()`
(`src/shell/commands/mod.rs:34-77`), implemented in `builtin.rs` / `fs.rs` /
`exec.rs` / `net.rs`:

| Command | Description |
|---|---|
| `echo`, `akuma` | echo text / ASCII art banner |
| `stats`, `free`, `pmm` | network stats / memory usage / PMM stats |
| `ps`, `kill`, `kthreads` | list processes / terminate by PID / list kernel threads |
| `pwd`, `cd` | working-directory management |
| `uptime`, `clear`, `reset` | system uptime / clear screen / reset terminal |
| `export`, `set`, `unset`, `env` | shell/environment variable management |
| `grep`, `help` | filter lines by pattern / command help |
| `ls`, `find`, `cat`, `write`, `append`, `rm`, `mv`, `cp`, `mkdir`, `df`, `mount` | filesystem commands (`fs.rs`) |
| `curl`, `nslookup`, `pkg` | network commands (`net.rs`, `#[cfg(feature = "smoltcp")]` only — need the native TCP/IP stack) |
| `exec` | process execution (`exec.rs`) |

Anything not in this table falls through `find_executable` to `/usr/bin` or
`/bin` and runs as an external ELF binary.

## Background

- `docs/archive/BOX_PTY_INTERACTIVE_SHELL.md` — the pty-vs-pipe wiring
  (`is_terminal` flag on `SPAWN_EXT`) that `execute_external_interactive`
  relies on.
- `docs/archive/RICH_TERMINAL_INTERFACE_OVER_SSH.md` — the terminal syscalls
  (raw mode, cursor control) underpinning interactive command execution and
  the `meow`/editor TUI integration.
- `docs/archive/OPTIONAL_SMOLTCP.md` — why `net.rs` and the interactive SSH
  path are `#[cfg(feature = "smoltcp")]`-gated (the rump-only devbox build
  drops the native stack entirely).
- `docs/archive/SHELL_ENVIRONMENT_VARIABLES.md`, `docs/archive/CWD.md` —
  `export`/`set`/`unset`/`env` and `cd`/`pwd` history.
