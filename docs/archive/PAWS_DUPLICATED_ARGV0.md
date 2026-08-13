# paws passed the command name twice — `tcc: error: file 'tcc' not found`

**Status: FIXED, 2026-08-13.** `tcc` compiles and links from a `paws` shell again,
including on the `extreme-size` profile at the 4.0 MB floor.

## Symptom

Anything launched from `paws` received its own name as an extra first argument.
Invisible for most commands; fatal for `tcc`:

```
~ # tcc -static -B /usr/lib/tcc -o /tmp/hello_c /tmp/hello.c
tcc: open('/usr/lib/crt1.o') -> SUCCESS (fd=3)
tcc: open('/usr/lib/crti.o') -> SUCCESS (fd=3)
tcc: error: file 'tcc' not found
```

Note the missing file is named `tcc` — which is not on the command line at all.
It is `argv[0]`, arriving a second time as `argv[1]`, where tcc reads it as the
first input file.

## Root cause

The `spawn` syscall builds argv itself as `[path, ...args]`
(`spawn_process_with_channel_ext` pushes `path` first). All four of `paws`'s spawn
sites passed their **whole** argument vector — `args[0]` is the command name,
which `paws` had just used for `find_bin(&args[0])` — so the name landed in argv
twice:

```
argv = ["/bin/tcc", "tcc", "-static", "-B", "/usr/lib/tcc", ...]
                    ^^^^^ read as an input file
```

The debug print two lines below the bug had `.skip(1)` on the same vector, i.e.
the code already knew element 0 was the command name. Only the value handed to
`spawn` was wrong.

**Why it hid for so long: busybox is immune.** A leading applet name is exactly
how a multicall binary expects to be invoked, so `/bin/busybox busybox ls /tmp`
re-dispatches and behaves identically to `/bin/busybox ls /tmp`. Since nearly
everything reachable from `paws` *is* busybox, the duplicate was invisible until a
non-multicall binary that takes positional operands (`tcc`) hit it.

## Scope — it is `paws`, not a profile

Worth stating plainly, because the first guess was wrong twice (a stale disk, then
the `extreme` profile). On the **release** kernel, with one disk image, the same
command:

| launched from | result |
|---|---|
| busybox `sh` (release/devbox default login shell) | links, runs |
| `paws -c "…"` (same kernel, same disk) | `tcc: error: file 'tcc' not found` |

It only *looked* extreme-specific because `extreme` is the profile whose login
shell is `paws` (`config::USERSPACE_SSHD_SHELL`). Also ruled out on the way: the
demand-paged ELF loader (forcing `HEAP_SLURP_MAX` non-zero changes nothing) and
argv marshalling itself (`busybox printf '[%s]\n' …` shows every argument intact).

## Fix

`.skip(1)` when building `arg_refs` at all four spawn sites
(`execute_external_reattach`, the pipe target, `execute_external_with_status`,
`execute_external_and_capture`), and drop the now-double `.skip(1)` from the two
debug prints.

## Verify

```
tcc -static -B /usr/lib/tcc -o /tmp/hello_c /tmp/hello.c   # exits 0
/tmp/hello_c                                               # Hello, Akuma!
busybox printf '[%s]\n' a b c                              # [a] [b] [c]
busybox ls -la /tmp                                        # flags reach busybox
```

Verified on `extreme-size` at 64 MB and at the 4.0 MB floor, and on `release`.

## Two neighbouring facts, so they are not re-discovered as bugs

- **`tcc` output must be linked `-static`.** `tcc hello.c -o /tmp/h` links
  successfully and produces a binary that segfaults; `tcc -static …` produces one
  that runs. This is the long-standing constraint recorded for
  `userspace/tcc` — the acceptance playbook always passes `-static`. tcc emitting
  a binary that cannot run, with no diagnostic, is a sharp edge worth closing
  separately.
- **At the 4.0 MB floor, `tcc` is memory-marginal, and OOM presents as SIGSEGV.**
  It succeeds from a fresh boot but is squeezed out if other commands ran first;
  the kernel log names it exactly:
  `[IA-DP] … single-page fallback OOM, 16 free pages` followed by
  `[Fault] Process N (/bin/tcc) SIGSEGV`. That is the documented OOM-kill policy,
  not a crash. `acceptance/05_meow_tcc_extreme_4mb.md`'s "run commands one by one"
  instruction and its free-RAM table describe this regime.
  The same floor shows a *pre-existing* limit on repeated piped spawns — the first
  `a | b` works and later ones report `paws: pipe target not found`, identically
  before and after this fix (A/B'd).

## Background

- [`EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md`](EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md)
  — the kernel bug that had to be fixed before `paws` could be reached over ssh on
  `extreme` at all, which is why this had gone untested there.
- `acceptance/05_meow_tcc_extreme_4mb.md` — the playbook whose compile step this
  unblocks.
- `crates/akuma-exec/src/process/spawn.rs` — `spawn_process_with_channel_ext`,
  where argv is assembled and `path` is pushed as `argv[0]`.
