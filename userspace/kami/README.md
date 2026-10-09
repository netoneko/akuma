# kami (紙, "paper")

A headless Chromium on the Linux framebuffer. Chromium renders in software and
sends a PNG screencast over the DevTools Protocol, and `kami` blits each frame
onto `/dev/fb0`. Keys typed on the tty go back as CDP input events.

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

**Page size.** The page is as big as the screen allows: `--scale` defaults to
auto (`display::auto_scale`), the largest scale (at most 2) that still leaves a
1280x720 page, minus the status bar. The 1920x1200 laptop gets scale 1 and a
1920x1176 page; a 4K panel gets scale 2 and 1920x1068. (It was a fixed 2, which
left the laptop a 960x588 page; example.com's link sat below the fold of that
and link hints found nothing.)

## Keys

Keys are modal, like vim (`src/nav.rs`, host-tested).

- **Normal** (start): `j`/`k` scroll, `d`/`u` half a page, `gg`/`G` top/bottom,
  `H`/`L` back/forward, `r` reload, `f` link hints, `i` insert mode. Other
  letters are swallowed. Arrows, PgUp/PgDn, Home/End, Enter, Tab, Backspace
  and Esc still pass to the page (Esc closes a page's own dialog).
- **Link hints** (`f`): every visible clickable element (links, buttons,
  inputs, `role=button`, `cursor:pointer` leaves; through open shadow roots
  and same-origin iframes) gets a yellow letter label (home row `asdfghjkl`,
  fixed width). Type the label to click it with a real mouse event; Esc
  cancels, Backspace un-types. Clicking a text field enters insert mode.
  Cross-origin iframes get no hints. When nothing is found the page helper
  says why in the input log (`hints: nothing found; {matched, small, off,
  hidden, ...}`).
- **Insert** (`i`): all keys go to the page; Esc blurs and returns to Normal.
- Ctrl-R reloads; **Ctrl-Q or Ctrl-C quits** (any mode, even when Chromium is
  not answering: a separate input thread watches for them).

Verified 2026-10-09 on the ryzen laptop, over an ssh pty so the tty and the
input thread were real: `j`/`k` scroll (a new frame each), `H` goes back,
`f` finds example.com's link, its label clicks it and Chromium starts
navigating, Ctrl-Q exits and the daemon detaches the session. Not yet tried
from the console keyboard itself, and not against Tumblr's cookie notice (it
did not appear in the frames captured; see Known gaps).

## The status bar

`kami` draws the status bar itself, in the bottom 24 pixel rows of the
framebuffer, with a built-in 5x7 bitmap font (`src/bar.rs`; upper case, digits,
punctuation). It is not part of the page, so it shows when the page is blank,
loading, wedged, or has no fonts. It shows, in order: the startup stage with
elapsed seconds (`starting chromium`, `waiting for chromium to answer`,
`opening the tab`, `loading the page`, `waiting for the first frame`), then
the mode, a `[loading]` marker and the top frame's URL. Any Chromium request
unanswered for more than 4 s is appended: `!! no answer to Page.navigate for
12s`.

## Architecture

The session is a state machine with no I/O (`src/machine.rs`). `handle(Event)`
returns `Effect`s:

| Event in | Effect out |
|---|---|
| `Tick(ms)`, `Cdp(message)`, `Key(input)`, `Connected`/`ConnectFailed`, `DaemonGone`, `InputClosed`, `Presented{..}` | `TryConnect`, `Send(cdp message)`, `Present{png}`, `Status(text)`, `Log(line)`, `Done(error?)` |

It numbers its own requests, remembers which are outstanding and since when,
and never waits for a reply. The shell in `main.rs` only polls the daemon
socket and the tty, turns what arrives into events, and performs the effects
(socket write, PNG decode and blit, status draw, log line). Why: the code this
replaced blocked inside every CDP call, and while blocked it read no keys,
drew nothing and could not quit, so any Chromium stall looked like "kami hangs"
(measured 2026-10-09: a `Page.navigate` that never got a reply froze it for
the rest of the run). Everything in the machine (startup phases, retries,
navigate deadline, polling fallback, hints, stall reporting, quitting) is
host-tested with scripted events.

Other pure pieces: `nav.rs` (modes, keys to actions), `display.rs` (the
`Display` trait: `page_size`, `blit`, `status`; and `auto_scale`), `bar.rs`
(font and rendering). `fb.rs` is the framebuffer `Display`. **A rio pane is a
second `Display`**: frames out as an inline-image protocol, the status as an
ordinary text line, sized from `TIOCGWINSZ`; the machine does not change (see
"Future: kami in a rio split pane").

## Logs and telemetry

