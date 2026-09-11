# amd64: an `ssh` session's child gets a `ProcessChannel` — and `^C` becomes a signal

> 2026-09-11. `proposals/NEXT_AGENT_AMD64_SSHD_CHILD_CHANNEL.md`, steps 1–3 of
> its four. Step 4 is **not** done and §6 says why — the reason turned out to be
> concrete rather than deferred.
>
> **Shared crates changed on purpose.** `akuma-exec` (the stdin sink and one new
> runtime hook), `akuma-kernel-glue` and `akuma-syscalls-glue`. AArch64 was
> booted on both sides: **307 PASSED / 0 FAILED**, unchanged.

## 1. What it was

`sshd` opens `/proc/<pid>/fd/0` and writes the client's keystrokes into it. On
the AArch64 kernel that lands in the mounted `ProcFilesystem`, which calls
`akuma_exec::process::write_to_process_stdin` — the tty's front door, where the
INTR character is consumed and raised as `SIGINT` on the terminal's foreground
process group.

On amd64 it landed nowhere near there. `fd::sys_openat` intercepted exactly that
path, ahead of the VFS, and handed back the **write end of the child's stdin
pipe**. So the bytes reached the shell, and `^C` reached it as a `0x03` byte.

Three facts stood in the way and each was checked before anything was written:

| claimed | actual |
|---|---|
| `SPAWN_FLAG_PTY` is "accepted and currently ignored" (`sys_spawn`'s own doc) | **not received.** The dispatch arm was `301 => sys_spawn(a1..a5)` and the flags word is the sixth argument. `syscall_entry` was never the problem — it pushes `r9` precisely so a sixth argument survives (`futex`'s bitset is the other user). |
| closing the interception "waits for a working AArch64 verification loop" | that clause was spent. `cargo build --release` + `MEMORY=2048M INSTANCE=3 sh scripts/cargo_runner.sh …` reports 307/0 in about two minutes. |
| the deferred work is "`/proc/<pid>/fd/0` + `delegate_pid`" | `delegate_pid` is `sys_reattach`'s — `box grab` — which moves a *target's* channel to a *grabber*. It has nothing to do with how `sshd` reaches its own child. A leftover phrase, now deleted from both places that carried it. |

## 2. What changed

**The sink learned to find a pipe.** `write_to_process_stdin`, after its ISIG
filtering and before its channel branch, resolves the target's own fd 0; if that
is a `FileDescriptor::PipeRead`, the surviving bytes go into that pipe. The
order is the substance — the INTR character is the line discipline's, not the
pipe's, and a pipe route taken first would hand `0x03` to the program as data,
which is the defect being closed.

This needed a new `ExecRuntime` hook (`pipe_write`), for the same reason
`pipe_close_write` is one: the pipe table lives in `akuma-syscalls-glue`, which
depends on `akuma-exec` and cannot be depended on back. Both kernels register
the same function, `pipe::pipe_write_no_sigpipe` — no `SIGPIPE`, because the
caller here is never the process whose pipe it is.

**An empty `data` is not offered to the pipe.** That is the lone-INTR keystroke
(`[0x03]`, filtered to nothing), the commonest thing on this path and the one
case where a zero-byte write could fail: a `^C` typed at a shell whose stdin pipe
had lost its reader would come back `EPIPE`, be reported to `sshd` as a failed
write of a byte the line discipline had already consumed, and be resent — a
`SIGINT` every bridge tick instead of one per keystroke. `stripped_count` still
counts it as accepted, which is what that variable has always been for.

**`sys_spawn` receives its flags**, and a `SPAWN_FLAG_PTY` child gets a
`ProcessChannel` with `set_terminal(true)`, registered as `Process::channel`,
plus `foreground_pgid = pid` on its (fresh) `TerminalState`. The AArch64 spawn
sets that same field, unconditionally, and without it a `^C` would broadcast at
`init`'s group: `TerminalState::default()` seeds `foreground_pgid = 1`.

`register_exec_process` takes the channel as a **parameter** rather than deriving
it. It derives "is this the console's process" from fd 0 being a
`FileDescriptor::Stdin`, and a session child that later takes its stdin from the
channel too would answer that the same way and be handed the *serial line's*
channel.

**The interception is gone.** `sshd`'s `open("/proc/<pid>/fd/0", O_WRONLY)` now
goes through glue like every other path under `/proc`. Nothing in `sshd` changed.

