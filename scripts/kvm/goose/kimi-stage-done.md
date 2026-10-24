goose-glm's driver for the current stage has exited (the supervise loop saw it; you do not need to wait for
it). Do this stage-close work now, in this order, then end your turn with ONE line (the loop re-prompts you
when the NEXT stage finishes, so do not wait for anything):

1. `git fetch litter`; find the stage branch goose just finished (`git branch -r | grep litter/taoxichuan-s`,
   newest). Read its `RESULT.md` on the ryzen (read-only ssh, see "The ryzen box"). Trust a PASS; do not
   re-run its gate. If it says FAIL or stopped on a quota, write a short continuation brief for the same
   stage instead of the next one.
2. **Kick off the next stage first (step 3b).** Write `scripts/kvm/goose/taoxichuan-0N-<name>.md` for the next
   stage of the ladder (stage 3 = busybox: `ash` runs a script, `ls`/`cat`/`echo`/`sh` behave on the ext2
   root; stage 5 "no herd" is a constraint checked in 3/4), commit it, `git push origin`, and send it in the
   background: `nohup python3 -I scripts/goose_acp_prompt.py BRIEF --out /tmp/goose-sN.log --name sN > /dev/null 2>&1 &`.
   Its section 1 must tell goose to fetch `origin` and merge before starting AND again right before its
   final gate run.
3. Then validate the finished stage on aarch64 (step 4): merge `litter/taoxichuan-s<N-1>`, run
   `scripts/build_taoxichuan_fc.sh`, boot in Lima `fc`, apply the gate with the diskless control. Fix
   portability-sized breaks yourself (step 5). Also `cargo check -p <crate>` on the Mac host for every new
   crate under `crates/` that the stage added (`git diff --stat` shows them): the kernel feature set does not
   build them, and unguarded x86 `asm!`/`naked_asm!` is the usual break (it was `boot.s` in stage 1). Fix
   with `#[cfg(target_arch = "x86_64")]` on the asm item, as `akuma-boot-amd64` does.
4. Log it (step 6): agent-log section with who-built-what and the aarch64 evidence, plus goose's cost.
   `goose_box_stats.py` crashes on the box's old copy; run the fixed repo copy through ssh as base64:
   `B=$(base64 < scripts/kvm/goose_box_stats.py | tr -d '\n')` then on the box
   `echo $B | base64 -d > /tmp/gbs.py && python3 /tmp/gbs.py 2026-10-24`.
5. Commit everything (including any uncommitted edits to `scripts/kvm/goose/kimi-babysit.md`,
   `scripts/goose_acp_say.py`, `scripts/kvm/goose/supervise-loop.sh`, `scripts/kvm/goose/kimi-stage-done.md`,
   `scripts/kvm/goose_box_stats.py`) and `git push origin rust-org-il-experiment`.

Do not end your turn before the next stage's driver is launched in step 2. Keep your context small.
