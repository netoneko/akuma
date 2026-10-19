# Run crush (Kimi / GLM) on Akuma

Crush is a Go TUI agent. On the ryzen box it runs from an ssh PTY or inside herdr, against
Kimi Code and z.ai GLM, the same endpoints and tokens as `goose-kimi` / `goose-glm`.

## Use it

```sh
crush            # GLM 5.3 Flash, low reasoning effort (the crushrc default)
crush-glm        # same, model pinned
crush-kimi       # kimi-for-coding
CRUSH_MODEL=glm/glm-5.3 crush     # any <provider>/<model> from `crush models`
crush run "one-shot prompt"       # non-interactive
crush --yolo     # auto-approve every tool call. NOT the default; deliberate.
```

Yolo by default: put `permissions allow view ls grep edit bash` in
`/root/.config/crush/crushrc` (or `alias crush='crush --yolo'`).

## Install / refresh (inside Akuma, from the checkout)

```sh
cd /src/github.com/netoneko/akuma
cp -f scripts/box/crush scripts/box/crush-glm scripts/box/crush-kimi /bin/ && chmod +x /bin/crush /bin/crush-glm /bin/crush-kimi
mkdir -p /root/.config/crush && cp -f scripts/box/crushrc /root/.config/crush/crushrc && chmod 600 /root/.config/crush/crushrc
cp -f scripts/box/10-local-bin.sh /etc/profile.d/
# the binary: crush_<ver>_Linux_x86_64.tar.gz from github.com/charmbracelet/crush/releases -> /usr/local/bin/crush-bin
```

## Traps (each one cost an hour on 2026-10-19)

- **`crush-glm: not found`** — an ssh/herdr shell has `PATH=/sbin:/usr/sbin:/bin:/usr/bin`; wrappers
  must be in `/bin`. (`/usr/local/bin` is only on `PATH` inside the `/etc/akuma-dev.env` wrappers.)
- **`No providers configured`** — no `HOME` in the session, so crush looked in the wrong place. The
  wrappers set it. Run `crush-bin` directly and you hit this.
- **401 "Invalid Authentication"** — `$(cat file)` inside `crushrc` is empty on Akuma (crush's
  built-in shell resolves `cat` through `PATH`, which is empty). The file uses `$(<file)`.
- With `CRUSH_GLOBAL_CONFIG=<dir>` the file must be `<dir>/crushrc`, not `<dir>/crush/crushrc`.
- Crush's first run in a directory offers to generate an `AGENTS.md` and swallows typed text.
- Anonymous metrics are on by default; `crushrc` sets `option metrics false`.
- Closing a pty does not kill its children on Akuma: `kill -9` a stale `crush-bin` by pid.

## Verify

```sh
cd /tmp && crush run "Reply with exactly the word OK and nothing else."      # -> OK
HOSTIP=<box> scripts/utils/ssh_pty_steps.py 2222 'exec /bin/sh -i' 150 '~2' 'crush\r' '@Ready' '~45' \
  'Reply with the single word PONG and nothing else.\r' '~15'                # reply after 45 s idle
```

## Background

[`scripts/box/README.md`](../../scripts/box/README.md); crush docs: `crush.charm.land` / the repo's
`docs/config/`.
