# Denver loop log — Part II: one entry per pass

**Voice: Record.** What we did (brief), what decision it forced, precedent cited (R9). Part I
(the lessons) lives beside this file in `denver/docs/lessons/`; the frozen plan is
`denver/docs/archive/KERNEL_HOWTO_BUILDING_CUSTOM_PLATFORM_KERNEL.md`; the procedure is
`denver/docs/runbooks/AUTORESEARCH.md`.

### PHASE-0-SCAFFOLD — 2026-11-13, session 1 (goose, glm-5.3-flash)

**What we did.** Oriented: the gate needs `akuma-jvm-kernel/` + `denver/` over
`akuma-tokio-kernel` — and neither the crate nor the branch's plan-cited files
(`crates/akuma-tokio-kernel/README.md`, `shanghai/README.md`) exist on branch `denver`; the
whole proven stack (13 new + 14 updated crates, the root/metal pairing, `akuma-runtime-tokio`)
lives on sibling branch `shanghai` (tip `d4a6c7d9`, 240 commits since merge-base `cf687e25`),
six gates green on this very machine (its `RESULT.md`). Research artifact first (R1):
`denver/docs/research/PHASE-0-SCAFFOLD.md`, decisions D1–D5. Then: merged `shanghai` into
`denver` (kept denver's planning docs; submodule pointers took shanghai's), added
`akuma-jvm-kernel` (1:1 feature-map pass-through, zero capabilities, `forbid(unsafe_code)`, the
linker-script relay one hop up) and `denver` (the README's three-file kernel, package name
`denver` = `uname -n`), root `members` += both, driver staging learned the root manifests (D5).
Builds green cold: `denver` 22.9s, `akuma-amd64` 38.0s, `taoxichuan` 15.8s — all
`x86_64-unknown-none` release, against the merged state, no code fixes needed anywhere
(composition, not porting: R2 paid off exactly as advertised). Boot probe: **blocked, escalated**
— the host was not booted with `kvmctl`, `/dev/kvm0` is ENOENT, no QEMU/Firecracker-viable
alternative exists on the box, and a reboot is beyond the plan's authority (the session runs on
the machine it would reboot). Escalation filed: `denver/ESCALATE.md`; capture:
`denver/gates/PHASE-0-SCAFFOLD.log`.

**What decision it forced.**
1. *Platform acquisition is a branch merge, not a port* — decided by the plan's own citations
   plus R2/R5 (a partial sync is fork-shaped); precedent: `proposals/B15_TOKIO_KERNEL_KIT.md`
   (the extraction these crates implement), shanghai `RESULT.md` (proof on this hardware).
2. *`akuma-jvm-kernel` is a pure pass-through* — decided by its own precedent one level down
   (`akuma-tokio-kernel`'s feature map onto its machine half; plan lesson 3: "the way `console`
   propagates today"); R6 keeps it off-by-default everywhere.
3. *The denver kernel is three files + a banner* — precedent: the README's `osaka` pattern;
   `shanghai/src/main.rs` (18 lines). Zero kernel code of our own.
4. *Rig and images live outside the tree* (`/root/denver/w/`) — precedent: shanghai `RESULT.md`.
5. *Driver staging must carry the root manifests* — a members-line gate commit without
   `Cargo.toml`/`Cargo.lock` is a tree that cannot build its own kernel; machinery precedent:
   the work log's driver-fix entries. Fixed in `denver/autoresearch.sh`, logged there too.
6. *Machine facts recorded as constraints, not fought*: the box's toolchain is standalone
   `/usr/local/rust` (1.101.0-nightly; no rustup, so `rust-toolchain.toml`'s pin is not
   enforced here) and carries no `aarch64-unknown-none` std — the aarch64 default build was
   never runnable on this host; the checkable R6 surface here is the x86_64 + host surface,
   which is green. Recorded in `RESULT.md` and the work log; nobody patched around it.

**Gate state:** NOT GREEN (build half proven, boot half pending the host reboot). NEXT GATE stays
on this gate until `RESULT PASS` appears in `denver/gates/PHASE-0-SCAFFOLD.log`.

### PHASE-0-SCAFFOLD — 2026-11-13, session 2 (goose, glm-5.3-flash; the redo pass)

**What we did.** Took the answered escalation (`ESCALATE_ANSWERED=1`; archive:
`denver/docs/archive/escalations/2026-11-13-PHASE-0-SCAFFOLD.md`): the human had rebooted the
ryzen into the kvmctl-armed kernel (GRUB entry 29, `kvmctl kvmmem=4096` — exactly the remedy
session 1 named), run the probe by hand over ssh, captured `RESULT PASS` into the gate log, and
committed it with the rig script as WIP `b0f1e325`. Research addendum first (R1): decisions
D6–D8 in `denver/docs/research/PHASE-0-SCAFFOLD.md`. Then the session's own first-hand rerun:
`sh denver/vm-probe.sh 2>&1 | tee -a denver/gates/PHASE-0-SCAFFOLD.log` — slot freed, guest
booted, ssh round trip answered `Linux denver 0.1.0-8dc2dbcc-denver`, tokioprobe
`RUNTIME PASS` (4 workers) / `UNAME PASS` / `HTTPS PASS` / `RESULT PASS`, guest stopped. One
machinery fix: the escalation archive step now never overwrites an earlier archive (work log,
finding 5). **Gate GREEN**; phase 0's done-when holds: Denver boots tokio, `uname` says denver.

**What decision it forced.**
1. *Accept operator-supplied machine state, then re-prove it first-hand* (D6) — the hand-run
   capture stands as evidence; the session's own capture proves the rig still green after it,
   so the gate's proof is not borrowed.
2. *Hidden device nodes are checked by opening, never by listing* (D7) — kvm.md says `/dev/kvm0`
   is invisible to `ls /dev` by design; this session initially re-read a bare `ls` ENOENT as
   "still not armed" and was corrected by the file evidence (`vmrun.log` carrying live guest
   cpuid/DNS lines). Lesson L3 records it; the check is the open or vm-run's own error text.
3. *The phase line stops the loop* — phase 0 ends with one recorded human intervention (the
   reboot, session 1), and the runbook's transition rule says a phase that needed interventions
   stops at its summary for human review ("autonomy is earned per phase"). Filed
   `denver/ESCALATE.md` (QUESTION: review the phase-0 summary); `NEXT GATE` stays on this gate
   with the recorded reason that it is already green — runbook step 2's "unless a recorded
   reason says otherwise" sends the next session to phase 1 (lessons 1/2/4) after the review.

**Gate state:** GREEN (`RESULT PASS` in `denver/gates/PHASE-0-SCAFFOLD.log`, first-hand session
capture, hand-run capture preserved above it). Phase 0 summary written in `denver/RESULT.md`;
loop stopped at the phase line for human review.

NEXT GATE: PHASE-0-SCAFFOLD
