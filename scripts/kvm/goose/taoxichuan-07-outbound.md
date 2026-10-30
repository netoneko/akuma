# Taoxichuan stage 7, session A: guest outbound TCP (H1), then two small open items (H2, H3)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Stage 6 passed
(offline `apk add`, `taoxichuan-s4` @ `0b33595a`). The next step toward `apk add curl` is the network: milestone M1 of
`docs/archive/TAOXICHUAN_STAGE6_PLAN.md` (§3 and §6). **Keep your context small** (quota is shared and finite). Read
`/root/dev-cycle/taoxichuan-s6/RESULT-C.md` first; read other docs selectively.

Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Checkout:
`/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`. Box log timestamps are UTC (IDT - 3 h). Work in
`/root/dev-cycle/taoxichuan-s7/`. Boot dance: as in session C (fresh copy of `/boot/akuma-dev-root-apk.img.orig` before every
boot; **edit both `/etc/herd/available/vm-dev.conf` and `/etc/herd/enabled/vm-dev.conf` and confirm the `guest disk:` line in
`/var/log/herd/vm-dev.log`**). `vm-dev` is currently running with the OLD stage-4 ssh image: stop it first. Add
`vm_outbound = true` to the conf (see the plan §3 for the keys). Stop `vm-dev` when you finish.

## Gates

- H1: from inside the guest, over the exec-only ssh path (`ssh -p 2224 root@localhost '<cmd>'`), a TCP connection to the host
  relay address (10.0.2.2) reaches a listener on the box: e.g. on the box run
  `while true; do printf 'HTTP/1.0 200 OK\r\n\r\nhello-from-host\n' | busybox nc -l -p 8123; done` in the background, and the
  guest's `busybox wget -qO- http://10.0.2.2:8123/` (or `nc 10.0.2.2 8123`) prints `hello-from-host`. The relay's outbound
  counters in `/var/log/herd/vm-dev.log` (or wherever the plan says) show a connection. Fix whatever syscall arms it needs
  (`socket`, `connect`, `getsockopt`, `poll`/`ppoll`, `getsockname`, `setsockopt` stubs...). Stop the listener loop at the end.
- H2: `dup2`/`dup3` (x86_64 33/292) handled, so `busybox dd if=/etc/hostname of=/dev/null` or any applet using them runs.
- H3: `ls` through a symlink to a directory works (`ls /etc/ssl1.1/certs` after the stage-6 install; see `resolve_effective`).
- A passing boot has zero `Fault|PANIC|sig!`. Re-run stage 6's G1 and G2 once at the end (no regression).

H1 is the target; do H2 and H3 only after H1 passes or if H1 is blocked on something outside your reach. Do not start DNS,
`apk update`, TLS or the wall clock: those are the next sessions.

Rules as before: commit and `git push litter taoxichuan-s4` at least every 10 minutes whenever anything is uncommitted, never
push `origin`, gate arch-specific code with `cfg` and keep amd64-only crates out of aarch64 default-members.

## When to stop

Stop when H1 passes in a clean boot (and H2/H3 if reached), when a gate fails in a way you cannot see past in two boots, when
the quota error appears, or after about 60 minutes. Then write `/root/dev-cycle/taoxichuan-s7/RESULT-A.md` (gates, the exact
failing step with console evidence, what was missing, what is next) and end the turn.
