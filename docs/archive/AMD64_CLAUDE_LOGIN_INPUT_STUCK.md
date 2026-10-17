# amd64: Claude Code `/login` stops taking input — paste, Esc and Enter (2026-10-17)

**Status: OPEN — reliably reproduced, root cause not found.** Read **§ Handoff**
at the end first: two of the mechanisms proposed below were later ruled out, and
the section headed "Mechanism (confirmed)" is only partly right.

## Symptom

On the ryzen, over ssh from Ghostty: `claude` → `/login` → "Claude account with
subscription" prints the OAuth URL and `Paste code here if prompted >`. After the
person has been away in the browser, nothing typed has any effect: the pasted
code never appears (it would show masked as `****`), Esc does not cancel, Enter
does nothing. Plain typing earlier in the session worked.

## What was ruled out, in order

Each was tested with `scripts/utils/ssh_pty_steps.py` against the metal box:

| hypothesis | test | result |
|---|---|---|
| keystrokes never reach claude over ssh | type `/login` (with and without Enter) | echoes at once |
| launched from a shell rather than by sshd | `/bin/sh -i`, then `claude` typed in | works |
| Ghostty's `TERM` | `TERM=xterm-ghostty`, and `xterm` (the user) | works / same symptom |
| Ghostty's replies to claude's queries (DA1, XTVERSION, kitty `CSI ? u`) | replayed at startup | works; claude then sends Esc as `CSI 27 u` and it is honoured |
| bracketed paste | `ESC[200~…ESC[201~` at the code prompt | works (masked) |
| focus events (`?1004h` is on) | `ESC[O`, `ESC[I` before the paste | works |
| **time at the code prompt** | **`~45` s, then paste** | **reproduced: nothing appears** |
| idle anywhere | 45 s at the main prompt, then type | works |

So: only on the login code screen, and only after a delay.

## Mechanism (confirmed)