## 3. What went away

| | before | after |
|---|---|---|
| `sys_openat`'s `/proc/<pid>/fd/0` block | 12 lines + 20 of doc | deleted; the module header's "what is left is one path" is now "nothing is left" |
| `alloc_pipe_fd(id, is_write, adopt_initial)` | 2 of its 4 combinations live | `alloc_child_stdout_fd(id)` — the interception was the only caller that wrote, and the only one that cloned rather than adopted |
| `stdin_pipe_for_pid` | `pub`, 2 callers | private, 1 caller (`sys_close_child_stdin`) |
| `Spawn::stdin_pipe`'s write end | reached by *path*, `sshd` holding a `PipeWrite` | reached by *id*, no descriptor anywhere |

That last row is why the reap's `close_write`-not-`free` reasoning had to be
rewritten rather than kept: the old reason was "`sshd` may still hold an open
descriptor over it". It cannot any more. The rule survives with a *different*
justification — a **reader** reference can outlive the row (a zombie whose fd
table is not yet torn down, a `fork` descendant that inherited fd 0), and `free`
would make its eventual close land on a reissued pipe id.

## 4. What this does not do, and the reason is not "later"

The prompt's step 4 was "fd 0 becomes the channel, and the `ioctl` preamble's
first term can go". It is not done, and the blocker is specific:

**glue's `Stdin` read arm writes the line discipline's echo to the channel's
*stdout* FIFO** (`ch.write(&result.echo)`). On AArch64 that is where the child's
stdout already is — `sys_spawn` hands `sshd` a `FileDescriptor::ChildStdout(pid)`
and the echo flows out with the program's own output. On this target a spawned
child's stdout is a **pipe** and `sshd` reads the pipe, so the echo would go into
a FIFO nothing drains: no echo for the user, and a buffer climbing to its 1 MiB
cap for the life of every session.

So fd 0 cannot move without fd 1/2, and moving those means `sys_spawn` returning
a `ChildStdout(pid)` and `sys_write`'s console preamble no longer claiming every
`Stdout` descriptor (on this target `FileDescriptor::Stdout` means *the serial
line*, not "the process's channel"). That is a real piece of work and it collides
with the two-channels-per-process shape: `register_child_channel` currently
carries the **exit** channel here, and glue's `ChildStdout` read arm resolves
through exactly that map.

Nothing about cooked input or echo over `ssh` regressed — there was none before
either.

## 5. Cautions that survived the change

- `Process::channel` is `clone`d by `inherit_from`, so a session's channel is
  shared by the shell and everything it forks. That is right for a session and
  is exactly why a per-process interrupt flag must not live there
  (`AKUMA_AMD64_SIGNAL_DELIVERY.md` §5d). `children.rs`'s comment on that is
  updated: the sharing is now **two** trees (the console's, and one per ssh
  session), not one machine-wide object and not "not an ssh session".
- Every amd64 process still has a second, *exit* channel under its task slot.
  `current_channel()` falls back to it and `ProcessChannel::new` defaults
  `is_terminal` to true, so "the process has a channel" proves nothing here.
  Three channels now exist in this kernel by role; know which one you hold.
- The new sink arm is **unreachable on AArch64** for every caller that exists —
  the one writer of `/proc/<pid>/fd/0` in the tree is `sshd`'s bridge, and there
  its target's fd 0 is a `FileDescriptor::Stdin`. The case where both a channel
  and a `PipeRead` at fd 0 exist is a shell pipeline's downstream child, which
  nothing writes to. Linux would deliver to the pipe, which is what this does.

## 6. Verification

| gate | before | after |
|---|---|---|
| QEMU/TCG `SMP=4` | 665 / 0 | **665 / 0** |
| Firecracker/KVM `SMP=4` | 643 / 0 | **643 / 0** |
| `amd64_ring3_check.py --smp 1 -n 20` | OK | **OK** (see below) |
| `amd64_mem_trials.py --local-only` | 10 / 10 | **10 / 10** |
| AArch64 boot suite (`MEMORY=2048M INSTANCE=3`) | 307 / 0 | **307 / 0** |
| host tests (`cargo test`) | 1375 / 0 | **1379 / 0** (+4, `version::parse_triple`) |
| clippy, amd64 `--release` and `--release --features no-tests`, and AArch64 | clean | **clean** |

