# Building rio for the amd64 fbdev stack (`netoneko/rio`, the Akuma fork)

How the rio terminal is cross-built for `x86_64-unknown-linux-musl` and run on
the trashcan panel. One-time work lives in the forks; the repeatable step is
`userspace/rio/build.sh` in this repo. Written 2026-10-04 after the first
successful build.

## Repos and layout

| what | where |
|---|---|
| the terminal, Akuma fork | `netoneko/rio` (fork of `raphamorim/rio`) — clone next to the other repos (e.g. `~/github.com/netoneko/rio`) |
| the wgpu custom backend it renders through | `netoneko/akuma-cli-wgpu`, library target (`src/lib.rs` exposes `wgpu_backend`) |
| the backend's future standalone home | `netoneko/akuma-wgpu-backend` (empty for now; the extraction is pending — until then sugarloaf path-depends on the demo crate) |
| the demo / test bed for the backend | `netoneko/akuma-cli-wgpu` (`deploy.sh`, `gpu-selftest`, `gpu-bench`) |

The fork's patches (all in-tree, all musl-gated so a normal Linux glibc build
is unaffected):

* **sugarloaf**: the native ash/Vulkan backend is compiled out on musl
  (`cfg(not(target_env = "musl"))` across the ~57 `target_os = "linux"` sites;
  `build.rs` skips the glslc/glslangValidator requirement). The default
  backend on musl is `SugarloafBackend::Wgpu`, and `context/webgpu.rs` creates
  the instance with `wgpu::Instance::from_custom(akuma_cli_wgpu::wgpu_backend::backend::Instance)`
  — the seam the plan wanted. The wgpu surface is `/dev/fb0`; the backend
  falls back to a RAM sink when there is no panel.
* **sugarloaf fonts**: per-codepoint fontconfig discovery (`font/linux.rs`)
  and the `yeslogic-fontconfig-sys` dep are musl-gated out — no fontconfig on
  the box. Unmatched codepoints render as tofu. `font-kit` is vendored
  (`extra/font-kit`, `[patch.crates-io]`): on musl its `SystemSource` is the
  plain filesystem walk (the code path Android uses), so fonts come from
  whatever is in `/usr/share/fonts` — Alpine packages work (`apk add` a
  font and rio finds it by family name). `fonts.additional_dirs` in rio's
  config also works (fontdb `load_fonts_dir`).
* **rio-window**: new `fb` platform (`src/platform_impl/fb/`, feature
  `fb`, modelled on the Redox `orbital` platform). Screen = `/dev/fb0`,
  opened once at `EventLoop::new`; because the device is single-open the fd
  is published to the wgpu backend through `AKUMA_FB_FD`. Input = the
  console tty on stdin in raw mode (escape sequences, control bytes, UTF-8).
  One window = the whole screen at scale 1. `build.rs` also now generates
  the macOS dispatch bindings by *target* OS, not host, so cross builds
  from a Mac work.
* **rioterm**: `Backend::Vulkan` on musl maps to the wgpu custom backend.

## Toolchain (build host = aarch64 macOS dev machine)

