# Taoxichuan stage 7, session D (DRAFT - becomes the execution brief once session C's PLAN exists): close stage 7's gates in one clean boot (H1, H2, H3, regression) while executing the trim plan

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
5. The probes below (this is the fun part).
6. Your own `RESULT-A.md` and the commit messages `git log --oneline -8` on the branch.

## Probes you already have (use them, bend them, write your own)

Small static C or musl programs that exercise exactly the syscalls these gates need, written for earlier hunts on the full Akuma
kernel and runnable as plain ELFs in a guest. `docs/reference/userspace-layout.md` has one-liners for all of them; most are C and
build with the repo's musl toolchain on the box (`userspace/build.sh --<name>-only`, or compile the single `.c` with
`musl-gcc -static`). The Taoxichuan guest implements a subset of Linux, so a probe that dies on a missing syscall is itself a
finding: the kernel's `[sc] unhandled` line names it.
- `userspace/ext2probe/c/` - `fs_ops_cost.c` (create / seq write / seq read / list-dir / delete on ext2) and
  `read_syscall_cost.c`; static musl, one binary for Linux and Akuma (the file side of H3). `build.sh` + a prebuilt
  `x86_64/` directory are there.
- `userspace/abiprobe/c/abi_write_probe.c` - every syscall that writes a kernel struct into a user buffer, checked byte for
  byte against what real Linux produces; run the same binary on both and diff the reports. `build.sh --push-akuma PORT` pushes
  a rebuilt probe into a running guest without a reboot.
- `userspace/forktest/c_stress/` - ~100 pure-C musl-static control probes (open/dup/pipe/fd/clock/...): `ls` it and pick the ones
  whose names match the gate.
- `userspace/nettest/` - libcurl / `std::net` + `poll(2)` clients (needs more syscalls; a good stretch goal for stage 8).
- `userspace/epollprobe`, `memprobe`, `futexprobe` - per-syscall behaviour and cost for the poll, memory and futex families.
Write a 20-line probe of your own for anything the existing ones do not cover, and keep it under `scripts/kvm/goose/probes/`
so the next session finds it. Explore: you are free to spend a boot or two on curiosity if a probe shows something odd.

## Composition rule (mandatory; stages 1-4 had it, stages 6-7 lost it)
`taoxichuan/` is a **composition root**, not a kernel written from scratch. At the start of stage 7 it was 4,450 hand-written lines
(`main.rs` 2,928) next to 101 existing crates and a full amd64 kernel that already implements nearly every syscall you will meet.
Before you write any syscall arm, subsystem or helper of more than about 20 lines:
1. **Look first**: `grep -rn "<name>" crates/ amd64/src/` for the syscall or concept; skim `docs/reference/crate-safety.md` and
   `ls crates/` for the crate that owns it (`akuma-syscalls-glue` and the `akuma-syscalls-*` family, `akuma-fdtable`, `akuma-vfs`,
   `akuma-ext2`, `akuma-net`, `akuma-exec`, `akuma-ring3-amd64`, `akuma-svm-amd64`).
2. **It exists in a crate**: add a path dependency and call it. Do not copy it.
3. **It exists only in `amd64/src/`**: extract it into a crate first (`akuma-<thing>`, named for what it owns; keep `#![forbid(unsafe_code)]`
   where it can, `scripts/check_unsafe_budget.py` gates new `unsafe`), make `amd64/` depend on the crate too, then depend on it from
   `taoxichuan/`. One implementation, two consumers.
4. **It exists nowhere**: write it, in a crate with host tests where the logic is pure, and say why in the RESULT file.
In every RESULT file report, per arm or feature you added: *pulled* / *extracted* / *written new (why)*, and `wc -l taoxichuan/src/*.rs`
at the start and the end. A session that closes a gate and makes `taoxichuan/src` smaller is better than one that closes it larger.

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
minutes. Then write `/root/dev-cycle/taoxichuan-s7/RESULT-C.md` (each gate with its captured output, per arm or feature *pulled / extracted / written new*, `wc -l taoxichuan/src/*.rs` before and after, the exact failing step with
console evidence, what is next) and end the turn.
