# Future work and unresolved items

*2026-11-16 (curated section last pruned 2026-11-19, branch `box0-followup`). A single place to collect what the tree, its documents and its comments say is left
undone. **Nothing here is verified**: an entry is a claim somebody made, and it stays until someone reproduces the problem
or finds the commit that fixed it. Then it is deleted from the curated list below, or (for the generated inventory) simply
disappears when the sentence is edited or removed.*

## How this file is organised

1. **Curated** (hand-written, next): the work the type-system implementation left open, the bypass removal, and problems
   met along the way. Each has what, why, and the check that would close it.
2. **Inventory** (generated, last): every sentence in the Markdown docs and every `TODO`/`FIXME` in Rust that uses
   the words authors write when they leave something undone. Regenerate with `scripts/future_work_scan.py`
   (`--check` says whether it is stale). Grep-level: it misses work described in other words and includes some uses of
   "deferred"/"follow-up" in an unrelated sense. A hit flagged `maybe resolved` also says fixed/done/resolved; that is a hint
   about where to look first, not a verdict.

The workflow (scan, triage, close or promote) is written up in [`docs/runbooks/future-work-scan.md`](../docs/runbooks/future-work-scan.md).

**Closing an item:** name the commit or the test that shows it fixed, then delete the entry. An entry whose claim you could
not reproduce is closed as "not reproducible, <date>", not deleted silently.

## Curated

### From the type-system work (`docs/reference/safety-types.md`, `docs/archive/TYPE_SYSTEM_SAFETY_REPORT.md`)

| # | Item | Why it matters | What closes it |
|---|---|---|---|
| T4 | `ThreadRef::with_live` is in (2026-11-17) and used at every effect on a recorded thread, the grace-expiry and single-core hard terminates, and `tkill`. **Open:** it has only run on host threads and on the QEMU/KVM boot suites, not on real multi-core hardware; and the `with_live` contract (short, non-blocking, takes no lock a slot claimant holds) is documented, not enforced by the type | a violating closure would make a rebirth spin | a stress test on real SMP, or a debug-build check that a section does not exceed a bound |
| T6 | Static spinlocks: classified, and the interrupt-reachable ones are `IrqSpinlock` (2026-11-17: `docs/reference/spinlock-classification.md`, `scripts/spinlock_audit.py`, `scripts/spinlock_irq_reach.py`; migrated `ALARM_QUEUE`, `KEYS` and 8 always-masked locks with their wrappers removed). **Open:** 19 always-masked locks with larger wrappers and `PROCESS_CHANNELS` (generic helper) are not migrated (hygiene, none interrupt-reachable by the analysis); the reachability analysis is name-based and cannot see function-pointer calls, so its 9 are a minimum; `KEYS` is build-tested only | a missed hook call would hide a reachable lock | per-lock reading of the big wrappers; a call-graph tool that resolves the `runtime()` hook tables |
| T10 | The behavioural isolation probe exists for amd64 (`userspace/amd64/isoprobe`, `scripts/run_isolation_probe.sh`, 2026-11-17: COW allowed, COW isolated, unrelated process `SIGSEGV`, kernel address `SIGSEGV`, plus a harness-sanity step). **Open:** an aarch64 counterpart (the probe is x86_64 raw syscalls; no aarch64 userspace target is installed here); it is not wired into the boot suite or any CI (it needs the disk image and `/bin/isoprobe`); the unrelated-process check was shown able to fail only by its harness-sanity step, because the one mutation aimed at it (aliasing PML4 slot 0) crashed the kernel before a verdict | the probe runs on amd64 only and on demand | an aarch64 probe, and a mutation that leaks a mapping into an exec'd peer without breaking the kernel |
| T12 | The report's "not worth doing" list is a decision, not an oversight: typed ids everywhere, `TidSlot` alone, lock-order types, may-allocate/may-sleep tokens, typed PTE flags (already planned), a canonical-path VFS type | recorded so nobody reopens them without a new incident | an incident that motivates one |

### Delete the self-test validation bypass (`BYPASS_VALIDATION`) — option 2

Done: sealed before init (audit M7, `ed65401d`), and compiled out of `no-tests` images (`akuma-user-access` `no-tests`
feature, `build.rs`).

Left: the flag and `BypassValidationGuard` still exist in the default image because ~125 boot self-tests pass kernel-stack
buffers as user pointers (`process_tests.rs` 69, `sync_tests.rs` 49, `pthread_tests.rs` 2, `tests.rs` 1, the amd64 suite 2,
and four self-tests inside `akuma-syscalls-glue`). To delete it each test needs a real user mapping in a test process
(a `with_user_buffer(|va| ...)` helper) so it exercises real validation. Some need redesign (the futex test with two
syscalls in one window). Do it a test file at a time; when the last user is gone, delete the flag, the guard, `seal_bypass`
and the `kernel_tests` plumbing in `akuma-user-access`. **What a conversion needs (found 2026-11-17 by reading, not by trying):** validation asks whether the range is mapped in the
*currently active* user address space (`is_current_user_range_mapped`), so a test that maps a page for itself must also make
that address space the active one while the syscall body runs (and restore the boot one after), on aarch64 `TTBR0_EL1` and on
amd64 `CR3`. So the helper is `with_user_pages(|va| ...)`: create a `UserAddressSpace`, map and zero the pages, activate it,
run the closure, deactivate, free. The tests then copy their structures in and out through `va` instead of passing a
kernel-stack address. `pthread_tests.rs` alone has about 20 `set_bypass(true/false)` pairs (signal sets, `sigaltstack`, timespecs
passed from the stack); `process_tests.rs` 69 and `sync_tests.rs` 49 uses are mostly `BypassValidationGuard`. A kernel thread
running the suite with a user address space active has not been tried here; the kernel's own identity map is shared into every
user space on aarch64, which is why it is expected to work. **Closes when** `grep -rn BYPASS_VALIDATION crates src` is empty.

### Kernel convergence (`docs/archive/KERNEL_CONVERGENCE.md`)

The main and tokio kernel families share most crates and duplicate the rest; the gate keeps finding the same bug twice.
Read the document for the inventory and the order. Open items that came out of this branch:

| # | Item | Why it matters | What closes it |
|---|---|---|---|
| C1 | Three copies of the socket wait hooks (`akuma-kernel-amd64/src/net.rs`, `akuma-kernel-glue`, `akuma-tokio-kernel-core/src/net.rs`) | the tokio copy lacked a rule the main one had (`TOKIO_KERNELS_LOOPBACK_STARVATION.md`) | one shared implementation behind a machine trait |
| C2 | `NetMachine::can_park` is `false` on the AArch64 platform kernels (taoxichuan, shanghai, denver): a blocking `accept` there still pumps and halts | the same cross-thread hang is expected; not booted because it needs the Mac | set it, boot `scripts/gate.py`'s `gateprobe` on the aarch64 kernels |
| C3 | The tokio kernels' spawn ring is 48 slots (`NSLOTS`); `clone` past it is `EAGAIN` | JVM and tokio blocking pools want hundreds; gate `threads_128` is an expected failure | the ring sized by feature, or the main kernel's thread table |
| C4 | The main amd64 kernel's `tcp_loopback` timed out once after the spawn checks (listed `flaky:` in `scripts/gate/xfail.txt`) | a hang the in-kernel suite does not see | reproduce under the gate, find the thread that does not run |
| C5 | Taoxichuan and Shanghai have no boxes (`current_box_id` is 0); Denver has them (`docs/archive/DENVER_BOX_WIRING.md`), but a writable `MAP_SHARED` of a boxed file is `ENODEV`, and seccomp and private namespaces in a Denver box are untested | the ecosystem demo and nested boxes need them | convergence step 4, or the demo uses the main kernel or Denver |
| C6 | The gate boots amd64 only | aarch64 kernels are built, not booted | an aarch64 rig that runs under the budget (the Mac, or KVM on an aarch64 host) |

### Ecosystem demo, stage 1 (`docs/archive/AKUMA_ECOSYSTEM_DEMO.md`)

Found while building and verifying the three images on the main amd64 kernel (2026-11-19). Not fixed on this branch.

| # | Item | Why it matters | What closes it |
|---|---|---|---|
| E1 | **`box run` / `box use` / `box open` child output never reaches an ssh exec session** (not with `-tt` either, not through a pipe or a file redirect of `box` itself, not on the serial console). The container runs and its files appear in `containers/<id>/upper`; only its stdout/stderr is lost. `box`'s own `print()` lines do arrive. `scripts/box_amd64_suite.py` expects the output, so this is a regression or an environment difference, not bisected | an agent's launcher or a harness cannot read what a boxed command printed; `eco-box` works around it by writing to a file in the box and reading it from the host | bisect against the suite on `headless` (the `sshd` pty change `53e06880` is the first suspect), then delete `eco-box`'s file capture |
| E2 | A path **through a relative directory symlink inside an overlay box** does not resolve (`cd /opt/scala/bin` ENOENT where `/opt/scala -> scala3-3.3.6`; the real path works). Plain-filesystem symlinks were not tested | scala's launcher scripts `cd "$(dirname $0)/.."`; any image built from a tarball with such links breaks silently | a box probe in `scripts/box_amd64_suite.py`, then the overlay resolver; the image build avoids the link |
| E3 | Firecracker guests have no outbound route (QEMU guests use SLIRP since 2026-11-19) | agents need model APIs | wire Firecracker (tap + MMDS) in stage 3 |
| E4 | The guest disk image is **not byte-reproducible** (the root layers are): `mkfs.ext2 -d` copies each file's host ctime/atime into the inode. `scripts/gate`-style identity is the layer digest | "rebuilt twice, same digest" holds for the roots only | build the ext2 image with `debugfs` and fixed times, or declare the layer digest the image identity |
| E6 | The kernel logs `peak live user threads` only at high-water steps; the capacity number in the demo doc is a lower bound | the 448-thread headroom claim is unmeasured at the top | a `/proc` or syscall counter for peak threads |
| E7b | **Linux-style `execve` from a thread** (`de_thread` + leader transfer): kill the other threads and the leader, the caller takes over the leader's pid, `Process` and wait identity, the new program keeps the pid a parent waits on | the detach fix (`docs/archive/EXEC_FROM_THREAD_DETACH.md`) changes pids and wait semantics | `stop_siblings` for a non-leader (everyone but the caller, the leader included); leader transfer across the identity caches (`THREAD_IDENTITY`, `THREAD_PID_MAP`, child channels); gate probe from `execthread`; check the AArch64 exec path's `stop_siblings` too |
| E8 | The kernel's `uname` sysname is `Akuma`, so the JVM reports `os.name=Akuma` and coursier's directories library (scala-cli) throws `UnsupportedOperatingSystemException`. Worked around in the image with `-Dos.name=Linux` in the scala-cli launcher | any JVM tool that switches on `os.name` fails the same way | decide whether sysname should be `Linux` for ABI-compat (musl/glibc ignore it; JVM tools do not) |
| E9 | **Intermittent `cargo build` failure: a rlib rustc just wrote is not found** (`error: extern location for gitserver_http does not exist: .../deps/libgitserver_http-....rlib`) in a box on the main amd64 kernel. 2 of 6 stage-2 runs, retry succeeds; same `cargo build --release --locked` otherwise passes in ~5 min | an agent's first build can fail for no reason in its code | a file-visibility probe (create, rename, hard-link, stat across threads/processes in an overlay box) |

## Inventory

<!-- BEGIN GENERATED -->

Generated by `scripts/future_work_scan.py`: **1788 hits in 552 files** (303 outside `docs/archive`, 1485 inside; 378 also say fixed/done/resolved and are flagged `maybe resolved`). All **unverified**.

## Inventory: Live docs, runbooks, scripts and code comments

### CLAUDE.md (2)

- L547: incident (symptom, cause, fix, how it was verified, what is still open) in a new
- L587: scripts/build_devbox.sh && overlays/devbox/run.sh # rump devbox (deferred; needs RUMP_NIC=1)

### PLAN-pn.md (2)

- L98: 'to_aarch64()'-style numbers — then fits for every family where the remaining work is
- L216: / **PK2 + rare boot stall still open** (gaps G1/G2) / more slots + more waits stress the open items / sequence the gates so a stall is attributable: S4 lands after pm's …

### acceptance/archive/03_two_vms_agent_workflow.md (1)

- L41: 6. Prints next steps and waits

### acceptance/archive/12_multikernel_demo.md (2)

- L179: for TLS); a shared '(offset,len)' bounce arena is the tracked §16 throughput follow-up.
- L205: open connection blocks new logins until reboot — connection-teardown robustness is future work.)

### crates/akuma-exec/docs/CONTAINER_ELF_LOADING.md (1)

- L40: ## Current limitations and future work

### crates/akuma-ext2/README.md (13)

- L35: - [Deferred inode frees](#deferred-inode-frees)
- L114: / 'Ext2Filesystem&lt;B: BlockDevice&gt;' / the public handle; owns 'dev', 'state', 'block_cache', 'deferred', 'write_lock_owner', 'time_fn' /
- L115: / 'Ext2State' / parsed superblock + geometry, behind a 'RecoverableCell'; also the in-memory 'bgd_cache' / 'bgd_dirty' / 'sb_dirty' for deferred metadata writeback (see …
- L120: ### Deferred metadata writeback
- L228: deferred to 'flush_meta' (end of every mutating op), 'sync()', or eviction of a
- L385: ## Deferred inode frees
- L408: yes -&gt; deferred.push(n); keep i_size and block pointers; return
- L550: deferred to dirty-victim eviction, 'flush_meta' (end of every mutating op) or
- L572: deferred to 'flush_meta'. Big read reduction; the write reduction only shows
- L671: (fast + slow), rename, 'read_at' boundaries, deferred frees ('deferred_free_len',
- L729: - No journal. 'sync()' flushes deferred superblock/BGD/bitmap metadata but there
- L732: flush) — deferred metadata still reaches disk because every mutating method
- L741: - ['docs/archive/SELFHOST_ZERO_PAGE_HUNT.md'](../../docs/archive/SELFHOST_ZERO_PAGE_HUNT.md) §14–15 — why deferred frees and 'on_inode_freed' exist

### crates/akuma-isolation/docs/IMPLEMENTATION_DETAILS.md (1)

- L132: ## Limitations and Future Work

### crates/akuma-kernel-amd64/README.md (2)

- L234: registers read off the box; the controller-touching driver is the next step and
- L343: bring-up ('akuma-net-rtl8169', already host-tested) is the next step.

### crates/akuma-kernel-glue/src/rump_proxy.rs (1)

- L230: /// TODO (channel-wiring trigger): currently detected by path-match in

### crates/akuma-kernel-tests-aarch64/src/process_tests.rs (1)

- L799: // TODO: Rework these tests to use mock thread IDs &gt;= MAX_THREADS.

### crates/akuma-middleware/docs/middleware.md (1)

- L415: while rump is deferred.

### crates/akuma-vfs-glue/src/proc.rs (1)

- L14: //! # TODO: Rewrite without allocations

### denver/RESULT.md (5)

- L293: 'os::init_2' cannot be unblocked by closing it); G4 (maps) third; G5 deferred.
- L660: ('userspace/nca/native-cli-ai') into the WIP commit; untracked in a follow-up
- L762: SET_BOX_STACK=324) is the D58 two-table lesson made concrete. G5 stays deferred.
- L998: **Standing state at close.** GREEN: G1–G4, G6–G13, G15. Deferred: G5 (vDSO/vvar fast path —
- L999: a perf pass, not an init blocker). Open: none — G14 was CLOSED post-experiment (commit  _(maybe resolved)_

### denver/docs/LOOP_LOG.md (10)

- L168: diagnostics-before-bugs rule and the attempt doc's own next step ("the fault handler
- L175: composition hole (R2), filed for the G2/PROCFS sessions' research, not fixed here.  _(maybe resolved)_
- L210: / research first (R1) / 'denver/docs/research/PHASE-2-FAULTNAME.md' (Finding 0, D17–D20) / precedents: the attempt doc's own next step — "a fault report that names the m…
- L573: times (a placer defect, recorded not fixed, D51). The real root cause (D49): the WX  _(maybe resolved)_
- L635: by 'git add -A' (untracked in a follow-up commit), and attempt-1's totalram extraction
- L679: per HEAPRO/MEMINFO's standing record. G2 **closed**; G5 stays deferred; phase 3 opens.  _(maybe resolved)_
- L715: '-version', a candidate blocker for anything that resolves paths. G5 stays deferred. The D58
- L762: read through the same pipe arm. G5 stays deferred per its own recorded condition ("revisit with
- L845: fix inside the existing 'fault-signal' feature, default build untouched) → G5 (deferred,
- L881: autoresearch experiment concluded 2026-11-14. G5 stays deferred (perf pass); the suspects

### denver/docs/REQUIREMENTS.md (15)

- L32: / G10 / ~~'java -version''s fate unreadable — the capture problem: three gates (HEAPRO→MEMINFO→GETCPU) recorded "no fault, no exit, output unrecoverable over ssh"~~ **CL…  _(maybe resolved)_
- L34: round-trip cost — measured, deferred (G5). (The NULL-store-after-CodeCache suspect left
- L45: first — the biggest unknown), G5 (deferred).
- L53: attempt doc's declared next step was exactly this diagnostic ("a fault report that names the
- L70: (PHASE-2-PROCFS) → G5 (deferred; its vDSO consumer now exists — musl's resolver — so a
- L83: serving the fallback) → G2 (PHASE-2-GETCPU) → G4 (PHASE-2-PROCFS) → G5 (deferred).**
- L85: **Re-rank 2026-11-14, after the operator merge verification (recorded reason, as this file requires):** the merge (3531cf69) moved the workload again, and this time the …  _(maybe resolved)_
- L87: **Re-rank 2026-11-14, after PHASE-2-NULLSTORE GREEN (recorded reason, as this file requires):** G4 closed (PROCFS), and G9's NULL store is no longer just *named* — the m…  _(maybe resolved)_
- L101: G2 behind it; G5 deferred. Recorded as suspects, unranked (D51): the slab placer's
- L105: **Re-rank 2026-11-14, after PHASE-2-GETCPU GREEN (recorded reason, as this file requires):** G2 is CLOSED — the tokio lineage had never grown the taoxichuan precedent's …  _(maybe resolved)_
- L107: **Re-rank 2026-11-14, after PHASE-2-MEMINFO GREEN (recorded reason, as this file requires):** G7 is CLOSED — the tokio lineage had simply never grown the metal kernel's …  _(maybe resolved)_
- L109: **Re-rank 2026-11-14, after PHASE-3-JAVAVERSION GREEN (recorded reason, as this file requires):** G10 is CLOSED — and the closure is a correction, not a fix: the phase-2…  _(maybe resolved)_
- L111: **Re-rank 2026-11-14, after PHASE-3-JAVAWORKLOAD GREEN (recorded reason, as this file requires):** the second rung is climbed — G11 CLOSED. 'denver/gates/PHASE-3-JAVAWOR…  _(maybe resolved)_
- L113: **Re-rank 2026-11-14, after PHASE-3-JAVACLASSLIB GREEN (recorded reason, as this file requires) — the OPERATOR PRIORITY CHANGE:** G12 is CLOSED — 'denver/gates/PHASE-3-J…  _(maybe resolved)_
- L115: **Re-rank 2026-11-14, after PHASE-3-JAVAC GREEN (recorded reason, as this file requires):** G13 is CLOSED (row above) — the fault-signal port built in session 18 needed …  _(maybe resolved)_

### denver/docs/archive/AUTORESEARCH_EXPERIMENT_RESULTS.md (3)

- L181: Not done, not scheduled; written down so the next person (or the loop, after the quota resets) can pick them up.  _(maybe resolved)_
- L309: - Not done: no loop gate/LOOP_LOG entry for PHASE-3-JAVAC, '-Xint' arm not run, default-build '.text' unchanged  _(maybe resolved)_
- L332: Still open: 'sigprobe' 'rax' edit (unexplained), the loop's own javac gate, the R6 default-'.text' check for the

### denver/docs/archive/AUTORESEARCH_WORK_LOG.md (3)

- L133: x86_64 + host surface. Not fixed, because nothing is broken: recorded as a machine fact.  _(maybe resolved)_
- L181: - **Open work left in the tree, noted not committed.** The working tree carries uncommitted
- L193: '180caf82' — probe-binary bug, PHASE-4-SIGRAX moot); G5 deferred (perf pass); suspects

### denver/docs/archive/CLOJURE_AND_JAVAC_ON_DENVER.md (1)

- L64: ## What is still open

### denver/docs/lessons/L1-collecting-requirements.md (2)

- L52: (diagnostics), G5 time cost (deferred). Decisions: D11 (A/B both kernels). Zero interventions.
- L58: (diagnostics), G5 time cost (deferred). Decisions: D11 (A/B both kernels). Zero interventions.

### denver/docs/lessons/L6-find-the-feature-flag.md (2)

- L3: **Motivating archive docs:** 'CLOJURE_AND_JAVAC_ON_DENVER.md' (the HTTP server, "What is still open"),
- L41: later", and the next step proposed was to write the syscalls. Interactive ssh was worked around for hours

### denver/docs/research/PHASE-2-FAULTNAME.md (1)

- L59: ("/proc/&lt;pid&gt;/maps is empty on this kernel") and its declared next step is "a fault report

### denver/docs/research/PHASE-2-HEAPRO.md (3)

- L124: - AArch64 twin carries the same hole; recorded, not fixed (denver is amd64; R6  _(maybe resolved)_
- L206: collisions) — a placer defect, recorded as D51, not fixed here.  _(maybe resolved)_
- L236: **D51 — recorded, not fixed (the re-rank owns them):**  _(maybe resolved)_

### denver/docs/research/PHASE-3-JAVAC.md (1)

- L34: fault cannot be deferred — the instruction re-executes; kill), the red zone, the

### denver/docs/research/PHASE-3-JAVAVERSION.md (1)

- L3: Session 16, 2026-11-14. Phase 2 is exhausted (G1/G6/G7/G9/G8/G4/G3/G2 GREEN, G5 deferred);

### docs/README.md (41)

