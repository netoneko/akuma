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

Keys: arrows, PgUp/PgDn and Home/End scroll; Enter, Backspace, Tab and Esc are
passed through; other text is typed; Ctrl-R reloads; Ctrl-C or Ctrl-Q detaches.

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

Two more fixes came from Chromium's first run on Akuma (2026-10-08):
`setsockopt` on a unix fd was `ENOTSOCK` on amd64, and crashpad
`CHECK`-crashed on it; and `execve("/proc/self/exe")` recorded that literal
path as the new image's name.

**Chromium on Akuma does not render a page yet.** It starts under Firecracker
and gets well into startup. The blockers are, in order:

1. `execve` copies the whole 250 MB binary into the kernel heap on every
   re-exec. It needs a streaming loader; a ≥ 8 GiB guest gets a 1 GiB heap
   and gets past this.
2. crashpad's `posix_spawn` fails `ENOENT`.
3. The ProcessSingleton `mkdtemp` fails.
4. Zygote children cannot load the V8 snapshot.

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
