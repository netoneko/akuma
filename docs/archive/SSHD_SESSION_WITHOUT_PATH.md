# sshd sessions had no `PATH`, so gcc could not find `cc1`

**Status: FIXED 2026-11-03** in `userspace/sshd/src/protocol.rs` (`session_env`). Found on Taoxichuan during
[`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md).

## Symptom

In an ssh session on the Taoxichuan guest, every compile failed before it started:

```
gcc: fatal error: cannot execute 'cc1': posix_spawnp: No such file or directory
```

`cc1` was where gcc 15.2 keeps it (`/usr/libexec/gcc/x86_64-alpine-linux-musl/15.2.0/cc1`), and `gcc -print-prog-name=cc1`
from a login shell found it.

## Cause

`session_env` builds the session's environment from scratch: `HOME`, `USER`, `LOGNAME`, `ENV`, and `TERM` for a pty. It
had no `PATH`. busybox ash searches a built-in default path for its own lookups but does not **export** it, so every
program the session ran saw `getenv("PATH") == NULL`.

gcc finds its own install directory with libiberty's `make_relative_prefix`. Given a bare `argv[0]` (`gcc`), that
searches `PATH` for it. With no `PATH`, the relative prefix comes out as `../`, gcc looks for `cc1` under that, and
`posix_spawnp` fails with `ENOENT`.

**A regression since 2026-10-17, on every kernel.** Until then `session_env` returned `TERM` for a pty and nothing
otherwise, and an empty list means "the kernel's default": `akuma-exec`'s spawn substitutes `DEFAULT_ENV`, whose first
entry is that `PATH`. `ac41e664` added `HOME`/`USER`/`LOGNAME` (Claude Code needed `HOME`), so the list was never empty
again and `DEFAULT_ENV` stopped applying. On Akuma an interactive session mostly hid it: the shell's `ENV=/etc/profile`
and the `/etc/profile.d` shim put a `PATH` back. An `ssh host cmd` exec session reads neither. Taoxichuan's `linux-abi`
spawn has no default at all, which is why it showed up there first.

## Fix

`session_env` always sets `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`, the value OpenSSH
derives from Alpine's `/etc/profile` and the one `DEFAULT_ENV` carries.

## Verification

- TCG, Taoxichuan, herd as init: `echo $PATH` in an exec session prints the value above; `gcc t.c -o t && ./t` for
  `int main(){return 3;}` exits 3.
- Ryzen guest, the same two checks after installing the new sshd.
- `cargo test -p sshd --lib --no-default-features` (host): 60 passed.

## Background

- [`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md): the run where gcc failed.
- `userspace/sshd/src/protocol.rs`, `session_env`'s doc comment: why `HOME` is set (Claude Code's `uv_os_homedir`).
