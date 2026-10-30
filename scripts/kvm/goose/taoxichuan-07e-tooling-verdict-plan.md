# Taoxichuan stage 7, session E: tooling that makes gates cheap, the definitive verdict on the committed kernel, and the plan for the rest of stage 6

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Session D (`RESULT-D.md`, `taoxichuan-s4` @
`0ecb1611`) executed trim steps 0-1 and got G1-G4, H1 and H3a/b/c to pass in separate boots, but the final kernel never got one clean pass of everything, H2 was
never reached, and gates cost far too many calls. **This session has three jobs, in this order. Keep your context small and stay under about 120 tool calls.**
Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`. Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`.

## Cost rules for this session (the last session spent 73% of its context on reading output)
- **Cap every read**: pipe output through `| cut -c1-180 | head -60`; use `grep -n -m 20 PATTERN file` and `sed -n 'A,Bp'` with windows of 60 lines or less; never `cat` a source file.
- **Never repeat a multi-step sequence by hand**: if you did the same three commands twice, it belongs in a script (job 1).
- **Print verdicts, not logs**: scripts write full output to files under `/root/dev-cycle/taoxichuan-s7/` and print one line per check; you read the file only for a FAIL.

## The key fact
The host side of outbound networking is finished and correct (the relay, `vm_outbound`); the host kernel is not rebuilt or changed in this experiment. The guest-side gaps session D
found were fd-family arms (`readv`, `F_DUPFD`, exec keeping open fds), each gate-verified. What is expensive now is not the kernel but the gate loop: output lost on the ssh client, a
conf snapshot clobbered, a stale spec, a forward that dies near 30 connections. Fix the loop first; then the kernel work gets cheap.

## Read first for jobs 1-2 (named parts only)
`RESULT-D.md` (§ Ops findings), `scripts/vm_ready.py` (docstring), `scripts/install_kernel_amd64.sh` (usage lines), `docs/reference/subsystems/kvm.md` (the `vm_*` conf keys and
§ VMs as services). Job 3 lists its own reading.

## Job 1: two scripts (shell, no kernel code), then use them for everything
Look first at what exists (composition applies to scripts too): `scripts/vm_ready.py` (ssh-round-trip readiness), `scripts/ssh_harness.py`,
`scripts/kvm/dev_cycle.py`, `scripts/install_kernel_amd64.sh`, `/bin/kbuild`, and your own boot dance in `RESULT-D.md` "Ops findings". Reuse their pieces. Put the new ones in
`scripts/kvm/goose/` and commit them.
1. **`boot_dev.sh [KERNEL_ELF]`**: stop `vm-dev`; `herd stop vm-serve-mc`; write BOTH confs (`/etc/herd/available/` and `/etc/herd/enabled/`) from a tracked template
   (`scripts/kvm/goose/vm-dev.stage7.conf`, with `vm_outbound = true`; the template must be in git so it cannot be clobbered again); `herd disable` then `enable` (herd re-parses
   only then); fresh copy of `/boot/akuma-dev-root-apk.img.orig`; install KERNEL_ELF ONLY with the explicit destination `/boot/akuma-guest-dev`; start; wait until a real ssh
   round trip answers (60 s polls, 300 s budget); verify `/var/run/herd/vm-dev.vm` contains `outbound=1` and the `guest disk:` line is the apk image. Print ONE line:
   `BOOT OK disk=<img> outbound=1 ready_in=<s>` or `BOOT FAIL <reason>`.
2. **`run_gates.sh`**: with the guest up, run G1, G2, G3 (+ sha), G4, H1 (starts and stops the `:8123` listener rooted at `/root/dev-cycle/taoxichuan-s7/www`), H2 (create
   `/etc/hostname` first: it is not in the image), H3a/b/c (three execs), pacing the connections as `RESULT-D.md` finding 3 describes. Evidence is **console-first**: the guest's
   `[SSH] Exit status: N` lines and probe lines in `/var/log/herd/vm-dev.log` are delivery-independent; client capture is a bonus. Where content matters turn it into an exit code
   (`grep -c … ` style). Print one line per gate: `PASS G1`, `FAIL H3b (see h3b.out)`, `UNKNOWN G2 (no console exit line)`. If the host forward stops reaching the guest (no
   `Accepted connection` on the console while 2224 still accepts), the script restarts the VM itself and resumes at the next gate, once.
