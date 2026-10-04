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

* **A process blocked in a console read can wedge `/proc` readdir
  system-wide** (observed twice: `ls /proc` over ssh hung until the wedged
  rio died). Suspicion, not proven — but it happened twice, and both times
  a frozen console-reader was alive.
* Console read ignores `VMIN=0`/O_NONBLOCK semantics (blocks
  uninterruptibly); the same is suspected for pty master reads — this is
  exactly where rio's still-open issue points (next section).

### Still open: keys reach rio, the shell never echoes (UNRESOLVED)

Symptom: rio renders, takes input (KeyboardInput events confirmed
delivered — 164 of them in one trace; ctrl+enter even dismisses rio's
config-error screen), but typed text never appears and the cursor is
frozen. The remaining suspects, in order:

1. rio's reactor (corcovado, its epoll wrapper) never sees the pty master
   become readable — epoll semantics on pty masters on this kernel are
   unverified. The shell's echo would never render.
2. The pty master write or read blocking (same kernel disease as the
   console read — the kernel ignores non-blocking semantics on the
   console, ptys may be the same).
3. ash spawn failing silently (check for a live ash child of rio-bin).

Next session should start from rio's own log (`--enable-log-file` flag;
a fresh 34 KB `/tmp/rio.log` was written 2026-10-04 20:55 and never
examined) and put trace points on the pty write (`messenger.send_write`)
and the reactor read path.

### Housekeeping notes

* Quit: rio's quit binding is Super+Q, which the console cannot express.
  Ctrl+D (shell EOF) is the working exit. Consider a rio binding patch.
* The panel accumulates stacked frozen rio instances if launched repeatedly
  while one holds `/dev/fb0` (subsequent launches get EBUSY and exit, but
  wedged ones linger and eventually hang /proc). `pidof rio-bin |
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
  chains, `copy_texture_to_texture` call sites, sugarloaf's `image.wgsl` /
  `text_shader.wgsl` / filter shaders (they compile and JIT; not rendered
  under test yet).
* Input is keyboard-only (console tty). Mouse would need evdev; there is
  no pointer on the panel anyway.
* Modifier reporting is per-keystroke recovered from the console encoding
  (upper-case = shift, control byte = ctrl, ESC prefix = alt); there are
  no modifier press/release events.
