# kami (紙, "paper")

A headless Chromium on the Linux framebuffer. Chromium renders in software and
sends a PNG screencast over the DevTools Protocol, and `kami` blits each frame
onto `/dev/fb0`, scaled up `--scale`x (default 2: 1920×1080 → a 4K panel). Keys
typed on the tty go back as CDP input events.

It is a static x86_64 musl binary with no dependencies beyond `libc` and
`miniz_oxide` (both already in `userspace/Cargo.lock`). The PNG decoder and the
JSON field scanning are hand-written. It talks to Chromium over
`--remote-debugging-pipe` (fds 3/4), not TCP, so it needs pipes, a unix socket
and `/dev/fb0`, and nothing else from the kernel.

```sh
./build.sh                                   # -> target/x86_64-unknown-linux-musl/release/kami
kami https://en.wikipedia.org/wiki/Paper     # first run starts Chromium
kami                                         # later sessions reattach to the same tab
kami --kill                                  # stop Chromium
```

With no URL, a fresh tab opens the home page, `https://www.tumblr.com/` (the
`HOME` constant in `src/main.rs`). Reattaching with no URL keeps whatever the
tab is showing.

Keys are modal, like vim (`src/nav.rs`, host-tested). A status line in the page
(bottom left) shows the mode.

- **Normal** (start): `j`/`k` scroll, `d`/`u` half a page, `gg`/`G` top/bottom,
  `H`/`L` back/forward, `r` reload, `f` link hints, `i` insert mode. Other
  letters are swallowed. Arrows, PgUp/PgDn, Home/End, Enter, Tab, Backspace
  and Esc still pass to the page (Esc closes a page's own dialog).
- **Link hints** (`f`): every visible clickable element (links, buttons,
  inputs, `role=button`, `cursor:pointer` leaves; through open shadow roots
  and same-origin iframes) gets a yellow letter label (home row `asdfghjkl`,
  fixed width). Type the label to click it with a real mouse event; Esc
  cancels, Backspace un-types. Clicking a text field enters insert mode.
  Cross-origin iframes get no hints.
- **Insert** (`i`): all keys go to the page; Esc blurs and returns to Normal.
- Ctrl-R reloads; Ctrl-C or Ctrl-Q detaches (any mode).

Untested against a live page as of 2026-10-08: the CDP mouse/wheel path and
`src/hints.js` have only been syntax-checked; the state machine has tests.

On Akuma the screencast's frames arrive fully transparent (Chromium's video
capture reads an empty buffer; see the archive record), so `kami` notices an
empty first frame and polls `Page.captureScreenshot` every 500 ms instead
(`--poll MS` forces it; a poll is ~0.2 s, so this is a few fps, not 45).
Debug knobs: `KAMI_DUMP=<path>` keeps the first PNG, `KAMI_TRACE=1` names every
CDP event.

## Sessions outlive the terminal

Chromium's CDP pipe dies with the process holding it, so `kami --daemon`
(started detached by the first `kami`, in its own session) owns Chromium and
relays whole NUL-terminated CDP messages to one client over `/tmp/kami.sock`.
Quitting `kami` stops the screencast and detaches; Chromium, the tab and its
state stay up for the next session. A new client replaces an attached one. The
daemon exits when Chromium does. Chromium's and the daemon's stderr go to
`/tmp/kami.log`.

## Measured (2026-10-07, the trashcan under Ubuntu 25.04, Alpine 3.22 container)

Alpine's `chromium` 142.0.7444.59 (musl), `--headless --disable-gpu`. The
panel is `nvidia-drmdrmfb` 3840×2160, 32 bpp, stride 15360.

| | PNG | PNG `optimizeForSpeed` | JPEG q90 |
|---|---|---|---|
| `captureScreenshot` Wikipedia 1080p | 191 ms, 379 KB | 124 ms, 458 KB | 96 ms, 358 KB |
| `captureScreenshot` Wikipedia 4K | 602 ms, 975 KB | 418 ms, 1.18 MB | 305 ms, 915 KB |
| screencast, animated page, 1080p | 45 fps | – | 60 fps |
| screencast, animated page, true 4K (`--force-device-scale-factor=2`) | 11.7 fps | – | 17.7 fps |

