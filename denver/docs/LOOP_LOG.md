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

NEXT GATE: PHASE-0-SCAFFOLD
