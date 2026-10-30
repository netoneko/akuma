# Taoxichuan stage 7, session D: execute the trim plan, steps 0 and 1 (budget base, then the WRITE/READ/LSEEK/FSTAT routing rewire) and gate H1-H3

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Session C wrote the plan
(`/root/dev-cycle/taoxichuan-s7/PLAN-trim.md`; in the repo `docs/archive/TAOXICHUAN_STAGE7_TRIM_PLAN.md`, `taoxichuan-s4` @ `587e2c5d`).
**This session executes its steps 0 and 1 only** (plan sessions S0 and S1); steps 2-4 are later sessions. **Keep your context small:**
the plan has the line numbers, so `sed -n` the arms it names and nothing else. Checkout `/src/github.com/netoneko/akuma`, branch
`taoxichuan-s4`. Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env` (without it
`git push litter` asks for a password). Box log times are UTC.

## The key fact
The plan is yours and it is verified against the code: `WRITE` sends fds 1 and 2 to the console before it consults the fd table
(`main.rs` ~1292), while `WRITEV` consults the table first. Step 1 makes `WRITE`/`READ`/`LSEEK`/`FSTAT` follow `WRITEV`'s dispatch order
(nested-child pipe, then fd-table kind, then unbound 0/1/2 as console / EOF / EBADF). Outbound TCP is a finished host feature: do not
change or rebuild the host kernel and never write `/boot/akuma-amd64*`. The gate host is the box's LAN IP `192.168.1.159`.

## Read first (the plan is the spec; read it fully, then only what it points at)
1. `docs/archive/TAOXICHUAN_STAGE7_TRIM_PLAN.md` (§2 step table, §3 H2/H3, §6 unknowns).
2. `docs/reference/subsystems/fdtable.md` and `docs/reference/subsystems/syscalls/fs.md` for the per-fd-kind rules the routing must obey.
3. The probes the plan names: `userspace/ext2probe/c`, `userspace/abiprobe/c`, `userspace/forktest/c_stress/` (one-liners in
   `docs/reference/userspace-layout.md`). Use one if a gate surprises you; keep any new probe under `scripts/kvm/goose/probes/`.

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

## Steps and gates
- **Step 0 (host only, no boot):** make `scripts/check_unsafe_budget.py` and `cargo clippy` green at the base (the plan §2: `akuma-ring3-amd64`
  has 5 `unsafe` sites against a budget of 2). Shrink them if you can; if they are irreducible, `scripts/check_unsafe_budget.py --update`
  with the reason in the commit message. Run the host tests the plan lists. Commit.
- **Step 1:** the routing rewire in `taoxichuan/src/main.rs`, no new dependency, about 40 lines moved. Then ONE boot that runs the whole gate
  set. Every command goes over `ssh -p 2224 root@localhost '<cmd>'` (one exec per connection), stdout to a file in
  `/root/dev-cycle/taoxichuan-s7/`, `cat` it afterwards, 60 s or more per command on a cold guest, retry once before concluding it printed nothing:
  - **G1** `sh -c "echo hi"` -> `hi` rc 0. **G2** `apk.static --version` -> `apk-tools 3.0.8-r0, compiled for x86_64.`
  - **G3** offline `apk.static --no-log --root / --no-network --no-cache --allow-untrusted --force-non-repository add
    /pkgs/ca-certificates-bundle-20261020-r0.apk` -> `OK: 174 KiB in 1 packages` rc 0, then `sha256sum /etc/ssl/certs/ca-certificates.crt` = `5961c0a3...`.
  - **G4** `ls /etc/ssl1.1/certs` rc 0.
  - **H1** with a listener serving `hello-from-host` on port 8123, the guest's `busybox wget -qO- http://192.168.1.159:8123/` prints
    `hello-from-host` and the relay counters in `/var/log/herd/vm-dev.log` show `to_host`/`to_guest` above 0.
  - **H2** `dd if=/etc/hostname of=/dev/null` exits 0.
  - **H3 (split into three execs, per the user's decision)**: `sh -c 'echo x > /tmp/f'` rc 0; then `cat /tmp/f` prints `x`; then
    `wc -c /tmp/f` prints `2`. The one-line `;` form is out of scope (no `fork`/`clone` arm exists).
  - Zero `Fault|PANIC|sig!` in the console log of a passing boot.
- The plan's unknowns U1-U3 settle in this boot: read the existing `[w]` write probe's output for which call `echo` rides, and report it.
- If a gate fails: revert only the step-1 change, say which gate and the console evidence, and stop. Do not start step 2.

## Ops traps
- Guest kernel installs only with an explicit destination: `scripts/install_kernel_amd64.sh <elf> /boot/akuma-guest-dev`. Never the default.
- `herd stop vm-serve-mc` before `vm-dev` (kvm0 is exclusive). Edit BOTH `/etc/herd/available/vm-dev.conf` and `/etc/herd/enabled/vm-dev.conf`,
  confirm the `guest disk:` line in `/var/log/herd/vm-dev.log`, fresh copy of `/boot/akuma-dev-root-apk.img.orig` before every boot and re-run the
  G3 install in it. Template conf: `/root/dev-cycle/taoxichuan-s7/vm-dev.conf.stage7-hostrelay` (`vm_outbound = true`).
- At the end: stop `vm-dev` and the `:8123` listener, restore both confs to `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4`, `herd disable vm-dev`,
  start `vm-serve-mc`.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after every boot that taught you something and after every fix that builds. WIP is fine. Never push
`origin`. `taoxichuan` stays amd64-only and out of the root `default-members`; pre-commit clippy and the unsafe-budget check must pass.

## When to stop
Step 1's gates all pass in one clean boot; or one step-1 gate fails and you have reverted it and reported it; or three boots with no new evidence; or
the quota error. Count boots, not minutes. Then write `/root/dev-cycle/taoxichuan-s7/RESULT-D.md`: each gate with its captured output, per change
*pulled / extracted / written new*, `wc -l taoxichuan/src/*.rs` before and after, the answer to U1-U3, and the exact failing step if any. End the turn.
