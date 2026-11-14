# Helix on the musl box — vi-shaped, rust LSP, four grammars

**Status:** INSTALLED 2026-11-13 — `hx` 25.07.1 built from source at
`/usr/local/bin/hx` (dynamic musl binary, 25 MB), runtime at
`/usr/local/lib/helix/runtime`, rust-analyzer 0.3.3113, config in
`/root/.config/helix/`. `hx --health rust` is fully green except
`lldb-dap` (debug adapter, deliberately absent).

Source of truth for the build decisions below is the inspection of the
helix source itself (clone at `/src/github.com/helix-editor/helix`,
tag `25.07.1`, commit `a05c151b`). Nothing here is from memory of older
helix versions.

## Why minimal

Inspection of 25.07.1 showed three feature knobs that matter on this box:

| Knob | Default | What it costs | Decision |
|---|---|---|---|
| `helix-term` feature `git` | on (`default = ["git"]`) | pulls gitoxide (`helix-vcs`) for gutter diff decorations | **dropped** — `--no-default-features` |
| grammars | 245 languages fetched+compiled | huge fetch+build surface | **4 kept**: `rust`, `toml`, `markdown`, `markdown_inline` |
| `HELIX_DISABLE_AUTO_GRAMMAR_BUILD` | unset → `helix-term/build.rs` fetches **all 245** grammars during `cargo build` | one flaky git fetch breaks the whole build (here: `gotmpl`) | **set to 1**; grammars built manually after install |

What is *kept*: tree-sitter highlighting (via tree-house), full LSP
(goto-definition/references), window splits, the built-in file explorer,
pickers. `unicode-lines` (ropey exotic line endings) was never in the
default set. The `✘ lldb-dap` line in `hx --health` is expected — no
debug adapter is installed, helix works fine without it.

## Install layout

| Path | What |
|---|---|
| `/usr/local/bin/hx` | the binary — **must stay dynamically linked** (see below) |
| `/usr/local/lib/helix/runtime/` | themes + queries + tutor; found via `HELIX_DEFAULT_RUNTIME`, baked in at build time (packager mechanism, `helix-loader/src/lib.rs`) |
| `/root/.config/helix/runtime/grammars/*.so` | compiled grammars; the grammar fetch/build writes here because the config-dir runtime has higher priority than `HELIX_RUNTIME` |
| `/root/.config/helix/config.toml` | editor config (below) |
| `/root/.config/helix/languages.toml` | `use-grammars = { only = [...] }` (below) |
| `/usr/local/bin/rust-analyzer` | prebuilt **glibc** binary, runs via `gcompat` (`apk add gcompat`) |
| `/usr/local/bin/gcc-no-threads` | gcc shim used to link helix (see the trap below) |
| `/src/github.com/helix-editor/helix` | source clone, has the box-local `.cargo/config.toml` |

## The box's linker trap (why three builds failed)

The nightly at `/usr/local/rust` is a **musl-host** toolchain, and
`~/.cargo/config.toml` (box-global, used by the kernel builds) sets
`link-arg=--threads=1` for the musl target — an **lld-only** flag.

