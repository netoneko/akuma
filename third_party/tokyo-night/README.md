# Tokyo Night colours

The shell theme in `overlays/ryzen/rootfs/etc/profile` and
`overlays/devbox/rootfs/etc/profile` uses the **Tokyo Night** colour palette
("night" variant). The colours are not Akuma's. This directory is where the
licences that come with them live.

| | |
|---|---|
| Taken from | [tokyonight.nvim](https://github.com/folke/tokyonight.nvim) by **Folke Lemaitre** (`folke`) — `lua/tokyonight/colors/night.lua`, which overrides `storm.lua` |
| Checked against | `folke/tokyonight.nvim` commit `cdc07ac78467a233fd62c493de29a17e0cf2b2b6` (2026-03-24); `night.lua` last changed in `9578c6fe68af1db513914a2a01bea5585f3027db` |
| Licence | **Apache License 2.0** — [`LICENSE-tokyonight.nvim`](LICENSE-tokyonight.nvim), a byte-for-byte copy of the upstream `LICENSE`. (Upstream leaves the appendix's `Copyright [yyyy] [name of copyright owner]` placeholder unfilled and ships no `NOTICE` file, so there is no copyright line to carry over.) |
| Which is a port of | the **Tokyo Night** VS Code theme by **Enkia**, [tokyo-night/tokyo-night-vscode-theme](https://github.com/tokyo-night/tokyo-night-vscode-theme) (formerly `enkia/...`), per the tokyonight.nvim README ("ported from the Visual Studio Code TokyoNight theme") |
| Licence | **MIT**, Copyright (c) 2018-present Enkia — [`LICENSE-tokyo-night-vscode-theme`](LICENSE-tokyo-night-vscode-theme), checked against `7c0f11eaef322f293621ca7befe462214b7ea468` |

## What was taken, and what was not

Taken: fourteen colour values (`bg`, `bg_dark`, `fg`, `fg_dark`, `comment`, `red`,
`green`, `yellow`, `blue`, `magenta`, `cyan`, `orange`, `teal`, and `fg_dark` again
as `white`), checked one by one against `night.lua` / `storm.lua` on 2026-10-16.

Not from upstream, written for Akuma: the `TN_*` variable names, the xterm-256
approximations next to each value, the `LS_COLORS` and `GREP_COLORS` mappings, the
prompt and everything else in the profile. `TN_BLACK` (`#15161e`) is **not** a value
in `night.lua` or `storm.lua`; nothing here claims it is.

## Modifications (Apache-2.0 section 4(b))

The values were copied unchanged into shell `export` lines; no upstream file is
included, so no upstream file was modified. The two `profile` files carry this
notice in their headers.

The full notice is in [`THIRD_PARTY_NOTICES.md`](../../THIRD_PARTY_NOTICES.md).