While stuck, claude's main thread sits in `epoll_pwait` with a 4 s timeout that
keeps returning **0**: stdin is never reported readable. Sending a bare **`\n`**
(Ctrl-J) after the stuck paste makes the whole paste appear at once. The input
was being held in the kernel's **canonical line buffer**: the session's
`TerminalState` had gone back to `ICANON` under a raw-mode TUI, and claude's
Enter (a bare `\r`, with `ICRNL` cleared by claude's own raw mode) never
completes a line.

The terminal state is one `Arc` shared by every process forked from the session
(`akuma-exec` `process/mod.rs`: fork clones `terminal_state`), which is Linux's
model too: a TTY's termios is per-TTY. On the login screen claude spawns helpers
to open the URL (`xdg-open` and `gio` exist on p3). `[PSTATS]` for the stuck
process shows 15 `vfork`s and a ~30 s spin of ~1 M `wait4` + `epoll_ctl` +
416 k `epoll_pwait`: claude polling for those children.

Not yet known: **which** process put the terminal back in canonical mode, and why
the same helper would not do it on Linux. amd64 rejects terminal ioctls on an
fd 0–2 that is redirected (`console_end` reads the fd table), so the writer holds
a real terminal fd (inherited, or `/dev/tty`).

## Diagnostic

`[tty] raw->canonical by pid=N (name) fd=F via {TCSETS|amd64 console TCSETS|set_terminal_attributes}`,
printed at every raw→canonical transition of a session's `TerminalState`,
16 lines per boot (`akuma_syscalls_glue::term::note_cooked`).

## PSTATS reading trap

amd64 records **x86_64** syscall numbers and `[PSTATS]` names them with the
asm-generic table: `io_setup` is `read` (x86 0), `getdents64` is `wait4` (61),
`madvise` is `epoll_ctl` (233), `memfd_create` is `epoll_pwait` (281), and the
folded ones are counted a second time under their asm-generic name. Decode before
reading a profile.

## Handoff (2026-10-17 evening, kernel a096dc54 on ryzen)

### Reproduce (about 2 minutes, no person needed)

Akuma up on ryzen (entry 20), no other `claude` running, then from the Mac:

```sh
HOSTIP=<station ip> XTERM=xterm-ghostty scripts/utils/ssh_pty_steps.py 2222 'exec /bin/sh -i' 110 \
  '~2' 'cd /src/github.com/netoneko/akuma; /usr/local/bin/claude\r' \
  '@Run/login' '/login' '@Sign in' '\r' '@Claude account' '\r' '@Paste code' '~45' \
  '\x1b[200~AbCdEf123456#xyzSTATE\x1b[201~' '@zSTATE'
```

It stops at the last step (the paste never shows as `****…zSTATE`). Without the
`~45` the same script passes. It reproduced on every attempt from an interactive
shell. Launching claude straight from sshd (`'cd /tmp; /usr/local/bin/claude'`)
passed once on a clean box and failed when an older claude was still alive.
Each run leaves an orphaned claude (closing ssh does not end it); `kill -9` them
between runs (safe since e672447e).

### What is established

- Input reaches the box and the kernel accepts it; nothing crashes; the box stays healthy.
- While stuck, claude's **main thread never calls `read`**. Its `epoll_pwait`
  keeps returning **1 event**, sometimes after 66 µs: some *other* fd is ready
  over and over, and stdin is never among what it is told about.
- During the first ~30 s on the login screen claude makes millions of syscalls:
  decoded from `[PSTATS]` (amd64 numbers named with the asm-generic table, see
  above), ~1 M `wait4`, ~1 M `epoll_ctl`, ~400 k `epoll_pwait`, 15–18 `vfork`.
  That is Bun polling for the URL-opener children (`xdg-open`; `gio` exists on p3).
  Whether that spin is the cause or a sibling symptom is open.
- ash (claude's parent) is parked in `wait4` during the stuck state on a096dc54.
  An earlier capture (5a577983, the user's own session) instead found ash in its
  **prompt loop** (`uname`/`getcwd`/`rt_sigaction`), i.e. back from its wait
  with claude still running. Both kernels lose input; the ash state may differ
  run to run. Check it on every repro.

### Ruled out

| hypothesis | how |
|---|---|
| `TERM`, Ghostty replies, kitty keys, bracketed paste, focus events | table above |
| a helper flipping the shared `TerminalState` back to canonical | the `[tty] raw->canonical` trace prints only ash's own transitions (leaving its line editor at Enter), never during the stuck window; a bare `\n` did **not** flush the paste on 5a577983 (it appeared to once on an older kernel with several orphaned claudes about) |
| sshd-side input path (`delegate_pid`, pipe-vs-channel routing) | `delegate_pid` is only set by `reattach`; claude's and ash's fd 0 both stay `Stdin` |
| `vfork` child `dup2` replacing claude's fd 0 | claude's fd table unchanged while stuck |
| exit published through a **stale SPAWN row** (`spawn_record_exit` names whatever pid the slot's row holds) | the `[exit] slot S row names pid X …` check (a096dc54) did not fire for claude. It **did** fire once at boot (`slot 11 row names pid 13 but the exiting task is pid 12`): a real stale row, worth its own look |

### Captured 2026-10-17 19:50 with `[stdin-starved]` (34e1f405)

The dump fires when a process's terminal stdin has held bytes for over 2 s while
an `epoll_pwait` lap ends, and prints every epoll in the fd table:

- **claude's main loop epoll (epfd 3) has fd 0 registered and armed**:
  `fd=0 events=0x40000011` (`EPOLLIN|EPOLLHUP|EPOLLONESHOT`) `disarmed=false
  last_ready=0x0 kind=Stdin`. **EPOLLONESHOT is ruled out** as the cause, and so
  is a lost registration. fd 0 has **never** been reported: a reported one-shot
  entry would read `disarmed=true`.
- **stdin really does hold the bytes** (pending ≥ 2 s) as seen from thread 856
  (an HTTP worker, `tgid=848`), via `current_channel().has_stdin_data()`.
- fd 0 sorts first in the scan, so a main-thread lap that saw stdin ready would
  report it first. **So the main thread's readiness check for fd 0
  (`epoll_check_fd_readiness` → `Stdin` arm → `current_channel()`) does not see
  the channel thread 856 sees.** Leading hypothesis: on amd64 every thread has its
  own `Process`; `current_channel()` returns `Process.channel` and otherwise
  falls back to `get_channel(tid)`, the per-slot **exit** channel (an empty FIFO).
  If the main thread's `Process.channel` is `None` or was replaced (vfork/exec of
  a helper, or the clone path for some threads), its stdin probe answers "empty"
  for ever, while the data sits in the session channel. That would also explain
  why typing worked before the helpers were spawned.
- Other content of epfd 3: claude's helpers are spawned with **pidfds** (fds 13,
  20, 21, 27, 31, `events=0x11`, level-triggered) and **Unix-socket stdio pairs**.
  An exited child's pidfd stays readable, which is probably the 1 event per lap
  the stuck main loop keeps getting.
- epfd 7 (thread 856's HTTP loop: eventfd + socket) and epfd 14 (an eventfd) are
  unrelated.

**First thing to do next:** print `Arc::as_ptr` of `current_channel()` for the
main thread (848) and for thread 856 inside the `Stdin` arm of
`epoll_check_fd_readiness`, plus whether it came from `Process.channel` or the
`get_channel(tid)` fallback. If they differ, find which path set the main
thread's `Process.channel` (or left it `None`).

### Next steps, in order

1. **Name the fd claude's epoll keeps reporting.** `[epoll-spin]` only fires
   after 200 000 *identical* returns; this loop alternates. Add a per-pid
   ring of `(fd, revents)` for the last N `epoll_pwait` returns, readable from
   `/proc/<pid>/` or dumped on demand, and look at it in the stuck state. If fd 0
   never appears with `EPOLLIN` while sshd has written bytes, check fd 0's
   registration: `EPOLLONESHOT` (4211281b, new this week) disarms an entry after
   one report until `EPOLL_CTL_MOD`; a re-arm Bun issues on a *different* epoll fd
   (claude holds three: 3, 7, 11/12/14) or via DEL+ADD would leave it disarmed
   forever. The ~1 M `epoll_ctl` during the spawn spin is exactly where a re-arm
   could be lost.
2. **Linux A/B.** Run the same `claude` binary (`/usr/local/bin/claude`, musl,
   2.1.301) in Docker on Pop with the same steps through `script`/a PTY. If Linux
   also loses the paste after 45 s, this is claude, not Akuma.
3. Bun's child reaping on Linux is pidfd-based; `pidfd_open` (x86 434) — check
   whether Akuma answers it. A fallback to `wait4` polling would explain the
   1 M-call spin.
4. The boot-time stale SPAWN row (slot 11) — separate bug, same family as
   c0002a3e's "don't reissue a zombie's tid".

### Workarounds for the user meanwhile

- Paste the code **immediately** after `/login` prints the URL. Have the
  browser tab ready, and do not leave the code prompt idle.
- Or log in on another machine and copy `~/.claude/.credentials.json` (Linux
  stores it as a file; macOS uses the Keychain).

## Background

- [`AMD64_TLB_STUCK_MMAP_REGIONS.md`](AMD64_TLB_STUCK_MMAP_REGIONS.md) — the wedge found the same afternoon, on the way here
- [`CTRL_C_SIGINT_DELIVERY.md`](CTRL_C_SIGINT_DELIVERY.md) — the line discipline's other consumer of session input
