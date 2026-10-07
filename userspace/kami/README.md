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
