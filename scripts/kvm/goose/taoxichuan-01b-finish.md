# Taoxichuan stage 1, part 2: finish the stage and give the banner a skyline

You are goose-glm, the metal track of the experiment in
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Your last session got
Taoxichuan to the stage-1 gate (`init spawn`, banner, zero faults) and then your 5-hour quota ran
out before the paperwork. The user liked what you wrote. This prompt finishes stage 1; stage 2
is a separate prompt later - do not build ahead.

Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Checkout:
`/src/github.com/netoneko/akuma` ($AKUMA_SRC), branch `taoxichuan-s1` (your commits
`43edbffc`, `107eb25c`, already on `litter`).

## 1. Take the supervisor's fixes first

The supervising session merged your branch into `rust-org-il-experiment` on `origin` and fixed two
things you could not see from an x86 box, because its Mac is aarch64 and runs clippy over every crate:

- `crates/akuma-boot-amd64/src/lib.rs`: the `global_asm!` of `boot.s` is now
  `#[cfg(target_arch = "x86_64")]` (it is AT&T x86 asm and failed to compile on the Mac host), and the
  crate allows `clippy::too_long_first_doc_paragraph` and `clippy::must_use_candidate`, as
  `akuma-bkl`/`akuma-elf` do.

Get them: `git fetch origin rust-org-il-experiment && git merge --no-edit origin/rust-org-il-experiment`
(reading `origin` over HTTPS works from this box; do NOT push to `origin`, ever). Expect no conflicts.
From now on, keep any new `asm!`/`global_asm!`/port I/O in the new crates behind
`#[cfg(target_arch = "x86_64")]`, and keep the crates passing `cargo clippy -p <crate> -- -D warnings`.

## 2. The banner: keep the words, improve the picture

The user approved the text and tone of your banner ("a nice vibe"). **Keep every word of it
exactly as it is now** (the four text lines, "Taoxichuan 0.1.0 (Akuma/amd64 crates) x86_64 -- stage 1",
and the `Taoxichuan:` status lines). Change only the ASCII art around the text:

- Draw an old-timey Chinese skyline as it looked for the emperor's kilns at **Jingdezhen** (JDZ): a
  river in front (the Chang), hills behind, and along the bank the egg-shaped / gourd kilns
  (`hulu yao`, `zhenyao`) with tall chimneys breathing smoke, upturned-eave roofs, a pagoda or two,
  a few junks on the water. Kiln-fire glow is welcome. Think woodblock print in 7-bit characters.
- **ASCII only (bytes 0x20-0x7E and `\n`).** The host console relay turns anything else into `?`; that is
  why your last edit existed. Width at most 78 columns, at most 24 lines for the art.
- Source hygiene: write line breaks as `\n` escapes inside the string literals, not as literal newlines
  (your last edit left real newlines inside the `concat!` strings; it compiles, but fix it). Keep it
  one `const ART: &str = concat!(...)` and `console::puts(ART)`.
- Do not add dependencies or allocate for it. A static string is the whole feature.

## 3. Re-run the gate, by hand

The gate is unchanged: the guest reaches init spawn, prints the banner, and the console log has zero
`[Fault]`, `PANIC` or `[sig!]` lines. Use the dev-cycle pattern you used last time
(`docs/archive/AKUMA_KVM_DEV_CYCLE.md`): `kbuild -j 4 -p taoxichuan --features no-tests`, install to
`/boot/akuma-guest-dev` only (never `/boot/akuma-guest`), `herd stop vm-serve-mc` /
`herd enable vm-dev` / `herd start vm-dev`, read `/var/log/herd/vm-dev.log`, then
`herd stop vm-dev; herd disable vm-dev; herd start vm-serve-mc`. `vm-serve-mc` must be running when
you finish. Never `pkill` by name; one `kvm0` VM at a time.

The harness prints `[KVM] akuma-vm guest: FAIL (banner=yes, init=no, ...)` at the end. That is herd's
generic marker check and appears on clean passes; the stage-1 gate is the kernel's own
`Taoxichuan: stage 1 gate reached` line plus the zero-fault grep.

Also boot the main amd64 kernel once as a guest after the merge (`kbuild -j 4 --features no-tests`, the
same install/start steps; the guest root's `/etc/guest.sh` prints `AKUMA-GUEST-UP` ... `AKUMA-GUEST-DONE`),
to prove the merge did not break it.

## 4. Paperwork (the part that was left undone)

1. Save the final gate console log to `/root/dev-cycle/taoxichuan-s1/vm-dev-pass.log`.
2. Commit **early and often** on `taoxichuan-s1` (commit after each numbered step, not only at the
   end - last time the quota cut left an edit uncommitted and the supervisor had to recover it by
   hand) and `git push litter taoxichuan-s1` after each commit
   (over 2 minutes: kill it and `git bundle create /root/dev-cycle/taoxichuan-s1.bundle taoxichuan-s1`,
   and say so).
3. Append a dated `### Stage 1 - metal track (goose-glm), finish` section to
   `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`: what you changed, the final banner as
   printed, the gate evidence, anything surprising. Do not rewrite other sections.
4. Write `/root/dev-cycle/taoxichuan-s1/RESULT.md`: PASS/FAIL against the gate, branch + commit,
   how it was booted, and what you would tell the stage-2 prompt (what must exist for "a static musl
   hello runs, exit 0").

Ops rules unchanged: single vCPU, no pty layer, no users machinery, no herd in the minimal image; do
not touch the dirty submodules (`userspace/rumpkernel/src-netbsd`, `userspace/tcc/tinycc`) or the
untracked `root/`. You are now on `glm-5.3-flash`: be economical with context (read files in slices,
no whole-file dumps), because the plan's quota is the budget.

Work until the gate passes and the paperwork is committed and pushed; then end the turn.
