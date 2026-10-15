# powersaved

The power-saving daemon. A **dedicated binary** (see
[`docs/reference/userspace-layout.md`](../../reference/userspace-layout.md),
"Binaries, not scripts" — this is the design the `klogd`-replaces-`klog.sh`
precedent established), supervised by herd like every resident service.

## What it does

Once a second it reads `/proc/power`, takes the kernel's `idle_ms=` fact
(console-input inactivity — a keystroke on the keyboard, serial or USB HID
resets it inside `amd64/src/input.rs`), and applies the idle staircase:

| idle | brightness |
|---|---|
| < 60 s | 100 % |
| 60 s | 60 % |
| 180 s | 30 % |
| 600 s | 10 % |

Each step is one `dim=<percent>` write to `/dev/power` (a line logged to
stdout, which herd drains to `/var/log/herd/powersaved.log`); a keystroke
makes the next observation `100 %`, so the screen is back one poll later.
Nothing is written on quiet seconds.

## The split

- **Policy** — `src/lib.rs`, pure, host-tested:
  `cargo test -p powersaved --lib --no-default-features`
- **Effects** — `src/main.rs`: read a file, maybe write one line, sleep.
- **Lever** — the kernel's: `/dev/power` (`amd64/src/power.rs`'s `dev_read` /
  `dev_write`) → `multiboot2::fb_set_dim` → `akuma-fbcon`'s global dim factor
  and `repaint()`.

The dim is the framebuffer console's colour scaling — it lowers what the
pixels emit, not the panel's backlight (that is the EC/backlight lever, P3 in
`proposals/RYZEN_POWERSAVE_DEMON.md`, and `/dev/power` grows the command for
it when the kernel has the register map).

## argv

`after_ms=<n>` moves the first step; `stage=<ms>:<pct>` (repeatable) replaces
the whole staircase (sorted by time); `period_ms=<n>` is the poll period. A
malformed argv prints and exits 2 — a typo in a boot config must not silently
dim on defaults.

## The herd service

`overlays/ryzen/rootfs/etc/herd/available/powersave.conf`, `restart = true` —
kernel restarts of the daemon re-read `/dev/power`'s `dim=` first, so a
restart mid-dim continues the staircase instead of double-stepping.
