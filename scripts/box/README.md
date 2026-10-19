# `scripts/box/` — the bare-metal box's own build environment

These five files are what the HP box (`the trashcan`) runs **inside Akuma** to
build, install and reboot into its own kernel. They live here, in git, and are
copied onto the machine — not authored there — so the rig can be rebuilt from a
checkout and cannot silently drift from what this repo documents.

| file | installed at | what it does |
|---|---|---|
| `akuma-dev.env` | `/etc/akuma-dev.env` | `PATH`, `LD_LIBRARY_PATH`, `HOME`, `CARGO_HOME`, `AKUMA_SRC`. Every wrapper sources it, because an sshd session inherits **no** environment |
| `kbuild` | `/bin/kbuild` | the kernel → `/root/ktarget` (`-c` clean, `-j N`, `-p PKG`, `--online`) |
| `ubuild` | `/bin/ubuild` | userspace members for `x86_64-unknown-none` → `/root/utarget` |
| `mbuild` | `/bin/mbuild` | `meow` → `/root/mtarget` |
| `kinstall` | `/bin/kinstall` | runs the repo's `scripts/install_kernel_amd64.sh` |

## Crush (the agent TUI) — added 2026-10-19

| file | installed at | what it does |
|---|---|---|
| `crush`, `crush-glm`, `crush-kimi` | `/bin/` (**not** `/usr/local/bin` — see below) | wrappers: `HOME`/`PATH`/`XDG_RUNTIME_DIR` (a session has none), then `exec /usr/local/bin/crush-bin`. `crush` uses the `crushrc` default (GLM 5.3 Flash, low effort); the other two pin a model |
| `crushrc` | `/root/.config/crush/crushrc` (mode 0600) | providers `kimi` (Kimi Code) and `glm` (z.ai coding plan), tokens read from `/root/.akuma/{kimi,z.ai}/token`, `metrics` off, **no yolo** |
| `10-local-bin.sh` | `/etc/profile.d/10-local-bin.sh` | appends `/usr/local/bin` to `PATH` for interactive shells — only takes effect once sshd passes `ENV` (the deployed sshd predates that) |

The real binary is `crush-bin` (crush v0.98.1, static Go, from the `charmbracelet/crush` release
tarball) at `/usr/local/bin/crush-bin`. **Wrappers live in `/bin` because an ssh session's `PATH` is
`/sbin:/usr/sbin:/bin:/usr/bin`** — `/usr/local/bin/goose-glm` and friends are not found by name
either. Runbook: [`docs/runbooks/crush-on-akuma.md`](../../docs/runbooks/crush-on-akuma.md).

Install or refresh the build rig from inside Akuma:

```sh
cd /src/github.com/netoneko/akuma
cp -f scripts/box/akuma-dev.env /etc/akuma-dev.env
cp -f scripts/box/kbuild scripts/box/ubuild scripts/box/mbuild scripts/box/kinstall /bin/
chmod +x /bin/kbuild /bin/ubuild /bin/mbuild /bin/kinstall
sync
```

Two properties are deliberate:

- **They are not in the checkout's own config.** Build output goes to
  `/root/*target` and the box-local cargo settings live in
  `/root/.cargo/config.toml`, so `/src/github.com/netoneko/akuma` stays
  `git status` clean and the box can commit exactly what it changed.
- **They `cd` to the manifest directory.** Cargo reads `.cargo/config.toml` from
  the working directory, not from `--manifest-path`; building the kernel from
  anywhere else drops `relocation-model`/`code-model` and the link fails on
  `_start`.

The environment they assume — the checkout, `/usr/local/rust`, `/root/.cargo`,
and the two musl `.so` files on lld's search path — is described in
[`../../docs/runbooks/amd64-bare-metal-loop.md`](../../docs/runbooks/amd64-bare-metal-loop.md)
§ "Working **on** the box", with the staging procedure and its traps.