- L53: / **amd64 boot suite under TCG at SMP=4 stops in 'sigregs'**: the log ends with ~8 '[sig!] ... sig=11 ... addr=0x0 -&gt; handler' / '[MM] fault REFUSED ... why=no-region…
- L68: / **Taoxichuan / Shanghai / Denver:** a program with one thread blocked in 'accept'/'connect'/'recv' and another that should wake from 'sleep' (or connect) makes no prog…  _(maybe resolved)_
- L74: / **amd64:** 'strace_err' shows 'nr=285 … -&gt; -38' (82 per headless-Chromium run); 'fallocate' answers 'ENOSYS' / x86_64 'fallocate' had no row, though glue had the ar…  _(maybe resolved)_
- L84: / **amd64:** 'goose' prints 'locking protocol' (SQLite code 15), 'goose --version' (or any Rust program that runs 'main' on a spawned thread, or 'nca run') prints its ou…  _(maybe resolved)_
- L103: / **amd64:** 'herd stop &lt;svc&gt;' (or 'herd disable') logs 'Stop requested' but the process keeps running; a following 'herd start' would launch a second copy / **Aku…  _(maybe resolved)_
- L104: / **amd64 bare metal:** the box answers ping but port 2222 is refused after boot; 'herd start sshd' at the console works and gives a *high* pid / **OPEN (2026-10-08); pa…  _(maybe resolved)_
- L105: / **amd64:** 'kami' (or any CDP client) over '--remote-debugging-pipe' gets a 'Page.screencastFrame' that is **all '(0,0,0,0)'** (a black screen on '/dev/fb0'), while 'P…
- L123: / **Taoxichuan:** a process dies with '[Fault] ring3 PageFault ... -- task killed' where it installed a SIGSEGV handler (a guard-page probe, a GC barrier) / Expected on …
- L128: / A kernel panic that contradicts its own message — 'Process table full (256 slots, 246 reclaimable RETIRED)' / The collector is starved, not the table exhausted. 'regis…  _(maybe resolved)_
- L132: / **amd64:** a stalled network peer (e.g. a backend HTTP response that never completes) wedges the whole box — '[BKL] stuck: owner=N' for tens of seconds straight, ssh t…  _(maybe resolved)_
- L134: / **Firecracker (AArch64):** vCPUs pegged at ~91% for hours, console silent, last lines a few '[bkls&gt;]' samples and no '[BKL] stuck' / **Open; leading hypothesis foun…  _(maybe resolved)_
- L135: / **ryzen host (Akuma 'smp=8', 'kvm0' guest running real work):** ssh dies, then the box resets itself to Pop after ~60 s; host console a growing '[bkls&gt;] core=N ... …
- L160: / **amd64:** a file reads back with **some 4 KiB blocks holding stale old data** (a rewritten config showing log text; a pushed kernel image whose md5 changes minutes af…  _(maybe resolved)_
- L164: / **ryzen, Akuma on wifi:** the box is not reachable for several minutes after the one-shot boots (ARP for the station MAC finds nothing, then it appears) / **OPEN, meas…
- L170: / **amd64:** Claude Code's '/login' code prompt ignores paste, Esc and Enter after ~45 s idle there (plain typing worked before); claude's main thread spins in 'epoll_pw…
- L174: / **amd64:** a thread that dies by fault or group-fatal signal leaves per-slot state standing — a leaked 'Process' and a 'ps' row that never goes away, '[TRAMP-MISMATCH]…
- L196: / A long-running campaign of repeated in-guest builds dies around the 9th trial: '[OOM] allocation of N bytes failed (heap ~755MB / ~796MB used)', rustc exits **137**, t…  _(maybe resolved)_
- L201: / '/bin/sh: can't fork: Out of memory' after a few SSH sessions on a 4–4.5 MB box, no '[OOM]' line (contrast: the execve-leak class always prints '[OOM]') / **OPEN.** Fi…
- L206: / 'SCM_RIGHTS' fd passing does nothing ('recvmsg' returns no ancillary data), or 'SO_PEERCRED' reports **uid 0** for every peer / **Both are known gaps, not bugs.** Anci…
- L208: / 'scripts/build_devbox.sh' (the rump-only devbox) fails with dozens of 'unresolved module or unlinked crate \'smoltcp\'' errors / A '#[cfg(feature = "smoltcp")]' has dr…  _(maybe resolved)_
- L213: / A multi-threaded CPU workload collapses when adding threads (llama.cpp '-t 1' fast, '-t 2+' 10-100x slower, all cores burning) while separate processes scale fine / Th…  _(maybe resolved)_
- L225: / 'box run &lt;official image&gt;' prints 'failed to spawn …/docker-entrypoint.sh', or starts and then does nothing at all / Two different things. 'failed to spawn' = 's…  _(maybe resolved)_
- L228: / 'e2fsck' after a 'cargo clean' reports inode and block **bitmap differences** ('-NNNN'): the deleted files have 'Links: 0' and a 'dtime' but still hold their blocks, a…  _(maybe resolved)_
- L229: / 'rm' of a file that was mapped or open does not free its space: 'df' does not move after the 'unlink', after 'munmap'/'close', or even after the process exits, until s…  _(maybe resolved)_
- L245: / '[SGI] POOL contended, skipped N ticks' climbing without bound; console still printing but ssh dead; onset during '[Cleanup] Thread N recycled' churn / **OPEN.** Not a…
- L269: / A process is SIGSEGV'd for OOM (or tcc/meow fails at the 'extreme-size' 4 MB floor) while the box plainly has free memory moments later / Candidate: the user-page esca…
- L278: / 'git clone' hangs or wedges / ['runbooks/debug-devbox.md'](runbooks/debug-devbox.md) / ['runbooks/debug-network.md'](runbooks/debug-network.md). Original pipe-deadlock…
- L280: / 'box grab &lt;name&gt;' does not reattach — no output, an error, or the wrong process / **OPEN, reported 2026-09-07, not reproduced.** A regression: this worked before…
- L289: / The terminal **stutters** — a TUI (nca) redraws in visible steps, output arrives in bursts, and it gets worse while the network is busy / **FIXED 2026-08-18.** Not net…  _(maybe resolved)_
- L327: / ext2 writes feel slow — a build's 'create'/'unlink' churn, 'rm -rf' of a tree, 'tar -x' — even on a **healthy** image (distinct from the damaged-image slowdown at the …
- L330: / **Every** syscall feels expensive, not just one subsystem — and you want the investigation, not just the numbers / ['archive/AKUMA_SYSCALL_PERFORMANCE_AUDIT.md'](archi…  _(maybe resolved)_
- L333: / A file written by multiple threads of one process (shared fd, 'CLONE_FILES') comes out with corrupted/torn content but the right total size — no crash, no error / Was …  _(maybe resolved)_
- L336: / 'free'/'uptime'/'df'/native-'ifconfig' broken, or want a read-only rootfs and the minimal busybox applet set for dev / Clustered gap inventory (procfs '/proc/{meminfo,…  _(maybe resolved)_
- L347: / A '-j4' build stalls with waiters parked in untimed 'FUTEX_WAIT' for hundreds of seconds, ~3 % CPU, but **no** '[FUTEX-ORPHAN]' and correct key namespaces / Usually **…
- L360: / A multi-threaded process 'fork's and dies 'EXIT=139' writing one of its **own globals**, '[WPF] cow_ref=0 lazy_self=NONE eager=NONE ap_rw=true' / **FIXED 2026-08-08, r…  _(maybe resolved)_
- L386: / 'cargo build' of a big third-party workspace fails on the first registry deps with 'could not execute process \'rustc …\' (never executed) / Bad address (os error 14)'…
- L388: / A guest TCP client hangs forever reading a response whose **first byte is delayed** &gt;~5 s (e.g. nca↔Ollama with a slow-prefill model), while instant responses on th…
- L442: / A kernel change builds '--release' but not 'extreme-size', or 'cargo build' left a stale 'akuma.bin', or a wall-clock A/B looks too good / ['archive/EXT2_WRITEBACK_FOL…
- L446: / Work on the **amd64 bare-metal** path — PCI enumeration, the USB keyboard, machine reset / 'crates/akuma-kernel-amd64/src/pci.rs' + 'crates/akuma-pci' enumerate config…
- L447: / Understand why a program that runs on Linux misbehaves after a syscall on amd64 — a value it had before the call comes back wrong / The kernel's 'syscall_entry' used t…  _(maybe resolved)_
- L513: **Still undocumented (deferred gap list):** audio ('src/audio.rs'), Tier C /

### docs/handoff-kernel-chromium-support.md (5)

- L38: rendering result, and the **"Still open"** list this prompt is built from.
- L107: § "Open: the CDP screencast delivers an empty frame". 'Page.captureScreenshot'
- L121: **Open divergence found by 'fallocprobe':** a 'MAP_SHARED' write is not visible
- L127: 1. (Fix 20 is cleared for the empty frame: same result without it. Still open for
- L180: '## Fix N' per closed item and correct "Still open".  _(maybe resolved)_

### docs/reference/README.md (2)

- L73: smoltcp devbox and the deferred rump one.
- L75: ## Not yet written (deferred gap list)

### docs/reference/build-profiles.md (5)

- L23: / **devbox** / 'release' / 'scripts/build_devbox.sh' / 'overlays/devbox/run.sh' / **rump only** (no smoltcp) / *(deferred — see 'devbox-smoltcp'.)* Rump-stack workstatio…
- L24: / **devbox-smoltcp** (default devbox) / 'release' / 'scripts/build_devbox_smoltcp.sh' / 'overlays/devbox/run-smoltcp.sh' / smoltcp (native) + userspace '/bin/sshd' / The…
- L117: shell**. Rather than leave it gated behind a cfg, the follow-up commit
- L239: - **Working inside Akuma as a Unix box (self-hosted toolchain, editor, daily use)** → 'devbox-smoltcp' (the default devbox: native smoltcp + real SMP; 'overlays/devbox/r…
- L317: - 'overlays/devbox/README.md' — devbox design rationale; 'devbox-smoltcp' (default) vs. the deferred rump 'devbox'.

### docs/reference/build-system.md (1)

- L35: / 'devbox' (rump-only, deferred) / 'release' / inherits release / 'scripts/build_devbox.sh' / 'overlays/devbox/run.sh' / '--no-default-features'; 'rump-default,sc-reboot…

### docs/reference/crate-safety.md (3)

- L118: 'crates/akuma-kernel-amd64/src/sched.rs', which is not counted here). The 2026-10-16 **Open** item
- L181: **Open (closed 2026-10-31, all three read 0):** the script flagged one 'unsafe' site each inside three forbidding crates  _(maybe resolved)_
- L505: / 'akuma-ext2' / the ext2 driver: on-disk codec, block cache, directory and inode operations, deferred frees /

### docs/reference/firecracker-amd64/README.md (1)

- L126: namespace, not '/dev'. Without it 'ip tuntap add' fails with "open: No such

### docs/reference/overlay/README.md (1)

- L14: / 'overlays/devbox/' / ['devbox.md'](devbox.md) / "give me a daily-driver dev VM" — 'run-smoltcp.sh' (the default: native smoltcp, real SMP) and 'run.sh' (the deferred r…

### docs/reference/overlay/devbox.md (2)

- L12: boots but is deferred.
- L18: / 'run.sh' / Deferred. 'release' profile, '--no-default-features' + 'rump-default' (see 'scripts/build_devbox.sh'), single kernel. 'RUMP_NIC=1' brings up the second NIC …

### docs/reference/safety-types.md (6)

- L5: it deliberately does not, and what is still open. Each proposal landed as its own commit on branch 'ouch'  _(maybe resolved)_
- L93: / ELF loaders ('load_elf', the interpreter) / **file** 'p_vaddr'/'p_memsz'/'p_filesz' / 'vaddr + memsz' unchecked in 'segment_span', the deferred path's 'end_page', relo…
- L113: core for the duration. Used for 'interrupt_thread' in 'kill_process'/'kill_process_with_signal' and for the deferred
- L117: deferred 'request_thread_kill' ('kill_process*', 'kill_thread_group'); the group signal delivery's interrupt and
- L152: them). Each was verified on both boot suites. **Open:** classify the remaining ~100 static spinlocks. The rule is "any lock an
- L251: ## Open work (what the report listed that is not done)  _(maybe resolved)_

### docs/reference/subsystems/containers.md (3)

- L277: state, nothing guards starting in it. Deferred fix:
- L364: deferred rather than "obviously fine".
- L511: '/proc/self' each cost, and which one is still open.

### docs/reference/subsystems/drivers/block.md (1)

- L67: driver is in-tree, what was verified, what is still open.

### docs/reference/subsystems/drivers/timers.md (2)

- L98: deadline (tickless idle) is the recorded next step
- L108: beyond 8 outstanding wakers. 'on_timer_interrupt' collects expired wakers

### docs/reference/subsystems/kvm.md (3)

- L256: are **reserved**: the picker hands them only tasks pinned to them ('sched::reserve_cores'). With 'kvmcores=' given, the whole set is reserved **from boot** (less core 0)…
- L384: Firecracker exercised them**; 'GET/SET_LAPIC', the I/O APIC, 'SET_GSI_ROUTING' and more than one vCPU are not done.  _(maybe resolved)_
- L392: Future work in ['AKUMA_MULTIARCH_ATTEMPT_0.md'](../../archive/AKUMA_MULTIARCH_ATTEMPT_0.md)

### docs/reference/subsystems/locking.md (7)

- L318: victim's behalf. Phase 7e deferred that drop and the accident stopped
- L344: - **Deferred cleanup gated to "must run from thread 0" has no recovery path
- L358: synchronously. Phase 7e's deferred reclamation keeps RETIRED zombies' clones
- L361: pipe read ends were released only by the deferred collector — the same
- L696: is the deferred collector: it frees a RETIRED slot only after
- L714: One consequence: deferred reclamation means cooled-but-uncollected zombies
- L821: the window still opened and closed but the fill assertion did not run.  _(maybe resolved)_

### docs/reference/subsystems/memory.md (6)

- L28: &gt; Three items still OPEN: per-run kernel-heap creep; reclaim-after-OOM below
- L30: &gt; reclaim cooldown (see [KNOWN GAP: a premature give-up while memory is merely
- L450: 'chunked-allocator' feature (Talc in 64 KB chunks). Deferred-free queue for
- L459: saturated table stalls ext2's deferred frees.
- L620: ### KNOWN GAP: a premature give-up while memory is merely cooling
- L882: - 'archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md' — why the free is deferred at

### docs/reference/subsystems/networking.md (2)

- L64: &gt; **Deferred (2026-07-19):** the default devbox is now **devbox-smoltcp** (box 0
- L324: reaped — and process teardown is deferred, so by then the freed slot may belong

### docs/reference/subsystems/power.md (2)

- L141: known gap (P1): an allow_tick halt arms no budget yet --
- L171: **Known gap (measured on metal, 2026-10-15):** on a 'nosmp' boot the idle

### docs/reference/subsystems/rng.md (1)

- L92: 'UNSAFE_AUDIT.md' §4 P2(e) analyses the first and last of these as **open

### docs/reference/subsystems/rump-stack.md (4)

- L3: &gt; **Status: DEFERRED (2026-07-19).** 'rump_server' work is on hold. The default
- L176: lists the socket syscalls not yet implemented, so 'intercept_box_syscall'
- L227: away in this build/link environment (root cause still open — likely a
- L262: Deferred, post-M1; no current implementation. 'archive/ARCHITECTURE_QUESTIONS.md',

### docs/reference/subsystems/smp-shared.md (5)

- L53: running it. Real per-AS ASIDs are deferred (a perf optimization; all use ASID 0 today,
- L96: / M4 — migration + hardening (cross-core wakeup deferred) / ✅ 1 thread on 4 cores /  _(maybe resolved)_
- L98: / M5b — BKL-free user page-fault path (per-AS 'as_lock') / ✅ Stages 1–3 + 4a: fault PTE edits under 'as_lock', file read/install split, **file-fault block I/O runs BKL-d…  _(maybe resolved)_
- L102: / M5e — deferred sibling kill at the EL1→EL0 boundary / ✅ 'kill_thread_group' PHASE 1 no longer hard-marks siblings 'TERMINATED' under smp-shared (which stranded spinloc…  _(maybe resolved)_
- L122: &gt; '[BKL] RECOVERED' log lines are live sightings of that still-unfixed leak. The fork-hammer's

### docs/reference/subsystems/ssh.md (1)

- L216: **Before** (unfixed binary — it does not know the flags:

### docs/reference/subsystems/syscalls/aio.md (1)

- L61: - 'archive/BUN_MEMORY_STUDY.md' "Follow-up Crash: io_setup Ring Buffer Not

### docs/reference/subsystems/syscalls/fs.md (2)

- L150: 'O_PATH' descriptor allows neither ('akuma_syscalls_linux::flags::open::{may_read,
- L342: there). 'F_SETLKW' re-polls every 5 ms. Not done: 'EDEADLK', 'F_OFD_*',  _(maybe resolved)_

### docs/reference/subsystems/syscalls/mem.md (1)

- L557: not done: 'docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md' §10.  _(maybe resolved)_

### docs/reference/subsystems/syscalls/poll.md (3)

- L22: &gt; no epoll crash fixes since. One item stays genuinely **OPEN**:
- L70: Each is **preserved, not fixed** — an extraction that quietly fixes something  _(maybe resolved)_
- L318: verified to fail on the unfixed kernel. Full account:

### docs/reference/subsystems/syscalls/proc.md (1)

- L95: passes 'getpid()' from a non-leader thread. Not yet fixed — 'getpid' is load  _(maybe resolved)_

### docs/reference/subsystems/syscalls/signal.md (4)

- L191: survive it at all: it has no consumer for the deferred kill that path posts
- L198: slot 'TERMINATED' and parks, as a posted deferred kill does at the EL0 boundary.
- L223: **Every blocking wait must leave on a deferred kill request**, not only on a pending signal:
- L293: Not covered: a signal that *does* set 'SA_RESTART' still has its handler deferred

### docs/reference/subsystems/syscalls/sync.md (2)

- L120: &gt; there is nothing for it to observe. The open question is what happens
- L228: a deferred kill ('PENDING_KILL', which 'exit_group' posts to every sibling on

### docs/reference/subsystems/syscalls/term.md (3)

- L7: (deferred gap, 'reference/README.md' § "Not yet written"). For the SSH-side
- L20: &gt; **OPEN (2026-08-10):** the blocking stdin-read path
- L147: 'try_lock' attempt, never the whole wait. **Not fixed**, and not attempted  _(maybe resolved)_

### docs/reference/subsystems/thread-lifecycle.md (2)

- L117: / kill group P1 / 'kill_thread_group' / none (atomics + wakers) / up to 2 s grace-wait under the caller's P; grace-expiry termination is now ownership-gated ('grace_kill…  _(maybe resolved)_
- L141: often the slots are 'TERMINATED' and simply uncollected, because deferred

### docs/reference/subsystems/tty.md (1)

- L73: 'ProcessChannel'. Making sessions eager is the next step (it would let '^C'/'^\' share one path

### docs/reference/subsystems/vfat.md (1)

- L96: ## Not done  _(maybe resolved)_

### docs/reference/subsystems/vfs.md (3)

- L129: it means "not pinned". Answering 'true' there kept unlinks deferred forever
- L130: ('archive/INODE_PIN_CONGESTED_WINDOW.md'). A deferred free finishes at the
- L391: ## Future work

### docs/reference/subsystems/wifi.md (3)

- L205: ## Known gaps
- L244: times out after 108 ms; RX and the join work regardless. Open: what that
- L291: steps). Readiness-on-state-change is the natural next step once a real radio

### docs/runbooks/README.md (1)

- L49: / '[SGI] POOL contended, skipped N ticks' climbing forever; console still printing but ssh dead / **OPEN.** The box is *unscheduled*, not hung — 'POOL' gates all preempt…

### docs/runbooks/amd64-bare-metal-loop.md (2)

- L1088: / **OPEN (2026-09-18):** an interactive 'ssh' session needs **one extra event to register the previous one** — you press Enter, nothing happens; the next keystroke makes…  _(maybe resolved)_
- L1190: - 'mkdir' is ENOSYS — the syscall is not implemented. **Still open**, and now

### docs/runbooks/amd64-console-shell.md (1)

- L248: 43 ms). Judged "much better" on the TV. Open: merging runs of background pixels into

### docs/runbooks/debug-delayed-first-byte.md (1)

- L278: 'remove_socket()' instead of 'sys_close' double-drops on deferred reap and

### docs/runbooks/debug-devbox.md (3)

- L38: **Still open**: a ~300ms floor on a single synchronous round-trip (one
- L64: / 'git clone' very slow (&gt;2min for tiny repo) / Per-syscall rump proxy round-trip; serialized. / **Open.** Measure against native smoltcp first. See 'archive/OPTIONAL…
- L72: / Shell pipeline 'cmd \/ head -N' wedges VM at ~99%, '[signal] tkill(tid=X, sig=13)' / SIGPIPE delivery to the writer spins instead of terminating the write syscall. / *…

### docs/runbooks/debug-futex-lost-wakeup.md (4)

- L136: &gt; only when 'killer != idx', and the whole deferred-kill path has the victim
- L188: That is a separate **open** bug (thread-spawn), not a lost wakeup — it has its
- L281: Still open at the time of the fix: the in-VM '-j4' build has **not** been re-run,
- L284: &gt; **Residual variant (2026-08-07, not yet fixed).** A focused probe  _(maybe resolved)_

### docs/runbooks/debug-memory-oom.md (3)

- L8: &gt; 2026-06; two items still OPEN (per-run heap creep; reclaim below ~5 MB).
- L36: / 'realloc' hangs / 'munmap' deadlocks when called from inside realloc / FIXED / Deferred-free queue ('DEFERRED_FREE_SLOTS=16'), flushed on next 'dealloc' /  _(maybe resolved)_
- L40: / 'EC=0x22 ELR==FAR==&lt;garbage&gt;' (low RAM, e.g. apk @64 MB) / Three bugs: user_frames refcount over-free; ELF heap-slurp; heap/boot-stack overlap / FIXED / 'FreeOut…  _(maybe resolved)_

### docs/runbooks/debug-network.md (4)

- L47: / 'httpd' stops answering after a load run — **process still alive** and its log still reads 'Listening for connections...', but every request returns '000' from **both*…
- L60: / nginx (or any epoll server) serves at **~17 ms/request** on some boots and ~0.1 ms on others; 'ab -k' halves it; **0 failed requests**; tight within a boot / **Lost ep…  _(maybe resolved)_
- L62: / 'select()' is never woken by a fast peer — it only ever returns on the 10 ms tick (every cargo/libcurl network wait) / 'sys_pselect6' passed 'None' for its waker, alon…  _(maybe resolved)_
- L63: / A process blocked in 'select()' cannot be interrupted by Ctrl-C or 'kill'; 'alarm()' + 'select()' sleeps through its own signal / 'sys_pselect6' had no 'should_interru…  _(maybe resolved)_

### docs/runbooks/debug-smp.md (1)

- L47: / Symptom / signature / Cause / Fix / next step /

### docs/runbooks/diagnose-hung-userspace-process.md (1)

- L79: next step is the reproduction ladder above, not a guess.

### docs/runbooks/future-work-scan.md (7)

- L1: # Scanning the docs for future work
- L3: A tree this size accumulates promises: "still open", "deferred", "follow-up", a 'TODO'. Most are in 'docs/archive', written
- L14: It greps every '.md' file and the 'TODO'/'FIXME' comments in '.rs' files for the words authors use when they leave work
- L16: Add a phrase by editing 'MARK' in the script. **What it cannot do:** find work described in other words, tell "deferred
- L17: drop" from "deferred work", or know whether a claim is still true. A second pass that *reads* a document (rather than
- L27: 3. **Is it still open?** Reproduce it if it is a bug, or confirm the missing piece is missing if it is a feature.
- L34: - Still open and worth doing: add a row to the **Curated** section of 'FUTURE_WORK.md' with *what closes it*, so the next

### docs/runbooks/recover-wedged-vm.md (3)

- L112: / Shell pipeline 'cmd \/ head -N' / '[signal] tkill(tid=X, sig=13)' then ~99% CPU; new SSH connects but stuck procs never clean up / **Open** (SIGPIPE delivery to writer…
- L113: / CPU-bound load starves SSH / Banner-exchange timeout during rustc/tcc codegen; QEMU pegged / **Open** (scheduling weight) / Run lighter; wait out the codegen. /
- L117: / Single rump client slot wedge / One rump box client blocked holds the slot / **Open** / Avoid concurrent heavy rump box clients. /

### docs/runbooks/run-docker-image.md (1)

- L226: — the plan this was built from, its open follow-ups, and the two bugs found

### docs/runbooks/run-on-firecracker.md (2)

- L260: 'akuma_exec::mmu::ram_base()' / 'kernel_va_end()' is outstanding.
- L262: ## 8. Known-outstanding, so you do not debug them twice

### docs/runbooks/run-redis.md (1)

- L131: listener pre-allocates 'MAX_BACKLOG' (8; 32 before 2026-11-11) smoltcp sockets at 32 KB each, and closed sockets sit in a deferred  _(maybe resolved)_

### docs/runbooks/selfhost-kernel-build-amd64.md (1)

- L219: * Not done: a fixed-point compare (the guest's kernel was built by a newer musl  _(maybe resolved)_

### docs/runbooks/selfhost-kernel-build.md (2)

- L323: ### The two remaining blockers — A still OPEN, B FIXED 2026-08-14; neither is the KTG class  _(maybe resolved)_
- L998: / All 4 vCPUs at 100 %, **serial log stops advancing**, sshd accepts TCP but sends no banner, heartbeat gone / Defect A — unexplained all-core wedge. Not KTG (zero '[KTG…

### docs/runbooks/update-bug-fix-list.md (2)

- L25: an open/unresolved issue, not a duplicate mention of a fix already listed
- L48: verification, what is still open — belongs in an archive doc. 'docs/reference/' is

### docs/runbooks/verify-trim-fat-change.md (2)

- L357: / 'bssfork' / 'failures=0' … 'bssfork PASS'. **Its pass rate at SMP=4 depends on how you sample it — see "Two ways of sampling" below.** Two measurements the same day di…
- L469: / 'nettest-unix all' / 10 '[probe] RESULT &lt;mode&gt; verdict=…' lines; **8 'OK' + 2 'UNSUPPORTED'** ('passfd', 'syslog'), **exit 0** / 0.2 s / The AF_UNIX object added…

### docs/userspace/kami.md (1)

- L17: (no GPU process, the measured slim flag set), the known gaps (no Japanese

### docs/userspace/sshd.md (1)

- L9: - 'EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md' — **open**: exec output &gt;1 MiB is

### proposals/AKUMA_MM_CONVERGENCE.md (2)

- L5: deferred there because it cannot be done one call at a time.*  _(maybe resolved)_
- L14: / 3. pin set in the shared 'UserAddressSpace' / **done** (2026-11-12): 'akuma_user_space::MappingPins' held by both structs, methods shared. AArch64 pins on 'push_lazy_r…  _(maybe resolved)_

### proposals/AKUMA_ONE_EXIT.md (2)

- L44: step 3. Still open on amd64: 'orphanthr''s thread-child case (a child of an already-exited
- L73: - **Open on amd64, found by step 1's gate and older than it:** a child forked by a worker thread

### proposals/ONE_BIG_KERNEL_LOCK.md (1)

- L3: **Status: proposal, not started.** Deferred to the big merge on purpose: the merge decides the

### proposals/PREVENTING_BROKEN_MAIN.md (1)

- L12: / 4 / (skipped by decision) CI (GitHub Actions) running 'premerge_build.sh --fast' on every push to a branch and the full set on PRs to main; branch protection requires …

### proposals/WAKEUP_LATENCY_UNDER_LOAD.md (1)

- L3: **Status: proposal, not started.** Deferred to after the big merge. The measurements below are

### scripts/kvm/goose/BABYSIT_GLM_PROMPT.md (1)

- L24: * **Over its call budget with the gate open:** let it write RESULT and stop; a fresh session with a short continuation prompt is cheaper than a long context.

### scripts/kvm/goose/SUPERVISOR_HANDOVER_2026-10-30.md (1)

- L14: ## Your next steps, in order

### scripts/kvm/goose/SUPERVISOR_RUNBOOK.md (1)

- L47: - **No mid-turn steer on the Mac goose (observed 2026-10-28, goose 1.41.0):** 'goose_acp_steer.py' reaches the secured server (stream 200, 'session/load' 202) but never …

### scripts/kvm/goose/taoxichuan-02-hello.md (1)

- L97: the 5-hour quota is the budget, and stage 1 showed a finish-the-paperwork follow-up

### scripts/kvm/goose/taoxichuan-07-outbound.md (1)

- L4: (offline 'apk add', 'taoxichuan-s4' @ '0b33595a'). The next step toward 'apk add curl' is the network: milestone M1 of

### scripts/kvm/goose/taoxichuan-07b-outbound-docs-first.md (1)

- L40: - **H2:** char-device open: 'dd if=/etc/hostname of=/dev/null' works ('/dev/null' open returns EACCES today).

### scripts/kvm/goose/taoxichuan-09b-m4-deps.md (1)

- L23: 1. 'docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md' § 2 table, § 3, § 6 (U1, U4, U7) - what is claimed and what it left open.

### scripts/kvm/goose/taoxichuan-09l-spawn-park-stall.md (2)

- L10: - Shape: '[SSH] Exec:' is printed, then '[TICK pre]', then cpu0 is halted forever; the tick is alive (about 200 exits/s), 'hlt=99%', relay counters frozen ('gw_frames_fr…
- L13: - **A near-deterministic trigger exists**: 'apk add git' over ssh on a fresh boot (the frozen mirror at '/srv/alpine-mirror' now carries git, libexpat and pcre2 besides …  _(maybe resolved)_

### scripts/kvm/goose/taoxichuan-11-kvm-guest-ram-cap.md (1)

- L52: About 70 tool calls; at 35 without a commit the supervisor nudges you. Past budget with the gates open: commit, write RESULT, stop.

### scripts/kvm/goose/taoxichuan-12-epoll-eventfd.md (1)

- L73: About 150 tool calls for this session (a cycle is ~3 min); the supervisor nudges at 90 calls without a commit. A 5-hour z.ai window is ~34M tokens; stay well under 10M. …

### shanghai/docs/MIDDLEWARE_COMPOSITION.md (1)

- L105: - **Open:** how a label from one policy combines with the plain 'Allow' of another inside 'compose' (simplest: the label

### userspace/box/docs/BOX_BUILD.md (1)

- L183: - **Not done:** chunked/resumable upload ('PATCH'), cross-repository blob  _(maybe resolved)_

### userspace/box/docs/ISSUES_2026_11_16.md (1)

- L101: ## A kernel divergence found on the way (not fixed here)  _(maybe resolved)_

### userspace/herd/README.md (3)

- L239: - **'start'** refuses while such a pid is outstanding (a second copy beside a
- L315: - Open: **'args = --listen grpc://127.0.0.1:47200' reportedly lost its first flag at spawn**
- L320: - Open: 'herd status' showed 'restart-pending ... last_exit=2' after a good start (2026-11-17). Not reproduced;

### userspace/herd/docs/SIGNAL_EXIT_HANDLING.md (2)

- L154: restart. That ordering is load-bearing; preserve it. A follow-up may want
- L174: in-VM checks below remain outstanding.

### userspace/kami/README.md (6)

- L58: did not appear in the frames captured; see Known gaps).
- L215: '/tmp/kami-input.log' in clear. True of the framebuffer kami as well. Open:
- L259: It numbers its own requests, remembers which are outstanding and since when,
- L328: - **Open (2026-10-09, after rebooting onto the pipe-cap kernel):** 'https://www.tumblr.com'
- L437: ## Known gaps
- L739: layout rather than Markdown; the '.md' output is still open.*

### userspace/libakuma/docs/ALLOCATOR_MEMORY_FIX.md (1)

- L6: &gt; for the history of why deferred freeing was tried and what its failure

### userspace/llama.cpp/docs/AMD64_BUILD.md (3)

- L9: 'Cargo.toml' the way the AArch64 build is — see "What is not done yet" below.  _(maybe resolved)_
- L145: ## What is not done yet  _(maybe resolved)_
- L162: all" baseline. Once that is proven (it is, as of this doc), the next step

### userspace/ncaprobe/README.md (2)

- L70: / 'eofedge' / After draining a pipe with the writer still open, does the **EOF edge** ever arrive? The minimal form of the bug. /
- L98: 'raw' is the one with an open question still behind it — see "Still open" in the

### userspace/nettest/README.md (1)

- L419: 'static-curl'" as the next step. That text is stale. 'rust/Cargo.toml' already

### userspace/rumpkernel/rumpuser/src/lib.rs (1)

- L184: /// SECURITY TODO (see docs/RUMP_SYSPROXY.md "Security / hardening TODOs"): this is a

### userspace/scratch/docs/CHECKOUT_PERFORMANCE.md (2)

- L23: 2. **open(dest, O_CREAT/O_TRUNC)** — create the output file:
- L46: ## What would help further (not yet implemented)

### userspace/scratch/docs/MEMORY_LEAK_FIX.md (1)

- L7: 1. **Allocator Leak**: The userspace allocator was leaking old memory blocks during 'realloc' because its internal deferred free queue was too small (16 slots) and only …  _(maybe resolved)_

### userspace/scratch/src/main.rs (1)

- L170: // TODO: Implement clone

### userspace/scratch/src/refs.rs (1)

- L206: continue; // TODO: Handle nested refs

### userspace/scratch/src/repository.rs (1)

- L327: // TODO: Could verify that local_sha is an ancestor of remote_sha

### userspace/sshd/README.md (1)

- L184: / ['docs/LIMITATIONS.md'](docs/LIMITATIONS.md) / Known gaps /

### userspace/sshd/docs/ISSUES_2026_11_16.md (1)

- L103: ## Not done here  _(maybe resolved)_

### userspace/sshd/docs/PROCESS_PER_SESSION.md (1)

- L98: is a connection-setup failure, not starvation. Unresolved; it is a reason the

### userspace/sshd/docs/PROTOCOL_UNDER_LOAD.md (1)

- L187: explicit follow-up byte to get the malformed packet processed at all

### userspace/sshd/docs/SECURITY_IMPROVEMENTS.md (2)

- L83: **Not fixed here, flagged for follow-up:** 'userspace/sshd/src/protocol.rs'  _(maybe resolved)_
- L232: ## Accepted limitations (documented, not fixed)  _(maybe resolved)_

### userspace/tcc/docs/IMPLEMENTATION_DETAILS.md (1)

- L55: * **Dynamic linking:** Binaries compiled without '-static' crash on Akuma (no dynamic linker). Future work could involve implementing a userspace ELF interpreter.

## Inventory: `docs/archive`

<details><summary>docs/archive/600_BUGS_ANNIVERSARY.md (1)</summary>

- L621: ## Open questions

</details>

<details><summary>docs/archive/AARCH64_SMP_HOOKS_RACE.md (1)</summary>

- L58: latency) and 'thread_slot_reclaim_on_spawn: hot_reclaim=98 (want 0)'. The unfixed kernel on a boot

</details>

<details><summary>docs/archive/AARCH64_SPAWN_RECLAIM_SKIPPED.md (2)</summary>

- L59: ## Still open
- L97: **Still open:** 'start_irqs_disabled', the '_for_process' variant's guard against a tick before

</details>

<details><summary>docs/archive/AKUMA_AMD64_4B_FOLD_BATCH2B.md (1)</summary>

- L33: dropping it (Phase 7e's deferred reclamation), so the 'Process' and its

</details>

<details><summary>docs/archive/AKUMA_AMD64_4B_FOLD_BATCH3A.md (2)</summary>

- L221: **It is not fixed in glue, and that is a decision, not an oversight.** It is a  _(maybe resolved)_
- L225: boot behind the change rather than a blind edit. **Open issue** — see §8.

</details>

<details><summary>docs/archive/AKUMA_AMD64_4B_FOLD_BATCH4B.md (4)</summary>

- L103: count on a target whose lap cost is not fixed. 'clock::uptime_us' is real  _(maybe resolved)_
- L245: ## 3. Found, not fixed: glue's 'struct termios' is one byte off  _(maybe resolved)_
- L264: **Not fixed here.** It is a one-line change on the AArch64 tty path, this  _(maybe resolved)_
- L424: 'ProcessChannel'**. Giving one to an sshd session's child — the deferred

</details>

<details><summary>docs/archive/AKUMA_AMD64_AGENT_STAGING_AND_ACCOUNTING.md (5)</summary>

- L6: 'goose' now works, a multi-threaded tokio stream no longer stalls. Still open: 'opencode', the TUI items in
- L359: Still open: 'opencode --version'; nca run-together tool previews. ('F_SETLK' — § 14.)
- L367: left open in § 9.
- L384: Not done: 'EDEADLK' detection, 'F_OFD_*', 'SEEK_END'; keyed by path, so two paths to one inode do not contend.  _(maybe resolved)_
- L417: **Not yet fixed. Options, with what each costs:**  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md (2)</summary>

- L382: &gt; figure is history. **5:** still open; the ryzen sessions of 2026-10-14 were idle
- L389: 10 '-j4' runs on 'main' — see §3's correction; the open question is now

</details>

<details><summary>docs/archive/AKUMA_AMD64_BKL_NETWORKING.md (1)</summary>

- L295: 'sock.rs' never having a carve-out to honor (§1-3, still open). A client using

</details>

<details><summary>docs/archive/AKUMA_AMD64_BLOCKING.md (1)</summary>

- L301: ### Not done, and named  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_BOXES_AUDIT.md (3)</summary>

- L29: 'HEAP_SLURP_MAX', which is *deferred* (lazy regions) and amd64's '#PF' handler
- L81: ## Still open
- L121: follow-up for the intermittent 'hget' spin.

</details>

<details><summary>docs/archive/AKUMA_AMD64_BOX_SPAWN_EXT.md (8)</summary>

- L18: invocation within one boot — is also still open; see "What's left" for both.
- L95: its sockets anywhere different. Not fixed — there is nothing to route to on  _(maybe resolved)_
- L474: it should have deferred to instead of restating.
- L648: unconditionally uses 'MapStrategy::Deferred' for a path-based load — this
- L665: a deferred 'PT_LOAD' segment faults '#PF' (not-present), 'page_fault_dispatch'
- L729: 'MapStrategy::Deferred' populates for any 'spawn_ext'-loaded binary over
- L744: verification of 'box' itself is still open.
- L755: a real divergence, not urgent, not fixed.  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_C1_5C_SURVEY.md (2)</summary>

- L96: **One case is carried, not fixed.** 'THREADS' holds non-main threads, so  _(maybe resolved)_
- L122: C1's remaining work is **not** "5c, a fold". It is:

</details>

<details><summary>docs/archive/AKUMA_AMD64_C2_SLICES_6_AND_7.md (3)</summary>

- L148: plan says must not ride along. Not done.  _(maybe resolved)_
- L237: against the unfixed openat before being kept.
- L327: against the unfixed 'sys_openat' (5 red, 'got 0x1f' — the 31 bytes), the fd-2

</details>

<details><summary>docs/archive/AKUMA_AMD64_C3_CLOCK.md (1)</summary>

- L310: The obvious next step, if the skew starts costing something, is a

</details>

<details><summary>docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md (14)</summary>

- L458: ## Open: the CDP screencast delivers an empty frame (found 2026-10-08, on the metal)
- L546: Still open, found by the probes and not fixed: 'read(2)'/'pread' do not see a  _(maybe resolved)_
- L556: ## Still open: what Chromium hits on Akuma now
- L563: Still open beside it: a 'MAP_SHARED' write is invisible to 'pread' until
- L612: suggests the GPU process was behind the "Open: the CDP screencast delivers
- L876: **Not fixed, and now separable from this:** Tumblr still paints one frame and  _(maybe resolved)_
- L881: by protocol (97)'), and local files never need it. Chasing that is the next step.
- L1050: / 27 / '/proc/cpuinfo' did not exist on either kernel ('Failed to initialize cpuinfo') / **fixed and booted on the metal 10-09** (Ryzen 7 8845HS, family 25 model 117, 8 …  _(maybe resolved)_
- L1068: **Open, found by the same work, not fixed:**  _(maybe resolved)_
- L1073: / a 'MAP_SHARED' write is invisible to 'pread' until 'munmap'; 'FALLOC_FL_KEEP_SIZE' is 'EOPNOTSUPP' / "Still open" above /
- L1074: / 256-row process table panics when full / "Still open" above /
- L1077: / ~~**sshd corrupts a large stdout stream, non-deterministically**~~ — **root-caused 10-09 night, fix in tree (Fix 36)**: not sshd and not the socket path. Glue's stdout…  _(maybe resolved)_
- L1081: / missing rows/files list ('sendfile', 'inotify_init', '/proc/cpuinfo', ...) / "Still open: what Chromium hits" above /
- L1086: / ~~**'akuma-ext2' host test 'rewrite_truncate_rename_and_concurrent_append_keep_files_apart' fails intermittently**~~ — **test isolation, fixed 10-09**: the captured 'e…  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_CLOCK_GETTIME_SWITCH_FRAME_GP.md (5)</summary>

- L16: **Grade: C — active risk, STILL OPEN** *(as of 2026-09-22; see the correction
- L138: Doc's own words: "The deeper fragility is not fixed and should be."  _(maybe resolved)_
- L153: reconcile_for_spsr -- NOT DONE, explicitly listed as pending.  _(maybe resolved)_
- L314: ## Status / next steps
- L331: detectors, and its own explicit "deeper fragility... not fixed" callout.  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md (4)</summary>

- L131: the bridge at all. That is the deferred '/proc/&lt;pid&gt;/fd/0' + 'delegate_pid'
- L147: Not fixed in 'akuma-terminal', deliberately: 'cc_index::VSUSP' belongs there and  _(maybe resolved)_
- L197: ## 7. What is still open
- L217: 'AKUMA_AMD64_4B_FOLD_BATCH2A.md' § '/proc' (the deferred design),

</details>

<details><summary>docs/archive/AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md (3)</summary>

- L92: Why *some* requests were answered (one of the handoff's open questions): each
- L188: ## 6. Found on the way — not fixed  _(maybe resolved)_
- L190: ### 6.1 OPEN: a kot worker on the metal spins on 'writev' (~650 k/s)

</details>

<details><summary>docs/archive/AKUMA_AMD64_EPOLL_EVENTFD_FOLD.md (10)</summary>

- L10: 'pidfd_send_signal', the one gap the second pass left open. Every syscall
- L183: ## Follow-up: 'sc-timerfd' and 'pidfd_open'
- L217: fourth "keep going", it now does — see "Follow-up 3" below.
- L225: from four before this follow-up and nine before the epoll/eventfd pass.
- L280: look). Left as a finding, not fixed here.  _(maybe resolved)_
- L282: ## Follow-up 2: 'sc-aio' and 'sc-sysv-ipc'
- L363: ## Follow-up 3: 'pidfd_send_signal'
- L365: A fourth "continue", closing the one gap Follow-up 1 explicitly left open:
- L382: 'pidfd_open''s in Follow-up 1) — another shared-number dispatch in
- L484: **Not done:** bare-metal verification on the physical reference box (this was  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_EXT2_CROSS_FILE_CORRUPTION.md (9)</summary>

- L89: 3. **Deferred inode frees** ('drain_deferred_frees', inode pins): 'mv' over a
- L130: ## 8. Suggested next steps
- L183: The deferred-free path ('drain_deferred_frees' freeing an unlinked-while-
- L195: or deferred frees, which is the next thing to add.
- L214: **Status after this session: the cross-file corruption is NOT fixed.** The  _(maybe resolved)_
- L281: deferred frees + concurrent appends + a latency-injecting device); it passes, so the
- L338: **Not fixed — existing damage.** The 170 cross-links are on disk. The newer owner usually won  _(maybe resolved)_
- L345: **Open:** the dmesg lines for the second owner are capped (80); raise 'PRINT_CAP' or add a
- L377: / Orphans / 14 allocated inodes with **no path** (35297, 35335-7, 35340, 36122, 36126, 36252, 36353-4, 48383-6): unlinked-while-pinned files whose deferred free never ra…

</details>

<details><summary>docs/archive/AKUMA_AMD64_FAULT_SIGNALS.md (1)</summary>

- L122: be *deferred* — the faulting instruction would simply re-execute and fault

</details>

<details><summary>docs/archive/AKUMA_AMD64_HDA_AUDIO_BRIEF.md (4)</summary>

- L356: Next steps (in order):
- L398: init (bound) and BCIS ticks advance. Open: the second M9c cluster (44.1k
- L401: next step is to make it re-bind (0x02270610) and re-unmute after the FMT
- L426: - Open: audible check pending (root); next lever if silent: SET_PIN_VREF 0x1b.

</details>

<details><summary>docs/archive/AKUMA_AMD64_HDA_REWRITE.md (1)</summary>

- L140: ## 6. Not done  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_KOT_REPLICA_WEDGE.md (1)</summary>

- L47: Found on the way, not fixed: the boot self-test 'spawn: the child's registered  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_LAPIC_ONESHOT_STALE.md (2)</summary>

- L52: Verified on ryzen: 40/40 'hget' runs complete (the unfixed kernel hung within
- L113: ## Still open

</details>

<details><summary>docs/archive/AKUMA_AMD64_MEMORY_CLOSEOUT.md (2)</summary>

- L302: ## What is still open
- L348: 2244' line will never return for this target. Not fixed here; worth fixing.  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_MEOW_TLS_STALL.md (1)</summary>

- L4: remaining defect characterised but not fixed).  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_MMAP_REGIONS.md (1)</summary>

- L258: ## What is still open here

</details>

<details><summary>docs/archive/AKUMA_AMD64_NO_SLOT_RECYCLER.md (4)</summary>

- L13: the tree; §8 is what landed and what is deliberately still open.  _(maybe resolved)_
- L94: or by a deferred kill that never reaches its boundary does not run it — and on
- L258: 'current_thread_is_terminated' gates the deferred-frame drain.
- L396: - **§3.1 ('CLEANUP_CALLBACK' / the leaked 'Process') — still open, deliberately.**

</details>

<details><summary>docs/archive/AKUMA_AMD64_ON_HP_500_502NJ.md (3)</summary>

- L733: polling. That is the next step and it needs the box to iterate on.
- L1034: ## Still open
- L1041: Also noted, not fixed: 'US_PER_TICK' assumes 10 ms per LAPIC tick, and under KVM  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md (6)</summary>

- L53: still open.)* **The real costs are the log sink and a watchdog, not the boot.** Akuma can
- L193: / W3 / **Transmit + 802.11 MLME:** TX ring, probe/auth/assoc frames, a station state machine. Test against an **open** AP (a phone hotspot) / associated, DHCP lease over…
- L326: Open: the home network's beacons arrived 17 times in 12 s against ~117 sent
- L381: / 16 / address-CAM hashes recomputed (below), the filter left open for the whole join with the station matching addresses itself, whole-join retries. Auth try 1 unanswer…
- L601: **Open:**
- L735: (47391, 42106, 26107). Open: the *full* and *absent* status codes, and mapping

</details>

<details><summary>docs/archive/AKUMA_AMD64_PIPE_TABLE_UNIFICATION.md (1)</summary>

- L109: reproduce on QEMU (0/8). Pre-existing, metal-only, and still open; recorded

</details>

<details><summary>docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE1.md (1)</summary>

- L88: doc on each is the argument for the emptiness, and neither is a 'todo!()'.

</details>

<details><summary>docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE5.md (1)</summary>

- L14: every 'fork' on this target takes. The fold itself is still the next step; what

</details>

<details><summary>docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE6.md (1)</summary>

- L192: 'AMD64_SSHD_INTERMITTENT_LOCKOUT.md' records as **signature A, still open**, and

</details>

<details><summary>docs/archive/AKUMA_AMD64_RIO_FBDEV_BUILD.md (2)</summary>

- L96: still open. All the debug tooling below lives in the rio fork's
- L332: ## Known gaps / next steps

</details>

<details><summary>docs/archive/AKUMA_AMD64_RUST_STD.md (2)</summary>

- L266: ## 7. Remaining divergences, deliberately not fixed  _(maybe resolved)_
- L333: same defect still outstanding.

</details>

<details><summary>docs/archive/AKUMA_AMD64_SC_CONTAINERS_MOUNT.md (3)</summary>

- L129: already-known gap, not a new one this session found).
- L149: epoll doc's Follow-up 2). A ring-3 probe was still written and run, to prove
- L295: matching the host meant for Firecracker. Left as a finding, not fixed,  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md (14)</summary>

- L9: below are the road there, in order; each one's "still open" is answered by a
- L158: ## Open: SMP=4 build load — two crash causes found and fixed, one wedge left (2026-09-17)  _(maybe resolved)_
- L277: ### The '-j4' verification run (2026-09-17, FC SMP=4, 6 GB) — big win, not done  _(maybe resolved)_
- L307: Speed (the original question, still open): it is slow, and the counters
- L316: ### Status and next steps
- L324: - **Open: the '-j4' wedge + its kill-class precursor** (§ above). Next:
- L390: ### Status and next steps
- L395: - **Cause 2 reproduced, not yet fixed.** Next: instrument the fork share  _(maybe resolved)_
- L533: distinct from the '-j4' one above — was found and is OPEN, not fixed.*  _(maybe resolved)_
- L753: next steps"), and the '-j4' silent wedge above is a separate, still-open bug.
- L1460: **The underlying defect is still open** *(closed 2026-10-08: the streaming loader, 'AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md' Fix 8 — 'execve' reads only the 256-byte head an…  _(maybe resolved)_
- L1473: #### Still open, and it is the thing between here and a fast build: the '-j4' wedge
- L1892: invalid and **§11's question is still open** *(answered 2026-09-18 the other way by 'AKUMA_AMD64_BARE_METAL_SELFHOST.md' §3: the link crash was a too-small heap, not LLD…
- L2275: **not** obviously reached when it dies by a deferred kill or a fault. A

</details>

<details><summary>docs/archive/AKUMA_AMD64_SHARED_WRITABLE_PAGES.md (1)</summary>

- L153: ## 6. Not done / still open  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_SIGKILL_NATIVE_PATH.md (3)</summary>

- L96: ## 4. What the unfixed kernel did (62653aaf, Firecracker on the trashcan)
- L255: / 'killtree 40 0', **8 vCPUs** — the configuration that wedged the unfixed kernel in round 22 / **PASS, 40 rounds**; 'kill -9' of a threaded process reaps in **13–104 ms…
- L301: the unfixed storm and is the ticket-accounting leak 'akuma-bkl' already

</details>

<details><summary>docs/archive/AKUMA_AMD64_SIGNAL_DELIVERY.md (2)</summary>

- L7: **Status:** done and verified; the deferred parts are §8.  _(maybe resolved)_
- L278: ## 8. What is still open

</details>

<details><summary>docs/archive/AKUMA_AMD64_SMP_WIDTH.md (1)</summary>

- L86: **Next:** soak (hours, rio running, a build), and the open question of why the

</details>

<details><summary>docs/archive/AKUMA_AMD64_SSHD_SESSION_CHANNEL.md (2)</summary>

- L34: / the deferred work is "'/proc/&lt;pid&gt;/fd/0' + 'delegate_pid'" / 'delegate_pid' is 'sys_reattach''s — 'box grab' — which moves a *target's* channel to a *grabber*. I…
- L263: during 'fdprobe' and come back during 'spawn' — which is deferred reclaim, not

</details>

<details><summary>docs/archive/AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md (2)</summary>

- L4: the structural fragility behind it is still open — see "The fix" below).
- L235: **The deeper fragility is not fixed and should be.** 'ON_CPU[cur]' being cleared  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_STALE_FALSE_HOOKS.md (2)</summary>

- L87: 'akuma-kernel-glue' gives it on AArch64, and the union of the deferred-kill bit,
- L108: framing was itself wrong), and the open question was which half of the path was

</details>

<details><summary>docs/archive/AKUMA_AMD64_STALE_GROUP_EXIT_STATUS_241.md (1)</summary>

- L162: ## 8. Still open, found on the way (not linked to any failure)

</details>

<details><summary>docs/archive/AKUMA_AMD64_STEP5A_ONE_WALKER.md (1)</summary>

- L153: possibly inheriting the owner's deferred frames. That is correct for what this

</details>

<details><summary>docs/archive/AKUMA_AMD64_STEP5B_SLICE1_REGISTRATION.md (3)</summary>

- L67: ### The two open questions from the hand-off, resolved  _(maybe resolved)_
- L245: ## Not done — slices 2-4  _(maybe resolved)_
- L307: - 'docs/archive/AKUMA_AMD64_CHROMIUM_KERNEL_WORK.md' — still open, and still the reason to

</details>

<details><summary>docs/archive/AKUMA_AMD64_STEP5B_SLICE4_PROCS.md (3)</summary>

- L51: The staging array existed because the *swap* was deferred to 'run_process',
- L214: reported a whole image outstanding. 'sys_waitpid' is now the fourth site,
- L290: &gt; **Follow-up, 2026-09-09** — the 'CLONE_VM'-sibling divergence this slice

</details>

<details><summary>docs/archive/AKUMA_AMD64_STREAMLINING.md (3)</summary>

- L458: &gt; live possibility, not fixed here.  _(maybe resolved)_
- L750: Still open: an unbound 0/1/2 is not "free" for allocation, so 'close(1);
- L827: followed, and where most of the "deferred, and here is why" notes live.

</details>

<details><summary>docs/archive/AKUMA_AMD64_STREAM_END_STALL.md (3)</summary>

- L96: explain 60 s — **which is the open question**: what makes the lap not happen, or
- L134: couldn't get clean numbers" — an **unresolved** 2026-08 sighting of the same
- L136: short-lived-connection shape". Closest prior art, and still open.

</details>

<details><summary>docs/archive/AKUMA_AMD64_TARGETED_SHOOTDOWN.md (1)</summary>

- L207: ## 7. Still open

</details>

<details><summary>docs/archive/AKUMA_AMD64_THREAD_LIFECYCLE.md (1)</summary>

- L153: ### OPEN: thread slots leak (the one still standing)

</details>

<details><summary>docs/archive/AKUMA_AMD64_TLB_SHOOTDOWN.md (1)</summary>

- L113: ## Costs and things not done  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_TOUCHPAD_I2C_HID.md (3)</summary>

- L8: metal run"); the remaining work is the rio-side consumer.
- L21: / rio fork: read the node, winit pointer events, cursor sprite / **open, lower priority** — the console bridge (below) covers terminal programs without it /
- L213: **Known gaps, on purpose:**

</details>

<details><summary>docs/archive/AKUMA_AMD64_UD_CRASH_CONTAINMENT.md (2)</summary>

- L88: docs/README.md's symptom matrix already had this one, unfixed: "'execve' of a
- L103: ## 4. Verification, and what is still open

</details>

<details><summary>docs/archive/AKUMA_AMD64_USB_XHCI.md (7)</summary>

- L34: deferred, which removes the one thing that forced EHCI. 'XUSB2PRM = 0' on this
- L277: fails on the metal is **open**; the '[xhci] .. &lt;step&gt;' breadcrumbs will say, and
- L452: two were never A/B'd against the same binary, so what it was is unresolved. If
- L649: **Slice 4 (why it stalls) is still open — it needs the metal.**
- L752: works and larger ones intermittently stall is still the open question —
- L862: reading DISABLED while transfers were working minutes earlier is unresolved
- L954: transfers were working minutes earlier is unresolved" — resolved: it was  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_AMD64_WGPU_KERNEL_WORK.md (1)</summary>

- L6: userspace had to work around, and what is still open, in priority order.

</details>

<details><summary>docs/archive/AKUMA_ARCH_CONVERGENCE_RESEARCH.md (5)</summary>

- L72: EL0 and init are B21 A5.3→A7, still open. So the *pattern* is proven; the aarch64
- L217: (framebuffer policy, rump proxy — see open questions).
- L289: / M0 / Census like B21 A0: classify every 'crates/akuma-kernel-amd64/src' file arch/neutral/neutral-after-hook with line counts; freeze baseline battery outputs under KV…
- L335: 'platform-firecracker' map) light up with no code change — B21's open question 1
- L382: ## 8. Open questions

</details>

<details><summary>docs/archive/AKUMA_BOOT_EXTRACTION.md (2)</summary>

- L174: ## 5. Open: no '/dev' at all
- L225: or a dedicated syscall path for raw block access — scoped as a follow-up, not

</details>

<details><summary>docs/archive/AKUMA_ECOSYSTEM_DEMO.md (5)</summary>

- L193: rerun from step 2; the dry run is not done until step 3 needs no environment fixes).  _(maybe resolved)_
- L258: / box child stdout not delivered to an ssh exec session / none (no errno; output lost) / **open**, E1 in 'proposals/FUTURE_WORK.md'; 'eco-box' captures via a file /
- L259: / path through a relative directory symlink inside an overlay box fails / 'ENOENT' / **open**, E2; the scala root has no such link /
- L260: / no outbound route / NAT for the bridge / n/a / **open**, E3 /
- L336: / **guest 1 kernel panic** during 'scala-cli run' of the board / 'execve' from a non-leader thread replaced the group's address space under its siblings ('[execve] from …  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_EXEC_AUDIT.md (3)</summary>

- L260: ## 5. Not fixed: six '&self' → '&mut' field casts  _(maybe resolved)_
- L474: ## 5c. Deferred: sweep for both patterns tree-wide
- L620: 5. RawWaker → 'alloc::task::Wake', opportunistically. Still open — and weaker

</details>

<details><summary>docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md (10)</summary>

- L199: and the RELR shared-page accumulation live ('instr_abort_relr_wedge', still open).
- L341: **What §3.5 buys:** 'UserAddressSpace''s 'Drop', the deferred-free path
- L657: ### 7.8 Still open
- L673: Follow-up on the same day. The audit §7.8 deferred turned out to be four sites of
- L758: ### 'akuma-pmm' is the harder one, and it is still open
- L775: 'akuma-pmm''s 1,139 production lines inside a 'forbid' boundary. The open question
- L781: ## 8. Deferred work
- L789: Not deferred; noted here because it is the one piece of the leaf that did move.
- L888: ### 8.5 Also still open, from §7
- L893: - 'akuma-pmm''s 'forbid' — **closed as won't-do**, not open: §8.6 explains why the  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_EXTRACT_MMAP.md (1)</summary>

- L378: Built 2026-08-29, on top of stage 1 and answering §7's open question in the

</details>

<details><summary>docs/archive/AKUMA_EXTRACT_SYSCALLS.md (11)</summary>

- L287: 'UNSUPPORTED' ('passfd', 'syslog'; both are known gaps, not regressions),
- L293: was deferred rather than run on a contended host, since a starved run's
- L442: **So Finding A is fixed here rather than deferred.** 'EPILOGUE_IDENTITY' in  _(maybe resolved)_
- L526: **E. The handoff's open question was not one.** It flagged "'wrap' is 167 or
- L618: ### 7.8 What is still open
- L628: deferred-kill handling at the EL1→EL0 boundary may already cover it, and
- L638: readers to the audit as **OPEN** at 440 ns — was rewritten to say RESOLVED and  _(maybe resolved)_
- L639: to point at the two sweeps it actually deferred. The original text of this
- L677: / 'pthread_join' hangs forever / 'futex_wake' published only to the 'tgid=0' queue / 'an_unresolved_identity_is_degraded_not_shared', the namespace tests /
- L798: **Seven divergences from Linux were preserved, not fixed** — an extraction that  _(maybe resolved)_
- L886: the 150 ns floor the dispatch must keep, and deferred items 2–4.

</details>

<details><summary>docs/archive/AKUMA_FDTABLE_EXTRACTION.md (1)</summary>

- L92: ## The dup fix (recommendation, deliberately not done)  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_FIRECRACKER_AMD64.md (14)</summary>

- L670: ### 3.12.4 Known gaps in the syscall path
- L1977: ### 3.24.5 Deferred: the allocation audit
- L2478: result runs was left as an open question). It turned out to already work,
- L2680: The remaining, unfixed blocker is structural, not a bug: this target has no
- L2772: is dated and left as an open question rather than guessed at.
- L2806: change, is future work, not attempted here):
- L2836: ### 3.30.5 What's verified, and what's still open
- L2851: **Not fixed, and found while verifying this**: 'apk update' run to  _(maybe resolved)_
- L2984: still open below.)
- L2986: ### 3.31.5 What's verified, and what's still open
- L2996: Still open, in the order they block:
- L3035: 'chown'/'fchown' are not wired — noted, not fixed here) and, on the packages  _(maybe resolved)_
- L3101: Still open, in the order they'd block next:
- L3171: §8, still open.

</details>

<details><summary>docs/archive/AKUMA_FIRECRACKER_KVM.md (5)</summary>

- L218: ### 4. Open questions for the user
- L302: **Not done / open:** the I/O APIC (the guest's ACPI lists one at 0xfec00000; the Akuma guest uses the PIC), 'GET/SET_LAPIC'  _(maybe resolved)_
- L340: once); metal, see the stress result below. **What is still open:** nothing guards a kernel stack (a canary or guard page
- L420: *Not yet done:* a thread that is blocked or running elsewhere when its mask changes migrates at its  _(maybe resolved)_
- L463: 'kvm0''s NIC guest ('kvmnet', a polled model device) works. Next steps: have the guest print from its netpoll/virtio-net path

</details>

<details><summary>docs/archive/AKUMA_FIRECRACKER_TERRAFORM.md (8)</summary>

- L95: **Deferred, not attempted.** Worth doing before any long-running work settles on
- L202: ## 6. Deferred: the boot partition
- L420: The four items §10.2 lists as "not done" were the point of this session. The  _(maybe resolved)_
- L450: **1. Inbound RX works.** This was the one open question, and the DHCP handshake
- L465: only, and it fails silently above that" as an open blocker. It is not open: the
- L584: ### a1.metal, for the deferred probe (§3) — and it constrains the region choice
- L617: This is what makes §3's deferred a1 probe worth more than its $0.10: 'a1.metal' is
- L660: Not done yet:  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_FROM_SCRATCH.md (2)</summary>

- L124: ## 4. Why this is the right next step and not a detour
- L138: **The SMP corruption is not fixed**, and it is the reason to sequence this  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_GENTOO.md (6)</summary>

- L27: compat is proven in the box, a bare-metal Gentoo root is a follow-up.
- L40: *not done*, whatever 'ls' says (aria2 preallocates, so size alone lies).  _(maybe resolved)_
- L72: Interactive ssh into the box is still open: openssh's 'openpty()' opens
- L204: **Open: p3 is 'not clean'.** 'e2fsck -fn' from Pop reports HTREE damage in
- L296: **Interactive login (follow-up, same day).** 'ssh -p 4444 root@…' with no command
- L352: **Still open:** the native 'emerge' then stops at "Emerging (1 of 1)": it forks

</details>

<details><summary>docs/archive/AKUMA_GENTOO_PORTAGE_CRASH.md (1)</summary>

- L48: **Still open:** the portage tree ('/var/db/repos/gentoo') is not synced, then

</details>

<details><summary>docs/archive/AKUMA_IPV6.md (1)</summary>

- L44: / Address autoconfiguration / the open question / no DHCPv6 in smoltcp 0.12; static config is easy, SLAAC depends on how completely smoltcp handles router advertisements…

</details>

<details><summary>docs/archive/AKUMA_KERNEL_HOOKS.md (1)</summary>

- L127: Not done in this pass; recorded here and in the reference page's "Known  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_KVM_AKUMA_GUEST.md (2)</summary>

- L176: **Open (the guest, not the device):** the stall reproduces on the metal and under TCG with the gateway
- L250: ## Still open

</details>

<details><summary>docs/archive/AKUMA_KVM_LINUX_GUEST.md (1)</summary>

- L258: ## Still open

</details>

<details><summary>docs/archive/AKUMA_MMU_TARGET_ARCH_GATE_FIX.md (1)</summary>

- L66: 'akuma-exec' — neither of which this pass attempts. **Still open.**

</details>

<details><summary>docs/archive/AKUMA_MMU_X86_ADDRESS_SPACE.md (1)</summary>

- L136: separate follow-up.

</details>

<details><summary>docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md (54)</summary>

- L81: Per the research doc's open question 1, this is the *sibling* shape (own crate, own API), not an
- L95: payoff and the next step, one consumer at a time. The AArch64 compile-time map
- L129: / E3 mm / walk should use 'akuma-user-tree' plans / 'mm.rs' already consumes 'akuma-mmap' and 'akuma-syscalls-mem' (18 + several sites); the **fork walk is 'share_parent…
- L137: / M6 scheduler / AArch64 full kernel onto hooked path / 'akuma-threading/hooked' exists and 'akuma-tokio-kernel-aarch64' uses it; the root kernel does not / **open** (D3…
- L138: / M7 kit / 'akuma-kernel' front + machine halves / not started / **open** /
- L139: / amd64 on 'akuma-kernel-core'/'-glue' / "consumes none" / still true ('crates/akuma-kernel-amd64/Cargo.toml' has neither) / **open — the real remaining convergence** /
- L219: that is D4's sequencing question, still open.
- L261: - **AArch64 compile-time map -&gt; 'Facts': not done, deliberately.** 'akuma-kernel-core::platform' is  _(maybe resolved)_
- L282: testing is **deferred**; until then amd64 does not build it by default, and the feature must not
- L311: - **D4, D5:** still open; neither blocks M1/M2.
- L339: on another core and 'close_all' has emptied the table first. Open: the fix needs a child that blocks
- L380: / glue 'rump_proxy' / D1: universal optional feature / not built / deferred (D1) /
- L450: **Open finding (pre-existing, not this slice): 'sigregs' can hang at SMP=4 under TCG.** Seen on the
- L585: open 'sigregs' SMP=4 TCG hang. **Not done, deliberately:** the 'CLAUDE.md' crate counts are stale (the  _(maybe resolved)_
- L763: **Open:** the two epoll findings above; the epoll/futex suites under both schedulers; the 2-in-180 ssh
- L764: session failures; signal and deferred-kill delivery on the IRQ-return path (W3c).
- L766: ### W3b follow-up — what the epoll test measures, the old kernels, and a hooked TLS bug (2026-11-10)
- L794: wake is slow" reading in the correction above is withdrawn: those were drain times. **Open:** the test
- L846: the 1 ms mode is load-only. **Open:** rerun both kernels with the same post-boot wait, and find
- L869: **Open:** rewrite 'test_epoll_socket_waker'; the epoll/futex suites under both schedulers; hooked BKL
- L916: The change was reverted. Which stretch holds the lock is still open. The holds are tagged 511
- L920: ## W3c — signal and deferred-kill delivery on the hooked IRQ return (2026-11-10)
- L932: - before the BKL is released, 'deliver_on_irq_return' consumes a deferred kill, then one
- L976: **Open, before hooked becomes the default:**
- L1025: **Open:**
- L1079: **Open:**
- L1124: **Open:**
- L1156: **Open:**
- L1164: still built by '--no-default-features' without it, until the next step deletes it.
- L1168: - the rump devbox ('build_devbox.sh', deferred): rump with hooked has never been tested;
- L1190: **Open:**
- L1228: hooked scheduler too. Neither is booted here: rump is deferred, and the Firecracker platform
- L1443: 'proposals/ONE_BIG_KERNEL_LOCK.md', deferred to the big merge. Taoxichuan images are
- L1741: Typing guest memory is deferred (it needs the ryzen). In the meantime the hypervisor moved out of
- L1759: ### Future work: host tests for 'akuma-kvm-amd64'
- L1782: ### Future work: a sweep of crates without host tests
- L1973: Pinned, not fixed:  _(maybe resolved)_
- L2025: / deferred kill / via 'should_interrupt_blocking_syscall' / 'has_pending_kill' / kept; amd64's 'drain' posts it with 'request_thread_kill' /
- L2027: / group exit flag / 'thread::should_leave_now()' / absent / not needed: 'drain' posts the deferred kill to the same threads /
- L2049: out. It is open, and it predates both slices. The next step is the same freeze under
- L2088: **Next step**: make 'heap_size_for' (or its AArch64 equivalent, once compared) scale from actual
- L2134: posted deferred kill does at the EL0 boundary. amd64 does not use glue's 'sys_exit_group'.
- L2167: **Future work** (found by this slice's gates, not fixed in it):  _(maybe resolved)_
- L2190: out after a 2 s grace. Now only the teardown's deferred kill does, and only once some thread of the
- L2194: - **'tkill' of an unknown in-range tid returns 0** (Linux: 'ESRCH'). Pinned, not fixed: it was the  _(maybe resolved)_
- L2230: this step does the same. This is an sshd/session item, logged here for the fd follow-up.
- L2310: - 'akuma_syscalls_linux::flags::open::{may_read, may_write}': the access-mode rule, with 'O_PATH'
- L2414: Open: AArch64 has no 'MAX_ARG_STRLEN' cap on an envp string ('envmax''s 'envstr'; argv strings are
- L2793: Not fixed by this: '^C' in an **interactive ssh** session on amd64 ('ssh -tt' with a typed 'sleep 30')  _(maybe resolved)_
- L2870: ### Slice 5: mm (deferred, 2026-11-11)
- L2962: / SMP / Floor (RAM, passes) / Next step down (fails) /
- L2996: next step, and is probably worth it before trusting the exact low-end numbers (24/24/28 MiB) too
- L3031: open. Left here rather than chased further; the next step is reproducing against the actual
- L3035: ## Future work: Taoxichuan's RAM floor and the auditable line count (2026-11-12)

</details>

<details><summary>docs/archive/AKUMA_NET_ISSUES.md (2)</summary>

- L795: ## 11. Not done  _(maybe resolved)_
- L873: from the netpoll drain at a high-water mark — is still the fix, and is NOT done.  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_NET_LOOPBACK_BIND_EXPOSED.md (5)</summary>

- L8: 2) were not done, since the kernel fix makes them unnecessary for this  _(maybe resolved)_
- L40: / the HP box (bare metal, RTL8169), '192.168.1.120' / amd64 'b4a55330' / a Mac on the same LAN / **open** /
- L41: / Firecracker guest on the ryzen host, '192.168.1.50' (proxy-ARP'd onto the LAN) / amd64 / the same Mac / **open** /
- L42: / Firecracker guest inside Lima 'fc', '10.0.2.15' / AArch64 '66416bc9' / the 'fc' VM, over 'tap0' / **open** /
- L45: that port. It's still open to its host.

</details>

<details><summary>docs/archive/AKUMA_NET_SPLIT.md (3)</summary>

- L5: and where does its 'unsafe' live. The split itself is **not done** — §5 is the  _(maybe resolved)_
- L350: ## 5. The split (planned, not done)  _(maybe resolved)_
- L879: §5.1c's option (b), still open now that the Device impls live beside the arena.

</details>

<details><summary>docs/archive/AKUMA_PAUSE_IGNORES_GROUP_EXIT.md (2)</summary>

- L28: sibling to leave with a **deferred kill request** ('request_thread_kill' -&gt; 'PENDING_KILL'),
- L51: ## Still open

</details>

<details><summary>docs/archive/AKUMA_PRIMITIVES_EXTRACTION.md (1)</summary>

- L132: ### The 'isb' divergence is preserved, not fixed  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_PROC_ENUMERATION_HANG.md (1)</summary>

- L73: ## Next steps

</details>

<details><summary>docs/archive/AKUMA_PROC_TREE_UNIFICATION.md (2)</summary>

- L142: ## 6. The Bun / Claude Code symptom: still open
- L188: **Not done:**  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_RYZEN_HERD_STOP_GUEST_RESET.md (1)</summary>

- L37: ## Not done  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_SAFE_GUEST_ENTRY.md (1)</summary>

- L117: ## Still open

</details>

<details><summary>docs/archive/AKUMA_SCHEDULING_EXTRACTION.md (6)</summary>

- L3: **Date: 2026-08-19.** Follow-up to
- L30: &gt; 0.2-5 ms per futex call on hardware. **Open item 4, not open item 2.**
- L245: path. **Open item 4 (explicit-wake latency), not open item 2
- L373: **OPEN, not fixed.** 'NETPOLL_ITERS' is incremented only by the BSP netpoll loop  _(maybe resolved)_
- L413: The Redis re-run is the cheapest outstanding measurement in the tree: pure
- L445: 4. **Re-run the Redis matrix** (§8). Cheapest outstanding measurement in the

</details>

<details><summary>docs/archive/AKUMA_SELF_HEALING_PORT.md (7)</summary>

- L15: hid, what is fixed, and what is still open — kept apart from  _(maybe resolved)_
- L101: **Mitigated, not fixed:** 'Console::SCROLL_ROWS = 8' — a scroll now advances 8  _(maybe resolved)_
- L104: (a RAM shadow buffer, or a WC-aware block copy) is still open — see below.
- L151: ## Still open
- L248: The keyboard is deferred, which removes the one thing that forced EHCI (a
- L332: string — worth finding where. The next step after this is mounting a real
- L364: **Deferred 2026-09-06** in favour of the USB disk (which is xHCI-only). The

</details>

<details><summary>docs/archive/AKUMA_SELF_HOSTING.md (14)</summary>

- L336: &gt; **TODO (still open):** (a) compile+run hello.rs across 'MEMORY' 4→16 GB with
- L570: Still open / follow-ups:
- L849: **Next step (repro v2, staged but NOT yet run).** 'userspace/selfhost_repro/build.rs' was
- L932: #### 7g.1 — Refinement after futex tracing + binary analysis (June 19 2026, STILL OPEN)
- L1036: **Residual (still open).** A heavier multi-threaded rustc compile (building
- L1204: debug symbols to name the function). **Still open.**
- L1229: - **The 'x29' corruption (still open).** Disassembly showed the faulting store
- L1287: ~1/build rate. **STILL OPEN** — see the hand-off in §7k.4.
- L1289: #### 7k.4 — HAND-OFF: the intermittent rustc register corruption (~~STILL OPEN~~ → **ROOT-CAUSED + FIXED in §7k.6**, June 22 2026)  _(maybe resolved)_
- L1368: Built the dedicated threading/signal-API conformance test set the §7 TODO called for
- L1403: **API non-compliances found — 2 FIXED, 1 REVERTED (it broke the self-host build!), 1 open:**  _(maybe resolved)_
- L1404: 1. **No RT-signal queue (STILL OPEN — it's a feature, not a small fix).**
- L1410: current consumer needs it (rustc doesn't), so it's deferred to a dedicated pass with a
- L1547: &gt; Follow-ups: (a) the guard is a robust *safety net* (like the JIT workaround) — the

</details>

<details><summary>docs/archive/AKUMA_SELF_HOSTING_AMD64.md (20)</summary>

- L110: &gt; **Not done from the B1 box:** file-backed mappings. B3 (widen the x86  _(maybe resolved)_
- L146: &gt; **Not done from the A1 box:** 'thread.rs' (391 lines) and the 'smp.rs'  _(maybe resolved)_
- L303: &gt; daemon running on every one. Left open; it gates nothing and wants the
- L671: &gt; **So C1's remaining work is two independent pieces, neither of them "5c":**
- L759: &gt; falsified against the unfixed code before being kept. Ring-3 witnesses on QEMU
- L842: &gt; the amd64 preamble and left open rather than fixed blind, for batch 2d's  _(maybe resolved)_
- L1092: │ deferred with reasons
- L1599: │ guessed at and unresolved since 09-12 morning), and 'ftruncate'
- L2195: Found beside it and still open: herd starting a service twice,
- L2269: the fleet was five cats. Still open: a 'kot' worker on the metal
- L2881: &gt; 'st_rdev' half is also outstanding — 'fd.rs''s 'encode_stat' fills it from
- L3063: 2026-09-20..24 diagram further up lists as "still open":
- L3065: - **6, a writer on a dead session channel (2026-09-12): half-fixed, still open.**  _(maybe resolved)_
- L3181: Still open, smaller: the shell on the persistent root has no 'PATH' (bare
- L3193: started — see "What's deliberately not done" below.  _(maybe resolved)_
- L3258: ### What's deliberately not done  _(maybe resolved)_
- L3266: deferred — "don't ask it join ryzen just yet, i am working on new version
- L3521: &gt; receiver restarted rather than the cursor moved. The claim in "Still open"
- L3603: remaining open question, and the counter is how it gets answered.
- L3622: ### Still open

</details>

<details><summary>docs/archive/AKUMA_SLOT_TABLE_EXTRACTION.md (2)</summary>

- L112: and 'test_hooks::stamp_unresolved' drop their '*_ptr' writes the same way.
- L182: — the deferred RETIRED→FREE reclamation and its cooldown, which is the

</details>

<details><summary>docs/archive/AKUMA_SMP_SHARED_SPLIT.md (1)</summary>

- L212: **Note this changes 'devbox'** (rump-only, deferred), which listed 'sc-reboot'

</details>

<details><summary>docs/archive/AKUMA_STALE_TID_ZOMBIE_SLOT.md (1)</summary>

- L84: ## 5. Still open

</details>

<details><summary>docs/archive/AKUMA_SYSCALLS_TOKIO_DEP_ANALYSIS.md (1)</summary>

- L358: 97,853 -&gt; **92,404** (-5,449, against -4,000 to -4,600 estimated; 'embedded-io-async' went with it). Rows 5-7 and 9-11 are not done;  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_SYSCALL_PERFORMANCE_AUDIT.md (6)</summary>

- L5: verbatim, followed by the deferred follow-ups.
- L197: ## Deferred: audit for duplicate instrumentation machinery
- L199: **Deferred — pick up with the waste sweep above.** Reading the entry path for
- L222: ## Deferred: a per-syscall-path waste sweep
- L224: **Deferred — do after the 'current'-resolution fix lands and is re-measured.**
- L374: Observed once, pre-existing (also on the unfixed kernel): a guest read-back

</details>

<details><summary>docs/archive/AKUMA_TEAHOUSE_CONSOLE_EXPERIMENT.md (5)</summary>

- L169: - Not done: chain compaction and the planning prompt (both need a primary).  _(maybe resolved)_
- L196: - **Open: memory discipline for the agents** — per-container limits, how large
- L353: - **Known gaps:** no arrow/function keys ('kbd.rs'); busybox's 'ESC[6n' query
- L372: which are unrelated to this work and unresolved. '.good' was **not** promoted.
- L403: picture and keep the columns aligned; known gaps: skin tones are not drawn (the base emoji

</details>

<details><summary>docs/archive/AKUMA_THREAD_ASID_SHARE.md (1)</summary>

- L74: ('deferred_asid') beside its deferred frames.

</details>

<details><summary>docs/archive/AKUMA_TIME_EXTRACTION.md (9)</summary>

- L6: (that doc's "Open questions" 1–3 are all answered here) and executes the
- L7: deferred-audit row of
- L80: deliberately re-entering the burn state teaches that). Future work if a
- L96: wearing a driver's filename (the deferred-audit row's words). Now:
- L122: 'akuma_exec::alarms' in a follow-up the same day — it is glue over
- L155: the natural next step is letting the alarm queue (now
- L161: 3. 'gic'/'gic_v3'/'ramfb'/'irq' extraction — the rest of the deferred-audit
- L163: 4. **Deferred: sweep manual 'IrqGuard::new()' placements to
- L186: § "Deferred audit" — the extraction's pre-existing blueprint.

</details>

<details><summary>docs/archive/AKUMA_USER_ACCESS_GATE_FIX.md (3)</summary>

- L1: # 'akuma-user-access' gated for x86_64, with the real mechanism left open — 2026-09-05
- L93: ### The real next step, not attempted here
- L134: approach as the real next step. 'akuma-el0-entry' is a newly-found,

</details>

<details><summary>docs/archive/AKUMA_USER_ACCESS_X86_FIXUP.md (2)</summary>

- L144: user copy: a lazy-region fault inside the loop is demand-paged, not fixed up [OK]  _(maybe resolved)_
- L158: honoured. "demand-paged, not fixed up" copies out of an armed lazy region and  _(maybe resolved)_

</details>

<details><summary>docs/archive/AKUMA_VIRTIO_IN_TREE.md (1)</summary>

- L75: ## Still open

</details>

<details><summary>docs/archive/ALLOCATOR_REALLOC.md (3)</summary>

- L1: # Allocator Realloc Issue and Deferred Free Queue
- L30: ## Solution: Deferred Free Queue
- L60: - Flush the deferred free queue (call 'munmap' on all queued buffers)

</details>

<details><summary>docs/archive/ALLOC_PRINT_AUDIT.md (6)</summary>

- L7: landed, what was deliberately kept, and what is still open. §1-§6 are preserved  _(maybe resolved)_
- L287: each without inventing anything new. This is a mechanical follow-up, not
- L313: Follow-up pass, same read-only scope, prompted by a direct question: is
- L555: ### 7.5 Still open
- L561: follow-up in the doc.
- L580: 'db022be' too (17 'E0433's, 'file_page_cache'/'container' unresolved, mostly

</details>

<details><summary>docs/archive/AMD64_BOX_SPAWN_EXT_REGRESSION.md (1)</summary>

- L30: Open: the dependency-analysis row should have listed amd64's remaining glue loader users before gating; any other

</details>

<details><summary>docs/archive/AMD64_CLAUDE_LOGIN_INPUT_STUCK.md (1)</summary>

- L175: ### Next steps, in order

</details>

<details><summary>docs/archive/AMD64_CONSOLE_NONBLOCK_READ.md (1)</summary>

- L226: terminal-capable 'ProcessChannel'** — the deferred '/proc/&lt;pid&gt;/fd/0' +

</details>

<details><summary>docs/archive/AMD64_CRUSH_ORPHANS_AFTER_PTY_CLOSE.md (1)</summary>

- L42: them. Related, already-open: the stale-tid kill in 'docs/README.md''s symptom table, the

</details>

<details><summary>docs/archive/AMD64_EXT2_CORRUPTION_HANDOFF.md (3)</summary>

- L26: forming a theory** ('ls docs/archive / grep -i -E 'ext2/fpcache/inode/deferred/cache'').
- L73: Neither has inode pins or deferred frees — add those ('akuma_primitives::inode_pin')
- L93: * **Deploying a kernel while this bug is open:** push to a *fresh* name (no

</details>

<details><summary>docs/archive/AMD64_FB_BACK_BUFFER_DEADLOCK.md (1)</summary>

- L57: ## Still open

</details>

<details><summary>docs/archive/AMD64_FORK_VS_THREADS_SEGV.md (1)</summary>

- L269: In progress / not yet done:  _(maybe resolved)_

</details>

<details><summary>docs/archive/AMD64_J4_BUILD_CRASH_HUNT.md (3)</summary>

- L242: ## 5. Left open
- L246: &gt; constant 27 (the 'munmap' shootdown). Still open: a larger metal sample, '-j8',
- L253: 'rust-lld', always in the first ~30 crates. Open: concurrent cold-read race vs

</details>

<details><summary>docs/archive/AMD64_KERNEL_UNSAFE_AUDIT.md (1)</summary>

- L67: Still open from the soundness list: 'read_val''s 'T: Copy + Default' still admits 'bool' and 'char',

</details>

<details><summary>docs/archive/AMD64_LAPIC_PERIODIC_SWITCH_QEMU_LIVELOCK.md (1)</summary>

- L85: ## Still open

</details>

<details><summary>docs/archive/AMD64_MAC_BUILT_KERNEL_METAL_HANG.md (2)</summary>

- L116: ## How to find it (not done)  _(maybe resolved)_
- L164: **This removes a variable; it does not prove the hang gone.** The next step is a

</details>

<details><summary>docs/archive/AMD64_METAL_CONTROL_RUN.md (1)</summary>

- L61: Not done: the AArch64 half of 'akuma-mmu' (needs the lima VM, stopped); the  _(maybe resolved)_

</details>

<details><summary>docs/archive/AMD64_PHYSMAP_SELFTEST_LAST_PDE.md (1)</summary>

- L174: **Not done, deliberately.** The stack still has no guard and still sits below the page  _(maybe resolved)_

</details>

<details><summary>docs/archive/AMD64_PROXY_ARP_UNREACHABLE.md (1)</summary>

- L18: / Mac (192.168.1.203) / guest '.50:2222' / **OPEN in 0.06 s** /

</details>

<details><summary>docs/archive/AMD64_SMOLTCP_STALE_HANDLE_KILL9_WEDGE.md (1)</summary>

- L71: / 'amd64_trials.py --local-only --smp 4' / 740 passed, 1 failed — the pre-existing 'mmap: the lazy path was actually taken' baseline failure (§8.4 of 'AKUMA_AMD64_NO_SLO…

</details>

<details><summary>docs/archive/AMD64_SPAWN_SELFTEST_SMP_RACE.md (1)</summary>

- L3: **Status: ROOT-CAUSED 2026-09-25, not yet fixed.** The kernel is fine; the  _(maybe resolved)_

</details>

<details><summary>docs/archive/AMD64_SSH_CLIENT_TOFU_PROMPT_CR.md (1)</summary>

- L107: ## 6. Still open, and adjacent

</details>

<details><summary>docs/archive/AMD64_SYSCALL_ABI_REGISTER_CLOBBER.md (1)</summary>

- L193: is still open; the AArch64 side saves 'q0'-'q31' for exactly this reason.

</details>

<details><summary>docs/archive/AMD64_THREE_CRASHES.md (1)</summary>

- L215: ## 6. Left open

</details>

<details><summary>docs/archive/AMD64_TLB_STUCK_MMAP_REGIONS.md (1)</summary>

- L25: '/src/github.com/netoneko/akuma' under an ssh PTY. Unfixed: 2/2 wedged, 13

</details>

<details><summary>docs/archive/AMD64_TRASHCAN_ISSUES.md (3)</summary>

- L616: only open question here.
- L948: ## 9. herd did not restart a killed 'sshd' — **OPEN (2026-09-24)**
- L1008: **Next step.** Reproduce in the local amd64 QEMU rig, which costs no reboot of

</details>

<details><summary>docs/archive/AMD64_UNSAFE_REDUCTION_TRAP_MSR.md (7)</summary>

- L76: ## Things found on the way that are not fixed  _(maybe resolved)_
- L131: compact them). **Still open**: lookups are a linear dirent scan, so a
- L134: non-htree directories; the proper followup is **htree ('dir_index') lookup
- L139: still open: directories grown by Akuma's own 'add_dir_entry' can end with an
- L156: and layout-sensitive failures are the open question of
- L161: ## Not done, and why  _(maybe resolved)_
- L184: ## Round 3 (2026-10-19): items from "Not done" that are now done  _(maybe resolved)_

</details>

<details><summary>docs/archive/AMD64_WIFI_JOIN_LATENCY.md (1)</summary>

- L43: ## Next step

</details>

<details><summary>docs/archive/AMD64_YIELD_POLLER_SHOOTDOWN_DEAF.md (1)</summary>

- L67: ## Still open

</details>

<details><summary>docs/archive/APK_OTMPFILE_DIR_FD.md (1)</summary>

- L126: printf("tmpfile open: ret=%ld errno=%d (%s)\n", r, errno, strerror(errno));

</details>

<details><summary>docs/archive/ARCHITECTURE_QUESTIONS.md (1)</summary>

- L96: package set runs on Akuma unmodified. This is deferred (post-M1); see

</details>

<details><summary>docs/archive/ASID_EXHAUSTION_TIGHT_THREAD_LOOP.md (9)</summary>

- L4: **Status:** **OPEN** — root-caused and measured, not fixed. Pre-existing; reproduces  _(maybe resolved)_
- L85: So ASID return is gated by the same 10 ms deferred reclaim as everything else, and
- L100: 10 ms deferred path, and a tight loop outruns it.
- L114: faster than the deferred path returns them", and the second is reachable from
- L179: not need §6.1's enumeration. It widens the pool so the existing 10 ms deferred path
- L184: ### 6.2 The precondition for anything that reclaims sooner — unresolved
- L196: stack-sharing races). So the open question is whether the older proxy is still
- L214: big enough that the deferred path has time to work. That is the change to make
- L239: faster than the deferred path returns them", and only the second is reachable

</details>

<details><summary>docs/archive/B12_AARCH64_SLOT_TABLE.md (1)</summary>

- L66: ## Open questions

</details>

<details><summary>docs/archive/B21_TOKIO_KERNEL_AARCH64.md (1)</summary>

- L125: ## Open questions

</details>

<details><summary>docs/archive/BENCHMARK_PERFORMANCE_ATTEMPT_0.md (2)</summary>

- L73: Docker VM against a 4-core devbox and flagged that as unresolved; it is resolved  _(maybe resolved)_
- L586: §5 left a question open: Akuma's forwarded arm runs at ~35 % of Linux with four

</details>

<details><summary>docs/archive/BKL_DRIVERS_CARVE_OUT.md (1)</summary>

- L69: goal is already partially met and that the remaining work is Phase 7 territory:

</details>

<details><summary>docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md (6)</summary>

- L526: self case only. Needs deferred reclamation: epoch/RCU (Phase 8 already floats RCU), or
- L638: 'lookup_process'/'current_process' deleted; deferred reclamation for the free path
- L1049: stand: thread-slot reclaim FIXED (§11.7), 'wait'/SIGCHLD still open (§11.3).  _(maybe resolved)_
- L1059: ('grep -r SIGCHLD' finds only clone-flag parsing). STILL OPEN (that doc §11.3); it makes
- L1120: - [~] IRQ handlers BKL-free — **deferred to Phase 7a**. All virtio devices are polling;
- L1139: - [ ] 7e process table: field grouping + locks, ~274 accessor sites, deferred reclamation

</details>

<details><summary>docs/archive/BKL_MM_CARVE_OUT.md (1)</summary>

- L195: ## 6. Status and next steps

</details>

<details><summary>docs/archive/BKL_PHASE7A_TIMER_IRQ_CARVE_OUT.md (2)</summary>

- L23: concurrent pair could restore DAIF while a critical section was still open
- L211: IRQ-handler goal was deferred from Phase 6 to Phase 7.

</details>

<details><summary>docs/archive/BKL_PHASE7B_PPOLL_CARVE_OUT.md (2)</summary>

- L107: still open. Either way, the healing tripwire worked as designed; what it revealed is that
- L182: extension + deferred reclamation for the free path). Piece 2 of this phase is not

</details>

<details><summary>docs/archive/BKL_PHASE7D_THREAD_CONTEXTS.md (1)</summary>

- L78: core — with no synchronization at all. This is accepted, not fixed: both are debug/stat  _(maybe resolved)_

</details>

<details><summary>docs/archive/BKL_PHASE7E_ACCESS_HALF.md (3)</summary>

- L31: structurally outlives the RETIRED→FREE deferred reclamation the Free half
- L56: sibling's 'Arc' clone disappeared with it. The Free half deferred that drop
- L61: released only by the deferred collector. A peer blocked on such a pipe hangs

</details>

<details><summary>docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md (7)</summary>

- L1: # Phase 7e ("Free" half): deferred process-table reclamation
- L5: deferred collector ('reclaim_retired_processes') always run, on every profile.
- L11: ['BKL_PHASE7_AUDIT.md'](BKL_PHASE7_AUDIT.md) §5: deferred reclamation for
- L94: 'reclaim_terminated_slots' retry, and for the same stated reason: deferred
- L294: 'kill 4x drift=0p' once the test drives the deferred reclaim (§3b) — confirming
- L329: a *new* pid was never the hazard (a fresh 'Process' has no outstanding
- L342: of this exact bug class (deferred cleanup starved under load until the

</details>

<details><summary>docs/archive/BKL_PHASE7F_OPTOUT_LIST.md (8)</summary>

- L68: / the deferred thread-kill at the EL1→EL0 boundary / for an opted-out excursion, closes the window and takes the lock before 'mark_thread_terminated' + the terminal yiel…
- L142: conversion makes getrandom join that class. **Follow-up flagged**: fold
- L147: self-neutralize to a nested depth bump (open: +1 and an idempotent no-op release;
- L205: §2's flagged follow-up, closed before any tranche-2 conversion.  _(maybe resolved)_
- L250: Two further deviations found while reading, both fixed here rather than deferred:  _(maybe resolved)_
- L427: getting recycled while a window was still open"), reproduced deterministically for
- L547: After tranche 2 the traversal's remaining work is no longer "audit each syscall" but
- L745: **Not done: the standing 'bkl_smp_regimen' contention A/B.** Every prior tranche carries  _(maybe resolved)_

</details>

<details><summary>docs/archive/BKL_PHASE7_AUDIT.md (6)</summary>

- L224: 'Box::drop' in 'unregister_process' needs deferred reclamation (an epoch/RCU scheme, or
- L262: still open. The BKL is what makes it safe today.
- L352: separate and still open.
- L377: under a regimen whose whole point is fork/exec churn. Left in place, not fixed here:  _(maybe resolved)_
- L449: - *Free* — deferred reclamation for 'unregister_process''s 'Box::drop', which no existing
- L559: §2 is what deferred the IRQ-handler goal to this phase.

</details>

<details><summary>docs/archive/BKL_RUSTC_SCALING_BASELINE.md (3)</summary>

- L48: &gt; which closes §6 follow-up 3.
- L182: follow-up and the thing that would separate "BKL serialization" from "host
- L185: ## 6. Follow-ups this baseline implies

</details>

<details><summary>docs/archive/BKL_VFS_CARVE_OUT.md (10)</summary>

- L29: §11.7; 'wait'/SIGCHLD still open §11.3). §16–§18 (2026-07-31) found and fixed a profiler attribution  _(maybe resolved)_
- L235: until a cleanup pass runs. In deferred mode ('DEFERRED_THREAD_CLEANUP = true')
- L275: ## 7. Remaining work
- L562: Same-family latent bug, documented not fixed: 'MADV_DONTNEED' zeroes mapped pages  _(maybe resolved)_
- L566: ### 10.4 Pre-existing llama-on-smp-shared issues found on the way (not fixed)  _(maybe resolved)_
- L848: ### 11.8 Still open
- L850: - **'wait' still hangs** (§11.3). Unfixed — it needs SIGCHLD delivery, which is a larger piece
- L1248: worth it at all was an open question. Converting both together (they share the exact same
- L1327: doesn't yet have a friendly name for tag 53; cosmetic only, not fixed here (out of scope for a BKL  _(maybe resolved)_
- L1505: - 'smp-shared.md''s "Separate issues surfaced 2026-07-20" section documents an **unresolved**

</details>

<details><summary>docs/archive/BOOT_STACK_BUG.md (1)</summary>

- L108: ### Future Work

</details>

<details><summary>docs/archive/BOOT_SUITE_PMM_DEFERRED_RECLAIM.md (3)</summary>

- L1: # Boot-suite PMM assertions vs. deferred process reclaim — 2026-08-02
- L9: deferred collectors — none of which run during the suite.
- L87: diagnostic for frames parked in 'SHARED_L0_TABLE''s deferred-free entries) so

</details>

<details><summary>docs/archive/BOX0_ONLY_AUDIT.md (3)</summary>

- L9: Follow-up: items 1–3, 5, 7 and 11 were addressed. Kernel log reads, clears and
- L58: This section records the original audit findings. The follow-up status at the top of this document and the containers
- L120: * Not done: a runtime probe of any candidate; fuzzing the box rule; the AArch64-only paths.  _(maybe resolved)_

</details>

<details><summary>docs/archive/BOX_BUILD.md (8)</summary>

- L99: re-export them (open question 1).
- L118: COPY --from=s read stage s's merged view (open question 2)
- L213: / 12 'save'/'tag'/'rmi'/GC / 'imgcmd.rs'. 'box pull' now keeps blobs and **verifies** layer and config digests, and writes 'manifest.json' (open question 1: yes) /
- L215: Open question 2 ('COPY --from' needs a merged view) was answered the second
- L273: trashcan: 12 of 12 runs 'uid=0', files '0:0'. Not fixed: a *real* signal with a  _(maybe resolved)_
- L284: documented in the reference and printed on every push, not fixed.  _(maybe resolved)_
- L316: ### Not done  _(maybe resolved)_
- L323: ## Open questions

</details>

<details><summary>docs/archive/BOX_DOCKER_COMPAT.md (2)</summary>

- L10: Plan and open follow-ups: ['BOX_RUN_OVERLAYFS.md'](BOX_RUN_OVERLAYFS.md).
- L233: / # exit → back at the host prompt, session still open

</details>

<details><summary>docs/archive/BOX_ISOLATION_SECURITY_FIXES.md (3)</summary>

- L99: unresolved '..' ('/containers/box1/../box3' passes 'starts_with' and then
- L150: / 2 / 'validate_nested_root' matches on a **component boundary**, requires an absolute path, and rejects unresolved '.'/'..' / 'box_mod/hierarchy.rs' /
- L208: / 'akuma-exec::box_mod::hierarchy::tests' (+3) / the sibling-prefix bug ('/containers/box10'), unresolved '..'/'.', relative roots /

</details>

<details><summary>docs/archive/BOX_PTY_INTERACTIVE_SHELL.md (3)</summary>

- L64: process is not yet confirmed clean; needs follow-up).
- L66: ## Follow-up (2026-06-25): the built-in ':2222' shell needed the same pty=true
- L138: **Next step:** instrument 'proc.context.pc' at the fork set-site and in the

</details>

<details><summary>docs/archive/BOX_RUN_OVERLAYFS.md (4)</summary>

- L42: gaps found while running the ladder are written up under "Follow-ups" — neither
- L116: A tmpfs upper for '--rm' needs an inode-namespacing scheme; deferred.
- L241: ## Follow-ups
- L260: - No layer GC. 'box rmi' / prune is a follow-up.

</details>

<details><summary>docs/archive/BUG_FIX_LIST.md (55)</summary>

- L199: - The syscall epilogue re-used the prologue's process identity across the whole dispatch (Finding A of 'IDENTITY_CACHE_SMP_REVIEW.md', left open there), reachable when '…  _(maybe resolved)_
- L202: - 'poll'/'select''s finite timeout was a lap counter ("200 laps per millisecond, capped at two million") on a target whose lap cost is not fixed; the wait machine reads …  _(maybe resolved)_
- L300: - glue's stdout 'write' applied ONLCR whenever the session's stdin was still open, without asking whether the channel was a terminal, so an ssh exec session raced the cl…  _(maybe resolved)_
- L440: - Follow-up: fork 'code_start' SIGSEGV
- L441: - Follow-up: vfork signal-interrupted wait
- L442: - Follow-up EL1 crash Bug 1: 'fork_process' missing THREAD_PID_MAP entry
- L443: - Follow-up EL1 crash Bug 2: 'clone_thread' plain EL1 store to a CoW-RO page
- L473: - 'realloc' calling 'munmap' directly hung the kernel; worked around via a deferred free queue
- L506: - 'test_mmap_file_oom_survives' halted the whole boot suite at its PMM-conservation assert because the deferred process-teardown chain (thread-slot recycle → 'unregister…  _(maybe resolved)_
- L556: (Fixes finding F-1 of 'EXT2_WRITEBACK_DESIGN.md' plus the keying half of D-9. D-9's capacity half is still open, and the doc's "a defect found and left alone" is by its …  _(maybe resolved)_
- L575: - 'drain_pending_ttbr_frees' 'swap_remove'd 'PendingAsFree' entries into a local 'Vec' and only then freed them, on a deferred-reclaim path documented as running on an a…  _(maybe resolved)_
- L763: - Go Build SIGSEGV four-bug follow-up (2026-03-26), incl. 'alloc_mmap' straddle bug
- L785: (Bug 5, NULL pointer deref, was still open at time of writing — not counted)
- L837: - 'write_stdin' ('process/channel.rs') used the same drop-oldest FIFO semantics as the (already-fixed) stdout side, so stdin past 'MAX_BUFFER_SIZE' (1 MiB) was silently …  _(maybe resolved)_
- L849: - 'exit_group' paid its full 2 s kill-grace per multithreaded process: a thread parked in an untimed 'FUTEX_WAIT' (or any yarn-driven blocking wait) was woken by the def…  _(maybe resolved)_
- L909: - the reap was not a reclaim drain site, so a **parent** collecting a child left a whole image outstanding — the child's own drain ran before the retire, the idle loop d…
- L1019: stale-core-id BKL wedge, and deferred-pending-kill fixes are counted there,
- L1022: - 'process_tests.rs''s 'test_kill_thread_group_reaps_futex_blocked_sibling' fabricated a futex-parked sibling from a bare claimed thread slot forced into WAITING — a sta…  _(maybe resolved)_
- L1033: - BKL fair-FIFO ticket leak — self-healing acquire added (root mechanism still open, but the deadlock itself is resolved)  _(maybe resolved)_
- L1040: - Terminated-thread lock leak — hard-terminated siblings stranded locks held mid-EL1 (fixed via deferred pending-kill)  _(maybe resolved)_
- L1084: - 'cleanup_process_fds''s 'strong_count == 1' gate never fired for externally-killed multithreaded groups (deferred reclaim), stranding pipes/sockets
- L1286: - 'sys_pselect6' had no 'should_interrupt_blocking_syscall()' check, which 'epoll_pwait' and 'ppoll' both had: a process blocked in 'select()' could not be interrupted b…
- L1391: - scrolling the framebuffer console redrew a near-full screen per printed line, which on a television reads as a tear sweeping down the picture; mitigated by scrolling 8…
- L1510: - Allocator leak — userspace deferred free queue too small during 'realloc'-heavy operations
- L1658: ### docs/archive/EXT2_WRITEBACK_FOLLOWUP_FIXES.md
- L1769: - Unauthenticated remote crash: 'process_unencrypted_packet'/'process_encrypted_packet' computed 'packet_len - padding_len - 1' from client-controlled bytes with no chec…  _(maybe resolved)_
- L1872: - Two threads sharing a fd ('CLONE_FILES' — any pair of 'pthread_create'd siblings) calling 'write()' close together could corrupt each other's output: 'sys_write' read …  _(maybe resolved)_
- L1902: (2026-08-26 follow-up only. Fixes A, B and D-lite of that section are write-deferral/zero-fill optimizations with no defect attached and are not counted; these two are t…
- L1914: - One lost pin set a single global overflow flag, which made 'is_pinned' conservative for every inode at once; overflow is now accounted per hash region, so a lost pin c…
- L2064: - 'inode_pin::is_pinned' still answered 'true' when it walked a full 32-slot window without finding the key, though 'acquire' only ever records a pin inside that window …
- L2067: - ext2 finished an unlink deferred by a pin only at the next unlink, rename or inode allocation, though the pin itself goes at an 'munmap', a 'close' or a reap, none of …
- L2098: - §3.7 'SCTLR_EL1.SA0' — KVM's reset value enables EL0 SP-alignment checking where QEMU's does not, killing every userspace binary deterministically at the same instruct…
- L2371: - 'suspend_until_signal' ('pause', 'rt_sigsuspend') never read the deferred kill request a group exit posts, so a sleeper outlived its group in a freed address space; it…
- L2405: (The 'akuma-exec' crate split itself is a refactor and is not counted; these are the defects it surfaced and fixed along the way. The 'akuma-bkl' dead-'RawRwSpinlock' de…  _(maybe resolved)_
- L2456: (Harness fixes only. Of the doc's two identity-cache findings, **Finding A — the syscall epilogue reusing the prologue's 'Process' across an open-ended dispatch — was fi…  _(maybe resolved)_
- L2689: - 'validate_nested_root' used a bare 'starts_with', accepting a sibling subtree ('/containers/box10' as a "child" of '/containers/box1') as well as unresolved '..' and r…
- L2734: - 'spawn_ext' loading a binary over 'HEAP_SLURP_MAX' took the deferred path, which needs lazy-region faults that amd64's '#PF' handler does not serve, and reset the mach…
- L2789: Also scanned 2026-10-20 (the amd64 metal, signal-return and '/proc' work, 16 archive docs added or edited). Counted above: AARCH64_SMP_HOOKS_RACE, AMD64_TLB_STUCK_MMAP_R…
- L2793: Also scanned 2026-09-30 (the 'even-more-cats' branch, 12 archive docs touched, ahead of closing it). Issues in the kot agent and the teahouse litter are out of scope for…  _(maybe resolved)_
- L2795: Also scanned 2026-09-24 (55 commits, 19 archive docs touched — the litter on the metal, the kot mesh and the teahouse — ahead of merging). Seven of those docs carry no c…  _(maybe resolved)_
- L2801: Also scanned 2026-09-02 (the 'oof-part-2' branch — 'akuma-exec' -&gt; '#![forbid(unsafe_code)]', ahead of merging it). AKUMA_EXEC_FORBID_UNSAFE is the campaign's complet…  _(maybe resolved)_
- L2807: Also scanned 2026-08-27 (the 'obviously-more-fixes' branch, ahead of closing it for the syscalls-refactor branch): AKUMA_SYSCALL_PERFORMANCE_AUDIT (the 'getpid' floor ta…  _(maybe resolved)_
- L2809: Also re-scanned 2026-08-25 (the 'more-fixes' branch, ahead of merging it): BENCHMARK_PERFOMANCE_ATTEMPT_1 (a benchmark record; its one actionable finding — nginx's lost …  _(maybe resolved)_
- L2817: Also re-scanned 2026-08-15 (the whole branch's unaudited archive docs, ahead of closing it): CARGO_HEAP_NULL_RC (the task brief for the cargo-null-'Rc' hunt; explicitly …  _(maybe resolved)_
- L2819: Also re-scanned 2026-08-12: CARGO_CRATES_IO_CONNECT_FAIL (root cause isolated, "fix not yet chosen" — five options, none landed; **fixed 2026-08-20, now counted under Ne…  _(maybe resolved)_
- L2821: Also re-scanned 2026-08-21 (the 'improve-portability' branch's archive docs, ahead of closing it — the list had not been touched in the branch's 19 commits): PORTING_POS…  _(maybe resolved)_
- L2823: docs/archive: 4MB_STABLE_AGENT, AI_DEBUGGING, ARCHITECTURE, BKL_DRIVERS_CARVE_OUT, BKL_PHASE7B_PPOLL_CARVE_OUT (piece 2 reverted after A/B caught real corruption), BKL_P…  _(maybe resolved)_
- L2833: Also scanned 2026-10-22 (the 'kvm-linux-rung' branch — the Linux guest under the SVM hypervisor, phases 5 through 8). Counted above: AKUMA_KVM_LINUX_GUEST, five fixes (t…
- L2835: Also scanned 2026-10-23 (the 'kvm-part-1' branch — the SVM hypervisor, 'kvm0' guests under herd, the '/dev/kvm' node, affinity, '/dev/net/tun', the goose-glm dev cycle, …
- L2839: Also scanned 2026-11-02 (closing 'rust-org-il-experiment', 536 commits and about 70 archive docs, most of them the Taoxichuan experiment and its cleanup). Counted above:…  _(maybe resolved)_
- L2841: Also scanned 2026-11-03 (the Infinity run on Taoxichuan and what it left open). Counted above: EXT2_DIRENT_TYPE_WITHOUT_FILETYPE, KVM_GUEST_4GIB_PVH_HANDOFF, UNIX_SOCKET…
- L2843: Also scanned 2026-11-08 (closing 'post-release-improvements', 136 commits, the Shanghai middleware work, the unsafe-reduction rounds and the ETXTBSY and signal fixes). C…  _(maybe resolved)_
- L2845: Also scanned 2026-11-16 (closing 'ouch', 29 commits ahead of main, 27 of them not merges: the pre-AMI security audit and its fixes, the userspace review of sshd, box and…  _(maybe resolved)_
- L2849: Also scanned 2026-11-18 ('more-improvements': real bubblewrap and codex's sandbox on private mount namespaces, the three platform kernels building again, herd, the secco…
- L2851: Also scanned 2026-11-20 ('ecosystem-demo-stage1', 39 commits: the box-0 policy follow-up, the merge gate, the ecosystem demo stages 1-2, Denver box wiring). Counted abov…

</details>

<details><summary>docs/archive/BUILDING_KERNEL_WITH_LOCAL_AI_PAIN_AND_SUFFERING.md (6)</summary>

- L62: looked wrong ('df /' showed only proc) and it went looking for 'curl' and the real root fs. Unresolved at the time
- L168: free of 16. 'kill -9' on the two leaders freed 10 GB and the same config then started cleanly. **Open:** an
- L319: '/root/infinity-no-aws' on Pop, run in a throwaway 'HOME'); then Akuma on ryzen. Open: 'check' prints 'mke2fs -d: no' in
- L404: - **Not done:** passphrase-protected keys (bcrypt-pbkdf plus aes256-ctr), other key types, 'ssh-keygen'-style certificates.  _(maybe resolved)_
- L405: - **Follow-up:** 'install.sh --vmm akuma' still derives its own key with an embedded RFC 8032 routine. With this client it could instead
- L481: in this build (its protocol has 'token_usage', nothing persisted); exact counts need a counting proxy between harness and ollama. (3) **Open:** after

</details>

<details><summary>docs/archive/BUILTIN_SSH_REMOVAL.md (3)</summary>

- L3: Follow-up to 'docs/archive/TRIM_FAT_SSHD.md', which argued the in-kernel SSH
- L157: ## Open: extreme double-binds port 22
- L271: ('crates/akuma-ssh/src/packet.rs:83'), still unfixed.

</details>

<details><summary>docs/archive/BUN_MEMORY_STUDY.md (3)</summary>

- L243: 2. **Deferred segment loading** ('src/elf_loader.rs'): instead of
- L666: **NOT YET FIXED.** This appears to be a bun-specific issue triggered by  _(maybe resolved)_
- L1160: ### Follow-up Crash: io_setup Ring Buffer Not Mapped

</details>

<details><summary>docs/archive/BUN_MISSING_SYSCALLS.md (2)</summary>

- L311: delivery (targeting a specific TID) is not yet implemented; process-level
- L699: ### Remaining Work

</details>

<details><summary>docs/archive/BUSYBOX_HASH_MISCOMPUTE.md (1)</summary>

- L631: ## Next steps

</details>

<details><summary>docs/archive/CARGO_CANNOT_REACH_CRATES_IO.md (1)</summary>

- L368: the archive doc names that as the cheapest next step and that text is stale.

</details>

<details><summary>docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md (3)</summary>

- L35: &gt; 2. **"The probe links dynamically against apk libcurl … the next step is to
- L137: The probe links **dynamically against apk libcurl** (c-ares, OpenSSL 3.5.7, system-compiled). To make the probe reproduce the failure we would need to rebuild it with 'c…
- L157: 1. **Rebuild the nettest probe with 'static-curl' + threaded-resolver features** ('userspace/nettest/rust/Cargo.toml') to reproduce the failure in a controlled binary. O…

</details>

<details><summary>docs/archive/CARGO_HEAP_NULL_RC.md (1)</summary>

- L26: *sharing* form rather than its rounding form. The rounding half is still open and

</details>

<details><summary>docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md (18)</summary>

- L16: Whether that was also cargo's null 'Rc' is a separate question, still open.
- L23: current state and open questions in the
- L342: ### 4.2 Deferred-flush audit — no missing flush found
- L435: ## 7. D1 fix plan (deferred — latent, not this defect)
- L520: ## 11. Next steps
- L676: ### 12.7 Open: the storm behind the fix
- L712: on *both* arms (nothing contends, so the unfixed kernel doesn't die of §12.2 first):
- L945: Both fit the evidence so far. Distinguishing them is the next step.
- L1006: doc against this evidence is the next step before adding new
- L1150: ### 13.7 Next steps (revised)
- L1171: is untouched and remains the open question**; the orphan leak was a real bug
- L1297: **Open:** whether this regressed since 2026-08-08 or was always there and §3.2
- L1383: free-page accounting rather than silently corrupting a process. **Follow-up:**
- L1385: match exactly — deliberately not done in the same change, since it is the risky  _(maybe resolved)_
- L1421: ## 13.10 STILL OPEN: a second premature-free route, and a second crash shape
- L1477: Next step is the same one that worked here: '[PMM-POISON]' already names the frame
- L1499: was re-allocated and zeroed before the read is **unresolved**. The discriminating
- L1500: step is cheap and not yet done: symbolize '0x30028b50' against the guest's  _(maybe resolved)_

</details>

<details><summary>docs/archive/CHILD_OF_EXITED_THREAD_UNREAPABLE.md (2)</summary>

- L69: Not fixed by this and still visible: the **first minute** after start is slow (first key 0.3-3.8 s, median up to  _(maybe resolved)_
- L73: Still open: the unit test 'reaping_a_buffered_child_keeps_its_output_but_not_the_child'

</details>

<details><summary>docs/archive/CLOCK_GETTIME_EINTR_ON_SIGNAL.md (2)</summary>

- L44: - **Not yet boot-verified.** The box this was found on was running the unfixed kernel, and there was
- L48: ## Still open

</details>

<details><summary>docs/archive/CLONE_AND_MUT_BUDGETS.md (1)</summary>

- L3: **Stability: B**: the gate and the counter are new; the counts are measured, the fixes the audit lists are not done.  _(maybe resolved)_

</details>

<details><summary>docs/archive/CONCURRENCY.md (1)</summary>

- L127: / 'IRQ_WORK_QUEUE' / 'executor.rs' / Deferred work queue / Uses 'try_lock', may drop work /

</details>

<details><summary>docs/archive/CONCURRENT_WRITE_POSITION_RACE.md (4)</summary>

- L102: is not fixed here; see "Not fixed" below.  _(maybe resolved)_
- L145: per chunk, matching its pre-existing (unfixed) behavior exactly.
- L188: ## Not fixed (left for a future session)  _(maybe resolved)_
- L200: tracked as a known gap.

</details>

<details><summary>docs/archive/CONNECT_TIMEOUT_LOCK_INVERSION.md (1)</summary>

- L216: **Not done, deliberately stated:**  _(maybe resolved)_

</details>

<details><summary>docs/archive/CONSOLE_LOG_COST.md (1)</summary>

- L284: '#[cfg]' pairs and a per-feature signature, for three stubs. Deferred to 0.0.8. A

</details>

<details><summary>docs/archive/CONTAINERS_STAGE_2_PLAN.md (1)</summary>

- L65: - If kernel registry is deferred, 'box' can just manage IDs or use PIDs as IDs.

</details>

<details><summary>docs/archive/COOPERATIVE_SCHEDULING_AUDIT.md (1)</summary>

- L389: natural follow-up is "can the async trait crate go too?" Answer: **partly** —

</details>

<details><summary>docs/archive/COWSTALE_FORK_THREAD_SEGV.md (1)</summary>

- L94: 'EXIT=139', ~1 in 5 runs, ~10 minutes per attempt) is still open — but there is now

</details>

<details><summary>docs/archive/COW_FRESHFRAME_STAGING.md (1)</summary>

- L5: bisect against.** Commits: 'da9a90a3' (the conversion) and the follow-up that restored free

</details>

<details><summary>docs/archive/COW_OPTIMIZATIONS.md (1)</summary>

- L260: little memory if deferred.

</details>

<details><summary>docs/archive/COW_PILE_AUDIT.md (8)</summary>

- L224: **CONFIRMED (mechanism) / NEEDS-REPRO (consequence). STILL OPEN — implemented and
- L248: &gt; **On "needs measurement" (the hold-time question):** unresolved, and not resolvable on
- L650: ('TRIM_FAT_EMBARASSING_DUPLICATIONS.md' §5.11, "Still open here").
- L770: / F9 / **W^X is not enforced on the instruction-abort permission-fault arm.** It upgrades the page to 'user_flags::RX' for **any** lazy region that is not 'PROT_NONE', w…  _(maybe resolved)_
- L771: / F10 / **Asymmetric recovery on a lazy-region miss.** When no lazy region covers the faulting VA, the data-abort arm falls back to re-mapping from the owner's *eager* '…  _(maybe resolved)_
- L869: ### Next step
- L976: a revived thread's TTBR0 install targets a deferred-not-freed L0 — intact tables — so the
- L1140: outstanding it calls one release and shows the slot goes **empty**. That is the

</details>

<details><summary>docs/archive/CPU_LOAD_REGRESSION_INVESTIGATION.md (2)</summary>

- L101: ## Open questions
- L123: the interrupts it needs and no more. Confirm the mechanism in "Open questions"

</details>

<details><summary>docs/archive/CROSS_CORE_THREAD_COLLAPSE.md (6)</summary>

- L3: **Date: 2026-08-19.** Follow-up to 'BENCHMARK_PERFORMANCE_ATTEMPT_0.md', which
- L86: needs it. (Follow-up candidate, NOT landed: 'tlbi aside1, #0' on cross-AS  _(maybe resolved)_
- L146: wakes, still open for explicit wakes.
- L208: **OPEN** — next probe: split 'EXCEPTION_ENTRIES' by vector class (sync-EL0 /
- L298: 5. Cross-AS 'tlbi aside1, #0' (kernel-global-preserving flush) — §1 follow-up.
- L319: follow-up that re-diagnosed open item 2, promoted item 4, ruled out per-core

</details>

<details><summary>docs/archive/CURL_MISSING_SYSCALLS.md (1)</summary>

- L8: kernel support not yet implemented.

</details>

<details><summary>docs/archive/CURRENT_TRAP_FRAME_STALE_ON_EXIT.md (1)</summary>

- L142: Also unresolved from §5.1: the addresses could not be symbolized. None of the

</details>

<details><summary>docs/archive/DASH_MISSING_SYSCALLS.md (2)</summary>

- L298: **Root Cause:** 'sys_wait4' had a TODO for the 'pid=-1' (wait-for-any-child) case:
- L302: None // TODO: implement wait for ANY

</details>

<details><summary>docs/archive/DEAD_CODE_SWEEP_FINDINGS.md (2)</summary>

- L149: // TODO: Rework these tests to use mock thread IDs &gt;= MAX_THREADS.
- L195: Follow the existing TODO: rework the five tests to use mock thread ids

</details>

<details><summary>docs/archive/DENVER_BOX_WIRING.md (2)</summary>

- L11: ## Evidence and remaining work
- L78: **Still open.** scala-cli *compile* with Maven Central, 'cargo' with the network and seccomp/private namespaces inside a Denver box are

</details>

<details><summary>docs/archive/DENVER_BUILD_BREAKAGE.md (1)</summary>

- L45: ## Still open

</details>

<details><summary>docs/archive/DENVER_CODEX_TOOLS.md (1)</summary>

- L28: ## Remaining work

</details>

<details><summary>docs/archive/DEVBOX_ISSUES.md (19)</summary>

- L28: ### ⚠️ Issue 3 is not fixed — cross-core console interleaving still tears lines  _(maybe resolved)_
- L282: ### Next step, if picked up
- L340: ### Next step, if picked up
- L356: "Next step" section below asks for is done for the current set.  _(maybe resolved)_
- L393: ### Next step, if picked up
- L472: ### Next step, if picked up
- L554: ### Next step, if picked up
- L596: ### Next step, if picked up
- L644: unresolved interpreter into the argv prefix and resolves separately into
- L702: ### Next step, if picked up
- L864: CLAUDE.md lists the rump devbox as **deferred**, so this is not a routinely
- L1076: ### Next step, if picked up
- L1141: **Status: still OPEN — but the script quoted below is not the one shipping
- L1418: **Deferred patch, recorded here rather than made** (2026-08-19, by request):
- L1493: next step.
- L1520: through to smoltcp), and still an open question as to what closes the channel at
- L1704: ### Next step, if picked up
- L1805: 'PPid: 0'. No CPU is burned, but every 'box run -d' leaks a zombie. Still OPEN.
- L2016: ### If you see it again — settle the one open question first

</details>

<details><summary>docs/archive/DEVELOPMENT_PRACTICES_REVIEW_AND_ASSESSMENT.md (1)</summary>

- L238: ## 7. Concrete follow-ups worth doing, if anyone wants to act on §5's pattern

</details>

<details><summary>docs/archive/DEVFS_KVM_NODES_NOT_LISTED.md (1)</summary>

- L37: ## Still open

</details>

<details><summary>docs/archive/DEVFS_MISSING.md (1)</summary>

- L227: not as a follow-up — leaving both versions alive at once is exactly the

</details>

<details><summary>docs/archive/EPOLL_MULTI_POLLER_PIPE_FLAKE.md (1)</summary>

- L230: The SMP=2 leg is the §7 repro verbatim, which predicts 1–2 failures in 6 unfixed. All

</details>

<details><summary>docs/archive/EPOLL_PERFORMANCE.md (1)</summary>

- L163: ### Known Gaps: Missing Syscalls

</details>

<details><summary>docs/archive/ERET_ELR_CLOBBER_ENTER_USER_MODE.md (1)</summary>

- L361: general; not done here to keep this change to the defect.  _(maybe resolved)_

</details>

<details><summary>docs/archive/ETXTBSY_TEXT_BUSY.md (1)</summary>

- L94: offset 0 -&gt; 1 on a write that failed with 'ETXTBSY'. Linux leaves the offset alone on error. Pre-existing, general, not fixed here.  _(maybe resolved)_

</details>

<details><summary>docs/archive/EXEC_FROM_THREAD_DETACH.md (1)</summary>

- L89: ## Future work: the Linux-style handler ('proposals/FUTURE_WORK.md' E7b)

</details>

<details><summary>docs/archive/EXT2_BLOCK_CACHE_DEFAULT_AND_CHUNKING.md (3)</summary>

- L14: closes that doc's §6 follow-up 3 ("startup cost is a bigger lever than the BKL").
- L191: - **Fault-path batching is still not done.** 'exceptions.rs' still calls  _(maybe resolved)_
- L206: this improves on; §6 follow-up 3 predicted this lever, §5.1 documents the 'big'

</details>

<details><summary>docs/archive/EXT2_DIRENT_TYPE_WITHOUT_FILETYPE.md (1)</summary>

- L68: ## Still open

</details>

<details><summary>docs/archive/EXT2_PERFORMANCE_AUDIT.md (11)</summary>

- L3: **Status: RESOLVED 2026-08-26 by a host I/O-probe follow-up — the cause is  _(maybe resolved)_
- L5: "2026-08-26 follow-up" at the bottom, and
- L7: for the full write-up. Four fixes landed the same day (deferred superblock/BGD  _(maybe resolved)_
- L9: O(N²), deferred bitmap writes): 2 MB write 5.7× → 3.6× amplification, create
- L34: 1. **Deferred-inode-free overflow leaking space.**
- L153: - Theories 1 and 2 (deferred-free leak, stale fpcache pin) remain untested —
- L171: ## Follow-ups if this is revisited
- L176: actually exercise the deferred-free path — needs a 'libakuma' file-backed
- L186: ## 2026-08-26 follow-up — the actual cause (host I/O probe)
- L193: and the deferred-free leak (theories 1, 2) are not involved.
- L309: (the 2026-08-26 follow-up's home, with diagrams and the allocation audit).

</details>

<details><summary>docs/archive/EXT2_PER_FD_INODE_READ_PATH.md (6)</summary>

- L10: and the write-back measurements), ['EXT2_WRITEBACK_FOLLOWUP_FIXES.md'](EXT2_WRITEBACK_FOLLOWUP_FIXES.md)
- L124: Wall-clock on this host has to be earned: 'EXT2_WRITEBACK_FOLLOWUP_FIXES.md' §8
- L244: ## Known gaps
- L260: thesis behind it did not survive measurement. See "Known gaps" in
- L272: can be freed, so the failure mode is deferred frees piling up rather than
- L315: condition — see §8 of the followup doc for the same lesson learned the same way.

</details>

<details><summary>docs/archive/EXT2_READ_PATH_STAGE_PROFILE.md (1)</summary>

- L45: rust_sync_el0_handler SPAN_EXC BKL entry, tripwires, dispatch, deferred kill

</details>

<details><summary>docs/archive/EXT2_TRIPLE_INDIRECT_AND_LARGE_FILES.md (1)</summary>

- L51: ## Still open

</details>

<details><summary>docs/archive/EXT2_UNLINK_INODE_BLOCK_LEAK.md (7)</summary>

- L6: making 'is_pinned' answer 'true' for every inode, which froze the deferred-free
- L99: deferred inode-free and block-free were abandoned, so the bits were never cleared.
- L181: free is deferred, 'munmap', then ask 'statfs' how much came back.
- L240: **Why the earlier "not load-bearing" call was wrong.** §6.3 originally deferred
- L338: the free is deferred until the last ['InodePin'] drops. The deferral list is a
- L368: deferred path, the 256-slot list saturates within a few hundred deletes, and
- L501: the deferred free were introduced, and the counters this doc says nobody reads.

</details>

<details><summary>docs/archive/EXT2_WRITEBACK_DESIGN.md (7)</summary>

- L11: After fixes A–D (redundant zero-fill skip, deferred superblock/BGD writes,
- L29: / 'bgd_cache' / 'bitmap_cache' / 'Ext2State' / per instance / group / block no. / — / **not caches**: deferred-write staging (fixes B/D) /
- L148: ['EXT2_WRITEBACK_FOLLOWUP_FIXES.md'](EXT2_WRITEBACK_FOLLOWUP_FIXES.md)
- L237: ## What is still open (audited 2026-08-26, after the A/B)
- L307: Not done:  _(maybe resolved)_
- L314: key served one mount another's page. The **capacity half is still open**:
- L327: premise): ['EXT2_WRITEBACK_FOLLOWUP_FIXES.md'](EXT2_WRITEBACK_FOLLOWUP_FIXES.md).

</details>

<details><summary>docs/archive/EXT2_WRITEBACK_FOLLOWUP_FIXES.md (1)</summary>

- L1: # ext2 write-back follow-up: defects found and fixed (2026-08-26)  _(maybe resolved)_

</details>

<details><summary>docs/archive/EXT3_EXT4_IMPLEMENTATION.md (2)</summary>

- L113: The deferred-free slot array lives on the heap. An inline 16 KiB array was
- L118: ## Verification and remaining work

</details>

<details><summary>docs/archive/EXTREME_SIZE_BUILD.md (1)</summary>

- L75: follow-up commit ('e9d08f1', "removed in-kernel ssh") deleted

</details>

<details><summary>docs/archive/EXTREME_SIZE_BUILD_FIX.md (2)</summary>

- L194: ## 4. Open: '/bin/curl' faults intermittently on extreme
- L238: ## 5. Open: 'mv' a binary, then exec its old name

</details>

<details><summary>docs/archive/EXTREME_STACK_TRIMMING.md (1)</summary>

- L77: Both are larger and riskier than the changes above; deferred for an explicit

</details>

<details><summary>docs/archive/FIBER_HANDOFF.md (2)</summary>

- L224: ### Open / next (latency — further levers, NOT yet done)  _(maybe resolved)_
- L398: - **Open architecture ponderings** (in 'HIJACK_VS_KERNEL_PROXY.md'): do we still

</details>

<details><summary>docs/archive/FIRECRACKER_BKL_TICKET_SILENT_WEDGE.md (2)</summary>

- L11: **Status:** **open, not root-caused.** Found by accident while diagnosing unrelated
- L21: &gt; was captured), so this stays **open** until a sighting shows it or the fix soaks.

</details>

<details><summary>docs/archive/FIRECRACKER_GUEST_HANG.md (5)</summary>

- L66: was; the interrupt that was pending is taken at the 'popfq', after the flag is set, and restores the periodic tick. Not done: finding out why  _(maybe resolved)_
- L103: * Reproduced and caught: with the unfixed kernel as the guest the ring dump fired on 1 run in 6 (A); the other five stopped at the probe (B).
- L105: * 'epollops' on the ryzen's unfixed kernel: 'FAIL poll_timeout_clears_revents', the only failure.
- L107: **Result: 42 of 42 clean** (12, then a 30-run soak; each run ≈ 28 s to 'AKUMA-GUEST-DONE' with 'udp probe: akuma-net-probe' and 'tcp echo: akuma-tcp-probe'), no ring dum…  _(maybe resolved)_
- L113: * 'UDP_REPLY_WAIT_TIMER_STORM.md' §Still open: 'allow_tick' itself could run the wake pass.

</details>

<details><summary>docs/archive/FIRECRACKER_PORT.md (3)</summary>

- L10: **Supersedes the open questions in:** 'docs/archive/PORTING_POSSIBILITIES.md'
- L23: **Still open**, both from 'AKUMA_FIRECRACKER_KVM.md' §5: a nondeterministic
- L36: 'PORTING_POSSIBILITIES.md' was an options survey with five open questions and a

</details>

<details><summary>docs/archive/FIX_MEMORY_MAPPING.md (2)</summary>

- L232: - 'sys_io_cancel': returns EINVAL (no outstanding requests)
- L430: 1. **Root cause not fixed**: 'ensure_user_page_mapped' only checks the **valid bit** in the  _(maybe resolved)_

</details>

<details><summary>docs/archive/FORKED_DESCRIPTORS_DID_NOT_SHARE_AN_OFFSET.md (2)</summary>

- L31: copies payloads by value, and the wider one-description-per-open design is still not done.  _(maybe resolved)_
- L47: ## Still open

</details>

<details><summary>docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md (1)</summary>

- L235: ## Follow-up

</details>

<details><summary>docs/archive/FPCACHE_MOUNT_IDENTITY.md (3)</summary>

- L5: **D-9**. The capacity half of D-9 is still open — see "What this does not do".
- L58: reached through. It cannot be dropped either: the deferred-free drain frees an
- L180: documented the mount-aliasing exposure as a known gap. The mount id is the answer

</details>

<details><summary>docs/archive/FRAMEBUFFER_REMOVED.md (2)</summary>

- L56: Worth recording, since it came up: those tables are **not fixed**. QEMU generates  _(maybe resolved)_
- L133: armed) vs '[EIO]' (no virtio-rng; the guard still opens and closes because

</details>

<details><summary>docs/archive/FRANKENLIBC_EVAL.md (1)</summary>

- L30: the remaining work is build/integration/test, not writing arch primitives.

</details>

<details><summary>docs/archive/FREEZE_INSTRUMENTATION_PLAN.md (1)</summary>

- L75: ## Out of scope (deferred)

</details>

<details><summary>docs/archive/FUTEX_REQUEUE_LOST_WAKEUP.md (1)</summary>

- L175: ## Unprobed sharp edge (flagged, not fixed)  _(maybe resolved)_

</details>

<details><summary>docs/archive/GETPPID_RETURNED_THREAD_PID.md (2)</summary>

- L37: 'ls /tmp / head -3' returned 'exit_code 0' with the listing; the unfixed kernel returned 143 with none. Host unit tests
- L40: ## Still open

</details>

<details><summary>docs/archive/GICD_IROUTER_ALIASING.md (1)</summary>

- L6: **Status:** OPEN. Not fixed. Benign today by coincidence; the coincidence is not  _(maybe resolved)_

</details>

<details><summary>docs/archive/GIT_MISSING_SYSCALLS.md (1)</summary>

- L691: ## Future Work

</details>

<details><summary>docs/archive/GOLANG_IPC.md (10)</summary>

- L429: 3. 'go' opens many new fds for the next steps (epoll 'ctl ADD' fd=2136..2155 — **fd numbers in
- L787: **Follow-up bug:** 'fork_process' sets 'parent_pid' to the **forking thread's**
- L1060: **Deferred page table freeing** ('crates/akuma-exec/src/mmu/mod.rs'):
- L1070: the owner already deferred, the last shared view frees all stored frames
- L1186: ## Go Build SIGSEGV — Four-Bug Follow-up (2026-03-26) — **FIX APPLIED**
- L1294: ### Next steps (not yet fixed)  _(maybe resolved)_
- L1342: ### Root cause still unresolved
- L1425: Not yet fixed. The lazy fault guard remains in place as a defence-in-depth measure. The ELF  _(maybe resolved)_
- L1489: Root cause is timer re-arm loss under full thread-pool load. Not yet fixed.  _(maybe resolved)_
- L1584: If the freeze persists after the siginfo fix, the next step is to add explicit IRQ-enable

</details>

<details><summary>docs/archive/GOLANG_MISSING_SYSCALLS.md (3)</summary>

- L976: Implemented a deferred cleanup mechanism:
- L1172: ## 38. Known remaining gaps (not yet fixed)  _(maybe resolved)_
- L1265: 6. 'asyncPreempt' runs with the goroutine stack shifted by -8 (from 'pushCall'). The second SIGURG is deferred to the *next* syscall.

</details>

<details><summary>docs/archive/GOOSE_GLM_LOCAL_OPTIMIZATIONS.md (3)</summary>

- L25: GLM-4.7-Flash 6-bit on 'mlx_lm.server', cold first turn vs. a cached follow-up turn
- L28: / context / cold TTFT / cached follow-up / decode /
- L86: one extra round trip). 'todo' and 'tom' are fine to leave on with goose 1.59

</details>

<details><summary>docs/archive/GOOSE_SERVE_IDLE_SPIN.md (2)</summary>

- L40: thread in the top three (sshd 17 ticks was first). Open question for the probe: how long after a restart, or
- L47: open, an unresolved tool-call future, a streaming read that returns zero bytes in a loop).

</details>

<details><summary>docs/archive/GO_BINARY_VA_SPACE.md (10)</summary>

- L146: ## Follow-up Fix: fork code_start SIGSEGV (2026-03-31)
- L205: ## Follow-up Fix: vfork signal-interrupted wait (2026-04-01)
- L268: ## Follow-up Fix: EL1 crash — missing THREAD_PID_MAP + CoW write (2026-04-01)
- L347: ## Follow-up Fix: clone flag routing and EL1 fault handler (2026-04-02)
- L372: ### Problem 2: EL1 user-copy fault handler noise (NOT fixed — deadlock risk)  _(maybe resolved)_
- L399: ## Follow-up Fix: clone_thread stack=0 crash guard (2026-04-02)
- L493: ### Follow-up: copy_to_user_safe crash at FAR=0x88 (2026-04-03)
- L505: ## Follow-up Fix: PROCESS_INFO_ADDR overwritten by cow_share_range (2026-04-03)
- L676: ## Follow-up Fix: exit/exit_group returned to userspace (2026-04-03)
- L810: ### Follow-up: is_interrupted flag never cleared → infinite EINTR loop (2026-04-03)

</details>

<details><summary>docs/archive/GO_COMPILE_CRASH_DEBUGGING.md (2)</summary>

- L89: ## Suggested follow-ups
- L95: ## Follow-ups (tooling / UX — not kernel)

</details>

<details><summary>docs/archive/GO_FORKTEST_DEBUG.md (5)</summary>

- L415: ### Proposed next steps (after this session)
- L452: ### Next steps (from this matrix)
- L905: ### C. 'clock_gettime' / SIGURG / syscall frame — **open**
- L914: **Conclusion:** **'clock_gettime'-only heuristics do not resolve forktest**; **'recover x0 as tp' was removed** from ['src/syscall/time.rs'](../src/syscall/time.rs) afte…
- L993: ### Recommended next steps (for the next agent)

</details>

<details><summary>docs/archive/GO_FORK_EXEC_FIXES.md (3)</summary>

- L787: **Why not fixed:** The early interrupt path is also used for SSH Ctrl+C ('is_current_interrupted'  _(maybe resolved)_
- L1236: thread state without proper context setup. TODO: Rework to use mock thread
- L1958: ### Next Steps

</details>

<details><summary>docs/archive/GRACE_EXPIRED_HARD_KILL_ORPHANS.md (2)</summary>

- L60: - **'pending_kill=false'** — the victim had no deferred-kill request. By the grace
- L120: 'mark_thread_terminated' fires only when 'killer != idx'. The whole deferred-kill path

</details>

<details><summary>docs/archive/HARD_LINKS.md (2)</summary>

- L31: - The file-page cache and deferred frees key on **inode**, so two names sharing
- L99: ### 2.5 Userspace follow-ups

</details>

<details><summary>docs/archive/HEAP_CORRUPTION_INVESTIGATION.md (1)</summary>

- L285: ## Next Steps

</details>

<details><summary>docs/archive/HIJACK_VS_KERNEL_PROXY.md (2)</summary>

- L632: Still open: the **sysproxy** 'rump_server' path (the SIGABRT above) — its sp-server
- L664: ### Open ponderings (deferred — decide after model-C networking works)

</details>

<details><summary>docs/archive/HOOKED_EL1_PREEMPTION.md (2)</summary>

- L48: the *baseline* moved. Two address spaces sat in the deferred-free queue
- L71: deferred-free queue and which core or slot gates each entry

</details>

<details><summary>docs/archive/HOOKED_IRQ_SIGNAL_DELIVERY.md (3)</summary>

- L54: 1. A **deferred kill** from a peer's 'kill_thread_group' ('take_thread_kill_request') ends the
- L73: **The SGI path is not fixed.** It is to be deleted once hooked becomes the AArch64 default (the  _(maybe resolved)_
- L74: next step of the multiarch work), so the fix was not ported to its frame.

</details>

<details><summary>docs/archive/HOOKED_TPIDR_EL0_SWITCH.md (2)</summary>

- L58: follow-up.
- L60: ## Still open

</details>

<details><summary>docs/archive/HOST_TESTS_AUDIT.md (1)</summary>

- L132: / 'test_term_state_lock_bounded_acquire_does_not_starve_peers' / 'process_tests.rs:2142' / Registration commented out at ':213' — "the test harness itself has an unresol…

</details>

<details><summary>docs/archive/HOW_MAIN_BROKE_2026_11.md (1)</summary>

- L36: ## Still open (recommendations, not done)  _(maybe resolved)_

</details>

<details><summary>docs/archive/HTTPD_STARVATION.md (3)</summary>

- L120: ## Next steps
- L136: Next step 1 above was run, in QEMU instead of Firecracker, and produced a
- L157: mechanism is the doc's remaining follow-up.

</details>

<details><summary>docs/archive/IDENTITY_CACHE_LAZY_RESTAMP.md (2)</summary>

- L4: **Scope:** follow-up to
- L216: are **still open**. They are safety defects (entries that resolve to something

</details>

<details><summary>docs/archive/IDENTITY_CACHE_SMP_REVIEW.md (5)</summary>

- L4: **Scope:** follow-up item 1 of
- L89: *requests* a deferred kill and grace-waits (up to 2 s) for each sibling to reach
- L381: the next step is a counter that observes the window directly (below), not a
- L387: the audit this follows up, its ablation ladder and deferred item list.
- L389: — why retirement is deferred and what the cooldown is sized against.

</details>

<details><summary>docs/archive/IMPLEMENTATION_PLAN.md (6)</summary>

- L52: **Explicitly deferred (later milestones):**
- L191: More moving parts (promiscuous capture, MAC filtering); deferred unless a
- L361: is Phase 5 (still ⏳). DNS (Phase 7) deferred — M1 uses a literal host IP.
- L376: ## 7. Risks & open questions (call these out in review)
- L443: rump stack**, DNS deferred.
- L555: unmodified. **Deferred, post-M1**; full demo + open questions in

</details>

<details><summary>docs/archive/INFINITY_ON_TAOXICHUAN.md (3)</summary>

- L68: Open: triple-indirect support in 'akuma-ext2'. It is needed before a guest disk on this box can be larger than
- L126: Still open:
- L132: it found nothing structural: 336 leaked inodes (unlinked files still open when the guest died; ext2 has

</details>

<details><summary>docs/archive/INLINE_ASM_CLEANUP.md (1)</summary>

- L6: tree still open-coded their own 'unsafe' block. This is the migration.

</details>

<details><summary>docs/archive/INODE_PIN_CONGESTED_WINDOW.md (1)</summary>

- L43: pinned for as long as the run lasted, so every unlink of one was deferred, and the drain skipped each of them.

</details>

<details><summary>docs/archive/INSTALL_SCRIPT.md (1)</summary>

- L156: ## Open questions

</details>

<details><summary>docs/archive/IRQ_HANDLER_TABLE_DEADLOCK.md (1)</summary>

- L158: - A 6-boot A/B at 'SMP=4' produced **0 storms in 6** on the *unfixed* kernel

</details>

<details><summary>docs/archive/J4_HANG_LIVE_AUTOPSY.md (4)</summary>

- L156: (deferred process reclaim) found nothing to close.
- L215: ## 8. Residuals / follow-ups
- L228: - **The RELR instruction-abort class is still open** — it is what makes '-j4'
- L258: - 'docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md' — deferred reclaim; why the 'Drop' backstop exists

</details>

<details><summary>docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md (22)</summary>

- L7: One (**B**) is characterised but unfixed, and is now the dominant blocker. The
- L16: / **B** / linker leaves a truncated/empty output, cargo execs it / **unfixed — the current blocker** /
- L437: mid-stream. **Recorded here, not fixed, not fully explained.**  _(maybe resolved)_
- L609: ### 7.8 The concrete next step: an untried, already-built probe
- L635: Also still open: identify '0x3d90e5e8'-style precisely (§7.5), and check
- L728: (see Next steps).
- L805: *notifying* thread of each barrier is blocked on. **Next step: instrument which
- L879: registered — not fixed there, out of scope here, but worth knowing if  _(maybe resolved)_
- L886: Picked up §7.10's own next step: instrument which thread in a stuck process is
- L947: independently documents an **open, unresolved** bug in exactly this territory
- L965: mark as the unfixed baseline. If any thread were genuinely starved from
- L1047: ### Next steps
- L1175: **Fix worth making, not done this session:** point '[THR-DUMP]''s 'elr='  _(maybe resolved)_
- L1240: for this, and is now more clearly the next step than reading more 'elr='
- L1357: this doc's own still-open Failure B; see that doc's "Not fixed" section.  _(maybe resolved)_
- L1359: **Revised next step:** candidate 1 (arrival count lost) now outranks
- L1386: original stall-gated 'mark()'-based dump left open). Rebuilt the probe with
- L1484: next step §7.16 already named (a 'sys_futex' 'FUTEX_WAKE'-entry tripwire,
- L1555: carrying the same pid, which are shorter and interleave less. Not fixed; a  _(maybe resolved)_
- L1623: **Design, not yet implemented:**
- L1653: ## 9. Next steps
- L1656: (§7.9).** The cheapest open question. With 'jobserver_stress' + 4 CPU hogs,

</details>

<details><summary>docs/archive/KERNEL_SPLIT_BUGS.md (2)</summary>

- L619: ### Remaining TODO: Clippy style cleanup
- L725: This is a significant effort better deferred to a dedicated task. The crate

</details>

<details><summary>docs/archive/KNOWN_ISSUES.md (4)</summary>

- L331: Implemented deferred process unregistration. 'kill_thread_group' now marks
- L408: already outstanding and queries actual device readiness without completing
- L421: ### Not fixed by this — see issue 11  _(maybe resolved)_
- L521: adopting 'idle_halt()' there too is a measured-safe follow-up, not this fix.

</details>

<details><summary>docs/archive/KTG_GRACE_EXPIRY_KILL_INTERRUPT.md (2)</summary>

- L5: consult the deferred-kill flag. Gates: 'test_pending_kill_interrupts_blocking_wait'
- L97: blocking wait in the kernel was interruptible by a deferred thread-kill.

</details>

<details><summary>docs/archive/KTG_STALE_TID_EXIT_STAMP_J4_HANG.md (5)</summary>

- L86: the table where the 'Drop' backstop (deferred process reclaim) closes them —
- L113: **A1. PHASE 1, '#[cfg(kernel_smp_shared)]' branch** — the deferred-kill
- L213: 'Drop' backstop (deferred process reclaim) closes them later — worst case, one
- L249: ## Residual risks / not fixed here  _(maybe resolved)_
- L281: - 'docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md' (deferred reclaim; why the 'Drop' backstop exists)

</details>

<details><summary>docs/archive/KVM_BKL_EXIT_HANDLING_AND_BULK_WEDGE.md (1)</summary>

- L69: **Next experiments** (cheap, not done): (a) the same transfer as '&gt; /dev/null' on a *fresh* guest  _(maybe resolved)_

</details>

<details><summary>docs/archive/KVM_GUEST_4GIB_PVH_HANDOFF.md (1)</summary>

- L73: ## Still open

</details>

<details><summary>docs/archive/KVM_GUEST_DISK_856_MIB_RACE.md (1)</summary>

- L51: ## Not done yet (before the cause was found)  _(maybe resolved)_

</details>

<details><summary>docs/archive/KVM_GUEST_RAM_CAP_LIFTED.md (1)</summary>

- L84: ## Still open

</details>

<details><summary>docs/archive/KVM_RELAY_SYN_STALL.md (1)</summary>

- L77: ## Still open

</details>

<details><summary>docs/archive/LIBAKUMA_AUDIT.md (7)</summary>

- L18: &gt; ("multiple libakuma fixes"). A follow-up, uncommitted as of this writing,
- L22: &gt; tests) is still open. Everything below is the audit as written; what
- L23: &gt; landed, what landed differently, and what is still open is recorded in  _(maybe resolved)_
- L125: stream therefore returns '0.0.0.0:0'. The TODO at 'net.rs:155' has been there
- L352: tls_conn.open::&lt;TlsRng, NoVerify&gt;(context)?;
- L677: **Still true: there is no verification.** Not done: the  _(maybe resolved)_
- L810: host tests for both crates) is the only item left open.

</details>

<details><summary>docs/archive/LIBAKUMA_LINUX_COMPAT.md (2)</summary>

- L3: &gt; **Archived 2026-11-11 from 'proposals/'** when that directory was cleared. Still an idea, not done:  _(maybe resolved)_
- L19: - **Not done:** 'scratch' has not been run in a guest. The cheapest first test is the unmodified binary with no arguments (expect 'scratch: missing command'), which prov…  _(maybe resolved)_

</details>

<details><summary>docs/archive/LINE_COUNT_ANALYSIS.md (5)</summary>

- L312: still open. And reading any of this as a size-efficiency win would be exactly the
- L385: the next step. Same underlying pressure — AI-accelerated contribution throughput
- L674: session's — a proper re-audit is the honest next step, not a quick rescale.
- L706: with 'sp=0'; the TODO is to rework them onto mock tids ≥ 'MAX_THREADS'. The 9
- L989: **To be measured and added to this doc.** Not done yet.  _(maybe resolved)_

</details>

<details><summary>docs/archive/LINE_COUNT_ANALYSIS_POST_AMD64.md (1)</summary>

- L188: 'MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md'). The planned before/after baseline is deferred until the working system.

</details>

<details><summary>docs/archive/LINUX_ABI_SPAWN_FD_LEAK.md (1)</summary>

- L31: ## Still open

</details>

<details><summary>docs/archive/LINUX_COMPATIBILITY_ISSUES.md (1)</summary>

- L84: / 'madvise(MADV_DONTNEED)' / zeroes the **physical frame**; Linux drops the *mapping* / **OPEN and the sharper of the two**: on a CoW-after-fork or 'file_page_cache' fra…

</details>

<details><summary>docs/archive/LINUX_COMPAT_PUSH.md (2)</summary>

- L128: / 'SPAWN_FLAG_PTY' / 'open("/dev/ptmx")', 'TIOCSPTLCK(0)', 'TIOCGPTN', 'fork'; child 'setsid', 'open("/dev/pts/N")', 'TIOCSCTTY', 'dup3' to 0/1/2, 'execve'; window size …
- L245: in a rump box is exec'ing a rump_server" is 'execve'. The rump devbox is "deferred" per

</details>

<details><summary>docs/archive/LITTER_TRASHCAN_RYZEN_JOIN.md (2)</summary>

- L65: **Still open in the same area:** 'TcpStream::connect' inside the probe is not
- L165: &gt; unbounded probe connect) is still open. This session also established the

</details>

<details><summary>docs/archive/LLAMA_MMAP_OOM_KERNEL_ABORT.md (4)</summary>

- L256: net-bounce paths, but it remains **open on whatever path allocated here**. A process
- L281: tied to this specific spike (see "open question").
- L302: ### Open question — where exactly do the ~3.4 GB of frames come from?
- L355: To close the "open question" and target Priority 1 at the right allocation:

</details>

<details><summary>docs/archive/LOCK_FREE_THREADING.md (1)</summary>

- L146: 2. Not done → 'YieldOnce::new().await' returns 'Pending'  _(maybe resolved)_

</details>

<details><summary>docs/archive/LONG_ROAD_TO_REDIS.md (2)</summary>

- L101: follow-up question it cannot answer.
- L135: actively editing, so the next step was a targeted reproducer instead.

</details>

<details><summary>docs/archive/LONG_ROAD_TO_REDIS_PART_2.md (5)</summary>

- L29: / 'sys_pselect6' registered no waker / boot test, **fails on unfixed kernel** /
- L30: / 'sys_pselect6' had no 'EINTR' check / boot test, **fails on unfixed kernel** /
- L54: The regression test measures it exactly: on the unfixed kernel,
- L128: **The 'pselect6' fixes were A/B'd against a deliberately unfixed kernel**, two
- L132: shown both tests passing on the "unfixed" arm and made the tests look worthless.

</details>

<details><summary>docs/archive/LOW_MEMORY_ENVIRONMENT.md (7)</summary>

- L104: **Still open:** memory **reclaim after process death** is incomplete — post-OOM at
- L210: **What is not fixed — the syscall boundary.** Every 'read'/'write'/'recv'/'send'  _(maybe resolved)_
- L242: **Mitigation (tracked, not yet done).** Replace the per-syscall 'vec![0u8; count]'  _(maybe resolved)_
- L550: a deferred segment's file data ends mid-page and the next PT_LOAD segment begins
- L847: / **Lazy file-backed 'mmap'** ('MMAP_FILE_BACKED_LAZY') / 'src/syscall/mem.rs' + 'src/config.rs' / every file-backed 'mmap' eagerly allocates all PMM frames / creates 'L…
- L1696: **Note on memory layout:** even at 948 KB, the kernel's boot-time memory reservation ('code_and_stack') stays at ~11–16 MB because the boot stack is hardcoded at 'KERNEL…
- L1716: (Item 6, the 'tcc -run' bug, is unrelated to memory and is still open.)

</details>

<details><summary>docs/archive/LTO_RELEASE_PROFILE.md (3)</summary>

- L130: ## 5. What is NOT done yet  _(maybe resolved)_
- L181: Both arms are gate-verified, so the remaining work is the two measurements that
- L204: So this item is still open, and it is now **blocked on the self-host wedge**,

</details>

<details><summary>docs/archive/M5C_BKLFREE_EL0_SCHEDULER_DEADLOCK.md (1)</summary>

- L57: unfixed build hung ~100%):

</details>

<details><summary>docs/archive/MADV_DONTNEED_SHARED_FRAME.md (1)</summary>

- L174: ## Still open, in the same arm

</details>

<details><summary>docs/archive/MAPPED_PAGE_PREMATURE_FREE_FIX.md (2)</summary>

- L4: 10/10 green). One rarer sibling route left open, with a standing tripwire.**
- L166: ## Residual, deliberately left open

</details>

<details><summary>docs/archive/MERGE_GATE_FIRST_RUN.md (1)</summary>

- L4: branch 'box0-followup' (x86_64 dev box, KVM), found the following. Nothing here is fixed by the gate; each is a line in  _(maybe resolved)_

</details>

<details><summary>docs/archive/MINIMAL_DEV_BUSYBOX_APPLETS.md (3)</summary>

- L66: **Verified 2026-08-12** (same build/harness). Originally deferred in this
- L67: doc; the pass below promotes it out of deferred status. 'vi' is excluded
- L267: unresolved interpreter into the argv prefix and resolves separately into

</details>

<details><summary>docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md (41)</summary>

- L144: 'lseek', 'poll') and check each for a '_ =&gt; EBADF'/stdio-only fall-through. Proper follow-up (not done): one audit pass over every  _(maybe resolved)_
- L282: - **Still open for stage 1:** goose's uncommitted ASCII-banner edit, its
- L778: **Open items found while using the guest (not gate failures).** 'ls /bin' over ssh prints "ls: cannot access
- L825: Rule: stop 'vm-dev' when a session ends. A 'hlt'/idle path in the guest's wait loop is a real follow-up (needs a wake source:
- L873: **Open.** 'access'(21) arm; remove the temporary '[sc.c]' per-syscall trace before the clean gate boot; the '.apk' path
- L1155: - **OPEN, found here: a refused outbound dial is never signalled to the guest.** With the repositories file live and no listener on :8123, the relay refuses the SYN ('gw…
- L1184: same mmap path M5 must fix. Not fixed here.  _(maybe resolved)_
- L1185: - **Open housekeeping:** the per-call '[statfs]' print is debug output and must be removed (or cmdline-gated) before the kernel is final; the automated Statfs verdict is…
- L1201: - **Recommended route:** pull 'akuma-exec' whole (all pieces resolve for 'x86_64-unknown-none'; node counts: exec 84, threading 40, mmu 29, pipe(s) 1-2, cow 1, exec-core…
- L1239: 6. **Not done yet:** a hard per-exec timeout in 'run_gates.sh' (about 90 s) in place of the ssh client's retry loop, which would have saved roughly an hour; 'run_gates.s…  _(maybe resolved)_
- L1253: - **Process notes:** a 200-call budget was overrun by 23 calls and a steer was needed to get the WIP committed; the monitor tool ('Monitor', 60 s poll for call count, li…
- L1261: - **OPEN QUESTION (user, 2026-10-26): why did the box reboot at about 15:09 UTC?** To drill down later. What is known: Akuma had been up about 5 hours; goose was running…
- L1276: - **Open / discrepancy:** O2 reports the host 'akuma-amd64' package does not compile at this HEAD (5 x E0425 in 'crates/akuma-kernel-amd64/src/mem.rs') and calls it fall…  _(maybe resolved)_
- L1300: - **Open:** re-measure docs reads for Q3/Q4 when the export includes them, and check whether the step is actually followed in the next fresh-context brief (the evidence …
- L1309: - **OPEN (new): the first exec after a completed HTTPS fetch wedges the guest's net path.** After Q1's 'Exit status: 0', the next exec ('sha256sum /tmp/h &gt; /tmp/h.out…  _(maybe resolved)_
- L1334: - **OPEN, host kernel, NOT licensed to change:** after the session, relay slot 0 froze ('up_pending=176', 'to_host' frozen, 'host_send_blocked' climbing +500/s, 'tcp_sta…
- L1342: - **Q11 (control, no client killed before its console exit line):** the wedge still reproduced on 'sh -c "sha256sum /tmp/h &gt; /tmp/h.out"', while plain 'sha256sum /tmp…
- L1354: **The suite is closed, and why.** The 35-exec main battery takes 9-10 minutes without 'apk add curl' and 25-30 with it ('PACE=15' s after every exec). The host forward d…  _(maybe resolved)_
- L1363: **Open.** (a) 'cargo build -p akuma-amd64 --target x86_64-unknown-none --release' fails (5 x E0425, 'crates/akuma-kernel-amd64/src/mem.rs:117'): this branch's '43edbffc'…  _(maybe resolved)_
- L1371: - **Not served:** 'sendfile' (40) answers ENOSYS once per 'cat' exec; busybox falls back to read/write. A cheap follow-up. Anything over 64 KiB, concurrent writer/reader…
- L1381: - **Open for M4 slice 1:** 'forkprobe' is the named M4 gate (libakuma, builds x86_64 static); 'timer_trap' is where the task switch lands, ring 3 and idle only, never ri…
- L1388: - **Open.** The hooks table in 'RESULT-pc.md' is the slice 1-2 record; slice 3 ('fork_process' + 'wait4' over child channels, gate 'userspace/forkprobe') is next, then p…
- L1404: - **Still open for the slice:** pipe park/wake (the dropped '_wakes' in 'pipes.rs'), PK1 'yes / head -n 20000 / wc -l', PK2 ('forkprobe'), and a SIGPIPE-default stand-in…
- L1410: ### Stage 8, session pe (2026-10-28): M4 slice 3 again, plan first — the real fork child, 'wait4' park, wake, switch back and reap WORK on three boots; the client verdic…  _(maybe resolved)_
- L1418: **Not done.** F1's client bytes and exit status through the new path, P1-P5 and PK1 through it, PK2 ('forkprobe' is a repo crate, not in the image). Goose's diagnosis, n…  _(maybe resolved)_
- L1446: - *goose-kimi planner*: server '127.0.0.1:3285' (pid from 2026-10-24, reused), session '20261028_1', driver log and worktree in the scratchpad ('kimi-plan.log', 'kimi-wt…
- L1451: **Open.** (1) Whether pf's watchdog boot confirms the ring-3-spin theory, and if so goose building preemption (the user's decision); the log is the place to correct this…
- L1477: **Open:** a residual intermittent stall (ph-7 clean, ph-8 stalled at exec 1, ph-9 clean): an un-converted spin on the single pinned host core. Next brief 09i: find it wi…
- L1481: ### First compiler probes on Taoxichuan (2026-10-29): tcc works; gcc and rustc blocked at known gaps
- L1527: - User, answering the plans' open questions (2026-10-30): (1) 'linux-abi' is switched on by a **cargo feature**; (2) keep 'clone(CLONE_VM/CLONE_VFORK/SIGCHLD)' working t…
- L1539: - **Another pre-existing limit, measured, not fixed:** after about 15 ssh execs in one boot, 'fork' fails ('Failed to allocate x86 task stacks': the claim picks a slot n…  _(maybe resolved)_
- L1547: - **A second pre-existing bug seen, not fixed:** a task killed by a fault does not wake its parent's 'wait4' (the clone hung after the helper faulted).  _(maybe resolved)_
- L1564: **Still open:** '[nosys]' after the battery: 'sendfile' (40), 'mount'/'umount2' (165/166, from apk's busybox trigger); 'sched_getaffinity' (204); dumb-HTTP 'git-remote-h…
- L1574: **Open, in the order the supervisor would take them:** the 'rustc' link result and then './hello'; 'sendfile'/'mount'/'sched_getaffinity'/'prctl' arms; commit of the wor…  _(maybe resolved)_
- L1630: Left open: the ledger bookkeeping of a disowned fork-child 'Process' leaks a little heap (a few hundred bytes per fork-exec; the heap read 2.5-4 MB across the runs); 'ex…
- L1651: **Left open:** HTTPS ('git clone https://...', crates.io) needs TLS/CA certs in the guest; '[nosys]' still lists statx/newfstatat 326, sendfile 40, linkat 265, getrusage…
- L1665: **What is not done / known:** fork still copies the whole address space eagerly (no CoW), which is why '-j4' needs the 8 GiB guest ('-m 3072' runs out of memory with fou…  _(maybe resolved)_
- L1834: Open: how "what tokio needs" is stated (a census of a tokio binary under strace on Linux, as stage 9's census did for
- L1907: decisions are in 'akuma-syscalls-poll' and the wait loop in 'akuma-net-yarn', both ready to pull. Not fixed here.  _(maybe resolved)_
- L2065: * **Open bug:** the in-guest '-j4' build stalls intermittently on the same kernel (a 4-vCPU BKL convoy of 'poll' laps, plus an 'ECHILD' and a ring-3 '#GP'
- L2280: / 385 / 11-01 / "it used to pass not long ago, cow has a long history of bugs" / the trail led to a BKL convoy, CoW not implicated; trigger still open ('TAOXICHUAN_J4_BK…

</details>

<details><summary>docs/archive/MIOT_MESH_ON_AKUMA.md (3)</summary>

- L12: a 'kot' replica goes deaf within minutes. Not fixed; it has a hypothesis and  _(maybe resolved)_
- L13: a next step.
- L251: **Next step:** a probe that runs connect-then-abort churn against a

</details>

<details><summary>docs/archive/MISSING_NTP_SYSCALLS.md (1)</summary>

- L4: **Status:** OPEN — diagnosed precisely, not fixed. Worked around by vendoring.  _(maybe resolved)_

</details>

<details><summary>docs/archive/MSGQUEUE_BOX_TEARDOWN.md (2)</summary>

- L8: - the 2026-11-11 pre-merge sweep found it still open
- L44: ## Still open

</details>

<details><summary>docs/archive/MULTIKERNEL.md (10)</summary>

- L136: robustness is future work); and the §16 hardening (caps map as descriptor data, owner-side capability
- L137: enforcement, BSP on its own restricted table). Deferred cleanup/tech-debt is tracked in §16.
- L339: &gt; not yet **BSP→secondary**. Putting the BSP on its own restricted partition table is future work.
- L566: …)' for its outstanding requests; the owner echoes the correlation id in the reply, and the
- L600: Routing a syscall to its owner is the easy half. The hard half (flagged as the open question)
- L924: subtraction real; it's a tracked follow-up (§16) gated on the caps map being data.
- L995: ## 13. Risks & open questions
- L1058: / **R4b — per-core syscalls + pinned process** (staged R4b.1–.5) / SVC from EL0 on the per-core vectors; spawn '/bin/hello' (then 'curl') pinned to core 1, forwarding 'o…  _(maybe resolved)_
- L1065: ## 16. Deferred cleanup / tech debt (later)
- L1180: Open question; design before a real boxed process runs on a secondary.

</details>

<details><summary>docs/archive/MULTIKERNEL_NETWORKING_EXPERIMENT.md (6)</summary>

- L48: outstanding ('FWD_AWAITING_REPLY'): it calls the new
- L144: as a follow-up since the periodic tick makes 1 ms a safe, simple, fully-measured win.)
- L259: reads but not directory listing; see the getdents64 TODO). That herd creates a 'stack=rump' box,
- L435: **Still open — one secondary issue:**
- L443: the sysproxy, not RX/tick. Separate follow-up.
- L460: **Also open (separate TODO):** forwarded directory listing ('getdents64') is classified-but-

</details>

<details><summary>docs/archive/NCA_CARGO_SPAWN_EFAULT.md (1)</summary>

- L92: ## Next steps if resumed

</details>

<details><summary>docs/archive/NCA_MISSING_SYSCALLS.md (5)</summary>

- L131: **Next steps if resumed:** strace-less guest means instrument musl std's
- L148: running while the spawn happens. The "next steps" instrumentation above is
- L149: still the right next step, but now specifically aimed at *live cargo*, not a
- L182: **Proper fix (kernel, not done):** named AF_UNIX stream sockets with a  _(maybe resolved)_
- L380: still opens. There is no config-level '--no-tui'; the flag is CLI-only.

</details>

<details><summary>docs/archive/NETWORKING_DEADLOCK_INVESTIGATION.md (1)</summary>

- L75: **Next Steps:**

</details>

<details><summary>docs/archive/NET_BOUNCE_OOM_KERNEL_ABORT.md (5)</summary>

- L17: - 🔲 **Open (broader):** the net buffer was only *one* unprotected multi-page
- L19: multi-page kernel growth — is still a TODO in 'src/allocator.rs'. See
- L20: "Remaining work".
- L78: // "the OOM killer will hook in at this point" — TODO
- L174: ## Remaining work

</details>

<details><summary>docs/archive/NEWTYPE_AUDIT.md (1)</summary>

- L114: ## Suggested next step

</details>

<details><summary>docs/archive/NGINX_MISSING_SYSCALLS.md (8)</summary>

- L9: churned connections where it used to die at 80. The one thing still open is
- L187: fd is still open in the current process before calling
- L369: **Status when first written: OPEN, root cause not fixed. Now RESOLVED — see  _(maybe resolved)_
- L407: in a way nginx's accept/close bookkeeping doesn't expect is the open question.
- L417: every time, from every one of several follow-up attempts.
- L467: connection shape, is unresolved — flagging it here rather than chasing a
- L619: ### What was NOT fixed here  _(maybe resolved)_
- L656: The one item from Issue E still open is 'SCM_RIGHTS' over 'AF_UNIX', which

</details>

<details><summary>docs/archive/NIGHTLY_CARGO_HVF_SIGILL.md (2)</summary>

- L112: ran. On the **unfixed** kernel: 'udf'/'sm3'/'sm4' all print 'PROBE …' then die
- L149: / Isolated SIGILL probe ('udf'/'sm3'/'sm4') / unfixed: killed, no 'RECOVERED'; **fixed: 'RECOVERED sig=1', RC=0** /  _(maybe resolved)_

</details>

<details><summary>docs/archive/NODEJS_LIBUV_IMPLEMENTATION.md (3)</summary>

- L82: / PipeWrite / N/A / 'pipe_can_write()' (readers still open) /
- L187: sigreturn) is not yet implemented. For SIGABRT specifically (the
- L244: ### Not Yet Implemented

</details>

<details><summary>docs/archive/NODEJS_MISSING_SYSCALLS.md (2)</summary>

- L107: (code_end = 0x12fce000). With deferred lazy segments and demand
- L122: ## Remaining Work

</details>

<details><summary>docs/archive/OOM_KILL_DEFERRED_RECLAIM_GAP.md (2)</summary>

- L13: of panicking) is **still open** — it needs a fallible 'register_process' and
- L138: — why the free is deferred at all, and the self-deadlock that constrains

</details>

<details><summary>docs/archive/OPTIONAL_SMOLTCP.md (3)</summary>

- L345: **Fixed 2026-07-05.** Follow-up test: 'git clone' + 'cargo build --release' of a real Rust  _(maybe resolved)_
- L436: signature as the already-documented, unfixed "shell pipeline can wedge the VM" backlog
- L494: Related backlog, still open: the single box-0 rump proxy serializes socket syscalls, which

</details>

<details><summary>docs/archive/PAGE_TABLE_UAF_BKL_STORM.md (8)</summary>

- L137: / no TLB+TTBR shootdown) or a refcount that drops early. That is the next step.
- L308: configure and enable a PMU event counter first; not done in this session.  _(maybe resolved)_
- L341: actual trigger is still open.
- L376: §4.2 left the trigger open: the storm reproduced on the 'execve'-siblings-fixed  _(maybe resolved)_
- L466: storm into a bounded deferred free — a leak-until-switch, not a dead machine.
- L483: unfixed kernel this session, **both of the first 2 rounds per lane stormed**
- L495: Against the unfixed baseline — 4 storms in 21 rounds on
- L507: the unfixed kernel each of those five events would have been a poisoned

</details>

<details><summary>docs/archive/PHASE1_COMPLETION_BASELINE.md (1)</summary>

- L108: ## Next Steps (Phase 2)

</details>

<details><summary>docs/archive/PHASE1_NETWORK_LOCK_FOUNDATION.md (2)</summary>

- L140: 2. **Per-socket locks**: Not yet implemented (reserved for future optimization)
- L144: ## Next Steps (Phase 2)

</details>

<details><summary>docs/archive/PHASE2_RUMPUSER.md (1)</summary>

- L94: ## Open question worth understanding

</details>

<details><summary>docs/archive/PHASE3_KERNEL_TAP.md (1)</summary>

- L185: '/dev/net/tap0' with the TUN/TAP-shaped ABI. The remaining work is userspace:

</details>

<details><summary>docs/archive/PLAN_SIGSEGV_COMPILE_FIX.md (2)</summary>

- L141: 3. **Phase 4** as follow-up hardening.
- L163: **Not done (optional)**  _(maybe resolved)_

</details>

<details><summary>docs/archive/PMM_DOUBLE_FREE_AND_EL1_CRASH.md (9)</summary>

- L15: - 🔲 **Open:** a *third* crash remains — 'EC=0x21' garbage-PC on Thread0 under
- L16: apk's heavy mmap/munmap churn (~55 regions / 27 MB, not OOM). See "Remaining work".
- L80: / '8e2f625' / 'ba60d72' faster munmap / 'mmu/mod.rs', 'syscall/mem.rs' / 'user_frames' 'Vec&lt;PhysFrame&gt;' → 'BTreeMap&lt;PA, u32 refcount&gt;'; deferred per-region T…
- L207: deferred, demand-paged loader ('from_elf_path') already existed but was only a
- L326: ## Remaining work
- L334: '0x207b3000' ×4), lazy-region split/merge, deferred-TLB-flush windows.
- L337: 2. **The concurrency variant of Bug 1 is detected, not fully prevented.** Deferred
- L344: tests) and Bug 2 fix (deferred loader) are self-contained; commit on
- L361: - 'crates/akuma-exec/src/process/spawn.rs' — size-gated deferred loader ('&gt;1 MiB'

</details>

<details><summary>docs/archive/PMM_EXTRACT.md (2)</summary>

- L410: is still open. It is the last file in the tree one would choose to move casually.
- L442: &gt; The extraction can proceed without this as an open question. The rest of this

</details>

<details><summary>docs/archive/PMM_TALC_LOCK_CYCLE_SILENT_WEDGE.md (2)</summary>

- L90: open question.
- L219: Baseline, unfixed kernel ('600640711bfb2e1d71963fc9db2d7928'), 2 parallel lanes,

</details>

<details><summary>docs/archive/PORTING_POSSIBILITIES.md (1)</summary>

- L262: ## 5. Open questions

</details>

<details><summary>docs/archive/POST_AMD64_CODE_AUDIT.md (3)</summary>

- L127: **Not done:** 'WakeBudget::observe_quiet/observe_work' (the adaptive staircase) —  _(maybe resolved)_
- L350: - **Deferred, not cancelled:** aarch64 becomes M-later, gated on owning any
- L509: tree-wide (amd64 alone 342 -&gt; 315 sites). **Still open from tier 1:** 'gdt.rs'

</details>

<details><summary>docs/archive/PROC_MAPS_WALK_FREED_TABLES.md (1)</summary>

- L74: ## Still open

</details>

<details><summary>docs/archive/QEMU_HVF_ISV_BUG.md (1)</summary>

- L145: threshold, and that it predates any local change. Two follow-ups if this is

</details>

<details><summary>docs/archive/QJS.md (1)</summary>

- L262: ## Future Work

</details>

<details><summary>docs/archive/REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md (4)</summary>

- L61: evidence didn't actually support the stated mechanism, and two follow-up
- L116: paths. At '-P 1', each client has one small outstanding command at a time —
- L131: **Two follow-up experiments, run after the review:**
- L167: something else entirely) is not identified. Concretely still open:

</details>

<details><summary>docs/archive/REDIS_ROUND_TRIP_CEILING.md (4)</summary>

- L23: connection harness" as *not done*. That gap is why the ceiling was never seen:  _(maybe resolved)_
- L175: workload; if a future workload is dominated by large inbound writes, this is
- L241: an outage. §11.2's "move reclamation off the accept path" is still not done,  _(maybe resolved)_
- L370: the guest down. §11.2's reclaim-off-the-accept-path is still not done. This  _(maybe resolved)_

</details>

<details><summary>docs/archive/REDIS_ROUND_TRIP_STAGE_TRACE.md (4)</summary>

- L321: **Ingress structurally cannot tell you, and upstream has a TODO saying so.**
- L327: // TODO: Propagate the PollIngressSingleResult from deeper.
- L336: which has no access to the return value. Threading it up is the TODO.
- L493: §11.2 (reclamation off the accept path, still not done).  _(maybe resolved)_

</details>

<details><summary>docs/archive/REDUCING_PLATFORM_DEPENDENCY.md (12)</summary>

- L19: / §0 / The arch gate ('target_os' alone stopped discriminating) / **APPLIED 2026-09-03.** 13 → 36 crates build for 'x86_64-unknown-none'. Re-confirmed 2026-09-04 — and s…  _(maybe resolved)_
- L20: / §1 / PTE permissions are AArch64 bits inside a crate that forbids knowing that / **DONE 2026-09-06 — see §1.5.** 'akuma_mmap::Prot' is the neutral vocabulary and 'Mmap…  _(maybe resolved)_
- L21: / §2 / Device discovery arrives as a DTB, threaded through the MMU / **OPEN**, re-confirmed 2026-09-04: x86_64 Firecracker passes no DTB at all — the memory map comes fr…
- L22: / §3 / TLB invalidation cannot express *who* / **OPEN**, re-confirmed 2026-09-04: 'crates/akuma-cpu/src/lib.rs''s 'mod tlb' still has only 'tlbi' forms ('vmalle1', 'vaae…
- L25: / §6 / Write down the per-CPU-identity rule / **OPEN.** One paragraph, no code change /
- L26: / §9.2 / *(new)* Process lifecycle (fork/exec/wait4/spawn) hand-rolled a second time / **OPEN, not previously tracked.** 'crates/akuma-kernel-amd64/src/usermode.rs' reim…  _(maybe resolved)_
- L365: #### Deliberately still open
- L393: **OPEN.** The second, non-DTB machine this predicted now exists: PVH's
- L456: **OPEN**, and sharper than when written: see the status table.
- L524: **OPEN.** A second hand-built 'Context' now exists in 'crates/akuma-kernel-amd64/src/sched.rs'.
- L624: **OPEN.** Still one paragraph and no code change.
- L880: this exact mistake. 'akuma-mmu' still has ten instances of the same gate, unfixed,

</details>

<details><summary>docs/archive/REPR_C_SIGFRAME_STATX.md (2)</summary>

- L31: ## 2. Two divergences from Linux, found and deliberately not fixed  _(maybe resolved)_
- L221: Recorded as a finding, not fixed here: see  _(maybe resolved)_

</details>

<details><summary>docs/archive/RTW89_WIFI_DORMANCY_AUDIT.md (1)</summary>

- L6: planned (not yet implemented).

</details>

<details><summary>docs/archive/RUMP_LATENCY_SLEEP_FIX.md (1)</summary>

- L204: ### Step 3 — Reduce the thread count at the source (DEEP; deferred/blocked)

</details>

<details><summary>docs/archive/RUMP_ON_SMP.md (2)</summary>

- L115: - Rump is **deferred** as a project (2026-07-19); devbox-smoltcp is the
- L227: ## Not done  _(maybe resolved)_

</details>

<details><summary>docs/archive/RUMP_PLUS_HERD.md (1)</summary>

- L6: This doc is the design for the next step: let **herd** spawn and supervise the

</details>

<details><summary>docs/archive/RUMP_SYSPROXY.md (14)</summary>

- L131: proven 'run_demo'. The kthread/request-queue model is deferred until this is
- L169: - ⏳ **Open sub-problem:** 'poll'/'ppoll' on a 'RumpSocket' fd (curl's nonblock
- L193: - ⚠️ **LATENCY — root-caused, not yet fixed (the remaining work).** Each forwarded  _(maybe resolved)_
- L261: that needs no client patch — deferred since the sic patch sufficed.)
- L262: - ⚠️ **Robustness bugs found (see project task / TODO):** box procs stuck in a
- L307: still TODO (project policy) — the DNS path is validated e2e, not by a self-test
- L351: - ⚠️ **OPEN — in-box fork+exec of a large *dynamic* binary faults.** 'busybox sh' →
- L372: Acceptable as-is for the non-prod showcase; TODO before any non-showcase use.
- L385: - **The sysproxy channel fd is private to rump_server (TODO).** With the kernel-pipe
- L392: - **rump_server is not killable from inside the box (TODO).** The 'rump_server' is the
- L400: - **Verify per-box isolation of the proxy (TODO for later).** Each box's 'rump_server'
- L409: - **Fiber rumpuser → one OS thread per NetBSD kernel (TODO).** Our 'rumpuser' uses the
- L415: these to ~one OS thread per kernel. Deferred: it's a different concurrency model, and
- L426: Deferred until rumpnet boots properly without '--net'; revisit with the tap-RX /

</details>

<details><summary>docs/archive/RUMP_SYSPROXY_LATENCY_FIX.md (10)</summary>

- L6: &gt; deferred); accepted for the merge in ['SGI_SCHEDULER_RESIDUE.md'](SGI_SCHEDULER_RESIDUE.md).
- L9: &gt; verified. Phase 3e/3f root-caused (not yet fixed) a residual ~300ms  _(maybe resolved)_
- L42: &gt; code kept). **The floor is still open** — Phase 3p (measure the
- L44: &gt; path specifically) has the honest next step.
- L269: ## Phase 3e — the ~300 ms single-round-trip floor: root-caused, not fixed  _(maybe resolved)_
- L411: ### Not yet done: actually testing a fix  _(maybe resolved)_
- L828: **Next steps**: Based on these findings, further optimization would require:
- L1092: net-poll loop — a separate follow-up with the same harness.
- L1161: net-poll loop — a separate follow-up with the same harness.
- L1163: ## Future work (Phase 3, not started)

</details>

<details><summary>docs/archive/RUST_ORG_IL_TALK.md (3)</summary>

- L139: / VFS & filesystem / 134 / 140 / ext2 wrote dirent types without 'INCOMPAT_FILETYPE', and read a 0 type as "file"; no triple-indirect blocks and 32-bit sizes (the 4 GiB …
- L149: and 4 GiB cap (2), and the pin table and deferred free behind deleted files keeping their space. **Six were in code
- L419: million, ~145M tokens per 10,000-credit week, below the ~195M measured. Unresolved; a legacy

</details>

<details><summary>docs/archive/RUST_TOOLCHAIN.md (5)</summary>

- L395: &gt; left as follow-up since it is a larger, higher-blast-radius change to a
- L452: ## 5. Follow-ups flagged during the investigation
- L461: **Status: resolved.** This was the open question of whether the §3a 'WILD-DA'  _(maybe resolved)_
- L472: 'munmap' per the PSTATS). Two distinct follow-ups, somewhat opposed:
- L486: were deferred until the toolchain worked — which it now does.

</details>

<details><summary>docs/archive/RUST_TOOLCHAIN_AMD64.md (4)</summary>

- L20: their open item 1 is closed. Follow-up to 'docs/archive/RUST_TOOLCHAIN_ISSUES.md'  _(maybe resolved)_
- L299: ### Open: 'rustc hello.rs' with no '-C linker' → 'could not exec the linker \'cc\': Function not implemented (os error 38)'
- L463: * **Open item 4 is fixed.** Spawn children do inherit cwd; the note below  _(maybe resolved)_
- L657: above ~16 pages wins and a dense one does not. If a future workload sits on the

</details>

<details><summary>docs/archive/RUST_TOOLCHAIN_ISSUES.md (3)</summary>

- L186: / Root cause / ⚠️ **not investigated** — found at the very end of a long debugging session, deferred to next time /
- L245: ### Next steps
- L379: ### Remaining work (lower priority)

</details>

<details><summary>docs/archive/SCHEDULING_AUDIT.md (1)</summary>

- L170: &gt; follow-ups: ['SCHEDULING_INVESTIGATION.md'](SCHEDULING_INVESTIGATION.md) →

</details>

<details><summary>docs/archive/SCHEDULING_INVESTIGATION.md (12)</summary>

- L40: recovered ([What is still open](#what-is-still-open) item 6). Still open:
- L267: - **The catastrophic floor is an SMP=1 phenomenon.** At SMP=4 the *unfixed*
- L297: build, left as the follow-up below.
- L368: is out of scope for the measurements (deferred work, handled separately), so a
- L397: &gt; method, and the remaining gaps in [What is still open](#what-is-still-open)
- L455: **rump is deliberately out of scope here** — it is deferred work and its owner
- L494: Not yet root-caused. Nothing here has been reverted. Next steps: reproduce
- L601: + concurrent traffic) is the outstanding step — not yet confirmed by a human
- L658: Residual risk, unresolved: the render loop holding 'state.lock()' across
- L684: ## What is still open
- L699: 4. **'extreme-size' on a 1 ms tick** — **DEFERRED by decision, 2026-08-18.**
- L842: &gt; [What is still open](#what-is-still-open).

</details>

<details><summary>docs/archive/SCRATCH.md (1)</summary>

- L260: ## Future Work

</details>

<details><summary>docs/archive/SECOND_LISTENER_SMP1_FREEZE.md (2)</summary>

- L47: silence — the deferred address-space free owned by the first child's thread is
- L237: Remaining follow-ups: (a) the §2a open sub-question (why healthy boots

</details>

<details><summary>docs/archive/SECURITY_AUDIT_2026_11_16.md (2)</summary>

- L4: has landed since, and what is still open, is in the next section.  _(maybe resolved)_
- L28: Still open:

</details>

<details><summary>docs/archive/SELFHOST_CARGO_BUILD_REGRESSION.md (1)</summary>

- L118: ## Next steps (all done, kept for the record)  _(maybe resolved)_

</details>

<details><summary>docs/archive/SELFHOST_DEVBOX_SMOLTCP.md (15)</summary>

- L24: Follow-up to 'AKUMA_SELF_HOSTING.md' §7j (the original '--release -p akuma',
- L254: ## Open issue: thread-spawn SIGABRT under real '-j4' parallelism (2026-08-03 follow-up, UNRESOLVED)
- L256: Follow-up session, HEAD '48abffc' ("damn it is taking forever" — includes the
- L307: site hasn't been captured live (no 'GDB=1' on this boot). Next step in
- L313: ## Open issue #2: a genuine lost-wakeup hang, distinct from both bugs above (2026-08-03, UNRESOLVED)
- L446: **One unprobed sharp edge, flagged not fixed:** 'futex_key_tgid' →  _(maybe resolved)_
- L609: **Recommended next steps, in the agent's stated order:** (1) settle the
- L966: ## Follow-ups the same session
- L977: requeued waiters, left open for dead ones — and invisible to 'futexops', which
- L1160: **Still open, and not touched by this:** the rustc '-j4' abort itself is real —
- L1243: **Next step, and it is cheap:** symbolize '0x300204e8' and '0x3801c58c' in the guest's
- L1394: 2. Thread-spawn 'SIGABRT' — **open**, unchanged, now known to be crate-agnostic.
- L1395: 3. Lost-wakeup stall — **open**, unchanged, but now reproducible in ~2 minutes
- L1544: the concurrency '-j4' supplies. Extending that probe is the cheap next step.
- L1668: ### The EINTR gap is real, still open, and is the next thing to fix

</details>

<details><summary>docs/archive/SELFHOST_KERNEL_HEAP_LEAK.md (6)</summary>

- L13: 'Vec' and only *then* frees them in a loop. It runs on the deferred-reclaim
- L219: '25c817b8') settled most of this doc's open questions.
- L280: deferred-reclaim machinery works, and 'docs/archive/
- L296: ### Instrumentation status and the next step
- L327: - 'docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md' — deferred process
- L408: / 'SHARED_L0_TABLE' deferred maps / not it / '[L0PARK]' flat **0** /

</details>

<details><summary>docs/archive/SELFHOST_ZERO_PAGE_HUNT.md (13)</summary>

- L3: &gt; **Status: the ODHT ICE is still OPEN. A real, pervasive use-after-free was found
- L156: / 'MADV_DONTNEED' zeroing a file-backed page ('MADV_DONTNEED_SHARED_FRAME.md' "Still open" #2) / '[DONTNEED-FILE]', 'DONTNEED_FILE_BACKED' / **0** /
- L230: end state. 'MADV_DONTNEED_SHARED_FRAME.md' "Still open" item 4 — 'complete_cow_break'
- L256: ('MADV_DONTNEED_SHARED_FRAME.md' "Still open" #4).
- L520: ## 11. What is NOT done  _(maybe resolved)_
- L617: Follow-up on one victim: inode 44076 was read at 1.0–1.1 MB offsets during the
- L673: T1 (read-copy shortness) remains untested; T3's SMP=1 wedge is still open; T4
- L858: ### What is NOT done (carrying §11 forward)  _(maybe resolved)_
- L863: - The inode-lifecycle defect itself is **diagnosed, not fixed** — no option  _(maybe resolved)_
- L918: sharing a number alias. Aliasing can only *add* pins, so it costs a deferred
- L981: class ('CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md', still open), and it is better
- L1024: / 'akuma-ext2' / 6 tests: **data readable after unlink**, **number not reissued while pinned**, reclaimed once unpinned, unpinned unlink still frees immediately, repeate…
- L1051: half, and the four "Still open" items, one of which (#4) is a live candidate here

</details>

<details><summary>docs/archive/SERIAL_TRACE_TRAFFIC_AUDIT.md (1)</summary>

- L105: ## 5. Follow-ups not done here  _(maybe resolved)_

</details>

<details><summary>docs/archive/SGI_SCHEDULER_RESIDUE.md (1)</summary>

- L109: - **'NETWORK_THREAD_RATIO':** not measured (rump devbox, deferred). The scheduler no longer

</details>

<details><summary>docs/archive/SHARED_FD_TABLES.md (1)</summary>

- L88: resource destruction is deferred to the 'Drop' implementation.

</details>

<details><summary>docs/archive/SIGCHLD_DELIVERY_FIX.md (11)</summary>

- L4: campaign left open:
- L6: &gt; **'wait' still hangs** (§11.3). Unfixed — it needs SIGCHLD delivery, which is a larger piece of
- L305: blocking syscalls. The consequence of *not* setting it is that SIGCHLD handler delivery is deferred
- L431: - Interrupting **in-flight blocking syscalls** on SIGCHLD (§5.3). Delivery is deferred to the next
- L498: follow-up worth doing once this lands: delete that workaround from the harness and confirm the §11.1
- L538: §4.4 (real SIGCHLD 'siginfo') was **not** done — still open, not required for 'wait'.  _(maybe resolved)_
- L544: **Not fixed. Follow-up, tracked separately from the delivery fix above.**  _(maybe resolved)_
- L578: **Conclusion: userspace bug in busybox ash, not a kernel bug.** Deferred — pick up later. Whoever
- L584: completed in this pass — still outstanding.
- L599: ('wait4' returns the correct status). Left open, to pick up later. 'forktest_smp_matrix.py'
- L600: regression run still outstanding.

</details>

<details><summary>docs/archive/SIGNAL_DELIVERY.md (2)</summary>

- L37: SIGURG handler was running was silently deferred.
- L52: 7. The second SIGURG is deferred to the *next* syscall. By that time the

</details>

<details><summary>docs/archive/SIGNAL_DELIVERY_FORKTEST_EVIDENCE.md (5)</summary>

- L65: ## Suggested audit scope (for follow-up work)
- L620: ### Deferred
- L728: The corruption is still open — see 'docs/AKUMA_SELF_HOSTING.md' §7k.4 for the full
- L741: - **2026-05-10:** Fixes 1, 3, 5, 6 implemented in 'src/exceptions.rs'; boot verified; fixes 2 and 4 deferred.
- L743: - **2026-06-22:** **Per-thread signal mask implemented** ('THREAD_SIGNAL_MASK[MAX_THREADS]'); all delivery / 'rt_sigprocmask' / 'sigreturn' / 'tkill' sites converted; re…

</details>

<details><summary>docs/archive/SIGNAL_FATAL_DEFAULT_LINUX.md (1)</summary>

- L120: ## Still open

</details>

<details><summary>docs/archive/SIGNAL_HELL.md (3)</summary>

- L180: NOT a decoder bug and NOT fixed. This is a runtime/QEMU concern: QEMU's TCG is  _(maybe resolved)_
- L469: issue follow-up network requests. Each adds runnable threads and more
- L563: ## Structural follow-up (not yet done)  _(maybe resolved)_

</details>

<details><summary>docs/archive/SMP_FORK_EXEC_CORRUPTION_FIX.md (4)</summary>

- L166: &gt; live sighting of the still-unfixed accounting leak; root-causing it is the
- L167: &gt; open follow-up** (start from thread-migration-while-in-EL1 and the
- L192: &gt; **Validated fix direction (TODO):** the *mechanism* (disable preemption during the
- L459: - ~~Live lldb over the gdbstub~~ Deferred in favor of protocol fixes; still useful for  _(maybe resolved)_

</details>

<details><summary>docs/archive/SMP_GO_STRESS_CORRUPTION_FIX.md (9)</summary>

- L3: &gt; **STATUS 2026-07-22 (same day, follow-up session): bugs 1 and 2 FIXED — see  _(maybe resolved)_
- L4: &gt; "Resolution" at the bottom. Bug 3 (ticket leak) still open; two new residuals
- L164: bug 3 still open).
- L270: ### Still open
- L273: (deferred kill at the EL1→EL0 boundary).** After the BKL fix, 7/7 stress runs
- L287: **Fix (deferred kill):** under 'cfg(kernel_smp_shared)', 'kill_thread_group'
- L313: - **SMP=4 boot self-test suite wedges — RESOLVED (2026-07-23, follow-up  _(maybe resolved)_
- L332: context). The deferred-kill fix's 'request_thread_kill' WAKES parked
- L337: 'mark_current_terminated'), making the test exercise the deferred-kill

</details>

<details><summary>docs/archive/SMP_SHARED.md (19)</summary>

- L17: which is now the default devbox — rump_server work is deferred.
- L52: for M0.** The planned "map all GICR frames" step is deferred to M2, when secondaries
- L115: **Deferred to M2 (deliberately — needs cross-core contention to test):** the
- L120: once M2 adds reconciliation. 'ThreadPool.current_idx' retirement also deferred to M2
- L175: deferred.
- L220: **Deferred within M2 → folded into M3** (only needed once *user* address spaces run
- L244: the switching core's own TLB). **ASIDs deferred** (still ASID 0 for all): correctness holds
- L246: broadcast — verified with no faults. Real per-AS ASIDs remain a *performance* follow-up.
- L284: **Cross-core wakeup — built, then deliberately NOT fired (deferred to M5).** Infrastructure:
- L300: **Open item — full devbox-smoltcp boot to userspace under SMP (investigated, not yet fixed).**  _(maybe resolved)_
- L328: herd 'sshd.conf' needs re-provisioning for shared SMP (the task-2 follow-up).
- L581: ### M5b Stage 4b — reconcile-aware full flip (faults never hold BKL) — OPTIONAL/DEFERRED
- L585: higher-leverage next step than making the last ~20 % of fault work BKL-free.
- L698: ### 3. BKL fair-FIFO ticket leak: self-healing acquire ✅ (root cause still open)  _(maybe resolved)_
- L713: '[BKL] RECOVERED (&lt;kind&gt;) by core N' — **each line is a live sighting of the unfixed
- L714: accounting leak**; root-causing it is an open follow-up (lead suspect: a thread
- L722: oldest outstanding disable. This is what turned each wedge into a one-line diagnosis.
- L918: - **Still open**: terminated-thread lock leak — 'kill_thread_group' PHASE 1
- L930: ## Deferred sibling kill at the EL1→EL0 boundary (2026-07-23)

</details>

<details><summary>docs/archive/SMP_SHARED_M5_FAULT_LOCK_PLAN.md (1)</summary>

- L178: 4. (Follow-up M5c) revisit cross-core wakeup ('wake_remote_idle') now that same-AS faults

</details>

<details><summary>docs/archive/SMP_SHARED_ONCPU_GATE.md (1)</summary>

- L78: per-core idle fallback — skips gated threads. A skipped wake is only deferred to

</details>

<details><summary>docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md (1)</summary>

- L30: &gt; answer the delayed-first-byte-vs-any-long-idle question this doc left open.

</details>

<details><summary>docs/archive/SQLD.md (1)</summary>

- L186: ## Future Work

</details>

<details><summary>docs/archive/SSHD_CHANNEL_WINDOW_NEVER_ADJUSTED.md (3)</summary>

- L80: §6 left the reachability question open: *"whether it is live-triggerable depends
- L125: - Client EOF is deferred through 'stdin_eof_pending' until the residue drains,
- L190: §6 (the unfixed stdin twin) and §8.5 Phase 0 item 5 (the work item this came

</details>

<details><summary>docs/archive/SSHD_DEAD_CLIENT_TEARDOWN.md (1)</summary>

- L32: 'ps' and the kernel's PSTATS lines. Open: give children their own process group ('setpgid' is not in libakuma).

</details>

<details><summary>docs/archive/SSH_PIPE_SESSIONS_LOST_STDERR.md (1)</summary>

- L56: ## Still open

</details>

<details><summary>docs/archive/SSH_SESSIONS_HAD_NO_PTY.md (1)</summary>

- L66: ## Still open

</details>

<details><summary>docs/archive/SSH_STREAMING_ARCHITECTURE.md (1)</summary>

- L281: The streaming infrastructure is correctly implemented. The limitation is architectural: SSH session threads cannot directly trigger network transmission, which is exclus…

</details>

<details><summary>docs/archive/STABILITY_URGENT_ISSUES.md (5)</summary>

- L16: Deferred Follow-ups section.
- L117: deadlock surface — landed on 2026-05-29. Summary of findings and follow-up  _(maybe resolved)_
- L175: the storm ends; a follow-up 'ssh_harness.py connect' completes in
- L341: ## SSH Lifecycle & Visibility (Deferred Follow-ups)
- L346: load-bearing change. They remain useful follow-ups whenever SSH gets

</details>

<details><summary>docs/archive/STACK_CORRUPTION_ANALYSIS.md (5)</summary>

- L238: ### Mitigation: Deferred Cleanup Mode
- L275: **After (deferred cleanup):**
- L286: 1. **Deferred cleanup mode** ('config::DEFERRED_THREAD_CLEANUP = true')
- L294: ### Remaining Work
- L308: - 'src/threading.rs' - Added deferred cleanup with cooldown

</details>

<details><summary>docs/archive/STALE_THREAD_SLOT_KILL.md (4)</summary>

- L7: This is the residual 'rustc -O big.rs' hang left open as §6 of
- L129: Under 'kernel_smp_shared', PHASE 1 does **not** hard-kill siblings. It posts deferred
- L223: PHASE 1 only *requests* a deferred kill, and its grace loop exits as soon as each
- L332: "draw the lifecycles" follow-up it motivates.

</details>

<details><summary>docs/archive/STDCHECK_DEBUG.md (1)</summary>

- L23: ### 3. Box Test Kernel Crash (DEFERRED)

</details>

<details><summary>docs/archive/SYSCALL_LAYER_AUDIT.md (2)</summary>

- L336: / 'msgqueue.rs' internal clones / ':51'↔':67', ':169'↔':208', ':189'↔':281', ':296'↔':307' / 4 blocks, 58 covered lines at 50 tokens / Not read at all this pass — msgque…
- L353: blocking-loop shape, this is the highest-value follow-up if someone

</details>

<details><summary>docs/archive/TAOXICHUAN_AMD64_EXTRACTION_PLAN_GLM.md (3)</summary>

- L181: leaks (COW_FORK "Still open": the BTreeMap refcount cost). Mitigation: pin-enum + per-value
- L212: COW_FORK "Still open" BTreeMap note). Acceptable to make the plan a '&mut FnMut' visitor
- L245: All three extraction slices of this plan that were still open (3, 4, 5) are done; the remaining 'Q's are the uaccess adoption (S6) and amd64 adopting 'akuma-syscalls-sig…  _(maybe resolved)_

</details>

<details><summary>docs/archive/TAOXICHUAN_B21_AARCH64.md (6)</summary>

- L86: / A2.2 / '1ad69051' / **'shmpages' moved to core**, behind a 'shmpages' feature ('disk' adds 'inode_freed'). It reaches the machine only through the PMM's share count an…
- L87: / A2.3 / '14ed9ac8' / **Demand paging is core's, over a page-table trait.** (a) The lazy-record codec moved from 'akuma-pte-amd64::lazy' to 'akuma-tokio-kernel-core::laz…
- L88: / A2.4 / '94cc73a1' / **The task record is core's.** 'core::task': 'Task' (root page table by physical address, the 'akuma-user-layout' layout, the exit code), 'Tasks&lt…
- L95: / A5.3 / '1fd39bfc' / **The machine comes up: device tree, PMM, vectors, tick.** (a) 'akuma_boot_aarch64::read_dtb' copies the tree '_boot' was handed (validated magic a…
- L100: / A5.6c / 'fe59580a' / **One process model: fork, exec, exit and reap in core.** New 'akuma-tokio-kernel-core::proc' (feature 'process'), moved from the amd64 half's 'ch…
- L101: / A5.7 / '5c7e895e' / **Signals on aarch64; delivery's decisions in core.** New 'akuma-tokio-kernel-core::signal', moved from the amd64 half's 'child.rs': which pending …  _(maybe resolved)_

</details>

<details><summary>docs/archive/TAOXICHUAN_B9_MADVISE_PIDFD_SIGSUSPEND.md (1)</summary>

- L66: ## Still open

</details>

<details><summary>docs/archive/TAOXICHUAN_BKL_NESTED_TICKET_DEADLOCK.md (3)</summary>

- L20: 'SERVING = 0x983'**. Five tickets outstanding for four spinners. Each spinner's own ticket is in 'RBP' (0x984, 0x985, 0x986, 0x987)
- L47: first lap of a wait and asks for the lock. **It fails on the unfixed 'enter'** (the assertion "a handler that waited behind its own
- L49: - **Under load, amd64, 'middleware' kernel, SMP=4:** the unfixed tree wedged on boot 9 and boot 16 of two separate runs (16 busy loops

</details>

<details><summary>docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md (4)</summary>

- L29: died before and runs on to the failure in section 7. Follow-up (same day): "too many entries" and "a string longer than 'STR_MAX'" now answer 'E2BIG' too
- L48: destroys the manifest and the run fails in 4 s. Next step: the same control with a pre-written 'Cargo.toml', at 8 GiB.
- L123: - **Still open:** 'register_process' is infallible (the note in 'table.rs' and 'docs/reference/subsystems/memory.md'); a genuine 256-live
- L136: - **Related, not fixed:** the 16-entry-order 'nr=9 -12' failures also hide that the slab is shared by a process's threads, and a mapping is  _(maybe resolved)_

</details>

<details><summary>docs/archive/TAOXICHUAN_BUG_FIX_LIST.md (5)</summary>

- L320: - 'UNLINKED_FILE_SPACE_RETURNED_LATE.md': ext2 finished a deferred free only at the next unlink or create, so a file unlinked
- L343: - **Open by their own text:** TAOXICHUAN_J4_BKL_CONVOY (the in-guest '-j4' BKL convoy; its p3 repair is a rig change);
- L355: - **Scanned 2026-11-03** (the Infinity run and the three items it left open): INFINITY_ON_TAOXICHUAN's other six bugs are
- L357: fix, and sshd-as-init's orphan miscount was retired by running herd as init, not fixed. TAOXICHUAN_LAZY_MMAP's prefault  _(maybe resolved)_
- L363: is a measurement. Fault-signal delivery is still open. A third pass: the measurement did not finish (the disk ran out before the final link) and the

</details>

<details><summary>docs/archive/TAOXICHUAN_CLEANUP_PLAN.md (6)</summary>

- L243: / analysis row 8: one ELF loader / '3e2bb725' / 6,643 (source unchanged); audit 97,853 -&gt; 92,404 / 'akuma-exec''s loader is a feature, 'elf-loader', off by default: t…  _(maybe resolved)_
- L259: / B9: signal delivery; 'kill' / (this commit) / 4,259 -&gt; 4,425 ('main.rs' + 'child.rs', 'wc -l') / 'kill', 'tkill', 'tgkill' are the x86 kernels' shared bodies ('akum…  _(maybe resolved)_
- L260: / No private syscall / (this commit) / -3 lines / Taoxichuan's last Akuma-private call, 'UPTIME' ('0x1000 + 319'), is gone with its constant in 'akuma-ring3-amd64::nr': …  _(maybe resolved)_
- L263: / B9 (rest): 'madvise', 'pidfd_open', 'rt_sigsuspend' on all three / (this commit) / 'taoxichuan/src' 5,925 -&gt; 5,957 ('wc -l') / Taoxichuan's 'madvise' (the decode an…  _(maybe resolved)_
- L268: / B12 plan / (this commit) / - / Not started: the step moves the AArch64 scheduler's switch out of 'akuma-threading''s default path, so it is written down first, in five…
- L269: / B13: docs sweep (2i) / (this commit) / - / 'taoxichuan/docs' rewritten to the tree after B11: 'reference/syscalls.md' is the dispatch order (legacy fold, console inter…  _(maybe resolved)_

</details>

<details><summary>docs/archive/TAOXICHUAN_COW_FORK.md (3)</summary>

- L9: user's open questions were answered by default (both fork paths, exact-zero leak bound, prints under 'trace-err',
- L29: ## Still open
- L67: Next step: a control with a pre-written Cargo.toml.

</details>

<details><summary>docs/archive/TAOXICHUAN_COW_FORK_PLAN_GLM.md (2)</summary>

- L29: **Taoxichuan's own pieces** (all read): the eager cloners ('fork_share_memory', 'clone_user_half', 'clone_user_tree' — walk PML4 0..256 only, refuse PS leaves, copy pare…
- L124: **If only two slices fit**: (1) **S2+S3 merged** — share loop + fault arm, minimal form (no 'akuma-mmu' extraction; two counters printed): this is the functional win — t…

</details>

<details><summary>docs/archive/TAOXICHUAN_CRATE_EXTRACTION_PLAN.md (1)</summary>

- L298: Not started: X6-X10. The nested-crossing retirement (S3) is still open.

</details>

<details><summary>docs/archive/TAOXICHUAN_EFFICIENCY_PLAN_GLM.md (2)</summary>

- L21: / 8 / COW refcounts without a 'BTreeMap' (page-tail refcount) / Contrast: 'crates/akuma-fpcache' reuses 'cow_ref_inc/dec' with O(1) bookkeeping; taoxichuan's 'COW_REFCOU…
- L50: - ~1-25: read 'src/syscall/utils/read_profile.rs' (AArch64) + 'read_stage_profile.py'; note its "within-one-boot" discipline (read_path_ab.py header: wall clock swings ~…

</details>

<details><summary>docs/archive/TAOXICHUAN_EXEC_ARGV_LIMIT.md (1)</summary>

- L56: ## Still open

</details>

<details><summary>docs/archive/TAOXICHUAN_JVM_ATTEMPT_2026_11_09.md (1)</summary>

- L39: failed mapping returning an error that HotSpot then dereferences, or a deliberate 'crash_with_segfault'. Next step

</details>

<details><summary>docs/archive/TAOXICHUAN_KILL_SIBLINGS_ON_CPU_RACE.md (1)</summary>

- L73: ## Still open

</details>

<details><summary>docs/archive/TAOXICHUAN_LAZY_MMAP.md (3)</summary>

- L68: which ext2's deferred free respects. A fork child inherits its parent's pins, and 'free_user_root' drops
- L113: ## Still open
- L115: - **Sharing** and **'PROT_NONE'**, left open here, are fixed in  _(maybe resolved)_

</details>

<details><summary>docs/archive/TAOXICHUAN_LOADER_SHARED_SEGMENT_PAGE.md (1)</summary>

- L60: ## Still open

</details>

<details><summary>docs/archive/TAOXICHUAN_M4_PI_STALL_PLAN.md (1)</summary>

- L64: IF windows (tick_window) still open, or the spinner is ring 3 (ticks preempt it freely).

</details>

<details><summary>docs/archive/TAOXICHUAN_M4_SLICE3_PE_RESULT.md (1)</summary>

- L176: PK1; PK2 deferred — forkprobe is a repo crate, not in the image, and

</details>

<details><summary>docs/archive/TAOXICHUAN_M4_SLICE3_PF_PLAN.md (1)</summary>

- L30: dropped in 'release'. PK1 is impossible through them; K9 still open.

</details>

<details><summary>docs/archive/TAOXICHUAN_M4_SLICE3_PG_PLAN.md (2)</summary>

- L55: - 'AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md' §6.1 — OPEN: a metal worker spins
- L58: kind; still open there; does not decide our case but CONFIRMS the

</details>

<details><summary>docs/archive/TAOXICHUAN_METAL_SH_SPAWN_EPERM.md (1)</summary>

- L27: ## Open: the host wedges under guest load

</details>

<details><summary>docs/archive/TAOXICHUAN_ONE_CLOCK_TIME_FOLD.md (2)</summary>

- L65: 4 KiB. busybox 'cat''s 'sendfile' answering 'ENOSYS' (the known gap) comes before the hang and falls back to
- L68: Open: not run on the ryzen under KVM, where the TSC rate comes from the LAPIC calibration rather than the PIT. The

</details>

<details><summary>docs/archive/TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md (1)</summary>

- L102: ## Still open

</details>

<details><summary>docs/archive/TAOXICHUAN_SHARED_MAPPINGS_AND_LAZY_IMAGE.md (4)</summary>

- L20: ['TAOXICHUAN_LAZY_MMAP.md'](TAOXICHUAN_LAZY_MMAP.md) left open.
- L127: ['TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md'](TAOXICHUAN_PROT_NONE_AND_SHARED_FILE_PAGES.md) left open.
- L148: deferred and never completed. That was a pin-table defect, fixed separately in  _(maybe resolved)_
- L168: ## Still open

</details>

<details><summary>docs/archive/TAOXICHUAN_SIGNAL_DELIVERY.md (1)</summary>

- L58: ## Not done (known, pinned)  _(maybe resolved)_

</details>

<details><summary>docs/archive/TAOXICHUAN_SMP.md (2)</summary>

- L30: ## Not caused by SMP (found on the way, still open)
- L49: logged 20 "stuck" lock reports), so SMP here buys correctness of the parallel ring-3 path, not speed yet. The next step is finer-grained locking.

</details>

<details><summary>docs/archive/TAOXICHUAN_STAGES9_11_PN_RUST_SYSCALLS_PLAN.md (2)</summary>

- L100: 'to_aarch64()'-style numbers — then fits for every family where the remaining work is
- L218: / **PK2 + rare boot stall still open** (gaps G1/G2) / more slots + more waits stress the open items / sequence the gates so a stall is attributable: S4 lands after pm's …

</details>

<details><summary>docs/archive/TAOXICHUAN_SYSCALL_MIDDLEWARE.md (3)</summary>

- L102: so there is no second mapping to refuse. That case is pinned in the probe as a known gap. It is a demonstration, not a wholesale
- L145: - **NOT done:** the feature-on cost against the syscall floor ('scripts/benchmarks', run A/B/A). With the  _(maybe resolved)_
- L162: is deferred ('docs/reference/subsystems/rump-stack.md'). It is the API's acceptance test if it returns.

</details>

<details><summary>docs/archive/TAOXICHUAN_UNAME_SAYS_AKUMA.md (1)</summary>

- L38: Open: the K2 guest left running on the ryzen predates this and still answers Akuma until a newer kernel boots there.

</details>

<details><summary>docs/archive/TAOXICHUAN_UNSAFE_REDUCTION_PLAN_GLM.md (2)</summary>

- L65: cross-core BKL-protected scans, and its contract is deferred reclamation. taoxichuan wants BSS
- L316: Not done (and why): 'TASKS'/'SLOTS'/'SLOT_STATE' hand out '&'static mut' that code holds across calls and the naked crossing  _(maybe resolved)_

</details>

<details><summary>docs/archive/TCC_LOW_MEMORY.md (1)</summary>

- L99: '.a'. (Deferred; it would be for self-containment, not for the floor.)

</details>

<details><summary>docs/archive/TERM_POLL_INPUT_PREEMPTION_FIX.md (4)</summary>

- L3: **Date:** 2026-08-11 (analysis), refined 2026-08-11 (follow-up, §9-§10),
- L253: ## 6. The fix space (not prescriptive, not yet implemented)
- L349: ## 9. Refined root cause (2026-08-11 follow-up)
- L465: ## 10. Fix plan (decided 2026-08-11, not yet implemented)

</details>

<details><summary>docs/archive/THREAD_SCHEDULING_INVESTIGATION.md (2)</summary>

- L28: **TODO**: Investigate why munmap syscall from realloc causes hangs.
- L146: ## Next Steps

</details>

<details><summary>docs/archive/THREAD_SPAWN_SEGV_INVESTIGATION.md (4)</summary>

- L355: ## 2f. STILL OPEN after 2e: a thread running in foreign page tables
- L401: Also still open, and possibly related: the same run logged **81 '[BKL] stuck'
- L808: ### Still open
- L1202: owner-core near-permanent hold) reproduces on both fixed and unfixed kernels —  _(maybe resolved)_

</details>

<details><summary>docs/archive/THREAD_STACK_ANALYSIS.md (1)</summary>

- L58: &gt; **Future work**: Refactor network initialization to support spawning async on a

</details>

<details><summary>docs/archive/TLS_DOWNLOAD_PERFORMANCE.md (3)</summary>

- L130: - Future work: per-thread request tracking or rate limiting
- L137: ### Future Work
- L144: **Potential solutions (not yet implemented):**

</details>

<details><summary>docs/archive/TLS_INFRASTRUCTURE.md (1)</summary>

- L195: Future work to add proper certificate verification to userspace:

</details>

<details><summary>docs/archive/TOKIO_KERNELS_LOOPBACK_STARVATION.md (1)</summary>

- L32: ## Still open

</details>

<details><summary>docs/archive/TOKIO_PIPE_EPOLL_HANG.md (1)</summary>

- L151: ## Still open: the terminal half

</details>

<details><summary>docs/archive/TRAMPOLINE_STALE_PROCESS_RELR.md (1)</summary>

- L229: ## 8. Still open

</details>

<details><summary>docs/archive/TRIMMING_FAT_SCHEDULER.md (1)</summary>

- L95: - 'TRIM_FAT_EMBARRESSING_DUPLICATIONS.md' § "Deferred audit" — the original

</details>

<details><summary>docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md (36)</summary>

- L16: quality floor — not started) and the three deferred audits at the end of this
- L19: deferred" rows were stale until 2026-08-24; see the corrections in place below. 'crates/akuma-primitives' exists and all six rungs
- L285: (slice vs path — pure plumbing) and **eager vs deferred mapping** (a real
- L287: deferred segments and loaded-from-bytes *implies* eager, which is an accident of
- L298: returns 'Vec::new()' for deferred segments where the path variant returns
- L859: **Found and not fixed here — fixed 2026-08-14:** 'src/console.rs''s 'print_dec'  _(maybe resolved)_
- L1011: wrong. **Not fixed** — Linux returns 'ENOTTY' for an ioctl on something that is  _(maybe resolved)_
- L1310: the follow-up question directly. Everything in the first six rows is integer
- L1335: ### Still open here — **CLOSED 2026-08-14, by the 'akuma-pmm' extraction**  _(maybe resolved)_
- L1407: **Still open:** the *speed* win this was chosen to buy is unmeasured — the decision
- L1472: ('userspace/sshd/docs/EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md', still open per
- L1690: / 11 / ~~**Errno spellings (§5.7)**~~ **DONE 2026-08-14** — one 'akuma_primitives::errno' table with the negated forms *generated* from the positive ones; 116 definition…  _(maybe resolved)_
- L1691: / 13 / **'#[inline]' audit (§5.10)** — deferred by request 2026-08-13. Both directions: missing attrs on cross-crate hot paths ('[profile.release]' sets no 'lto'), and 3…
- L1727: / 4 / 'MADV_FREE' → 'EINVAL' (unblocks redis) / 'src/syscall/mem.rs' / **DONE.** Verified in-VM: '-1 errno=22', 'MADV_DONTNEED'/'MADV_WILLNEED' untouched. The 'MADV_DONT…  _(maybe resolved)_
- L1741: **Item 4 — what is still open.** Allocators that probe 'MADV_FREE' (jemalloc,
- L1762: with deferred EOF. 'sys_write''s 'File' arm returns 'EAGAIN' on a 0-byte accept,
- L1769: which answers §6's open question — the drop-oldest overflow was **not**
- L1854: - **'MapStrategy::Deferred { path, inode }' carries the pager's inputs,** which
- L1855: makes 'Deferred' over an in-memory image *unrepresentable*, not merely unused.
- L1902: / Path + Deferred / '/bin/busybox' (1,116,408 B, over the 1 MiB 'HEAP_SLURP_MAX') /
- L2002: 'LazyRegionMap'; the path constructor pushed the image's deferred segments
- L2003: *first*. The merged body always does deferred-then-heap-then-stack, which is
- L2089: **Not done:** the redundant 'UnsafeCell' in 'block.rs'/'audio.rs' was listed in  _(maybe resolved)_
- L2146: six rungs) was already closed 2026-08-13 — Phase 4 has nothing left open.  _(maybe resolved)_
- L2191: &gt; row of its own "Still open" table below was already struck through — the last
- L2193: &gt; Nothing was left; the label was the only thing outstanding.
- L2251: **Still open, in the order they are worth doing:**
- L2256: / ~~'exceptions.rs''s duplicated 'Drop' impls (§8 item 5)~~ / **DONE — both halves.** Guard half 2026-08-13 (above); the ~330-line DA/IA demand-paging body merge landed …  _(maybe resolved)_
- L2303: ### Deferred audit: bare 'unwrap()' / 'expect()' in kernel code (raised 2026-08-14)
- L2337: ### Promoted out of "deferred" — then DONE 2026-08-14  _(maybe resolved)_
- L2357: / Safe sysreg readers / **HALF DONE, and the halves landed under different phases.** DAIF is centralised: 'akuma_primitives::irq' exports 'read_daif', 'DAIF_I_MASKED', '…  _(maybe resolved)_
- L2358: / 'Pte'/'PageTable' newtype (~−50) / **Still open, and this row was accurate.** 'PageTable' exists ('akuma-exec/src/mmu/types.rs:57') but as 'pub entries: [u64; ENTRIES_…  _(maybe resolved)_
- L2366: ### Deferred, inherited from BKL Phase 7: the **7g atomics audit**
- L2385: 7b run). Verified still open 2026-08-14: 'crates/akuma-exec/src/process/mod.rs'
- L2396: ### Deferred audit: GIC/timer/ramfb/irq → a leaf crate (raised 2026-08-14)
- L2481: / deferred-free AS fixture / 2 / 'new_as_with_one_mapped_page' /

</details>

<details><summary>docs/archive/TRIM_FAT_HAND_ROLLED_PARSERS.md (4)</summary>

- L122: - Config-file skeleton × 4 remains open (explicitly deferred by the original
- L304: / 'parse_response' / 'parse_headers' × 2 (both in 'scratch') / **Open — not touched.** / Same crate, same file shape, byte-identical helpers ('parse_status_line', 'find_…
- L305: / 'find_crlf' × 2 (both in 'scratch') / **Open — not touched.** / Same crate, same one-liner. /
- L403: there, this doc is the follow-up noting it didn't reach 'auth.rs'/'keys.rs'.

</details>

<details><summary>docs/archive/TRIM_FAT_HERD_PLUS_BOX.md (1)</summary>

- L4: except the deferred one (OCI types, explicitly gated on a second consumer that

</details>

<details><summary>docs/archive/TRIM_FAT_MMIO_NEWTYPE.md (2)</summary>

- L11: §5.2 ("deferred" item 7 in its §7 table), re-scoped here because the driver
- L220: — "Still deferred, genuinely" is where this item was parked, including the

</details>

<details><summary>docs/archive/TRIM_FAT_MULTIKERNEL.md (1)</summary>

- L121: ## Deferred: herd's core-pinning feature

</details>

<details><summary>docs/archive/TRIM_FAT_PART_3.md (1)</summary>

- L81: - 'docs/C_STUBS.md' — already noted quickjs as the sole remaining C-stub consumer after 'sqld''s removal; that note now needs its own follow-up since quickjs is gone too

</details>

<details><summary>docs/archive/TRIM_FAT_PROFILES_AND_ACCEPTANCE.md (2)</summary>

- L8: Most of what follows is **not fixed** (C and F are; the rest are not). Each item  _(maybe resolved)_
- L137: 2222 to 2323. Unresolved — see 'BUILTIN_SSH_REMOVAL.md'.

</details>

<details><summary>docs/archive/TRIM_FAT_PTE_NEWTYPE.md (2)</summary>

- L388: ### Not done  _(maybe resolved)_
- L400: deferred" for why that merge does not subsume this item.

</details>

<details><summary>docs/archive/TRIM_FAT_SSHD.md (3)</summary>

- L3: Follow-up investigation after 'docs/archive/SSHD_IMPLEMENTATION_PLAN.md' turned
- L31: error[E0432]: unresolved import 'libakuma' (config.rs, main.rs, protocol.rs, auth.rs)
- L222: behavior of userspace 'sshd' for a follow-up doc

</details>

<details><summary>docs/archive/TTBR0_AND_THREADING_FIXES.md (1)</summary>

- L188: /// Enable deferred thread cleanup (only thread 0 cleans up)

</details>

<details><summary>docs/archive/TTY_SHENANIGANS.md (2)</summary>

- L191: reader source not set'. That panic, plus the round-2 diff's own unresolved
- L241: ### Follow-up: 'FIONREAD' (fixed)  _(maybe resolved)_

</details>

<details><summary>docs/archive/TYPE_SAFETY_DEFECTS_2026_11_17.md (1)</summary>

- L12: 2. **ELF 'PT_LOAD' segments were not validated.** 'vaddr + memsz' was unchecked in 'segment_span', the deferred path's

</details>

<details><summary>docs/archive/UART_SMP_INTERLEAVE_FIX.md (3)</summary>

- L146: - **'docs/reference/subsystems/console.md'** — "Known gap" callout
- L240: follow-up; not blocking.
- L305: "Known gap" callout now reflects shipped/default-on.  _(maybe resolved)_

</details>

<details><summary>docs/archive/UDP_REPLY_WAIT_TIMER_STORM.md (1)</summary>

- L43: ## Still open

</details>

<details><summary>docs/archive/UNIX_PERMISSIONS.md (3)</summary>

- L58: / 'sys_kill' checks neither box nor uid (parent doc §7, **still open**) / 'proc.rs:1855' /
- L288: check, which is still open ('proc.rs:1855') — one predicate, 'can_signal', not
- L387: ## Open questions

</details>

<details><summary>docs/archive/UNIX_SOCKET_IMPROVEMENTS.md (2)</summary>

- L72: This is G7, and it is why Phase 2 got done rather than deferred.  _(maybe resolved)_
- L749: is unresolved by design — a '[BKL] stuck' line or a hang at 'SMP=4' means the

</details>

<details><summary>docs/archive/UNIX_SOCKET_READ_WRITE_BYPASSED_RECORDS.md (1)</summary>

- L87: ## Still open

</details>

<details><summary>docs/archive/UNLINKED_FILE_SPACE_RETURNED_LATE.md (4)</summary>

- L6: - 'crates/akuma-primitives/src/inode_pin.rs' and 'crates/akuma-ext2/src/ext2.rs': a deferred free now finishes once
- L32: only 'free_user_root' dropped it, at reap. 'munmap' never did, so ext2 saw the inode as mapped and deferred the
- L35: 2. **ext2 finished a deferred free only on the next unlink, rename or inode allocation.** Dropping the last pin is not
- L61: ext2 write hold ('write_state') compares that count with the value its mount last drained at, and drains the deferred

</details>

<details><summary>docs/archive/UNSAFE_CONTEXT_TABLE_DESIGN.md (3)</summary>

- L1: # Unsafe audit, next step: the thread context table, the IRQ-frame layout, and the exceptions page copies
- L193: crate (generic 'SlotTable&lt;C, N&gt;' owning states + contexts) is plausible but is **out of scope**: record it under "Not done" instead of starting it.  _(maybe resolved)_
- L251: site one 'unsafe fn' call and keeps the same reads elsewhere (net ~0). Not done.  _(maybe resolved)_

</details>

<details><summary>docs/archive/UNSAFE_FORBID_THREE_CRATES_PLAN.md (1)</summary>

- L94: (the ryzen is on Pop for the local-model trials; deferred by the owner to verify on Akuma later).

</details>

<details><summary>docs/archive/UNSAFE_KERNEL_CPU_STEP.md (3)</summary>

- L52: ## Not done  _(maybe resolved)_
- L76: Next step (design for glm): 'UNSAFE_CONTEXT_TABLE_DESIGN.md'.
- L78: Not done on purpose: the SVM run loop, per-CPU GDT, physmap slices, 'deliver_pending', and 'akuma-threading''s context table (~29)  _(maybe resolved)_

</details>

<details><summary>docs/archive/UNSAFE_OWNERSHIP_TYPES_DESIGN.md (1)</summary>

- L104: bootable under HVF locally). 'claim_fixed' is the open question; if (1) shows the fixed slots are never 'FREE', add a 'FixedSlot' constructor whose doc states the weaker…  _(maybe resolved)_

</details>

<details><summary>docs/archive/USERSPACE_NETWORKING_SUCCESS.md (1)</summary>

- L143: - Deferred cleanup via queue (freed by network runner)

</details>

<details><summary>docs/archive/USERSPACE_SOCKET_API.md (3)</summary>

- L161: Static allocation of 32 socket buffer pairs (4KB RX + 4KB TX each). Uses atomic flags for lock-free allocation. Deferred cleanup queue ensures buffers aren't freed while…
- L182: 4. **Deferred buffer cleanup**: Queue buffers for cleanup by network runner after polling
- L225: ## Future Work

</details>

<details><summary>docs/archive/USERSPACE_SSHD_FIX.md (1)</summary>

- L117: ## Next Steps for Phase 2

</details>

<details><summary>docs/archive/USER_COPY_BYTE_LOOP.md (1)</summary>

- L187: That reorders the remaining work: with the per-byte term now 9x smaller, the

</details>

<details><summary>docs/archive/USER_COPY_FOLD.md (8)</summary>

- L10: rather than a rewrite, the bug that fell out, and the hole that is **still open**
- L18: - **Not fixed by the fold, fixed later the same day:** a mapped *kernel* VA passed  _(maybe resolved)_
- L337: &gt; half of §11 item 3. What is **still open** there: the ~85 raw 'store(true)' /
- L372: &gt; 'read_current_pid' — **on an unfixed kernel it hangs rather than failing**, the
- L456: ### 10a. The AP-bit follow-up (§7), verified 2026-08-14
- L522: ## 11. Follow-ups this leaves behind
- L524: / / Item / Why it is not done here /  _(maybe resolved)_
- L528: / 3 / ~~Make 'BYPASS_VALIDATION' per-thread with an RAII guard~~ **MOSTLY DONE 2026-08-14** — it was a kernel-wide flag flipped by ~85 hand-paired 'store(true)'/'store(f…  _(maybe resolved)_

</details>

<details><summary>docs/archive/VEC_AUDIT.md (2)</summary>

- L221: ## Suggested next step
- L224: worth a follow-up:

</details>

<details><summary>docs/archive/VIRTIO_MMIO_LEGACY_TO_MODERN.md (1)</summary>

- L108: ## Follow-ups

</details>

<details><summary>docs/archive/WIFI_FUZZ_AND_LOSSY_SIM.md (2)</summary>

- L155: ## Not done, and what must run on the metal  _(maybe resolved)_
- L164: open question). Watch for '[rtw] eapol: pairwise rekey:' lines in

</details>

<details><summary>docs/archive/XBPS_MISSING_SYSCALLS.md (1)</summary>

- L250: Syscalls that XBPS or its dependencies may call but are not yet implemented. These haven't been hit yet but could surface during package install/remove operations:

</details>

<details><summary>docs/archive/ZERO_PAGE_ICE_FIX.md (7)</summary>

- L15: &gt; plus a deferred free in ext2.
- L191: ## The blocker that stayed unresolved
- L203: other theories wanted, and it is still open — worth fixing before any arm that needs to
- L303: deferred free, the page-cache invalidation it required, and the poison evidence that
- L317: - **Unresolved:** the last three arms went 14/14 green with no decode error
- L333: - **Still open:** 'open(O_TRUNC)' truncates a mapped file in place (matches
- L336: to explain anything. Also unfixed: 'file_page_cache::insert' increments the

</details>

<!-- END GENERATED -->
