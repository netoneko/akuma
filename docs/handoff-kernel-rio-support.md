# Handoff prompt: kernel support for rio on the amd64 panel

Paste everything below the line into a fresh session started in this repo.

---

You are working in the Akuma kernel repo (`~/github.com/netoneko/akuma`). The
goal of this session is kernel support for the **rio terminal**, which already
runs on the trashcan's 4K panel through a software wgpu backend, but only
thanks to userspace workarounds for kernel gaps. Make those gaps go away, most
valuable first, and prove each fix on the box.

## Read first, in this order

1. `CLAUDE.md` (this repo's rules — they override anything below), especially
   "Kernel conventions" (justify every allocation; console output only through
   `safe_print!`/`tprint!`) and "Working with Claude Code in this repo" (**the
   user drives all commits**: never commit, push, reset or rebase; run clippy
   and the host tests, leave the work uncommitted; no fork/multi-agent
   fan-out; never launch background agents without asking).
2. `docs/archive/AKUMA_AMD64_WGPU_KERNEL_WORK.md` — **the work list**: what was
   built (`/dev/fb0`, slices S1–S6), every open gap with evidence, and the
   priority order. This prompt is a pointer into it.
3. `docs/archive/AKUMA_AMD64_RIO_FBDEV_BUILD.md` — how rio is built and run,
   the debug tooling, the session findings, and **"Kernel spec: Linux ptys"**
   (the call-by-call contract for task 1).
4. `docs/archive/AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md` — task 2.
5. `docs/runbooks/amd64-bare-metal-loop.md` — how to change kernel code and see
   it on the trashcan. Its first rule applies here: **grep `docs/archive/`
   before forming a theory**.

## The tasks, in priority order

1. **Linux ptys: `/dev/ptmx` + `/dev/pts/N`.** Today `openpty`/`forkpty`
   fail, so rio falls back to a userspace "pipe pty" (rio fork,
   `teletypewriter/src/unix/pipe_pty.rs`): the shell runs on pipes behind a
   relay thread that fakes a cooked line discipline. Consequences the user
   hits every day: `stty size` inside rio prints `stty: standard input: not a
   tty`, `ssh late.sh` from inside rio misbehaves (no remote pty, no raw
   mode, double echo), vi/top/less do not work. The line discipline already
   exists (`crates/akuma-terminal::TerminalState`); what is missing is the
   device nodes, master/slave fd types (beside `/dev/tty` and `/dev/dsp` in
   `crates/akuma-syscalls-glue/src/fs.rs` `openat`), `TIOCGPTN`,
   `TIOCSPTLCK`, `TIOCSCTTY`, `TIOCSWINSZ` + SIGWINCH, `O_NONBLOCK` that is
   honoured, and level/edge epoll readiness on the master (EPOLLHUP + EIO
   after the slave side closes; re-arm edges on drain — the
   `TOKIO_PIPE_EPOLL_HANG.md` lesson). Follow the spec table in the rio build
   doc.
2. **ext2 data integrity.** Concurrent `O_APPEND` writers lose writes —
   reproducible from a shell (two `>>` loops of 150 lines → 298 lines); make
   "find EOF, allocate, write, update size" one critical section. Then the
   cross-file corruption seen once (a file's blocks handed to another after a
   truncate); hypotheses and a stress-test plan are in the corruption doc.
3. **Alt on the USB keyboard.** `crates/akuma-usb/src/hid.rs` `emit_key`
   ignores Alt. When `MOD_LALT|MOD_RALT` is held and the key emits a byte,
   emit `0x1b` first (Linux "meta sends escape"). rio's bindings are declared
   as Alt+letter and its window platform already decodes ESC+letter as Alt;
   users currently press Esc, then the letter. Check `amd64/src/kbd.rs`
   (PS/2) for the same gap.
4. **Process lifecycle**: `SOCK_CLOEXEC` not honoured on socketpairs;
   SIGCHLD not reliably delivered; a shell that hung in its own exit
   (`State: R` forever after `close(0)`); orphans never reparented/reaped;
   `/proc` readdir wedging while such a process exists.
5. **Console**: the console read ignores `VMIN=0`/`VTIME` (rio uses
   `O_NONBLOCK` + sliced poll); the console shell could re-sane the shared
   terminal state at each prompt (a SIGKILLed raw-mode program leaves the
   console without NL→CRLF). Also check a suspicion from 2026-10-05: kernel
   log lines appeared on the panel while rio owned `/dev/fb0` — possibly a
   *failed* second `open("/dev/fb0")` (EBUSY) running the S6 console
   handback; verify that only the owner's close hands the screen back.

Do task 1 first. If it is too big for one session, land the device + ioctls
+ blocking I/O first, then readiness, and say exactly where you stopped.

## How to verify on the box (do not claim anything you did not run)

* Host tests and clippy per `CLAUDE.md` for every crate you touch; add host
  tests for new pure logic (ioctl decisions, the line-discipline wiring, the
  ext2 append path).
* The box: `ssh akuma` (Alpine userland; busybox; no python3/rg). rio's
  binary is `/tmp/rio-bin`; its config `/root/.config/rio/config.toml`; its
  trace `/tmp/akuma-fb.log` (rotate it with `mv`, **never truncate** — see the
  corruption doc). `/tmp/rio` is a rio *source checkout*, not the binary.
* **Ptys, without rio first**: a small C program built with
  `x86_64-linux-musl-gcc` on the dev host: `openpty`, fork `sh -i` on the
  slave, write `echo hi\n` to the master, poll + read until `hi` returns;
  then `isatty`, `TIOCGWINSZ`, resize → SIGWINCH, child exit → EIO/EPOLLHUP
  on the master.
* **Ptys, with rio**: when `openpty` works, rio stops taking the fallback.
  Proof: no `[ctx] create_context failed` line in `/tmp/akuma-fb.log`, no
  `pipe-pty relay` thread in `ps`, `stty size` inside rio prints the pane's
  rows and columns, `vi` opens full screen, `ssh late.sh` behaves. rio can be
  driven without anyone at the panel via `/tmp/rio-expect.sh` on the dev host
  (expect over `ssh -tt`; use `\033` for ESC in Tcl, not `\x1b` before a hex
  letter).
* **ext2**: the two-appender reproduction above must give 300/300, plus the
  new stress test.
* **Alt**: with rio running from the console, hold Alt+S — the two panes swap
  (`[fb] tty read 2B: "\u{1b}s"` in the trace).
* Never `kill -9` a rio someone launched from the console: it leaves the
  shared console terminal state raw. If you must, tell the user to run
  `stty sane`.

## Report at the end

What changed (files, with the reasoning for every new allocation), the
commands you ran on the box with their real output, which rio workarounds are
now unnecessary (and which still are), and what you could not verify. Update
`AKUMA_AMD64_WGPU_KERNEL_WORK.md` (mark items done, add findings) and the
relevant `docs/archive/` doc. Leave everything uncommitted for the user.
