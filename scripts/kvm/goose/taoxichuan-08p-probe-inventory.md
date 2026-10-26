# Taoxichuan stage 8, session P: RECON - the probe and test inventory for the guest (reading only: no boots, no kernel code)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: O (`RESULT-O.md`). **Keep your context small. This session boots nothing, changes no kernel or crate code, and produces one document.**
Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4` (fetch `litter` first if behind). Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
Every build session so far has rediscovered, by reading, which probes, suites and tests already exist for the capability it is working on, and about half of the briefs carried a premise that turned out wrong (a probe that builds only for aarch64, an applet missing from the image, a syscall assumed served). Reading was 73% of the carried context cost of the most expensive session.
This session builds the thing that stops that: **`docs/archive/TAOXICHUAN_PROBE_INVENTORY.md`**, a map from "what I need to check" to "the probe, suite, host test or gate that already checks it, and exactly how to run it here". Later briefs will cite it instead of making goose search.

## What to produce (aim for under 220 lines; tables, not prose)
1. **Guest-runnable probes**: for each of `userspace/abiprobe`, `epollprobe`, `memprobe`, `futexprobe`, `ncaprobe`, `ext2probe`, `nettest`, `userspace/forktest/c_stress/` (list its members by name in groups), and anything else under `userspace/` that is a probe: what it checks (the syscalls or behaviours), the build command, **whether it builds for x86_64** (the abi probe's `build.sh` uses an aarch64 compiler - check each; a probe that does not build for the guest is marked so), where its output goes, what a pass looks like, and whether a Linux reference run exists (`--linux` flags, `scripts/*_suite.py`).
2. **Suites and harnesses in `scripts/`**: `futex_suite.py`, `epoll_suite.py`, `mem_suite.py`, `scripts/benchmarks/*`, `ssh_harness.py`, the `scripts/kvm/goose/` gate scripts (`run_gates.sh` groups and gates by ID with what each asserts), `boot_dev.sh`: what each is for and whether it can run against the Taoxichuan guest on this box.
3. **Host tests that cover logic Taoxichuan consumes**: for each `akuma-*` crate in `taoxichuan/Cargo.toml` (19 plus `akuma-elf-exec` if present), the test command (`cargo test -p <crate> --target $HOST`) and a one-line description of what the tests pin. Run the cheapest few to confirm the command works here and say how long it took.
4. **Syscall coverage map**: a table of the Linux x86_64 syscalls the guest dispatches today (from the `ring3::nr::` arms in `taoxichuan/src`; one grep), the ones that fall to the unhandled arm that real programs hit (take them from the `RESULT-*.md` files on the box: `grep -l "unhandled" /root/dev-cycle/taoxichuan-s7/RESULT-*.md`), and for each the probe that exercises it, if any. Mark "no probe" explicitly: those are the gaps a future session should fill first.
5. **Reference-behaviour recipes**: how to measure what a correct answer looks like without guessing: run the same static binary on the full amd64 kernel on this box; run a dynamic binary through `/lib/ld-musl-x86_64.so.1 --library-path ...` against `/tmp/refroot` (the reference root); use `refroot.sh`; compare with a Linux run. For each recipe: the exact command, what it was verified to print on 2026-10-26 (run it once; paste the first line), and when to use it.
6. **Guest facts and traps** (one short list, each with the command that proves it): no fork (`sh -c 'a; b'` runs only `b`), no procfs (`df`, `ps` print only a header), no `stat` applet, one core, the forward ceiling near 30 connections per boot, the 1 GiB image, herd rotates `vm-dev.log`, a refused outbound dial hangs the guest. Verify each by reading the earlier RESULT files, not from memory; mark any you could not verify.

## Premises (examine these first; write one line per row in RESULT-P.md: HOLDS / WRONG (what you saw) / UNCHECKED)
| # | premise | read-only check |
|---|---|---|
| 1 | the probes live in `userspace/` and are listed in `docs/reference/userspace-layout.md` | `ls userspace | head -60`; `grep -c . docs/reference/userspace-layout.md` |
| 2 | `abiprobe`'s `build.sh` builds for aarch64 only | `grep -n "gcc\|target" userspace/abiprobe/c/build.sh | head` |
| 3 | the futex/epoll/mem suites run the same binary on Linux and Akuma | `sed -n 1,30p scripts/futex_suite.py | cut -c1-160` |
| 4 | `cargo test -p akuma-fdtable --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)` passes here | run it, note the seconds |
| 5 | the guest has no `stat` applet and no procfs | `grep -n "stat\|procfs" /root/dev-cycle/taoxichuan-s7/RESULT-K.md /root/dev-cycle/taoxichuan-s7/RESULT-M.md | head` |
| 6 | `/tmp/refroot` and `apk.static.guest` exist on the box | `ls /tmp/refroot/usr/bin/curl /root/dev-cycle/taoxichuan-s8/apk.static.guest` |

## Read first (`sed -n` windows of 60 lines or less, never whole files)
`docs/reference/userspace-layout.md`; `docs/README.md` (symptom matrix) only for pointers to probe docs; the heads of the probe sources and scripts named above (`sed -n 1,40p`, comments only); `scripts/kvm/goose/run_gates.sh` head comments; `scripts/kvm/goose/BRIEF_TEMPLATE.md` (so the inventory answers what it asks for).

## Composition rule
Nothing to compose: this session writes no code. If you find a probe that should be a crate's host test instead, or two probes that duplicate each other, **record it in the inventory under "Observations"** and do not change either.

## Cost rules
Cap every read (`| cut -c1-180 | head -60`, `grep -n -m 20`); never `cat` a source file; write the inventory file early and refine it so a lost context loses nothing. Expect 60-90 tool calls and about 2M input tokens; at 100 calls write what you have and stop.

## Gates
- `docs/archive/TAOXICHUAN_PROBE_INVENTORY.md` exists with sections 1-6, every build and run command either run once here or marked "unrun", every path checked to exist (`ls`).
- Zero boots, zero source changes (`git status` shows only the inventory and, if you wrote any, small helper scripts under `scripts/kvm/goose/`).
- Out of scope: running any VM, changing the gate scripts, fixing anything you find.

## Ops traps
- Do not start, stop or restart any VM or herd service; do not touch `/boot`.
- **Never run `python3 -`, `python3 <<EOF` or any python fed from stdin on this box.** Never run goose/crush wrappers on the box.

## Commit rule (you have no clock)
Commit the inventory and `git push litter taoxichuan-s4` when sections 1-3 exist and again at the end. Never push `origin`.

## When to stop
All six sections written and checked, or 100 tool calls, or the quota error. Then write `/root/dev-cycle/taoxichuan-s7/RESULT-P.md` (the premise rows, the five biggest gaps in probe coverage, and the three probes you would run first for each of: the process model, the dynamic linker, TLS) and end the turn.
