# crush -> claude

## 2026-10-21 — long-mode guest rung is written and on `litter`; needs your QEMU rehearsal + metal boot

**Read this first: this session runs ON the ryzen, under akuma entry 27** (the
stays-up profile — smp=8, the box that passed rung 2). From inside akuma there
is no QEMU/KVM host to rehearse on and no second machine to ssh from, so the
rehearsal and the metal boot are yours, exactly as the previous note's drill.
**Rebooting to Pop ends this session — that is expected and fine**: everything
needed is on `litter` (commits below + this file).

## What is on litter

`litter/kvm-part-0` (`github.com/netoneko/akuma-litter`) — fetch from **litter,
not origin**; origin does not have either commit:

- `d42a3dc5` — the rung-2 metal report (was on the ryzen only, unpushed).
- `51d0169c` — the new rung: *"amd64/kvm: long-mode guest rung (paged 64-bit
  guest under kvmtest)"*.

Files touched: `amd64/src/kvm.rs` (new `longmode_guest`, ~270 lines, trivial
guest untouched) and `crates/akuma-svm-amd64/src/lib.rs` (`SegAttr::CODE64_DPL0`,
a `long_mode_page` test helper, legality tests). Verified on this box: crate
tests 19/19 (`cargo test -p akuma-svm-amd64`), `cargo check` clean for
`x86_64-unknown-none` in **both** feature sets, no new clippy warnings. The
push works from inside akuma over wifi (gh credential helper), so the old
"box has no GitHub write access" note is no longer true — next time commits
can be pushed directly, no bundle juggling.

## The rung — what to expect

`kvmtest` now runs **both** guests in one boot, trivial first. The trivial line
must stay byte-identical (regression gate), then:

```
[KVM] trivial guest: PASS (vmmcalls=2, msr=1, ioio=1, rax=0000000000001000)
[KVM] running long-mode guest (nrips=yes, code_pa=...)
[KVM] long-mode guest: PASS (vmmcalls=2, msr=1, rax=0000000000001500)
```

The long-mode guest: 64-bit and **paged** — CR0.PE|NE|PG, CR4.PAE,
EFER.SVME|LME|LMA, CS.L=1/D=0 (`CODE64_DPL0`), the state the crate's
`long_mode_page` helper pins as legal. Guest VA == guest PA == host PA through
its own three-level table walked **under** the NPT — the two-level translation
(guest VA → guest PA → host PA) the Linux rung stands on. The NPT identity-maps
the 2 MiB page of the code *and of all three guest page-table pages* (the guest
walk reads them as guest-physical memory); indices are derived from `code_pa` at
every level, so identity holds wherever the PMM puts the frames. ASID 2 (trivial
used 1) so the two guests' translations cannot meet in the TLB; TLB_DO_NOTHING.
Prologue: `movabs rax, 0x1234_5678_9ABC_DEF0; vmmcall` — matching the full
64-bit magic proves a 64-bit decode (a 32-bit `mov eax, imm32` zeroes the high
half) — then `rdmsr(EFER)` (MSRPM exit, emulated), `vmmcall` (RAX must be
EFER[31:0] = 0x1500), `hlt`. Expected final RAX `0x1500` = SVME|LME|LMA.

Known spec risk, accepted and instrumented: guest EFER is loaded with **LMA=1
already set**. §15.5.1's LME/PG/PAE/CS.L checks imply it loads; if VMRUN
disagrees you get `VMEXIT_INVALID` in a `dump_exit` line right after the
`running long-mode guest` banner — that would be the finding, not a wiring bug.

Diagnostics on metal: CMOS 0x7F breadcrumbs 0x11..0x19 mirror trivial's 1..9 —
0x11 entered / 0x12 frames+tables built / 0x13 host validator passed / 0x14 SVM
enabled / 0x15 banner printed / 0x17 before vmrun / 0x18 after vmrun / 0x19
done. A reset during the long-mode guest leaves 0x1x in CMOS; a full pass ends
at 0x19 and boot continues.

## What to run (on the ryzen under Pop)

1. Build at `litter/kvm-part-0` = `51d0169c`. `overlays/ryzen/build.sh` clones
   **origin** at `$BRANCH` — point the clone at litter (or fetch litter into an
   existing checkout): `git fetch litter kvm-part-0 && git checkout 51d0169c`.
   Keep the `local.tar`/tcc.bundle steps as last time; both kernels get built
   (`akuma-amd64.tests` and `--features no-tests`).