The **first** ring-3 run reported FAILED, and it is worth recording why it did
not mean anything: its *pre-workload* sample came back unparsed — both
`free`'s columns and `/proc/meminfo`'s `Slab:` row read `None` — so the heap
column had no baseline to difference against, which the script counts as a
failure by design ("this run proves nothing about the kernel heap"). Every
probe in that same run passed and the *post*-workload sample parsed fine. The
re-run read both samples and reported `heap: 1499 -> 1536 kB (drift +37 kB,
tolerance 8192)`, sessions 20/20, `ps` 5 -> 5, OK. It is the first ssh command
after boot that is flaky, not the change.

### The observable, run by hand

No script covers `^C` over `ssh`, so it was driven through a real `pty.fork()`
client against `INIT=/bin/sshd` (`ssh -tt`, so the client sends the `pty-req`
that sets `SPAWN_FLAG_PTY`):

- **a foreground job dies and the shell lives.** `echo MARK1; sleep 60; echo
  AFTER_SLEEP` then `0x03`: the prompt came back at once instead of 60 s later,
  `AFTER_SLEEP` never printed, and the next command reported the *same* shell
  pid. `sleep` does not read its stdin, so nothing but a signal could have ended
  it.
- **the INTR byte is consumed, not delivered.** The coincidence above is not
  proof on its own, so: `busybox sh -c 'trap "" INT; busybox od -An -c'` — a
  foreground reader that ignores `SIGINT` and prints every byte it gets — fed
  `0x03`, then `Z`, then newline. `od` printed `Z 
 004 …`. **No `003`.**
  (The `004` is there because only the ISIG strip happens on this target: fd 0
  is still a pipe, so there is no canonical `VEOF` processing behind it. That is
  the design, not a gap.)

A `sleep 120 &` **background** job survives, which is also correct and worth
saying so nobody re-files it: `ash` without job control sets `SIGINT` to
`SIG_IGN` for a background command, on Linux too.

The serial-console control (`AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md` §6's delayed
feed, `INIT=/bin/busybox INITARGS=cat`) still prints each typed line **twice** —
the line discipline's echo and `cat`'s output — which is what it printed before
this change and is the check that the shared sink's new arm did not disturb the
path that does not take it.

## 7. The version regression this turned up

Unrelated to the above, found while it was in flight and fixed with it: `uname -r`
reported **`0.1.0`** on both kernels, which is `akuma-syscalls-glue`'s package
version. `UTSNAME`'s release field was `env!("CARGO_PKG_VERSION")`, which expands
to *the crate the macro is written in* — so the moment `src/syscall/` became a
crate (2026-09-01) it silently stopped naming the kernel. Exactly the same class
of break as `AKUMA_GIT_SHA`, which that same move caught only because it failed
to compile.

`akuma-syscalls-glue::version::RELEASE` is the one literal now: `uname -r` reads
it, `VERSION_TRIPLE` (and so `akuma_get_version`) is `parse_triple`d from it, and
amd64's banner prints it plus `-amd64`. `banner::RELEASE` had a third hardcoded
spelling (`"0.1.0-amd64"`) under a doc comment claiming it was "shared with
`usermode::UTSNAME` so the banner and `uname(2)` cannot disagree" — there is no
`usermode::UTSNAME`, and they did disagree. Both kernels now report `0.0.8`.

`parse_triple` rejects malformed input as a **compile error**, not a `0`: it runs
once per build on a literal in its own file, and a lenient parse would turn a
typo into a plausible wrong version, which is the failure the const exists to
remove.

## Background

`AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md` (the worked example — a *producer* was
the missing half there and it was again here),
`AKUMA_AMD64_SIGNAL_DELIVERY.md` §5d and §5f,
`AKUMA_AMD64_SPAWN_ROW_STDIO.md` (why `Spawn::stdin_pipe` outlived its
siblings), `AMD64_SPAWN_PIPE_LEAK.md` (the adopt-vs-clone rule
`alloc_child_stdout_fd` still carries), `TTY_SHENANIGANS.md` round 3 (the two
interactive-shell architectures), `CTRL_C_SIGINT_DELIVERY.md` (the ISIG branch
this piece finally reaches), `SRC_SYSCALL_EXTRACTION.md` (§7's regression).