`/tmp/kami-input.log` (override with `KAMI_INPUT_LOG`; appended, one block per
session, seconds since launch in front of each line): the tty's termios before
and after raw mode, every raw chunk read in hex, each decoded input with the
mode and the actions it caused, the startup phases with their durations
(`startup: Target.getTargets: 3198 ms` ...), page lifecycle events, the top
frame's URL, every `captureScreenshot` call, every frame shown with decode and
blit times, the first-pixels marker, and the hint replies. When keys "do
nothing", the hex line says whether they reached kami at all.
Other knobs: `KAMI_DUMP=<path>` keeps the latest PNG shown. If the first
screencast frame is fully transparent (as Akuma's was before the GPU flags
below), kami switches to polling `Page.captureScreenshot` every 500 ms and
drops the screencast's frames (`--poll MS` forces it); a poll is ~0.2 s, so
that is a few fps, not 45.
`/tmp/kami.log` has Chromium's stderr plus the daemon's timestamped lines
(`+7540 ms: first message from chromium`). The timestamps come from per-core
clocks that differ by up to about a second on Akuma, so lines can look out of
order; the machine only ever uses the clock through a `max`.

## Tumblr: why scrolling stopped (2026-10-09, ryzen)

Telemetry from one `kami https://tumblr.com` session (`/tmp/kami-input.log`,
`/tmp/kami.log`):

- Cold Chromium: `Target.getTargets` answered 3.7-8.5 s after launch, every
  time (`first message from chromium` at +7.5 s). Navigation to `www.tumblr.com`
  committed after 4.6 s; first pixels at 25 s; `Page.loadEventFired` at 72 s.
- **The last frame was presented at 118 s.** For the next ~600 s `j`/`k`,
  ArrowDown, Tab and Enter reached kami (`tty chunk` -> `input` lines) and
  produced no frame at all. `ps` showed no `CrRendererMain`: the tab's renderer
  was gone and its CPU time was not moving.
- Cause, from `/tmp/kami.log` at +101 s: ~40 renderers had been forked (site
  isolation: one per cross-site iframe; pids 138 -> 400), then
  `[zygote_linux.cc:426] FATAL Check failed: Too many open files in system (23)`,
  `NOTREACHED hit. Did not receive ping from zygote child`, `Failed to send
  GetTerminationStatus message to zygote`. `ENFILE` is `amd64/src/pipe.rs`'s
  machine-wide `MAX_PIPES` (256; a `socketpair` is two pipes). Raised to 2048.
  **Unverified live** until the kernel is rebuilt and tumblr reloaded.
- kami does not react to a dead renderer (`Inspector.targetCrashed` /
  `Target.targetCrashed` are not handled): the status bar says nothing and the
  user sees a frozen page. Open.
- Not the cause: the 990 ms `decode`/`blit` values in the log are the per-core
  clock skew described above (a thread migrating between cores), not stalls.
- **Fix 2 (kami only, verified live 2026-10-09, kernel still at the 256 cap):**
  `--disable-site-isolation-trials --renderer-process-limit=4` plus
  `IsolateOrigins,site-per-process` in `--disable-features`. Tumblr: 4 renderers
  instead of ~40, first pixels 15.6 s instead of 25.5 s, `loadEventFired` 31.8 s
  instead of 72.8 s, no zygote FATAL, frames still arriving at 90 s (n=1).
  `probe/page_try.py` printed `DIED` for it: its test is a substring match and
  the run had `session done: None`; ignore that label here.
- Download speed, measured separately: 65 KB/s from a Mac on the LAN and 66 KB/s
  from the internet, so the link/driver is the ceiling (`/dev/wifi0` counters:
  `tx 3640, stack dropped 1556`, `retry-limit 54`). TLS/DNS setup is fine
  (`tls=0.3 s`). That is why tumblr's `loadEvent` takes 72 s; it is not what
  killed scrolling.

## Chromium flags, and what crashes it (2026-10-09, ryzen)

`daemon.rs::chromium_args` is the list.

- **No GPU process at all**: `--disable-gpu --disable-gpu-compositing
  --disable-software-rasterizer`. `--disable-gpu` alone still starts a GPU
  process for SwiftShader/ANGLE, which fails on every navigation here
  (`eglInitialize SwANGLE failed ... VK_KHR_surface not supported`, then
  `Exiting GPU process due to errors during initialization`) and respawns; after
  several the browser process aborted with SIGTRAP ~0.1 s after a navigation
  committed. With the three flags there were no GPU errors and the screencast
  frames are real (the first frame is no longer fully transparent, so the
  polling fallback below is not needed). Chromium's own headless guidance is
  `--disable-gpu --disable-software-rasterizer`, and the SwiftShader fallback
  is being phased out (opt-in from about Chrome 139). The exact "GPU process
  isn't usable" abort text was not confirmed in a source; the link between the
  respawning GPU process and the SIGTRAP is inference from the logs.
