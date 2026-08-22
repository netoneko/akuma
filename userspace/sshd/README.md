# Akuma SSH (Userspace)

The SSH-2 server (`sshd`) and a companion client (`ssh`) for Akuma OS,
built as two binary targets in one package — both `no_std` userspace
processes on `libakuma`. On the devbox images `sshd` is the **only** SSH
server — the in-kernel one is compiled out via the `userspace-sshd`
feature.

The client is documented separately: [`docs/SSH_CLIENT.md`](docs/SSH_CLIENT.md)
(usage, scope, identity keys, `known_hosts`) and
[`docs/SECURITY_IMPROVEMENTS.md`](docs/SECURITY_IMPROVEMENTS.md) (audit
findings and fixes). The rest of this README is `sshd` (the server).

## Protocol support

| | |
| --- | --- |
| Key exchange | `curve25519-sha256` |
| Host key | `ssh-ed25519` |
| Cipher | `aes128-ctr` (both directions) |
| MAC | `hmac-sha2-256` (both directions) |
| Compression | `none` |
| Auth | `publickey` (`ssh-ed25519` only), or accept-anything via `disable_key_verification` (requires the `insecure-disable-key-verification` build feature — off by default) |
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
| `disable_key_verification` | `false` | Accept **any** auth without checking `authorized_keys` — dev/demo only. **No-op unless built with `--features insecure-disable-key-verification`** (off by default); otherwise parsed but ignored, with a warning logged. See `docs/SECURITY_IMPROVEMENTS.md` |
| `banner` | `true` | Print the ASCII-art welcome banner on interactive `shell` sessions (mirrors the in-kernel server's login banner) |

Booleans accept `true`/`yes`/`1`/`on`.

### CLI flags

Flags override the config file. Precedence for the port is CLI → config → 2222.

```bash
/bin/sshd --port 22 --shell /bin/sh
/bin/sshd --shell /bin/toybox --shell-arg sh    # multicall: argv = ["/bin/toybox", "sh"]
/bin/sshd --no-banner                           # skip the welcome banner
```

`--shell-arg` may be repeated and appends one argument each time; it exists for
multicall binaries that select an applet by argv. It is CLI-only — there is no
config-file equivalent.

`--no-banner` is CLI-only (disable-only); to re-enable a banner turned off in
the config file, set `banner = true` in `sshd.conf` instead.

Under `herd`, the whole command line goes in `args`:

```
# /etc/herd/enabled/sshd.conf
command = /bin/sshd
args = --port 22 --shell /bin/sh
```

## Client (`ssh`)

A minimal interactive SSH-2 client — `ssh [-p port] [-l user] [-i identity]
[-t term] [user@]host [command...]` — speaking the same algorithm suite as
the table above, Ed25519/curve25519-sha256/aes128-ctr/hmac-sha2-256 only, no
negotiation. Interactive shell over a `pty` by default, or a one-shot
`exec` when a command is given. No SFTP/SCP, no port/agent/X11 forwarding,
no rekeying — see [`docs/SSH_CLIENT.md`](docs/SSH_CLIENT.md) for the full
scope, identity-key resolution, and `known_hosts` TOFU behavior.

It's a second `[[bin]]` target in this same package
(`src/client/main.rs` → binary `ssh`), not a separate crate — the two sides
parse opposite halves of the same wire format, so `cargo build -p sshd`
already produces both binaries.

## Build

```bash
cd userspace && ./build.sh --sshd-only     # -> bootstrap/bin/sshd
cd userspace && ./build.sh --ssh-only      # -> bootstrap/bin/ssh
```

Must be run from `userspace/` — the script resolves `../bootstrap` relative to
the cwd, and `-p sshd` only resolves inside the userspace workspace. `ssh`
isn't a package of its own (`cargo build -p ssh` doesn't resolve); `--ssh-only`
knows to build the `sshd` package and copy out just that binary. A plain
`cargo build --release -p sshd`, or the full `userspace/build.sh`, already
builds both.

## Tests

Host unit tests cover the pure wire-format logic: `src/wire.rs` (server
channel-message builders) and `src/client_wire.rs` (client KEXINIT
build/parse, the KEX exchange-hash, and encrypted/unencrypted packet
framing — including several buffer-safety regressions found while writing
these tests, see [`docs/SECURITY_IMPROVEMENTS.md`](docs/SECURITY_IMPROVEMENTS.md)):

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
| [`docs/SSH_CLIENT.md`](docs/SSH_CLIENT.md) | The `ssh` client: scope, usage, identity keys, `known_hosts` |
| [`docs/SECURITY_IMPROVEMENTS.md`](docs/SECURITY_IMPROVEMENTS.md) | Security audit of the client: fixes and accepted limitations |
| [`docs/FLOW.md`](docs/FLOW.md) | Session/channel lifecycle, the multiplexer, `shell` vs `exec` |
| [`docs/EXIT_STATUS_FIX.md`](docs/EXIT_STATUS_FIX.md) | Why remote commands used to always return 255 |
| [`docs/INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md`](docs/INTERACTIVE_SHELL_BRIDGE_DRAIN_FIX.md) | Output lost at child exit, and the drain that fixed it |
| [`docs/CLIENT_REAL_SERVER_INTEROP_FIX.md`](docs/CLIENT_REAL_SERVER_INTEROP_FIX.md) | `ssh` client vs a real server: unhandled interleaved `GLOBAL_REQUEST`/`WINDOW_ADJUST` |
| [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) | Known gaps |
| [`docs/MIGRATION_SUMMARY.md`](docs/MIGRATION_SUMMARY.md) | History of the kernel → userspace port |