* rustup toolchain `1.96.1` (rio's `rust-toolchain.toml` pin) with
  `x86_64-unknown-linux-musl` installed:
  `rustup target add x86_64-unknown-linux-musl --toolchain 1.96.1`
* `x86_64-linux-musl-gcc` (musl-cross, same as `akuma-cli-wgpu/deploy.sh`
  needs) — used as linker for the final link and as `CC` for the C
  dependencies freetype-sys and onig_sys build from source.
* C deps cross-built via the cc crate need:
  `CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc`
  `AR_x86_64_unknown_linux_musl=x86_64-linux-musl-ar`

## The build

```sh
cd rio
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc
export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
export AR_x86_64_unknown_linux_musl=x86_64-linux-musl-ar
cargo +1.96.1 build --release -p rioterm \
    --no-default-features --features wgpu,fb \
    --target x86_64-unknown-linux-musl
# -> target/x86_64-unknown-linux-musl/release/rio  (~29 MB, static musl)
```

Feature meanings: `wgpu` = sugarloaf's wgpu context (our custom backend);
`fb` = rio-window's framebuffer platform. Default features (`wayland`, `x11`)
are pointless on Akuma and must be off — with them on, rio-window's linux
platform pulls X11/wayland client libraries that do not exist for musl here.

`userspace/rio/build.sh` in this repo wraps all of it and pushes the binary
to the box over HTTP (the box has `wget`, no scp; same transport as
`akuma-cli-wgpu/deploy.sh`).

## On the box

```sh
. /etc/akuma-dev.env
/tmp/rio
```

The panel switches to rio full screen. The console tty goes raw while rio
runs; on exit the line discipline is restored. Fonts: `apk add` an Alpine
font package (e.g. a ttf monospace) before first run, or point rio's config
`fonts.additional_dirs` at a directory of ttf files. rio reads its config
from `$HOME/.config/rio/config.toml`.

## Input and the console (2026-10-04, second session)

rio renders and takes input now; what was fixed to get there, and what is
still open. All the debug tooling below lives in the rio fork's
`rio-window/src/platform_impl/fb/event_loop.rs`.

### Debug tooling

* `/tmp/akuma-fb.log` — the fb platform appends its whole lifecycle trace
  here unconditionally (loop iterations, every raw tty read, every decoded
  key, and per-event `handler <name> enter/exit` lines around the client's
  event handler). This file exists because the console shell cannot be
  relied on for `VAR=x cmd 2>file` redirection.
* `AKUMA_FB_DEBUG_INPUT=1` additionally mirrors the trace to stderr.
* `/tmp/rio-expect.sh` (on the dev host) drives a full rio session over
  `ssh -tt` with scripted keystrokes — the input path is testable without
  anyone at the panel.
* The console shell (the kernel's own) appears not to parse `VAR=x cmd`
  prefixes or `2>file` redirects — commands that depend on those silently
  mislaunch. Pass configuration through files, not the command line.

### Bugs found and fixed (all in the forks, all with commits)

1. **Redraw livelock (the big one).** The fb event loop drained its redraw
   queue with `while let Some = queue.pop_front()`. rio repaints
   continuously and requests the next redraw from inside the redraw
   handler, so the drain never ended: after the first frame (which is why
   rio "looked cool"), AboutToWait, the tty reads and everything else were
   starved — the window rendered and ignored every key. Fix: drain a
   `mem::take` snapshot. Any future winit platform must do the same.
2. **Console read blocks forever with `VMIN=0`.** The kernel console read
   does not honour the zero-timeout contract (nor, before the fix was
   tested, did we trust O_NONBLOCK): the first `read(0)` never returned and
   the loop froze inside the kernel. Fix: tty fd carries O_NONBLOCK
   (restored on drop) and poll is never called with timeout -1 (33 ms
   slice). O_NONBLOCK *is* honoured by the console channel — verified: the
   log shows clean EAGAINs.
3. **Incomplete escape sequences** (`ESC [` split across reads) decoded as
   Alt+`[`. The decoder now waits for the CSI/SS3 final byte; unit tests
   cover the decoder (`cargo test -p rio-window --features fb`, run the
   test binary on the box — macOS cannot execute musl binaries).
4. **Clipboard panic** without X11 (rio-vt): nop clipboard under musl.
5. **No shell**: the box has no `/etc/passwd` and no `$SHELL`, so rio's
   pty had no shell. Fixed via rio's config:
   `shell = { program = "/bin/sh" }` in `~/.config/rio/config.toml`
   (which also sets `[fonts] family = "Source Code Pro"`, size 18, from
   `apk add font-adobe-source-code-pro`).

### Kernel-side findings (for the kernel repo)

Consolidated, with priorities: `AKUMA_AMD64_WGPU_KERNEL_WORK.md`. The ext2
corruption and the lost `O_APPEND` writes: `AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md`.

* **A process blocked in a console read can wedge `/proc` readdir
  system-wide** (observed twice: `ls /proc` over ssh hung until the wedged
  rio died). Suspicion, not proven — but it happened twice, and both times
  a frozen console-reader was alive.
* Console read ignores `VMIN=0` semantics (fixed on our side with
  O_NONBLOCK, which the console does honour, and a sliced poll).
* **No ptys at all** (no `/dev/ptmx`, no `/dev/pts`) — the cause of rio's
  dead input; spec below.
* **One terminal state per console session**: interactive children
  share the console shell's termios (`spawn_inherits_terminal`,
  `crates/akuma-exec/src/process/spawn.rs`). A raw-mode program killed
  with SIGKILL leaves the console raw — output loses NL→CRLF ("line
  breaks are gone") for every later program; `stty sane` repairs it.
  (Linux behaves the same; Linux shells re-sane the tty at each prompt.)
  rio's fb platform now heals a raw state it finds at startup (fork
  349d585).
* **A shell can hang in its own exit** (once in four runs, 2026-10-05):
  `sh -i` on pipes (rio's fallback), stdin at EOF after ^D, printed its
  exit newline; `/proc/<pid>/syscalls` ends with `close(0)` (generic
  numbering, 57); then `State: R` forever, never a zombie, parent never
  gets SIGCHLD. Not reproduced deliberately yet.
* **`SOCK_CLOEXEC` is not honoured** (socketpair fds leak into exec'd
  children; `pipe2(O_CLOEXEC)` looked fine). rio's pipe pty now closes fds
  3..1024 in the child.
* **ext2: a file's data replaced by another file's** after truncating
  `/tmp/akuma-fb.log` (`: >`) and appending again: the rewritten
  `/tmp/crt-home/.config/rio/config.toml` held trace text, size 2243 B vs
  1391 lines. Looks like freed blocks reallocated while still owned.
  Workaround: rotate with `mv`, never truncate. Investigate in the kernel.
* **Alt is dropped by the USB keyboard driver** — `crates/akuma-usb/src/hid.rs`
  `emit_key` consults only Shift/Ctrl. Suggested fix: when `MOD_LALT|MOD_RALT`
  is held and the key emits a byte, emit `0x1b` first (Linux "meta sends
  escape"); rio's fb platform already decodes ESC+key as Alt+key.
* **Orphans are never reaped**: a child whose parent was killed stays a
  zombie (`State: Z`, `PPid` = the dead parent) — nothing reparents it to
  init. Killed rio instances and their shells pile up in `ps` this way.

### Resolved 2026-10-05: keys reached rio, the shell never echoed

Symptom was: rio renders and takes input, typed text never appears, pink
cursor frozen. **Cause: the kernel has no ptys.** There is no `/dev/ptmx`
and no `/dev/pts` (`ls /dev` = `fb0 null random tty urandom zero`; even an
`ssh -tt` session reports "not a tty"). rio's `forkpty` (default path) or
`openpty` (`-e <program>` path, which sets `use_fork = false`) failed, and
rioterm silently substituted a **dead context** — it renders and accepts
keys with no shell behind it. The error went only to `tracing`, which never
reaches disk on the box. epoll, the reactor and the input path were fine.

How it was found: a trace line in rioterm's `create_context` error arm
(`[ctx] create_context failed, dead context: … forkpty failed using
/bin/sh`), after `[pty]` trace points in the rio-vt reactor showed the
reactor thread never started. The earlier "start from rio's own log" lead
was a dead end: `/tmp/rio.log` is only the expect harness's stderr redirect
of the fb trace; `--enable-log-file` writes `~/.config/rio/log/rio.log`,
which has never existed on the box.

Fixes (rio fork, `main`: 4c747d1, a65ce4a, dd71a2a):

1. **Pipe pty fallback** — `teletypewriter/src/unix/pipe_pty.rs`, musl
   only, taken only when `forkpty`/`openpty` fail (both creation paths).
   The shell runs as `sh -i` (when no args are given) on pipes, in its own
   session (`setsid`); rio's end of the "pty" is one end of an AF_UNIX
   socketpair, so the reactor/epoll code is untouched; a relay thread plays
   a cooked line discipline: echo, UTF-8-aware erase, ^U, ^W, ^C → SIGINT to
   the shell's process group, ^D → EOF on an empty line, ICRNL in, ONLCR
   out, escape sequences swallowed. Host unit tests:
   `cargo test -p teletypewriter --lib pipe_pty`. Limits: no tty
   (`isatty` false in the shell), no job control, no line editing or
   history, full-screen programs (vi, top, less) do not work.
2. **fb platform sends `ModifiersChanged` before `KeyboardInput`** (winit's
   order). rio's `ctrl_seq` reads the modifier state while handling the key;
   the old order gave every key the previous key's modifiers, so Ctrl+D was
   sent as nothing.
3. Trace: `[pty]` lines (each poll wakeup with tokens/readiness, read
   bytes, writes, queued input, reactor exit reason) and `[ctx]` lines go
   to `/tmp/akuma-fb.log` with the fb input trace, one `write` per line.

Verified: over the ssh expect harness with and without `-e /bin/sh`
(prompt `/tmp # `, `ls` echoed and its listing rendered, ^D → shell exits
with status 0 → rio exits), and by a person at the panel typing on the
console keyboard (`/tmp/rio-bin -e /bin/sh`).

### Resolved 2026-10-05 (evening): keys on the real pty

With real ptys rio takes `forkpty` and the pipe fallback is not used. Running
late.sh through `ssh` inside rio then showed Enter sending Ctrl+J, Esc and
Backspace doing nothing, and `^[[A` echoed over the screen. Causes and fixes
(details in `AKUMA_AMD64_PTY.md` §7): the fb platform held a lone ESC (now
flushed as Escape after 400 ms, `ESC_FLUSH_MS`; Esc-then-letter bindings must
land inside that window), sent Enter as LF and gave Escape no text (rio writes a
key's text to the pty as is); and the kernel's `SET_TERMINAL_ATTRIBUTES`
ignored the fd, so `ssh` never made the pty slave raw. Debugging aid kept: the
`[pty] input NB queued [bytes]` trace line in `/tmp/akuma-fb.log` shows exactly
what rio wrote. Also: panel font is 32, cursor blink on (`[cursor]` in the
shipped config; rio defaults to no blink), and `HOME` is empty in ssh sessions —
`export HOME=/root` (or source `/etc/akuma-dev.env`) before `/bin/rio` or it
reads no config. rio deploys to `/bin/rio`.

### Images and a full TUI (2026-10-05)

With ptys and raw mode fixed, late.sh over `ssh` inside rio on the panel shows images through
the **kitty graphics protocol** (inline thumbnail plus a full-size preview popup), photographed
at the panel. This is the first run of sugarloaf's image path on the wgpu backend, so the
"`image.wgsl` not rendered under test" gap below is closed for that shader (a `gpu-selftest`
for it is still worth adding). Known and undiagnosed: slow redraws, and stale or missing cells
while late.sh scrolls regions (suspects: rio's damage tracking, or the backend's region
memoization).

### Kernel spec: Linux ptys (`/dev/ptmx` + `/dev/pts/N`)

The real fix; with it rio (and any terminal emulator, tmux, script, expect)
works unmodified, including full-screen programs, and the fallback above is
never taken. What musl's `openpty`/`forkpty` and rio need, in call order:

| step | call | kernel behaviour needed |
|---|---|---|
| allocate | `open("/dev/ptmx", O_RDWR\|O_NOCTTY[\|O_CLOEXEC])` | new pair N; returns the master fd (new `FileDescriptor::PtyMaster(N)`, opened in `openat` beside `/dev/tty` and `/dev/dsp` in `crates/akuma-syscalls-glue/src/fs.rs`) |
| unlock | `ioctl(master, TIOCSPTLCK, &0)` (0x40045431) | accept; may be a no-op |
| name | `ioctl(master, TIOCGPTN, &n)` (0x80045430) | write N |
| slave | `open("/dev/pts/N", O_RDWR\|O_NOCTTY)` | `FileDescriptor::PtySlave(N)`; `/dev/pts` listable is nice-to-have |
| setup | `tcsetattr(slave)` (TCSETS), `ioctl(slave, TIOCSWINSZ)` | per-pair `TerminalState` (reuse `crates/akuma-terminal`) and winsize |
| child | `setsid()`; `ioctl(slave, TIOCSCTTY, 0)`; `dup2` slave to 0/1/2 | controlling tty = pair N; `isatty(0)` true; TCGETS/TIOCGWINSZ/TIOCGPGRP/TIOCSPGRP on the slave |
| master I/O | `read`/`write` on master | write → slave input through the line discipline (`process_canon_input`, echo goes back to the master's read queue); slave write → `translate_output` (ONLCR) → master read queue |
| nonblocking | `fcntl(O_NONBLOCK)` on master | **must** return EAGAIN (the console channel ignores it — see findings) |
| readiness | epoll/poll on master, level and edge | EPOLLIN when output queued, EPOLLOUT when input has room; EPOLLHUP + read EIO after the last slave fd closes; re-arm edges on drain (`epoll_on_fd_drained`, the TOKIO_PIPE_EPOLL_HANG lesson) |
| resize | `ioctl(master, TIOCSWINSZ)` | store and send SIGWINCH to the slave's foreground group |
| signals | ^C/^Z/^\\ in ISIG mode | SIGINT/SIGTSTP/SIGQUIT to the foreground process group |
| close | last master close | SIGHUP to the session; slave reads return EOF/EIO |

`crates/akuma-terminal::TerminalState` already implements the line
discipline (ICRNL, canonical editing, echo, ONLCR, raw mode) and the
sshd/`SPAWN_FLAG_PTY` channel wiring in `BOX_PTY_INTERACTIVE_SHELL.md` shows
how a channel becomes a terminal; the missing pieces are the device nodes,
the master/slave fd types, the ioctls, and readiness. Test from userspace
with rio's harness or a 30-line C program (`openpty`, fork `sh -i`, write
`echo hi\n` to the master, poll + read until `hi` comes back).

### Housekeeping notes

* Quit: rio's quit binding is Super+Q, which the console cannot express.
  Ctrl+D (shell EOF) is the working exit — the shell exits and rio with it
  (verified 2026-10-05). Consider a rio binding patch.
* rio is deployed to `/bin/rio` (2026-10-05) by `userspace/rio/build.sh`, next to the
  other userspace binaries (the older `/tmp/rio-bin` is retired; `/tmp/rio` is a source
  checkout). The process name is `rio`: `pidof rio | xargs -r kill -9`.
* The panel accumulates stacked frozen rio instances if launched repeatedly
  while one holds `/dev/fb0` (subsequent launches get EBUSY and exit, but
  wedged ones linger and eventually hang /proc). `pidof rio |
  xargs -r kill -9` from ssh — but expect it to hang if /proc is wedged;
  then only a reboot helps.
* Two pacmans on the panel = the fbcon handback banner drawn twice (two
  rio lifecycles), not a rio bug.

## Verification done so far (2026-10-04)

* `cargo check`/`build` of rio for musl: clean (see the fork's git log for
  the per-step commits).
* The backend itself is verified by `akuma-cli-wgpu`: `exec-selftest`
  (18 snippets x executors + fuzz), `gpu-selftest` (22 tests incl. the
  surface test and sugarloaf's real `grid.wgsl`/`renderer.wgsl`), the demo
  checksums, and `gpu-bench` (full 4K terminal-like redraw ~19 ms mean).
  Those run on the trashcan via `akuma-cli-wgpu/deploy.sh`.

## Known gaps / next steps

* The backend extraction into `netoneko/akuma-wgpu-backend` (sugarloaf's
  path dep `../../akuma-cli-wgpu` becomes a real crate dep).
* rio's surface semantics under real use: `Rgba16Float`/HDR filter targets
  (librashader builds but is untested on the backend), mipmapped filter
  chains, `copy_texture_to_texture` call sites, sugarloaf's
  `text_shader.wgsl` / filter shaders (they compile and JIT; not rendered
  under test yet; `image.wgsl` has now rendered in real use, see above).
* Input is keyboard-only (console tty). Mouse would need evdev; there is
  no pointer on the panel anyway.
* Modifier reporting is per-keystroke recovered from the console encoding
  (upper-case = shift, control byte = ctrl, ESC prefix = alt); there are
  no modifier press/release events.

## Direction: rio as the whole computing experience (2026-10-07)

The intent is for rio on the panel to be the machine's desktop, not only a
terminal — which means a **status bar**: clock, battery and AC, wifi, and a
bell/notification indicator. Rio itself has none (its only chrome is the tab
bar, which shows titles and the cwd; the Akuma fork has no status-bar code
either), so the bar is a patch to the fork's fb platform: reserve one row,
shrink the pty by one row, draw it from a small poller.

The kernel side comes first and is separate work, so the bar only has to read
files: **`/proc/power`** (battery, AC; shipped 2026-10-07 —
`docs/archive/AKUMA_ACPI_POWER.md`), `/dev/wifi0` (exists, `key=value`), and the
clock (`clock_gettime`). The bell needs no kernel support: rio sees `BEL` on its
own pty and sets the flag itself. Order: ACPI/power exposure on QEMU and the
trashcan (done) → verify the battery decode on ryzen → the rio bar.