The screencast ignores `Emulation.setDeviceMetricsOverride`'s scale factor
and sends frames at window size; only `--force-device-scale-factor` gives
native 4K. In `kami`, a 1920×1080 Wikipedia frame decodes in about 18 ms, and
the 2x blit to the 4K mapping takes about 40 ms.

`probe/` holds the Python harness that produced these numbers: `cdp.py`, a
pipe-CDP client that runs inside the container from `probe/Dockerfile`, and
`analyze.py`, which summarises an `strace -f` log.

## Kernel status on Akuma (2026-10-08)

`userspace/forktest/c_stress/chromeprobe.c` checks each of the three things
below: `SCM_RIGHTS`, cross-process shared files, and the huge `PROT_NONE`
reservations. Run it on Linux first; that run is the control.

On Linux it passes **16/16**, and since 2026-10-08 it passes **16/16** on the
amd64 kernel under Firecracker too. Four fixes got it there:

- **`SCM_RIGHTS`** on `AF_UNIX` (`SOCK_STREAM`, `SOCK_SEQPACKET`,
  `SOCK_DGRAM`). See
  [`docs/reference/subsystems/syscalls/net.md`](../../docs/reference/subsystems/syscalls/net.md)
  § "SCM_RIGHTS".
- **amd64 `sendmsg`/`recvmsg` on a unix fd.** These go to glue now; before,
  they answered `ENOTSOCK`.
- **Write-back by inode.** A shared-writable mapping of an unlinked file now
  keeps a page written by a process that has since exited. See
  [`docs/reference/subsystems/amd64-shared-write-mmap.md`](../../docs/reference/subsystems/amd64-shared-write-mmap.md).
- **`ftruncate`/`fallocate` by inode.** Chromium creates its shared-memory
  file, unlinks it, and only then sizes it. Going by path, that sizing was
  `ENOENT`.

The 1324 GiB reservation works, but costs about 290 ms against Linux's 0.5 ms,
nearly all of it in `munmap`.

Chromium's runs on Akuma (2026-10-08) found and fixed, in order:

- `setsockopt` on a unix fd was `ENOTSOCK` on amd64, and crashpad
  `CHECK`-crashed on it.
- `execve` copied the whole 250 MB binary into the kernel heap. It streams
  now, so a 4 GiB guest is enough.
- `int3` arrived as `SIGSEGV`. It's `SIGTRAP` now, so a `CHECK` reads as one.
- Regular-file reads stopped at 64 KiB.
- `CLOCK_REALTIME` was frozen at 0 with no network. musl's `mkdtemp` then
  retried one name 100 times, which was the ProcessSingleton failure.
- `prctl(PR_SET_NAME)` rewrote `/proc/self/exe`, so Chromium looked for its
  crashpad handler, its V8 snapshot and itself in `""`.
- `/proc/<pid>/task` did not exist, and the zygote's sandbox helper counts
  threads there.
- `mkdir` ignored its mode, and the ProcessSingleton `CHECK`s that its
  socket directory is exactly 0700.
- `SO_PASSCRED` produced no `SCM_CREDENTIALS`, and the browser takes each
  zygote child's pid from them.
- x86_64 `capget`/`capset` had no row, so the `capset` every zygote child
  makes right after `fork` (inside a `CHECK`) was `ENOSYS`, and every child
  died before its ping. Gate `capprobe.c`.
- `mmap`/`mprotect` refused `PROT_WRITE|PROT_EXEC`, which V8 asks for on its
  512 MB code range; every renderer died on it. Gate `jitprobe.c`.
- The kernel held 64 non-main threads **system-wide**; the GPU process's
  sixth thread got `EAGAIN`. Now 448. Gate `thrprobe.c`.
- x86_64 `fallocate` had no row (Chromium's shared-memory sizing, 82 `ENOSYS`
  per run). Gate `fallocprobe.c`.

**Chromium on Akuma renders (2026-10-08).** `chrome-once.sh` under
Firecracker exits 0 in about 28 s of guest time and its screenshot reads
"JavaScript ran: 6 x 7 = 42". The open list (the 256-row process table, `gettid()` of a main thread not
being its pid) is in the record below. `kami` itself on the trashcan's metal
was staged 2026-10-08 (kernel, Chromium and `/bin/kami` on the Akuma
partition); first boot result is in the handoff.

