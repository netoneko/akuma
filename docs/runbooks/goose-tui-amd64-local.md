# Check that a TUI (goose) renders on Akuma/amd64 — locally, no HP box

**Stability: B.** Verified 2026-10-03 with goose 1.52.0 (musl, dynamic, 150 MB) on
`410117ce`.

Goal: prove a terminal UI draws and takes keys on the amd64 kernel using only the
Mac. The harness is the terminal (it renders with `pyte`), so the screen it
prints is what a terminal would show.

## Do

1. Pull the binary and its two libs from the box (`goose` is not in the repo):
   `/usr/local/bin/goose`, `/lib/ld-musl-x86_64.so.1`, `/usr/lib/libgcc_s.so.1`.
   Stream them as **bytes** (`subprocess.run(hpbox.AK + ['cat <path>'], stdout=f)`);
   `hpbox.akuma()` decodes to text and corrupts binaries. 150 MB takes ~5 min.
2. Build the kernel and a disk big enough (the 512 MiB default is too small):
   ```bash
   cargo build -p akuma-amd64 --target x86_64-unknown-none --release --features no-tests
   sh amd64/mkdisk.sh $TMP/root.img 700
   D=/opt/homebrew/opt/e2fsprogs/sbin/debugfs
   for d in usr usr/local usr/local/bin; do $D -w -R "mkdir $d" $TMP/root.img; done
   $D -w -R "write goose usr/local/bin/goose"      $TMP/root.img
   $D -w -R "write libgcc_s.so.1 usr/lib/libgcc_s.so.1" $TMP/root.img
   $D -w -R "set_inode_field usr/local/bin/goose mode 0100755" $TMP/root.img
   ```
   (`debugfs write` makes the file `0644`; without the `set_inode_field` it will not exec.)
3. Drive it:
   ```bash
   python3 -m venv $TMP/v && $TMP/v/bin/pip install pyte
   $TMP/v/bin/python scripts/amd64_tui_probe.py $TMP/root.img \
       'export TERM=xterm-256color HOME=/root; /usr/local/bin/goose session' \
       --wait 200 --key right:20 --key enter:90 --out $TMP
   ```

## Verify

`snaps.txt` after `right` shows the cliclack prompt with the selection moved:
`*  Share anonymous usage data ...` / `| Yes / > No`. After `enter` goose prints
`error: No provider configured` and returns to `/ #`. Colour (`ESC[2m`, `ESC[36m`),
cursor hide (`ESC[?25l`) and arrow keys all work; `stty size` reports `24 80`.

## Traps

- **Answer `ESC[6n`.** ash (and most TUIs) query the cursor and block until a
  terminal replies. A harness that only reads looks like a hung guest.
- **TCG is slow.** The kernel reads the whole 150 MB ELF into heap first
  (`[HEAP] ... alloc=149764512`); allow ~4 min before the first prompt.
- Kernel `[PSTATS]`/`[HEAP]` lines share the serial port and scribble over the
  emulated screen. That is the guest console, not a goose rendering fault; read
  the raw stream or the earlier snapshot.
- This covers the **serial** path. The HP box's TV console (`akuma-fbcon`) is a
  different renderer — see [`amd64-console-shell.md`](amd64-console-shell.md).
- The provider is not configured on this image; it proves rendering and input,
  not a model round trip.

## Background

- [`../archive/GOOSE_TOKEN_AUDIT.md`](../archive/GOOSE_TOKEN_AUDIT.md) — the same-day look at goose's request logs.
- [`amd64-console-shell.md`](amd64-console-shell.md) — cursor-position replies on the TV console.