- **Slim set**: `--disable-background-networking --disable-sync
  --disable-extensions --disable-component-update --disable-default-apps
  --no-default-browser-check --disable-client-side-phishing-detection
  --disable-domain-reliability --disable-breakpad --disable-crash-reporter
  --disable-features=Translate,MediaRouter,OptimizationHints,BackForwardCache,
  AcceptCHFrame,InterestFeedContentSuggestions`. Measured with
  `probe/flags_ab.py` (4 cold starts per arm, 25 s hold): GPU flags only 3/4
  survived, slim 4/4, slim + crash flags 4/4; time to first pixels 8.6-12.8 s,
  10.0-13.7 s, 10.3-13.8 s. **Start-up time did not change and the stability
  difference is within noise at n=4**; the set stays for the background traffic
  it drops. The crashpad handlers still start despite the crash flags.
- **Still crashing**: Chromium's browser process still died on some cold starts
  after these flags (SIGSEGV on attach to a leftover instance, SIGSEGV loading
  `https://akuma.sh`, an earlier SIGTRAP with no GPU errors in the log). One in
  four or so; cause not found. When it dies the daemon exits and the next
  `kami` pays a 10-20 s cold start, which is most of "kami is slow to
  appear".
- **dbus**: Chromium tries the system bus on startup and keeps failing
  `NameHasOwner` calls (48 log lines in a session). `DBUS_SESSION_BUS_ADDRESS`
  and `DBUS_SYSTEM_BUS_ADDRESS` are set to `disabled:` for it; that changed the
  message ("Could not parse server address") and halved the count (24), but
  Chromium does not honour it fully. Harmless noise, not yet silenced.
- **Cleanup**: the daemon starts Chromium in its own process group and kills
  the group when the browser exits, so a crash no longer leaves zygotes, GPU
  processes and crashpad handlers behind (found: a GPU process from a crashed
  Chromium still alive hours later). It also detaches every CDP session a
  client attached and did not detach (a `kill -9`, or Ctrl-Q): left attached, a
  dead client's screencast waits for acks nobody sends and the next client's
  `Page.startScreencast` never gets a reply.
- **DNS**: `/etc/resolv.conf` listed the QEMU-only `10.0.2.3` first; every
  lookup paid for it on the wifi LAN (removed 2026-10-09).

## Chromium on Akuma can wedge the kernel (2026-10-09)

Twice in half an hour the ryzen box hung hard (needed bringing back by hand)
right after Chromium trees were killed: once inside a loop of cold starts that
`killall`ed Chromium between runs (`probe/flags_ab.py`), once straight after a
single `kill -9` of the browser process of a running session. The last klog
before the first hang ends in `[BKL] stuck: owner=2 waiter=7 tag=501 ...
spins=33554432`, next to `[TRAMP-MISMATCH] ... stale tid` and `killed by
signal 15`: the Big Kernel Lock held by the IRQ/scheduler bucket (tag 501) for
tens of millions of spins, the class of wedge in
`docs/archive/AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md` and
`BKL_VFS_CARVE_OUT.md`. This is a kernel problem, not kami's; kami's part is
not to provoke it: the daemon now takes the group down with SIGTERM first (not
shown to help), and test loops that kill Chromium should be rare and spaced.
Until it is understood, treat "kill Chromium's tree" as a risky operation on
this kernel, and `kami --kill` too.

**Understood and fixed in the kernel, 2026-10-09** (`docs/archive/AKUMA_AMD64_SIGKILL_NATIVE_PATH.md`):
`kill(2)` on amd64 went through the AArch64 hard-kill path, which marks every
thread of the victim `TERMINATED` from the killer's core after a 2 s grace,
skipping the threads' own teardown; each `kill -9` of a Chromium tree leaked
~100 thread rows, and once the 448-row table was full the kernel sat in a
`[BKL] stuck` storm. Two things on kami's side were never doing what they
looked like: `kill(-pgid, …)` answered `ESRCH` on that kernel (the group
decode did not exist), so `kill_group`'s SIGTERM and SIGKILL reached nothing
and `group_alive` always said "gone"; the kill that actually landed was
`child.kill()` on the browser. Both work now, and the gentle order is fine to
keep.

## Known gaps

- **Japanese (CJK) fonts: unverified.** The Alpine rootfs on the Akuma
  partition has `font-noto` (Latin and other scripts, not CJK),
  `font-noto-emoji`, `-math`, `-symbols`, `font-dejavu`, `font-liberation`,
  `font-opensans` and `font-adobe-source-code-pro`, no CJK package. Yet
  `https://akuma.sh/` drew its 悪魔 heading correctly (frame captured
  2026-10-09), so either that page ships its own web font or something already
  covers those two kanji. Not tested with a page that relies on system fonts
  (a local `file://` page of mixed kana/kanji would settle it). If it fails,
  stage a Japanese-capable package (`font-noto-cjk`, or a smaller one such as
  `font-ipa`; check what `apk` offers) onto the partition.