Finding these took a trace of failing syscalls (`strace_err` on the kernel
command line):
[`docs/runbooks/trace-failing-syscalls-amd64.md`](../../docs/runbooks/trace-failing-syscalls-amd64.md).

Full record:
[`docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md`](../../docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md).

## Running Chromium on Akuma (`probe/akuma/`)

The rig that runs Alpine's Chromium on the amd64 kernel under Firecracker, on
the trashcan:

- `push.sh` (laptop) builds the static probes and copies the scripts to
  `/root/cdp-probe/akuma/`.
- `mkimg.sh` builds the 2 GiB ext2 image from `probe/Dockerfile`.
- `run-fc.sh` boots a copy of it with `kami-smoke.sh` as the init script and
  dumps any screenshot into `out/`.

Details are in the scripts' headers and in
[`docs/handoff-kernel-chromium-support.md`](../../docs/handoff-kernel-chromium-support.md),
which is also where the remaining kernel work is laid out.

## Tumblr on the ryzen laptop (2026-10-08)

`kami https://www.tumblr.com/` renders on **ryzen's Akuma over wifi** (entry 12,
1920x1200 `/dev/fb0`, Alpine's `chromium` 142 staged onto p3 with
`apk.static --root`, `/bin/kami`). Confirmed by a captured frame and by a
photo of the panel: Tumblr's layout paints (header, buttons, a dialog card,
the loading placeholder).

