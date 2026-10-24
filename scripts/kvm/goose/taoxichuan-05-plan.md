# Taoxichuan: plan stage 6 (`apk add curl`) - PLANNING ONLY, write no kernel code

You are goose-glm, the metal track of the experiment in `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the
ryzen box. Stage 4 passed (`taoxichuan-s4` @ `29d4b562`): an ssh round trip into the Taoxichuan guest runs
`echo` and `cat` with byte-equal output. The owner wants the NEXT stage planned now, with the little quota left in this
window, and executed after the quota refills (about 02:39 IDT). **Your only deliverable is a plan document.**

**This is a short job: aim for 10-15 minutes and a small context.** Read selectively; do not read whole large files. Do not
edit any Rust, do not run `kbuild`, do not boot a VM (`vm-dev` is running for the owner to ssh into: leave it and
`vm-serve-mc` exactly as they are; do not run `herd` at all). Do not touch `/root/.config/goose`.

Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Checkout:
`/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`. Box log timestamps are UTC.

## Where the ladder stands

Stages 1-4 PASS. Stage 5 is a constraint, not a stage: init is kernel-spawned, no herd, no container machinery - the
stage-4 image already works that way, so say in one line whether anything is left to verify. **Stage 6 is the target:
`apk add curl` inside the guest** - the index fetch over the network succeeds, the package unpacks, `/etc/apk/world` is
updated, the `curl` binary is present and linked. The gate runs over the exec-only ssh path that stage 4 built (no pty,
no ash session: one command per connection, e.g. `/bin/ssh -p 2224 root@localhost 'apk add curl'`).

Facts from your own stage-4 RESULT.md (`/root/dev-cycle/taoxichuan-s4/RESULT.md`), which you should treat as the starting
point: outbound is untried (`Gateway::out_accept`/`out_refuse`, `vm_outbound`, `amd64/src/kvm/relay.rs`); DNS must be
decided (`akuma-net`'s `dns.rs` vs what the full-kernel guest uses); there is no wall clock (`utc_seconds` is `None`,
CLOCK_REALTIME is boot-relative; the boot-time SNTP client lives in `akuma-syscalls-time`); `writev`, `fstat`, `readlink`
and `uname`-class arms are missing; the 1 MiB mmap and brk slabs were never pressure-tested; `ls /bin` fails (stat and
directory support on that path); `[sc] unhandled nr 4415` repeats on the console and nobody has identified it.

## What the plan must contain (write it to `/root/dev-cycle/taoxichuan-s6/PLAN.md`, and a copy to `docs/archive/TAOXICHUAN_STAGE6_PLAN.md`)

1. **The syscall gap, measured not guessed.** `docs/archive/APK_MISSING_SYSCALLS.md` has a measured 48-syscall list for
   apk; diff it against the arms that `taoxichuan/src/main.rs` dispatches today and give the exact missing set, ordered by
   when apk first needs each one. Say what `nr 4415` could be and how you would find out.
2. **Filesystem needs.** apk writes: `/lib/apk/db`, `/etc/apk`, `/var/cache`, unpacking into `/usr`, `rename`, `unlink`,
   `symlink`, `fchmod`/`chown`, `fsync`, `statfs`/`flock`. What does the ext2 path in this kernel support today for
   writes, and what does the 16 MiB root image have to become (size, contents: the `apk` binary, `libc`, the Alpine
   keys, `/etc/apk/repositories`, a seed `world`)? Include how the image is built reproducibly.
3. **Network path outbound.** Whether the host relay can do outbound for this VM today; the exact `vm-dev.conf` keys;
   DNS (which resolver, which server address); TCP behaviour apk needs (many short connections, `connect` with a
   timeout, `getsockopt`, `poll`/`select`). HTTP vs HTTPS for the index (Alpine mirrors serve both: say which the gate
   uses and what TLS would add: entropy, wall clock for certificate validity, a CA bundle).
4. **Clock.** What `apk` and TLS need from `CLOCK_REALTIME`, and the cheapest correct way to give the guest the wall clock
   (the existing SNTP client, a value passed on the command line, or the host).
5. **Process model.** apk runs scripts (`.post-install`, triggers) and may `fork`/`execve`. The kernel runs spawned children
   to completion with no `fork`. State exactly where that breaks for `apk add curl` and the smallest change that works;
   do not propose a scheduler unless nothing smaller does.
6. **Milestones with mini-gates.** Split the stage into steps each ending in something the owner can see over ssh (for
   example: outbound TCP to the host works; DNS resolves a name; `apk --version` runs; `apk update` fetches the index; `apk add
   curl` unpacks; `curl --version` runs). For each: what to build, which files, the gate command, a rough count of
   boot cycles, and the risk.
7. **How the gate is verified against the host:** what reference output on a Linux or Akuma host the supervisor compares
   with (package file list, sizes, checksums), and the exact commands.
8. **Token and time estimate per milestone, and the order to run them in.** The window is about 41M input tokens and the
   cost per call grows with the context (a 220k-token context burned about 60% of a window per hour; a fresh 59k context
   a tenth of that). Propose session boundaries (where the owner should stop and restart goose with a fresh context) and
   which milestones fit one session. Name the cheapest first milestone to run when the quota refills.
9. **Unknowns and what would kill the plan.** At most five, each with the experiment that would settle it.

Keep it concrete and short (a page or two plus the tables). If a number is a guess, say so.

## Rules

- Portability: nothing in this job touches code, so there is nothing to gate; the plan itself should say which new code needs
  `#[cfg(target_arch = "x86_64")]` gating and must not add amd64-only crates to the root `default-members`.
- Commit the two plan files and `git push litter taoxichuan-s4`. Never push to `origin`. Do not touch the dirty submodules.
- End the turn when the plan is committed and pushed. The supervisor will read it and draft the execution brief.
