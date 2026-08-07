# overlays/devbox-smoltcp/

Grade: B (verify behaviour)

**Not the same thing as `overlays/devbox/run-smoltcp.sh`.** This is a
separate overlay directory whose entire purpose is being a rump-free A/B
control: it boots the **same `devbox.img`** with a build that has zero rump
code, so the network stack is the only variable held against `overlays/devbox`
(rump). It is single-core (`unset SMP`) and uses the **built-in in-kernel**
SSH server, not the userspace `/bin/sshd`.

| File | Role |
|---|---|
| `run.sh` | `cargo run --release --no-default-features` with an explicit feature list that keeps `smoltcp` but drops every rump feature. `RUMP_NIC=0`, `unset SMP`. SSH is the built-in in-kernel server, publickey-only, on `:2222`. |
| `README.md` | The A/B rationale and the exact `nm ... \| grep -c rump` verification (0 here vs ~76 in the rump devbox build). |

```bash
overlays/devbox/bootstrap.sh                              # once, if devbox.img doesn't exist
cp ~/.ssh/id_ed25519.pub bootstrap/etc/sshd/authorized_keys
DISK=devbox.img scripts/populate_disk.sh --etc-only
overlays/devbox-smoltcp/run.sh
ssh -o StrictHostKeyChecking=no -i ~/.ssh/id_ed25519 -p 2222 root@localhost
```

## Why the name collides with `overlays/devbox/run-smoltcp.sh`

Both scripts boot Akuma on the native smoltcp stack, and both are called
some variant of "devbox-smoltcp" — but they diverge on everything else:

| | `overlays/devbox-smoltcp/run.sh` | `overlays/devbox/run-smoltcp.sh` |
|---|---|---|
| Purpose | Rump-tax A/B control | The actual recommended daily-driver devbox |
| Profile | `release` | `release-smp-shared` |
| Cores | 1 (`unset SMP`) | N (`SMP=`, real shared-kernel SMP) |
| SSH | Built-in in-kernel, publickey-only | Userspace `/bin/sshd` via herd |
| Committed | 2026-07-19 12:59:13 +0300 | 2026-07-19 13:46:48 +0300 (~45 min later) |

The later one superseded the former as *the* devbox without renaming or
removing it — the A/B control still had (and still has) a live purpose
measuring the rump tax, so both stuck around under near-identical names.
Read the script header comments (not just the directory name) before running
either.

## Background

- `overlays/devbox-smoltcp/README.md` — the full A/B rationale.
- [`../subsystems/rump-stack.md`](../subsystems/rump-stack.md) §"Rump tax vs
  native smoltcp" — the measurement this overlay exists to produce a clean
  control for (~8.7× on HTTP GET, ~6× on HTTPS, at time of writing).
- [`../../archive/SCRIPTS.md`](../../archive/SCRIPTS.md) §"Overlays" — the
  same collision, documented alongside the rest of the scripts audit.