**It first rendered with no text, images only** (system-font text missing
everywhere; web-font text such as Tumblr's own nav labels drew). Cause and fix:
Chromium 152's font service hands fonts to the renderer through an unlinked,
`MAP_SHARED` temp file, and four kernel gaps broke that on Akuma
(`docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md` Fixes 21-24: `pwritev2`
`RWF_NOAPPEND`, writes to unlinked files, reopening `/proc/self/fd/N`, and
read-only mappings not seeing shared-writable pages). Diagnosis tools, all in
`probe/akuma/`: `fontcdp.py` (which platform font does Chromium pick?),
`fonttrace.py` (Chromium trace of the font service), and the C probes
`fontmapprobe.c`/`pwv2probe.c`/`shmregionprobe.c` in `forktest/c_stress`. Do
the work on ryzen's Firecracker, not by rebooting the laptop: a Chromium-**152**
image (Alpine `latest`) is the one that matters, because Alpine 3.22's 142
never used this path.

Other findings from the same session:

- `kami` polls `captureScreenshot` because the screencast is empty (see the
  archive record). Once polling, it **must not blit the empty screencast
  frames**: each one painted the screen white over the last screenshot (a
  white blink). Fixed in `src/main.rs`; `KAMI_DUMP` now keeps the latest
  frame, polled screenshots included.
- The first screencast frame is blank by design of the page load; judge a run by
  the latest frame, not the first.
- Chromium logs `pthread_getschedparam failed: 38` (a scheduler syscall
  answers `ENOSYS`), `CreatePlatformSocket() failed: Address family not
  supported` (IPv6) and the shared-memory `Corruption detected` line, all
  non-fatal so far.
- A `chromium --screenshot --virtual-time-budget` run of Tumblr never finishes
  (the page keeps timers alive); use `kami`/CDP for a screenshot of a live page.

## Future: kami in a rio split pane

The goal is to run `kami` inside one of [rio](../rio/build.sh)'s split
panes, with a browser beside a shell on the same screen, instead of
`kami` taking over all of `/dev/fb0`. Today it cannot: rio owns `/dev/fb0`
(the device is single-open), and `kami` assumes it has the whole screen.

Two routes, cheapest first:

1. **An image protocol in the pane's pty.** Rio can draw images that a
   program prints into its terminal (sixel and the iTerm2 inline-image
   protocol; check which ones the Akuma build of rio supports). `kami` would
   get an output mode that skips the framebuffer and writes each screencast
   frame to stdout in one of those protocols, sized from `TIOCGWINSZ`
   (rows and columns, times the cell size in pixels). It would re-request the
   viewport when `SIGWINCH` says the pane was resized. Input already arrives
   on the tty. Nothing in rio changes. The cost is encoding and parsing every
   frame through the pty, so it suits reading more than video.
2. **A shared surface.** Rio composites a pixel buffer that `kami` shares with
   it, for example a `MAP_SHARED` file passed with `SCM_RIGHTS` (both now work
   on Akuma), plus a "frame ready" message. That means no encoding and full
   frame rate, but it needs a small protocol on rio's side, and the
   `sugarloaf` renderer would have to treat a pane as an image layer.

Route 1 is mostly `kami`-side work. Its prerequisites are the kernel's
`TIOCGWINSZ`/`SIGWINCH` and pty support (task 1 of
`docs/handoff-kernel-rio-support.md`).

## Future: reading mode (markdown TUI and `.md` output)

Not every use needs pixels. A reading mode would ask Chromium for the page's
content instead of a screencast, convert it to Markdown, and show it in a
simple text TUI on the tty, with no `/dev/fb0` (so it also works over ssh and
inside any terminal pane). The same conversion runs from the CLI with no
TUI, writing a `.md` file, for example `kami --md https://... > page.md` or
`kami --md -o page.md https://...`.

Sketch:

- **Extraction**: `Runtime.evaluate` of a small script that walks the
  readable part of the DOM (`<article>`/`<main>` first, else `<body>` minus
  `nav`/`header`/`footer`/`aside`/`script`/`style`) and returns a compact
  tree (headings, paragraphs, lists, links, code and `pre`, block quotes,
  tables, image alt text). The DOM is post-JavaScript, so pages that build
  themselves render too.
- **Conversion**: the tree to Markdown happens in `kami` (Rust, no new
  dependencies), so it can be host-tested on saved trees in `testdata/`.
- **TUI**: wrapped to the terminal width (`TIOCGWINSZ`), headings and
  emphasis via ANSI attributes, numbered links followable by typing the
  number, the existing scroll keys. It needs no framebuffer, so it is also
  the cheapest route into a rio pane (below).

## What Chromium asks of the kernel

From `probe/cdp.py strace` on one Wikipedia load plus a screenshot, in normal
multi-process mode (`--single-process --no-zygote` crashes at startup in this
build). This is the list Akuma has to cover:

- **fd passing**: 628 `SCM_RIGHTS` messages, over 41 `AF_UNIX`
  `SOCK_SEQPACKET` socketpairs plus `SOCK_STREAM` ones.
- **Processes**: about 120 threads and processes.
  - Two zygotes, which `fork()` 16 times.
  - Two crashpad handlers.
  - Utility processes started with `execve("/proc/self/exe")`.
  - The Alpine `chromium` wrapper script also runs `readlink`, `id` and `stat`.
- **Shared memory**: no `memfd_create`. With `--disable-dev-shm-usage` it
  opens 256 files `/tmp/.org.chromium.Chromium.*` (`O_CREAT|O_EXCL`), then
  `ftruncate`/`fallocate`s them and makes 621 `MAP_SHARED` mappings
  (528 `PROT_READ|PROT_WRITE`). These must stay coherent across processes.
- **Address-space reservations**, all `PROT_NONE`: one 1324 GiB (`MAP_NORESERVE`,
  the V8 sandbox), 32 GiB, 16 GiB, about 17 × 4 GiB and one 1 GiB. Also 786
  `mremap` calls, of which 31 return `ENOMEM` as Chromium expects.
- **Smaller items**:
  - `prctl` with `PR_SET_VMA` region names (52 calls), `PR_SET_NAME` and
    `PR_SET_NO_NEW_PRIVS`;
  - a `landlock_create_ruleset` probe, and `capset`;
  - netlink `RTM_GETADDR`;
  - `inotify`, `eventfd2`, `epoll_pwait`, `ppoll`;
  - heavy `futex` and `getrandom` use;
  - 286 reads of `/proc/<pid>/task/<tid>/status`, plus `/proc/self/exe`;
  - `/sys/bus/usb` enumeration, and an `open` of `/dev/dri/renderD128` that
    may fail.
