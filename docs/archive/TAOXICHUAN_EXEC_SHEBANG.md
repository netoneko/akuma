# Taoxichuan's execve did not run `#!` scripts

**Status: FIXED 2026-11-03** in `crates/akuma-tokio-kernel-amd64/src/child.rs`. Found during
[`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md).

## Symptom

GLM wrote its first `RUSTC_WRAPPER` as a Python script starting with `#!/usr/bin/env python3`. cargo failed
before compiling anything:

```
error: could not execute process `/usr/local/bin/rustc-argvfix rustc -vV` (never executed)
Caused by:
  Exec format error (os error 8)
```

From busybox `sh`, the same file "ran". ash falls back to interpreting a file as a shell script on
`ENOEXEC`, so it fed the Python to itself (`import: command not found`).

## Cause

`exec_real_task` read the image and returned `ENOEXEC` unless it started with `\x7fELF`. The AArch64 kernel's
`do_execve` and `spawn` follow `#!` lines (`exec_shebang`, `resolve_shebang_chain`). Taoxichuan's own exec
path never called them.

## Fix

Before the ELF check, `exec_real_task` follows up to 4 `#!` hops (`SHEBANG_MAX_DEPTH`, Linux's
`BINPRM_MAX_RECURSION`). It uses the shared `akuma_exec::process::{parse_shebang, shebang_hop}`, so the
parser, the 256-byte line window (`SHEBANG_MAX`) and the argv rule are the AArch64 kernel's:
`[interpreter, optional arg, path as written, argv[1..]]`, with each hop dropping the previous `argv[0]`. An
unreadable interpreter is `ENOENT`. A chain that is still a script after 4 hops falls through to `ENOEXEC`.

## Verification

- TCG: `env /root/s.sh hello`, where `s.sh` is `#!/bin/sh`, printed
  `shebang-ok argv0=/root/s.sh argc=1 first=hello`.
- Ryzen guest: `env /tmp/sb.py a b`, where `sb.py` is `#!/usr/bin/env python3` (one hop to `env`, which
  then `execvp`s `python3`), printed `py-shebang-ok ['a', 'b']`.

## Background

- `crates/akuma-exec/src/process/spawn.rs`: the shared parser and its host tests (`shebang_tests`).
- [`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md).
