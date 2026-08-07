# overlays/devbox/

Grade: B (verify behaviour)

The daily-driver dev VM: SSH in, use it like a tiny Unix workstation. Lives
entirely under `overlays/devbox/` and doesn't touch the default `bootstrap/`
tree or normal run scripts.

**Default = `run-smoltcp.sh`, not `run.sh`.** As of 2026-07-19 the
recommended image runs the native smoltcp stack with real shared-kernel SMP
and the userspace `/bin/sshd`; the rump path (`run.sh`) still builds and
boots but is deferred.

| File | Role |
|---|---|
| `bootstrap.sh` | Host build: build `herd`+`sshd` → create image → base binaries → wipe `/etc` → lay down the overlay `/etc`. |
| `run-smoltcp.sh` | **Default.** `release-smp-shared` profile + `devbox-smoltcp` feature (`userspace-sshd` + `smp-shared`, default features kept). No `RUMP_NIC`; box 0 is native smoltcp. SSH is the userspace `/bin/sshd` on `:2222`. `SMP=` env var controls core count (default 2). |
| `run.sh` | Deferred. `devbox` profile + `devbox` feature (`rump-default` + `userspace-sshd`), single kernel. `RUMP_NIC=1` brings up the second NIC rump DHCPs on. SSH on `:2223`. |
| `rootfs/` | The **sole** source of the image's `/etc` — `bootstrap.sh` wipes whatever `bootstrap/etc/` shipped and replaces it wholesale. |
| `README.md` | Full walkthrough: quick start for both paths, the feature-flag mechanics, verification steps, and a papercut backlog. |

```bash
overlays/devbox/bootstrap.sh                    # build (Docker; ~1 GB image)
overlays/devbox/run-smoltcp.sh                  # boot the default (SMP=2 by default)
ssh -o StrictHostKeyChecking=no -p 2222 root@localhost
```

## Background

- `overlays/devbox/README.md` — the full narrative, feature-flag tables, and
  a running papercut backlog (several already fixed, logged as strikethrough
  entries with root causes).
- [`../subsystems/smp-shared.md`](../subsystems/smp-shared.md) — the
  shared-kernel SMP model `run-smoltcp.sh` boots into.
- [`../../archive/SCRIPTS.md`](../../archive/SCRIPTS.md) §"Overlays".
