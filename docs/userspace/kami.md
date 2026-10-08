# kami

紙 ("paper"): a headless Chromium on the Linux framebuffer. A detached daemon
owns Chromium and its CDP pipe; each `kami` session attaches, starts a PNG
screencast and blits it to `/dev/fb0`, and on exit leaves the browser
running. The default home page is `https://www.tumblr.com/`.

The main doc is [`userspace/kami/README.md`](../../userspace/kami/README.md).
It covers usage, the daemon, measured frame rates, the kernel features
Chromium needs (and the `chromeprobe` gate for them), and the plan to run
`kami` inside a rio split pane.

Since 2026-10-09 the session is a no-I/O state machine (`src/machine.rs`) behind
a `Display` trait, the status bar is drawn by kami on the framebuffer, the
keys are vim-style with link hints (`f`), and every input and startup phase is
logged to `/tmp/kami-input.log`. The README has the Chromium flag findings
(no GPU process, the measured slim flag set), the known gaps (no Japanese
fonts on the Akuma partition) and the probes under `probe/`.