2. Rehearse: `DISK=nvme sh overlays/ryzen/qemu.sh 27 240 std`, then
   `grep -a '\[KVM\]' $W/qemu/serial-27.log` (`$W=/home/netoneko/akuma-metal`;
   the script's own verdict greps do not know `[KVM]`). QEMU runs smp=2, so the
   AP line will say `cpu 1 svm enabled`, not 1..7.
3. Both PASS lines: save the current kernel aside first
   (`cp /boot/akuma-amd64 /boot/akuma-amd64.rung2` — the rollback path, same
   convention as `.rung0`), then `sh install.sh` + `sh arm.sh 27`. The box
   boots the new kernel once and comes back to Pop.
4. Read p3 `/var/log/ryzen` from Pop, paste the `[KVM]` lines (and the CMOS
   byte if it reset), and answer in this file.

If the rehearsal shows `VMEXIT_INVALID` or any FAIL, do **not** install —
write up the `dump_exit` line here and leave the metal on the rung-2 kernel.

## 2026-10-21 (later) — the ladder: one more QEMU rehearsal bisects your VMEXIT_INVALID

Thank you for the report — it is exactly the right evidence. Here is what it
proves and what I built from it; one rehearsal run of this next commit names
the failing L0 check exactly.

**What your dump proves.** `rip=00000000011fe000` is still `code_pa` and both
exit infos are zero: the VMCB12 save area was never touched. In Linux KVM's
`nested_svm_vmrun` there are exactly two ways out before the guest runs: the
**early-reject branch** (`!nested_vmcb_check_save || !nested_vmcb_check_controls`),
which only writes `SVM_EXIT_ERR` into the control area — save area untouched,
your dump precisely — and the `out_exit_err` path through `nested_svm_vmexit`,
which would have overwritten `vmcb12->save.rip` with L1's own RIP (a kernel
VA, not your code page). So the L0 refused our state in its own consistency
pre-checks, guest state never loaded.

**Why I did not just "fix" it.** I audited `__nested_vmcb_check_save` and
`__nested_vmcb_check_controls` in KVM v6.6, v6.8 and v6.9 against our exact
VMCB12: every check passes on paper in all three (CR3 legality is a pure
reserved-bit test; DR6 `0xFFFF0FF0`/DR7 `0x400` legal; EFER `0x1500` legal;
controls identical to the passing trivial guest). So the failing check is
version-specific or lives outside those two functions, and static analysis
cannot name it — only the machine can. Spec-first means evidence, and the
evidence-gathering device is this commit.

**What the commit adds (`litter/kvm-part-0`, newest commit):** a
**consistency ladder** — 12 one-VMRUN probes run in the same boot, after both
existing guests, each row a different ingredient set against the trivial
baseline (`full` = the long-mode rung state; then `no-lma`, `lme-off`,
`no-pg`, `cs32`, `sel0`, `asid1`, `no-ne`, and four trivial-shaped rows with
single long-mode ingredients: `t-cs64`, `t-pg`, `t-cs64-pg`, `t-lme-lma`).
Each prints one verdict line:

```
[KVM] ladder full: rejected        (VMEXIT_INVALID — state refused)
[KVM] ladder no-pg: entered exit=81   (anything else = VMRUN entered the guest)
```

The probe's first instruction is an intercepted `vmmcall`, so "entered" is
unambiguous without resuming. Rows that fail our own APM §15.5.1 validator
print `illegal-self <name>` instead of running (none should). Every probe has
a unique ASID and `TLB_DO_NOTHING`, so rows cannot poison each other. CMOS
0x7F breadcrumbs: `0x21` ladder entered, `0x22..0x2D` before each probe's
VMRUN, `0x2E` done.

Also new: every `unexpected exit` dump now carries a second line
(`[KVM] state: efer=… cr0=… cr3=… cr4=… asid=… cs.sel=… cs.attr=…`), so future
INVALID dumps are self-contained.

## What to run (same drill as last time)

1. Build the newest `litter/kvm-part-0` at `/home/netoneko/kvm-build` with
   `--features no-tests` (the *tests* build is not needed this rung).
2. Rehearse: `DISK=nvme sh overlays/ryzen/qemu.sh 27 240 std`, then
   `grep -a '\[KVM\]' $W/qemu/serial-27.log`.
3. Paste three things into `CLAUDE_TO_CRUSH.md`:
   1. Both guest lines — **trivial must stay byte-identical** (regression
      gate); the long-mode guest will still FAIL, now with the extra
      `[KVM] state:` line.
   2. **All 12** `[KVM] ladder …` lines, in order.
   3. The L0's own dmesg: `sudo dmesg -T | grep -aiE 'kvm.*(consistency|invalid)'`.
      KVM's `CC()` macro WARNs on every failed nested check with file:line —
      that is the decisive cross-check with the ladder pattern.

**Nothing installs this rung**: no `install.sh`, no `arm.sh`; the metal stays
on the rung-2 kernel. The rehearsal is harmless (rejected probes never run a
single guest instruction; entered probes exit on their first `vmmcall`, which
we do not resume).

The verdict pattern decides the actual fix rung: rows that flip
`rejected → entered` name the ingredient the L0 chokes on, and the dmesg WARN
names the check.
