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
  (upper-case = shift, control byte = ctrl, ESC prefix = alt); there are no
  modifier press/release events.