Both must be idempotent and safe to re-run. They must never write `/boot/akuma-amd64*`.

## Job 2: the definitive verdict on the committed kernel
Build the committed head (`kbuild -j 4 -p taoxichuan --features no-tests`), then `boot_dev.sh` + `run_gates.sh` ONCE. Write the result (the one-line-per-gate table plus the files'
paths) to `/root/dev-cycle/taoxichuan-s7/RESULT-E.md`. If a gate fails, fix only what the evidence names, in as few lines as the composition rule allows, and re-run; if you fail the same
gate twice with no new evidence, record it and go on to job 3. Also answer **one question with evidence**, read-only: `RESULT-D.md` says the forward "leaks at about 30 connections
per boot" and calls it host-side. Which side holds the resource that runs out: does the guest close its session sockets when an exec ends (look at the relay counters and slots in
`/var/log/herd/vm-dev.log`, `docs/reference/subsystems/kvm.md` § relay, `docs/archive/KVM_RELAY_SYN_STALL.md`, and the guest's close/shutdown arms)? Report what the evidence shows.

## Job 3: the plan for the rest of stage 6, and what comes with it (a plan document, no kernel code)
Table stage 6 (`apk add curl`) is about half done: offline `apk add` passes and outbound TCP passes (H1). Still needed: DNS, a wall clock, `apk update` from a **LAN HTTP mirror**
(no internet, no TLS), `apk add curl` (and `/etc/apk/world`), and `curl --version` running. Write `/root/dev-cycle/taoxichuan-s7/PLAN-stage6-rest.md` (copy to
`docs/archive/TAOXICHUAN_STAGE6_REST_PLAN.md`, commit, push `litter`). Read first, selectively: `docs/archive/TAOXICHUAN_STAGE6_PLAN.md` (§3 network, §4 clock, M4-M6),
`docs/archive/TAOXICHUAN_STAGE7_TRIM_PLAN.md` (steps 2-4: where the trim fits), `docs/archive/KVM_OUTBOUND_NETWORKING.md` (the host's DNS forwarder at `10.0.2.3`),
`docs/reference/subsystems/syscalls/time.md` and `crates/akuma-syscalls-time` (the SNTP client and what is callable), `docs/archive/APK_MISSING_SYSCALLS.md`, and `userspace/nettest`
(`nettest-std` is a ready client). The plan must contain: (1) the syscall/feature gap per milestone, diffed against what `taoxichuan/src` serves today, each row with the existing crate or
`amd64/src` function that already does it and a verdict (depend / extract / write new); (2) how the LAN mirror is built and served reproducibly (which packages, signed or `--allow-untrusted`,
how its checksums are compared with a Linux reference); (3) the clock: cheapest correct source; (4) dynamic linking: is `curl` static or does it need `PT_INTERP`/`ld-musl`, and what the
loader has; (5) milestones with a mini-gate command each, boots and token estimates, session boundaries, the cheapest first milestone, and where trim steps 2-3 should be interleaved;
(6) at most five unknowns with their read-only settle. Do not describe unverified fixes as facts.

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

## Ops traps
- Guest kernel installs only to `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`; the host kernel is not rebuilt in this experiment.
- `herd` re-parses a conf only on `disable` then `enable`; the truth is `/var/run/herd/vm-dev.vm`. kvm0 is exclusive: `vm-serve-mc` stays stopped while `vm-dev` runs.
- At the end: stop `vm-dev` and the listener, restore both confs to `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4`, `herd disable vm-dev`, `herd start vm-serve-mc`.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after each script works, after each boot that taught you something, after each fix that builds, and after the plan. Never `origin`.
`taoxichuan` stays amd64-only and out of root `default-members`; pre-commit clippy and the unsafe-budget check must pass.

## When to stop
All three jobs are done, or about 120 tool calls, or three boots without new evidence, or the quota error. Then make sure `RESULT-E.md` has: the gate table, per change
*pulled / extracted / written new*, `wc -l taoxichuan/src/*.rs` before (4,608) and after, the leak answer, and the plan's path. End the turn.
