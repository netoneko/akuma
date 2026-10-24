# Taoxichuan stage 6, session B: make sshd able to spawn a real binary, then run apk.static offline

You are goose-glm, the metal track of the experiment in `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen
box. Your 6A session hit the 5-hour quota at 01:04 IDT. The window has now reset (02:44 IDT), so this session has a
full window; **spend it carefully anyway: keep your context small** (a 220k-token context burned ~60% of a window per hour;
a fresh one a tenth of that). This prompt continues 6A. The plan is `docs/archive/TAOXICHUAN_STAGE6_PLAN.md`; the 6A brief
(`scripts/kvm/goose/taoxichuan-06a-offline-apk.md`) still defines the staged image, the gates G1-G4, the boot dance, the
rules and the stop conditions. Read it first; this prompt only changes the situation and the order.

Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Checkout:
`/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`. Box log timestamps are UTC (IDT - 3 h).

## State, verified by the supervisor (do not redo)

- `taoxichuan-s4` @ `06070926` (pushed to litter, tree clean): your ET_DYN static-PIE base in the ring-3 loader and the
  phase A/C syscall arms. `vm-dev` is **disabled**, its conf is **restored** to the stage-4 one (backup at
  `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4`), `vm-serve-mc` is running. `/boot/akuma-dev-root-apk.img.orig`
  (sha256 `f3aa35d6...`) is intact; copy it to `/boot/akuma-dev-root-apk.img` before every boot.
- **The finding that changes the plan.** In `/var/log/herd/vm-dev.log` every ssh exec shows `[SSH] '/bin/sh' failed to spawn
  ... retrying with the built-in emergency shell /bin/paws` and then `'/bin/sh' and the emergency shell both failed to
  spawn ... running built-in paws in-process`. That is also true in the stage-4 boot, before any of your 6A changes. So the
  stage-4 gate (`echo`, `cat`) passed on **paws builtins inside sshd**; **no external binary has ever been spawned from
  an ssh session in this kernel** (which also explains `ls /bin` failing: `ls` is not a builtin). `apk.static` is an external
  binary, so G1 cannot pass until this is fixed.
- Also seen: `unhandled nr 5` (`fstat`) and `unhandled nr 4415` (`UPTIME`, `0x1000 + 319`) from sshd itself.

## Order of work

1. **Find out why `/bin/sh` fails to spawn** from sshd's `SPAWN` (private syscall, `taoxichuan/src/child.rs` and the
   loader path in `akuma-ring3-amd64`). Stage 3's init ran `/bin/busybox` directly and worked, so suspect: how the path
   is resolved (is `/bin/sh` a symlink or a hard copy in the image, does the open follow symlinks?), what the SPAWN arm
   returns as errno, argv/envp marshalling, or a missing arm that the child hits at startup. Make the failure *say* what
   failed: print the errno or the unhandled syscall *name* on the console (the 6A brief asked for names instead of numbers
   in the `unhandled nr` arm; check it is in). Add the `fstat` and `UPTIME` arms if not.
2. Once `ssh ... 'sh -c "echo hi"'` really spawns `/bin/sh` (not the in-process fallback; the log line must be gone),
   run **G1-G4** from the 6A brief. `apk.static` is static-PIE; your ET_DYN base is untested against it.
3. Same rules as 6A: fresh image copy per boot, restore `vm-dev.conf` and **stop `vm-dev` when you finish** (the Taoxichuan
   guest never halts its vCPU and pins a host core at 100%; the owner's fans spin), commit and `git push litter
   taoxichuan-s4` every 10 minutes, zero faults in a passing boot, never push `origin`, gate arch-specific code.

## When to stop

Stop when a gate fails in a way you cannot see past in two boots, when the quota error appears, or after about 90
minutes. Then write `/root/dev-cycle/taoxichuan-s6/RESULT-B.md`: which gates passed, the exact failing step with console
evidence, what the spawn problem turned out to be, what is still missing, and the next experiment. Commit, push litter,
restore the conf and `vm-serve-mc`, end the turn.
