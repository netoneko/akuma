# Taoxichuan stage 7, session C: close stage 7's gates in one clean boot (H1, H2, H3, regression)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Session B
(`taoxichuan-s4` @ `b7646dd9`) ended on the z.ai quota limit with no `RESULT-B.md`. Its work is committed. This session's job
is verification and the one gate still failing. **Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch
`taoxichuan-s4`. Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`
(without it `git push litter` asks for a password). Box log times are UTC.

## The key fact
Outbound TCP is a finished, metal-verified host feature (`vm_outbound`); do not change or rebuild the host kernel, and do not
touch `/boot/akuma-amd64*`. Connect to real addresses: the gate host is the box's LAN IP `192.168.1.159`, never `10.0.2.2`.
What session B left: the host's `python3 -m http.server 8123` logged `GET / HTTP/1.1 200` for the guest and the relay counters
read `to_host=81 to_guest=201`, but the guest-side output of the gate commands was not captured in a clean boot. `/dev/null`
open (`OpenFile::Null`) is implemented and not yet gated. Write-after-create (H3) still fails.

## Read first (in order; `sed -n` the named parts, do not read whole files)
1. `docs/reference/subsystems/syscalls/fs.md` - the file-syscall family: what each fd-taking call must do per fd kind.
2. `docs/reference/subsystems/fdtable.md` - the fd table vocabulary the kernel shares (what an fd can be, dup/close semantics).
3. `docs/reference/subsystems/vfs.md` - the VFS write path and the ext2 mount; and `docs/archive/AKUMA_EXT2_CLEANUP.md` for the
   ext2 write side (inode/bitmap/size updates).
4. `userspace/ext2probe` (create / sequential write / sequential read / list) and `userspace/abiprobe`: existing guest-side probes
   for exactly these paths; `docs/reference/userspace-layout.md` has their one-liners and build commands.
5. Your own `RESULT-A.md` and the commit messages `git log --oneline -8` on the branch.

## Gates (every gate command's stdout goes to a file in `/root/dev-cycle/taoxichuan-s7/` and you `cat` it afterwards)
- **H1:** listener on the box serving `index.html` containing `hello-from-host` on port 8123; over the exec-only ssh path
  (`ssh -p 2224 root@localhost '<cmd>'`), `busybox wget -qO- http://192.168.1.159:8123/` prints `hello-from-host`; relay counters
  `to_host`/`to_guest` > 0 in `/var/log/herd/vm-dev.log`.
- **H2:** `dd if=/etc/hostname of=/dev/null` exits 0.
- **H3:** `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` prints `x` and then a size of 2.
- **Regression:** stage 6's G1, G2, G3 (command lines in `RESULT-C.md` of stage 6), plus `ls /etc/ssl1.1/certs` rc 0.
- Zero `Fault|PANIC|sig!` in a passing boot. Out of scope: DNS, `apk update`, TLS, the wall clock.

A cold guest is slow to answer its first ssh command: give each gate command a generous timeout (60 s or more) and retry once
before concluding it printed nothing.

## Ops traps
- Guest kernel installs only with an explicit `/boot/akuma-guest-dev` destination; never write `/boot/akuma-amd64*`.
- Stop `vm-serve-mc` before `vm-dev` (kvm0 is exclusive). Edit BOTH `/etc/herd/available/vm-dev.conf` and
  `/etc/herd/enabled/vm-dev.conf`, confirm the `guest disk:` line in `/var/log/herd/vm-dev.log`, fresh copy of
  `/boot/akuma-dev-root-apk.img.orig` before every boot. The stage-7 conf template is
  `/root/dev-cycle/taoxichuan-s7/vm-dev.conf.stage7-hostrelay` (drop `hostrelay=`; keep `vm_outbound = true`).
- At the end: stop `vm-dev`, stop the `:8123` listener, restore both confs to `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4`,
  restart `vm-serve-mc`.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after every boot that taught you something and after every fix that builds. WIP is
fine. Never push `origin`. Gate arch-specific code with `cfg`; keep amd64-only crates out of aarch64 default-members.

## When to stop
All gates pass in one clean boot; or three boots on the same gate without new evidence; or the quota error. Count boots, not
minutes. Then write `/root/dev-cycle/taoxichuan-s7/RESULT-C.md` (each gate with its captured output, the exact failing step with
console evidence, what is next) and end the turn.