1. Default build (rust's `gcc-ld/ld.lld` wrapper): release build-script
   binaries come out **malformed** — dynamic sections with `NEEDED
   libc.musl` but **no `PT_INTERP`** — and every cargo build dies at the
   first build script with `Exec format error (os error 8)`. Debug
   profile and plain bins were fine, which made this miserable to bisect.
2. `RUSTFLAGS="-C target-feature=+crt-static"` fixes the exec problem but
   breaks proc-macros on musl (`cannot produce proc-macro ... does not
   support these crate types`).
3. Fix: link **with gcc** through a shim that drops `--threads=N`
   (`/usr/local/bin/gcc-no-threads`), via the clone-local
   `.cargo/config.toml` (repo-local target config; the clone is throwaway
   tooling, so the box-global config is not touched).
4. Second trap: that first successful build produced a **fully
   static-pie** `hx` — and musl static binaries **cannot `dlopen`**,
   so every grammar showed `Tree-sitter parser: None` despite the
   `.so` loading fine under `ctypes`. Helix loads grammars at runtime,
   so `hx` itself must stay dynamic: rebuild with
   `CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=-crt-static"`.

Full rebuild recipe (the one that produced the installed binary):

```sh
cd /src/github.com/helix-editor/helix
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=-crt-static" \
HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1 \
CARGO_TARGET_DIR=/tmp/helix-target6 \
HELIX_DEFAULT_RUNTIME=/usr/local/lib/helix/runtime \
cargo build --release --no-default-features -p helix-term
cp /tmp/helix-target6/release/hx /usr/local/bin/hx && chmod 755 /usr/local/bin/hx
```

After installing (or re-installing) `hx`, (re)build the grammars — they
survive hx reinstalls, so this is only needed once or when bumping helix:

```sh
export HELIX_RUNTIME=/usr/local/lib/helix/runtime
hx --grammar fetch        # fetches only the 4 selected grammars
hx --grammar build        # needs a C/C++ compiler: apk add g++ (cc crate probes `c++`)
```

`apk add g++` is also needed once on this box (grammar builds probe for
`c++`, which Alpine's `gcc` package does not provide).

## Config

`/root/.config/helix/config.toml` — vi-shaped basics + rust LSP extras:

```toml
theme = "tokyonight_storm"

[editor]
line-number = "relative"      # relative numbers, absolute on the focused line
cursorline = true
true-color = true
color-modes = true
bufferline = "multiple"       # tab bar once >1 buffer is open
rulers = [100]
idle-timeout = 80             # completion popups appear faster (default 400ms)
completion-trigger-len = 2
auto-completion = true
auto-format = true            # rustfmt on save (rust language config)
auto-save = false
end-of-line-diagnostics = "hint"

[editor.lsp]
display-messages = true       # show rust-analyzer progress in the statusline
display-inlay-hints = true

[editor.inline-diagnostics]
cursor-line = "warning"       # diagnostics inline on the current line, signs elsewhere

[editor.cursor-shape]
insert = "bar"
normal = "block"              # vi-style block cursor in normal mode
select = "underline"

[editor.statusline]
left = ["mode", "spinner", "file-name", "read-only-indicator", "file-modification-indicator"]
right = ["diagnostics", "selections", "register", "position", "file-encoding", "file-line-ending", "file-type"]
mode.normal = "N"
mode.insert = "I"
mode.select = "V"

[editor.indent-guides]
render = true

[editor.whitespace.render]
tab = "all"
space = "none"
newline = "none"

[keys.normal]
C-s = ":write"
esc = ["collapse_selection", "keep_primary_selection"]
```

`/root/.config/helix/languages.toml` — the whole grammar trim:

```toml
use-grammars = { only = ["rust", "toml", "markdown", "markdown_inline"] }
```

All other language metadata (LSP definitions like
`[language-server.rust-analyzer]`, per-language settings) is merged in
from helix's built-in config, so nothing else is needed here. rust
`auto-format = true` comes from the built-in rust entry; `rustfmt` is on
this box (`/usr/local/rust/bin/rustfmt`, nightly 1.11).

Every key above was checked against the bundled book
(`/src/github.com/helix-editor/helix/book/src/editor.md`) so the config
cannot emit startup warnings.

## Key map worth knowing (all defaults unless marked)

| Keys | Action |
|---|---|
| `space e` / `space E` | **file explorer** (tree picker) at workspace root / current buffer's dir — `Esc` hides it again |
| `space f` | fuzzy file picker |
| `gd` / `gD` / `gr` | goto definition / type definition / references (rust-analyzer) |
| `Ctrl-w s` / `Ctrl-w v` | split window below / right; `Ctrl-w h/j/k/l` to move; `Ctrl-w q` closes |
| `C-s` | `:write` (custom) |
| `esc` | collapse multi-cursors back to one, then normal mode (custom) |
| `dd` / `yy` / `p` | (helix normal mode, vim-like) |

Note the explorer is a picker overlay, not a persistent sidebar — helix
25.07 has no sidebar; open/close is the idiom. Persistent visibility
comes from splits instead.

## Verify

```sh
hx --version            # helix 25.07.1 (a05c151b)
hx --health rust
#   ✓ rust-analyzer: /usr/local/bin/rust-analyzer
#   Tree-sitter parser: ✓        <- must NOT be None (None = dlopen failed,
#                                   usually means hx was built static)
#   Highlight/Textobject/Indent queries: ✓
```

Quick functional check: open any crate in this repo (`hx crates/akuma-mmu/src/lib.rs`),
wait for `rust-analyzer` to index (spinner in the statusline), put the
cursor on a type and press `gd`; jump back with `Ctrl-o`. `space e` /
`Esc` exercises the tree.

## Related

- [`GOOSE_GLM_LOCAL_OPTIMIZATIONS.md`](GOOSE_GLM_LOCAL_OPTIMIZATIONS.md) —
  same box, same keep-it-minimal reflex, for the editor's AI counterpart.
- [`../../userspace/tcc/docs/`](../../userspace/tcc/docs/) — the other
  compiler story on akuma images (tcc cannot link dynamically, see
  `TRIM_FAT`/devbox backlog notes).
- Upstream docs: `book/src/editor.md` in the clone is the authoritative
  config reference for the exact installed version.
