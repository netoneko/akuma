# scripts/kvm/goose - notes on running goose (GLM) unattended on the ryzen

The supervision procedure lives in `SUPERVISOR_RUNBOOK.md`; the briefs and prompts for
each session are the `*.md` files here. This README only documents the boot starter.

## The Shanghai starter (`shanghai_glm_starter.sh` + `shanghai-glm.service`)

Task 2 of `CLAUDE_HANDOFF_2026-11-02.md`: after the next boot, GLM picks up
`shanghai-01-new-kernel.md` once, with nobody at the keyboard.

**What fires.** A one-shot systemd service on Pop (`shanghai-glm.service`, enabled under
`multi-user.target`, `After=network-online.target`). It runs
`/usr/local/sbin/shanghai_glm_starter.sh`, which waits 240 s (so the network and
services are up), then fires `goose run --with-builtin developer -n shanghai-01 -i
<brief>` exactly the way earlier GLM sessions on this box were started (`. /etc/goose.env`,
`GOOSE_MODE=auto`; the GLM model settings live in root-only `/root/.config/goose`,
the z.ai key in its `secrets.yaml` - never printed or copied). The machine boots Pop
next (efibootmgr BootOrder, checked 2026-11-02), and Pop has the repo
(`/root/akuma-handoff`), the Rust toolchain, `e2fsprogs`, and QEMU/KVM - everything the
brief's six gates need.

**Paths.**

| thing | path |
|---|---|
| marker (written before goose starts; checked first) | `/root/shanghai-starter/fired` |
| log (starter narration + all goose output) | `/root/shanghai-starter/goose.log` |
| working directory | `/root/shanghai-wt` - a dedicated `git worktree` of `/root/akuma-handoff`, detached at `origin/rust-org-il-experiment`; the brief's first step creates the `shanghai` branch inside it, so the user's checkout stays on `rust-org-il-experiment` |
| brief | the worktree's `scripts/kvm/goose/shanghai-01-new-kernel.md`, falling back to a copy at `/root/shanghai-starter/shanghai-01-new-kernel.md` |

**How it is installed** (done 2026-11-02 on Pop, as root):

```
install -m 755 scripts/kvm/goose/shanghai_glm_starter.sh /usr/local/sbin/shanghai_glm_starter.sh
install -m 644 scripts/kvm/goose/shanghai-01-new-kernel.md /root/shanghai-starter/shanghai-01-new-kernel.md
install -m 644 scripts/kvm/goose/shanghai-glm.service /etc/systemd/system/shanghai-glm.service
systemctl daemon-reload && systemctl enable shanghai-glm.service
```

**How to re-arm.** Delete the marker: `rm /root/shanghai-starter/fired`. The next boot
fires goose again. To fire it immediately instead, run the same command the unit runs:
`/usr/local/sbin/shanghai_glm_starter.sh`.

**How to test without a reboot.** Run the starter with a delay of 0 and scratch paths:

```
SHANGHAI_DELAY=0 SHANGHAI_MARKER=/tmp/fired-test SHANGHAI_LOG=/tmp/goose-test.log \
  SHANGHAI_WT=/root/shanghai-wt-test /usr/local/sbin/shanghai_glm_starter.sh
```

Check the test log until goose has read the brief, then kill that goose process by its
pid (never by pattern), and clean up: `git -C /root/akuma-handoff worktree remove
--force /root/shanghai-wt-test`, delete the scratch marker, and delete the scratch
`shanghai` branch if it was created with no commits of its own. The real marker stays
absent, so the starter still fires after the reboot.

**An older starter, already fired.** `/root/goose-handoff/run.sh` (with `go.sh`,
`prompt.md`) is a one-shot that waits for the fixed point and then fires THIS handoff.
It was fired by hand on 2026-11-02 at 16:55 (marker `/root/goose-handoff/fired` present),
so it will never fire again; do not re-arm it - after a reboot only the Shanghai starter
should fire.
