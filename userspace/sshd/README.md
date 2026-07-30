# Akuma SSHD (Userspace)

The SSH-2 server for Akuma OS, running as an ordinary `no_std` userspace process
on `libakuma`. On the devbox images this is the **only** sshd — the in-kernel one
is compiled out via the `userspace-sshd` feature.

## Protocol support

| | |
| --- | --- |
| Key exchange | `curve25519-sha256` |
| Host key | `ssh-ed25519` |
| Cipher | `aes128-ctr` (both directions) |
| MAC | `hmac-sha2-256` (both directions) |
| Compression | `none` |
| Auth | `publickey` (`ssh-ed25519` only), or accept-anything via `disable_key_verification` |
| Channel requests | `shell`, `exec`, `pty-req`, `window-change`; `exit-status` / `exit-signal` on the way out |

`password` auth is **not** implemented and is always rejected. Primitives
(packet framing, key derivation, byte helpers) come from
[`crates/akuma-ssh-crypto`](../../crates/akuma-ssh-crypto), shared with the
in-kernel server, so they are covered by that crate's host test suite.

## Sessions

- **Concurrent.** Every connection is one future in a cooperative multiplexer in
  `main()`; sessions are polled round-robin, so an idle session doesn't block
  the others. See [`docs/FLOW.md`](docs/FLOW.md).
- **Interactive** (`shell` request) spawns the login shell on a **pty**, so the
  kernel's line discipline cooks input and `TIOCGWINSZ` reports the client's
  real terminal size (kept current via `window-change`).
- **One-shot** (`exec` request, i.e. `ssh host cmd`) spawns `<shell> -c <cmd>` on
  a pipe. Both paths share the same `bridge_process` I/O pump, so output
  draining and exit-status reporting behave identically.
- **No fallback shell.** `config.shell` must name a real executable; a spawn
  failure ends the session with an error message and exit status 127. The old
  hand-rolled built-in shell was removed (see
  [`docs/MIGRATION_SUMMARY.md`](docs/MIGRATION_SUMMARY.md)).

## Files

| Path | Purpose |
| --- | --- |
| `/etc/sshd/sshd.conf` | Configuration (below) |
| `/etc/sshd/id_ed25519` | Host private key — generated on first run if absent |
| `/etc/sshd/id_ed25519.pub` | Host public key |
| `/etc/sshd/authorized_keys` | Accepted client keys, unless key verification is disabled |

## Configuration

`/etc/sshd/sshd.conf`, `key = value` per line, `#` comments. Unknown keys are
ignored.

| Key | Default | Meaning |
| --- | --- | --- |
| `shell` | `/bin/sh` | Login shell for both interactive and `exec` sessions |
| `port` | `2222` | Listen port |
| `disable_key_verification` | `false` | Accept **any** auth without checking `authorized_keys` — dev/demo only |

Booleans accept `true`/`yes`/`1`/`on`.

### CLI flags

Flags override the config file. Precedence for the port is CLI → config → 2222.

```bash
/bin/sshd --port 22 --shell /bin/sh
/bin/sshd --shell /bin/toybox --shell-arg sh    # multicall: argv = ["/bin/toybox", "sh"]
```

`--shell-arg` may be repeated and appends one argument each time; it exists for
multicall binaries that select an applet by argv. It is CLI-only — there is no
config-file equivalent.

Under `herd`, the whole command line goes in `args`:

```
# /etc/herd/enabled/sshd.conf
command = /bin/sshd
args = --port 22 --shell /bin/sh
```

## Build

```bash
cd userspace && ./build.sh --sshd-only     # -> bootstrap/bin/sshd
```

Must be run from `userspace/` — the script resolves `../bootstrap` relative to
the cwd, and `-p sshd` only resolves inside the userspace workspace.

## Tests

Host unit tests cover the pure wire-format logic in `src/wire.rs`:

```bash
cd userspace && cargo test -p sshd --lib --no-default-features \
    --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

`--no-default-features` is **required**: it drops `libakuma`, whose
`#[panic_handler]` and `#[global_allocator]` cannot link against a std target.
Only logic that needs no syscalls can live in the lib target — see
[`src/lib.rs`](src/lib.rs). Everything else is exercised in QEMU.

## Docs

| Doc | Contents |
| --- | --- |
| [`docs/FLOW.md`](docs/FLOW.md) | Session/channel lifecycle, the multiplexer, `shell` vs `exec` |
| [`docs/EXIT_STATUS_FIX.md`](docs/EXIT_STATUS_FIX.md) | Why remote commands used to always return 255 |
| [`docs/INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md`](docs/INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md) | Output lost at child exit, and the drain that fixed it |
| [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) | Known gaps |
| [`docs/MIGRATION_SUMMARY.md`](docs/MIGRATION_SUMMARY.md) | History of the kernel → userspace port |
