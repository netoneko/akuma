# Handoff result - 2026-11-02

Executed by goose (GLM, session `handoff-2026-11-02`, working dir `/root/akuma-handoff`), fired at 16:55 by
`/root/goose-handoff/go.sh` (the manual fire of the wait-for-fixed-point starter in that directory). Branch
`rust-org-il-experiment`. Everything below was done on the ryzen booted into Pop, as root.

## Task 1: the B15 fixed point - PASS

| generation | built by | md5 | bytes | build time |
|---|---|---|---|---|
| K0 | the Mac | `0272f1eb7de0e031e4266e9d89aec181` | - | - |
| K1 | a guest running K0 | `5f568279177b7edf28bbc9445fee4d4c` | 2,304,624 | ~450 s (Mac-driven; its `/tmp/fp.status` was not preserved) |
| K2 | a guest running K1 | `5f568279177b7edf28bbc9445fee4d4c` | 2,304,624 | 452 s cargo (from the guest's `/tmp/fp.status`, pulled out of the overlay) |

**K1 == K2**, `cmp`-identical, and the pulled K2's md5 equals the guest's own `md5sum`. K1 on disk also
matches the gen-1 in-guest md5 given in the handoff. Fault-ish console lines (`fp_pop.py`'s grep): gen 1: 0
(as reported); gen 2: exactly 1 - `[BKL] cpu 0 stuck: owner cpu 2` (the BKL watchdog's "stuck" wording; no
Fault/PANIC/BADFRAME anywhere). Recorded in
`docs/archive/TAOXICHUAN_FIXED_POINT.md` ("After B15 (2026-11-02)") and in the B15 row of
`docs/archive/TAOXICHUAN_CLEANUP_PLAN.md` (replacing "Fixed point not rerun yet").

**What was finished by hand.** The Mac drove only half of generation 2: it booted the K1 guest at 16:48
(qemu.pid 19249) and the build finished at 16:56 (checked out 16:48:32, built 16:56:11 per `/tmp/fp.status`;
overlay writes stopped at 16:56:12), but never came back to stop the guest and pull K2. After the 18:00
deadline passed (polled every 30 s until then, as instructed), the session finished the generation with
`fp_pop.py`'s own commands: `TX_RUN=/root/tx-fp bash /root/tx-pop.sh stop` (kill by pidfile only), then
`qemu-nbd --read-only -c /dev/nbd7` + `debugfs dump` of the kernel and `/tmp/fp.status`, `qemu-nbd -d` at
18:02. Nothing was rerun and nothing was booted in `/root/tx-fp`; `/dev/nvme0n1p3` was never mounted.

## Task 2: GLM starts on Shanghai by itself after the next reboot

**Which OS boots next: Pop.** `efibootmgr`: BootOrder starts with `Boot0002* Pop!_OS 22.04 LTS`, which is
also BootCurrent. So the starter is wired into Pop's systemd.

**Pop has everything the brief's six gates need** (checked, not guessed): goose 1.52.0 at
`/usr/local/bin/goose` with `/etc/goose.env` and root-only `/root/.config/goose` (`config.yaml` carries the
GLM settings - provider openai, model glm-5.3-flash, low effort, GOOSE_MODE auto; the z.ai key stays in
`secrets.yaml`, never printed or copied); the repo at `/root/akuma-handoff`; `cargo`/`rustc`;
`e2fsprogs` (`debugfs`, `mke2fs`); `qemu-system-x86_64` with working `/dev/kvm`. 16 cores. One caveat:
`/` is 96% full (7.5 GB free) - enough for Shanghai's build, but if a gate dies on ENOSPC, free space first.

**What is installed (commit `559a4e1c0`, pushed to litter):**

| thing | value |
|---|---|
| starter script | `/usr/local/sbin/shanghai_glm_starter.sh` (source: `scripts/kvm/goose/shanghai_glm_starter.sh`) |
| systemd unit | `/etc/systemd/system/shanghai-glm.service`, **enabled** (multi-user.target, After=network-online.target, TimeoutStartSec=0) |
| fires on | Pop, ~240 s after the unit starts (network/services settle) |
| marker | `/root/shanghai-starter/fired` - checked first, written immediately before goose starts, and only after every prerequisite passes (a failed prerequisite leaves it absent, so the next boot retries) |
| log | `/root/shanghai-starter/goose.log` (starter narration + all goose output) |
| working directory | `/root/shanghai-wt` - a dedicated `git worktree` detached at `origin/rust-org-il-experiment`; the brief's own first step creates the `shanghai` branch inside it, so `/root/akuma-handoff` stays on `rust-org-il-experiment` |
| goose invocation | `. /etc/goose.env; GOOSE_MODE=auto goose run --with-builtin developer -n shanghai-01 -i <brief>` - the exact shape the earlier GLM sessions on this box used |

**Manual test (without a reboot): done, passed on the second attempt.** Fired by hand with
`SHANGHAI_DELAY=0`, a scratch marker, scratch log and scratch worktree. The first attempt found two real
bugs and one dangerous fallback: the repo check used `-d .git` (the checkout is itself a worktree - `.git`
is a file), `origin/rust-org-il-experiment` did not exist locally because this checkout shipped a
**main-only fetch refspec**, and the script's fallback would have run goose directly in the user's
checkout (the test goose did exactly that and created a `shanghai` branch there before being stopped by
pid). Fixes: fetch the branch by explicit refspec into its tracking ref and verify it; remove the fallback
(exit 1 instead); write the marker only after all checks pass. Second attempt: worktree created detached at
`4ee8fc3d6`, goose started in it, announced "I'll start by reading the brief", read it and worked ahead
(noticed KVM/TCG on its own); stopped by pid once the brief-read was confirmed. Scratch worktree, scratch
`shanghai` branch and scratch marker were removed; **the real marker is absent**, so the starter fires after
the reboot. Two box-side fixes support the brief itself: `remote.origin.fetch` is now the standard
`+refs/heads/*:refs/remotes/origin/*` (was main-only), and the brief file is also installed at
`/root/shanghai-starter/shanghai-01-new-kernel.md` as the starter's fallback (single-file copy).

**The older starter in `/root/goose-handoff/`** (`run.sh`, `go.sh`, `prompt.md`) fires THIS handoff, not
Shanghai, and was already fired at 16:55 (marker `fired` present), so it will never fire again. It was
deliberately left fired: after a reboot only the Shanghai starter should fire. Re-arm it only if the handoff
itself should ever rerun.

## What the user must do before/after rebooting

- **Push to origin** - the box cannot: every github.com credential stored for root identifies to GitHub as
  `netoneko`, and that identity has write to `akuma-litter` (all litter pushes succeeded) but gets HTTP 403
  on `netoneko/akuma`; `ssh -T git@github.com` has no key here. No credentials were created, copied or
  printed. After this session, origin is missing three commits. The exact command:
  ```
  cd /root/akuma-handoff && git push origin rust-org-il-experiment
  ```
- Nothing else. On the next boot into Pop, `shanghai-glm.service` waits 240 s and fires goose on
  `shanghai-01-new-kernel.md` once; re-arm later by deleting `/root/shanghai-starter/fired`; watch
  `/root/shanghai-starter/goose.log`.

## Commits made by this session (branch `rust-org-il-experiment`)

- `559a4e1c0` goose: Shanghai boot starter - systemd one-shot fires GLM on the brief after reboot
- `577f2f44c` docs: B15 fixed point recorded - K1 == K2 on 961a77be
- this file

Both earlier commits are on **litter**; origin needs the push command above.

Written by goose (glm-5.3-flash) on pop-os