- **Tumblr renders its skeleton** (header, nav, fonts, loading placeholders)
  but the feed did not fill in within 50 s over the wifi link (measured about
  110 KB/s earlier; the app is several MB of JavaScript). Not tested for longer.
  The cookie notice did not appear in the frames captured, so clicking it is
  untested.
- `https://akuma.sh` renders correctly (3 of 3 loads on 2026-10-09: the neon
  title, the 悪魔 heading, the terminal window with its ASCII art, the footer).
  One earlier load killed Chromium with SIGSEGV; that is the general crash rate
  above, not the page.
- The first blit after launch took 1 s once (page faults on the framebuffer
  mapping); later blits take 1-6 ms.
- Not tried from the console keyboard (only over an ssh pty).

## Probes (`probe/`)

- `keyprobe.py`: a local Chrome driven over `--remote-debugging-pipe` with
  kami's exact messages (Tab+Enter, hints, wheel). Needs Chrome on the
  machine; no network needed with a local page.
- `ssh_keys.py HOST URL --keys "j,f,a,CTRL-Q" [--env KAMI_DUMP=/tmp/x.png]`:
  runs kami on a live Akuma over an ssh pty, sends keys, prints the new
  input log. **It paints on `/dev/fb0` and takes the console from whoever is
  using it.**
- `scroll_try.py HOST URL [--slow N] [--burst N] [--gap S] [--settle S]`:
  runs `kami --fb none` over an ssh pty from a cold Chromium, sends `j` keys
  (one at 1 s spacing, then a burst) and reports the time from each key to the
  next presented frame and the frames per second. 2026-10-09 on a 7200 px local
  page: median 121 ms for an isolated key, ~9 frames/s and 42 ms in a burst.
- `page_try.py HOST URL --runs N`: loads a URL N times with the null display
  (`kami --fb none`: nothing is painted, the console and keyboard are left
  alone), reports survival and time to first pixels, and saves the final frame
  of each run as a PNG. Each run starts a cold Chromium unless `--keep`, which
  is the kill-the-tree pattern warned about above: use few runs.
- `flags_ab.py HOST --runs N`: cold-start A/B of Chromium flag sets (the table
  above). Owns the kami daemon on the box.

## Session management

The daemon (`kami --daemon`, started detached by the first `kami`) owns one
Chromium and supervises it; each `kami` is a client of it on `/tmp/kami.sock`.

- **One daemon, one Chromium.** A daemon first connects to the socket: if
  something answers, it is serving and the new one exits. It then reaps the
  previous generation (`/tmp/kami.sock.pid` holds `daemon-pid chromium-pgid`;
  the old group is killed and the profile's `Singleton*` files removed), so a
  crashed Chromium's zygotes, GPU process and crashpad handlers cannot pile up.
- **Supervised.** When Chromium exits (a crash) or closes its pipe, the daemon
  starts another on the same socket and sends the attached client a
  `Kami.chromiumRestarted` event. More than 5 restarts in a minute is a crash
  loop and the daemon stops. The group is taken down gently (SIGTERM, up to
  3 s, then SIGKILL).
- **Pinned.** The tab a session attaches to is written to
  `/tmp/kami.sock.target`; the next `kami` prefers it if it is still there,
  rather than the first page.
- **Recovery.** On `Kami.chromiumRestarted`, or when Chromium has not answered
  any request for 45 s (the client asks `Kami.restart`; if nothing happens 20 s
  later the session ends saying so), the client starts again from
  `Target.getTargets`, attaches to the new tab and navigates back to the URL it
  was on. If the daemon itself vanishes the client replaces it, up to 3 times.
  The status bar shows `(chromium restarted xN)`.
- **Introspection.** `Kami.hello` asks the daemon for its pid, Chromium's pid,
  generation, restart count and uptime (logged at connect). `Kami.shutdown` is
  what `kami --kill` sends: Chromium's group is stopped and the daemon exits
  (an older daemon does not know it and `kami --kill` falls back to
  `Browser.close`).
- **Detach.** The daemon detaches every CDP session a client attached and did
  not detach (a `kill -9`, Ctrl-Q): a dead client's screencast otherwise waits
  for acks nobody sends and the next client's `Page.startScreencast` never gets
  a reply.

The recovery path is covered by host tests with scripted events; it has not yet
been watched on the box (see "Chromium on Akuma can wedge the kernel" below).

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

*2026-10-09 correction: the empty screencast and the polling described below
were the GPU-process problem (see "Chromium flags"). With
`--disable-gpu-compositing --disable-software-rasterizer` the screencast frames
are real.*

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
