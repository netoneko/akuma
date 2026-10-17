# amd64: Claude Code `/login` stops taking input — paste, Esc and Enter (2026-10-17)

**Status: OPEN — mechanism confirmed, culprit not yet named.** A diagnostic that
names the process is in the tree (`[tty] raw->canonical by pid=…`).

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

## Background

- [`AMD64_TLB_STUCK_MMAP_REGIONS.md`](AMD64_TLB_STUCK_MMAP_REGIONS.md) — the wedge found the same afternoon, on the way here
- [`CTRL_C_SIGINT_DELIVERY.md`](CTRL_C_SIGINT_DELIVERY.md) — the line discipline's other consumer of session input
