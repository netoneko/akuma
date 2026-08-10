# paws — experimental minimal shell

> **EXPERIMENTAL. `extreme-size` demo purposes only.**
> Not a general-purpose shell and not a busybox replacement. If you want a shell
> that can run real scripts, use busybox `ash`.

598 lines, pure `libakuma`, no third-party dependencies.

## Why it exists

The `extreme-size` profile targets a 4 MB box. Its shared file-page dedup cache
is sized `RAM/8` — **144 pages at 4.5 MB** — while `/bin/busybox` maps **265
pages**. A binary bigger than the cache cannot be deduplicated across processes,
so concurrent shells each fault private copies of their text and the box dies on
`fork`. See
[`../../docs/archive/FPCACHE_UNDERSIZED_AT_LOW_RAM.md`](../../docs/archive/FPCACHE_UNDERSIZED_AT_LOW_RAM.md).

paws maps **8 pages** — about 5% of that cache.

| shell | file bytes | RO-mapped pages |
|---|---:|---:|
| `/bin/busybox` as shipped | 1,116,408 | 265 |
| busybox rebuilt, ash-only | 198,848 | 41 |
| **paws** | 37,296 | **8** |

## What it is not

- **Not busybox/ash compatible.** Hand-rolled parser, fixed builtin match.
- No `exec`, `printf`, `test`, `[`, `read`, `getopts`, functions, variables,
  globbing, quoting rules, job control, or history.
- Pipes and redirection exist but are limited to a hardcoded set of builtin
  combinations — do not rely on them.
- `ls` takes directories, not file arguments.

`acceptance/archive/08_meow_clone_compile_run.md` already documented these constraints
back when paws was the VM's shell: *"It lacks `printf`, `which`, `find -name`,
`head`, `tail` … Pipes and complex redirections are unreliable."*

## What it is good for

Being the login shell handed to `sshd --shell /bin/paws` when all that shell has
to do is exec one binary. That is enough for the agentic demo: meow's Shell tool
uses its own in-process "pretend shell" (`USE_PRETEND_SHELL = true`), so tool
commands never touch paws at all.

Verified 2026-08-10: `acceptance/archive/08` passes end to end on a 4.5 MB
`extreme-size` kernel with paws as the login shell — meow (ollama `qwen3:4b`)
clones with `scratch`, compiles `hello.c` with `tcc`, and runs it.

## Builtins

`cd pwd ls cat cp mv rm mkdir rmdir touch echo uname uptime sleep clear whoami
free find grep pkg exit`

`free` reads `sysinfo(2)` (syscall 179) directly — this kernel has no
`/proc/meminfo`, so it is the only way userspace can see PMM page counts.

Anything else is resolved against `/bin` and `/usr/bin` and exec'd.

## History

Added 2026-02-13, removed 2026-02-26 at `c0af6c7` (*"remove paws shell, maybe I
will build one later"*), revived 2026-08-10 for the 4 MB demo. It built on the
first try after six months — the only change needed was dropping an unused
`noshell` dependency that was declared but never used, and one dead import.
