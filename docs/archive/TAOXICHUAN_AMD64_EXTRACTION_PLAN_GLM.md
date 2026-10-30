# PLAN-glm-extract — shared-crate extraction plan: taoxichuan × amd64 (PLANNING ONLY)

Session glm-wt, 2026-10-30. Scope: plan the extraction of logic duplicated between
`taoxichuan/src/` (10,491 L: main.rs 5,453, child.rs 2,319, sock.rs 1,047, exec_twin.rs 675,
net.rs 443, pipes.rs 353, futex.rs 201) and `amd64/src/` (57,206 L, kernel proper
usermode.rs 9,189, fd.rs 3,819, mm.rs 3,522, sched.rs 3,093, signal.rs 1,578, uaccess.rs 618,
futex.rs 502, sock.rs 832, net.rs 1,384), per the repo policy (CLAUDE.md "Layout",
`docs/reference/crate-safety.md`, `taoxichuan/docs/reference/switch-to-crates.md`).
**No file but this one is written; no build, no QEMU, no git ops.**

## 0. Method and what was read

Every candidate was verified in code by `grep -n` (locations below are real, read this session).
Docs read in full or in the named sections: `docs/archive/TAOXICHUAN_COW_FORK.md` (all 33 L,
read first per brief), CLAUDE.md §Layout 7–130, §Crate naming 440–452, §Testing 605–631,
`docs/reference/crate-safety.md` 1–120 of 887, `taoxichuan/docs/reference/switch-to-crates.md`
(all 86 L), `taoxichuan/docs/gaps.md` (all 47 L), `taoxichuan/docs/runbooks/gates.md` (all 48 L),
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (head 45 L + tail ~120 L incl. the
three 2026-10-30 milestones), `scripts/check_unsafe_budget.py` (docstring), Cargo.toml
`default-members`, `taoxichuan/Cargo.toml` deps. Code verified by grep: all symbols named in the
brief, in both kernels and in `crates/akuma-{mmap,cow,user-space,mmu,net-unix,fdtable,pipes,pipe,syscalls-sync,syscalls-net,exec,exec-core}`.

## 1. Inventory table

Columns per brief. "forbid(unsafe)?" = could the piece live in a crate carrying
`#![forbid(unsafe_code)]` as the crate-safety policy requires.

| # | Piece | taoxichuan location | amd64 location | existing crate that already covers it | duplicated? / divergences | forbid(unsafe)-able? |
|---|---|---|---|---|---|---|
| 1 | mmap **hole allocator** (`add_hole`, `mmap_alloc`, first-fit reuse, MAX_HOLES=64 fixed array in `Task`) | `main.rs:837` (`MAX_HOLES`), `:990` (`holes` field), `:1153` `add_hole`, `:1217` `mmap_alloc` | **none** — amd64 has no hole table (grep `add_hole\|hole_table\|first_fit` in `amd64/src` = 0 hits; its mmap plans live in `akuma-syscalls-mem`) | `akuma-mmap` (2,329 L) owns mmap region *records/algebra* (`MmapRegion`, `inherit_mmap_regions_for_cow_child`, `detach_eager_regions_in_range`) but **not** the hole set | taoxichuan-only piece amd64 would want when it grows lazy/unmap-reuse mmap. Pure first-fit range algebra | **Yes** — ranges and capacities only; empty `[dependencies]` like `akuma-mmap` today |
| 2 | **signal-handler table** (per-task `struct sigaction` ×64 + mask, `oldact` readback) | `main.rs:965` (table field), `:1323` `sigaction_arm`; real per-task table+mask added 2026-10-30 (agent log, defect 2: musl `signal()` read stale `oldact`) | `signal.rs` (1,578 L): full delivery (`rt_sigframe` :227, FPU image :1021–1104, `deliver_pending`, `sys_rt_sigreturn`) — decode/table parts embedded | none — `akuma-syscalls-linux` has the wire structs only; agent log twice names "a small `akuma-syscalls-signal`" as the missing crate | **Duplicated logic, different depth**: tx stores/echoes, never delivers (gaps.md: "No pty, no signals"); amd64 stores *and* delivers. Divergence to pin: delivery stays amd64-side | **Yes** for table+sigset algebra (store/decode/oldact plan/mask ops); **No** for delivery (writes user frames, FPU) — that stays in `amd64/src/signal.rs` |
| 3 | **CoW share/break/leaf-walk**: `leaf_entry` :331, `cow_resolve` :352, `cow_prepare_write` :407, `cow_privatize` :420, `clone_user_half` :4656, `clone_user_tree` :4765, `in_shared_span` :4820, `free_subtree` :4831, `free_user_root` :5098 | `idt.rs:775` `cow_write_fault` (uses `akuma_cow::decide`); `usermode.rs:3388` `fork_share_memory` (share walk; partially crate-side already — `akuma-mmap/src/region.rs` docs cite `cow_share_range` twice) | **`akuma-cow`** (240 L, the break *decision*, both consume — verified in `tx main.rs:352` and `amd64 idt.rs:776`), **`akuma-user-space`** (606 L frame ledger), **`akuma-mmu`** (AArch64 walk; amd64 walk is `paging.rs`+`usermode.rs`), `akuma-mmap::inherit_*` | **Walk duplicated in shape, not bytes**: tx demotes RO + PTE **bit 9** (marker local to main.rs, per COW_FORK doc), walks raw 4-level tables by physical address; amd64 walks its own paging encoding. Divergences: (a) `MAP_SHARED` writable spans — tx **copies** them (`in_shared_span`), amd64's `inherit_mmap_regions_for_cow_child` shares regions but "owns none of the frames" (region.rs:386–394); (b) tx `free_user_root` frees lower-half leaves→tables→root for the twin; amd64 frees via `akuma-user-space` ledger. Behaviour pins needed for both | Partially — the traversal **plan** yes (op lists over an abstract leaf-table), the raw pointer walk no (that is the kernel's one `unsafe` justification) |
| 4 | **user-pointer copy + region check**: `user_ok` :1510, `copy_from_user` :1534, `copy_to_user` :1559 (CoW-aware since 2026-10-30: `cow_prepare_write` on kernel write) | `uaccess.rs` (618 L): `read_bytes` :286, `write_bytes` :303, `read_val`/`write_val` :319/:336, `read_cstr` :351, SMAP `clac_if_enabled` :240 | `crates/akuma-user-access` exists (not yet read this session — see Q5); `akuma_cow::Fault` contract cited at `uaccess.rs:391` | Same *shape* (check-then-copy), different mechanism: tx checks task's mapped halves through its own tables; amd64 SMAP+page-walk. The **decision** (canonical range, length overflow, iovec totals, cstr bound, EFAULT-vs-boom) is duplicated in spirit | Yes for the decision/check functions; the `ld`/`st` (or physmap write) stay kernel-side |
| 5 | **pipe table** | `pipes.rs` (353 L) is already a thin effects layer: `static mut PIPES: akuma_pipes::PipeTable<()>` :73, arms `pipe2_arm` :94, `socketpair_arm` :137, `write_no_sigpipe` :288 | uses `akuma-fdtable` + `akuma-pipes` family (fd.rs) | **`akuma-pipes`** (703 L, deps: `akuma-pipe` buffer leaf only), `akuma-fdtable` (817 L, empty deps) | **Extraction already done** (verified: tx deps + code). Remaining tx-only bits are effects (park, wake fire) — correctly kernel-side | n/a — done; nothing to extract |
| 6 | **futex** | `futex.rs` (201 L) — header literally says "the EFFECTS only … `amd64/src/futex.rs`'s shape" :1–9; consumes `akuma_syscalls_sync::{deadline,key,op,table}` :18–22 | `futex.rs` (502 L) — header says the decisions are the crate's | **`akuma-syscalls-sync`** (1,360 L) — done | **Extraction already done on both kernels.** Pinned divergence: tx's group-exit/EINTR purge differs on purpose ("Not copied from amd64", futex.rs:10–12) | n/a — done |
| 7 | **sockets** (`sockaddr_in` marshalling, `net_err`) | `sock.rs` :446 `sockaddr_in_from_user`, :465 `sockaddr_in_to_user`, :37 `net_err`; `net.rs` (443 L) device/pump side | `sock.rs` :108/:127/:144 — **same three function names**, same shapes; `net.rs` (1,384 L) NIC side | `akuma-net` (proto), `akuma-net-unix` (2,903 L: `UnixName` encode/decode, `plan_read`/`plan_write`, scm rights), `akuma-syscalls-net` (380 L — coverage to verify, Q4) | **Literal near-duplication** of the inet sockaddr/errno marshalling; state machines differ by design (tx: devices.md pump; amd64: real NIC). Select/poll arms differ (tx `select_arm` :243, `poll_arm` :85) | Yes for marshalling/errno (pure, over `akuma-syscalls-linux` wire structs); no for the stacks |
| 8 | **process/child table + wait4** | `child.rs` (2,319 L): SpawnSlot ring :40–265, M2 crossing :919–1454, `PARENT_OF` :643–664, private wait scans :1652–1855 (line refs from switch-to-crates.md, spot-verified) | `usermode.rs:6112` `sys_waitpid`, `:4493` `register_exec_process`, `:8804` `run_init`; `exec_runtime.rs` (642 L) | **`akuma-exec`** (9.3k L, forbid), `akuma-exec-core`, `akuma-slot-table`, `akuma-threading`, `akuma-fdtable` | **Already planned by the pm session** — `taoxichuan/docs/reference/switch-to-crates.md` + `PLAN-pm.md` (slices S1–S7, ≈1,600–1,900 L to delete). **This plan adopts that; it does not re-plan it.** Doc wins over brief: the brief lists this as a candidate, but the decision doc exists and forbids a second plan | n/a — covered by pm |
| 9 | **x87/SSE per-slot save** | per-slot 512-byte `fxsave` areas added 2026-10-30 (agent log defect 7: missing FPU save caused rustc ICEs); power-on `FCW 0x37F/MXCSR 0x1F80`, reset on slot reuse | `sched.rs`/`smp.rs:961–966`: `xsave64` vs `fxsave64` selection, 832-byte area (512+64+256 ymm) | none | Duplicated *policy* (area size, alignment 64, init image, reset-on-reuse) with a hardware divergence: tx fixed `fxsave` 512 B, amd64 `xsave` up to 832 B. **Pin, don't unify**: tx's 512 B is a deliberate subset | Yes for the policy/algebra (sizes, init image bytes, selection rule); the two `fxsave`/`xsave` asm pairs stay in the kernels (cf. `akuma-port`'s one-crate-owns-the-instruction rule) |
| 10 | **stat / openat / getdents marshalling** | `main.rs` :38 (openat absolute-only, AT_FDCWD only), `fill_statfs` :2112; getdents cursor per open description :865–1749 | `usermode.rs:1778` getdents64, `:1461` legacy folds via `akuma_syscalls_abi::legacy`, path resolution `akuma_syscalls_glue::fs::resolve_path_at` :1214 | **`akuma-syscalls-abi`** (1,412 L wire structs+layout asserts), **`akuma-syscalls-glue`** (19.6k L) — both already tx deps (`taoxichuan/Cargo.toml:84–91`) | Divergence to pin: tx is absolute-path-only + stage-gated; amd64 resolves `dirfd`-relative paths. Adoption of glue is pm S6's job (two-table dispatcher) — **do not duplicate here** | Yes for pure wire marshalling — but that already exists; it's adoption, not extraction |
| 11 | **syscall dispatcher** | one match on `ring3::nr::*` in `main.rs` (~:3687 region) | two-table shape: legacy fold `usermode.rs:1461` + disjointness assert :7506–7530 into `akuma-syscalls-glue` | `akuma-syscalls-abi` (+`legacy`), `akuma-syscalls-glue` | pm S6 plans exactly this migration. **Adopt pm's slice; not re-planned here** | n/a — covered by pm |

## 2. Proposed crates

House rules applied (CLAUDE.md §Crate naming 440–452; the naming is `akuma-<thing>`, `-amd64`
suffix only for arch-specific halves, never `x86`; each new crate **forbids `unsafe` from its
first commit**, starts at budget 0 in `docs/reference/unsafe-budget.json` per
`scripts/check_unsafe_budget.py`; effects returned, not performed; empty dependency tables where
the crate is a leaf, deliberate minimal deps otherwise).

### 2.1 `akuma-mmap::holes` (extension — preferred over a new crate)
- **Owns**: the first-fit hole-set algebra: insert (`add_hole`'s merge/sort/coalesce), take-first-fit
  (`mmap_alloc` minus the frame mapping), fixed-capacity refusal (ENOMEM-shaped `Option`), the
  `MAX_HOLES` sizing rule as a const generic so `Task`'s `[(u64,u64); 64]` keeps its layout.
- **Contract**: `HoleSet<const N: usize>` — `insert(range)`, `take_first_fit(len) -> Option<addr>`,
  `remove(range)`; invariants: holes disjoint+sorted, coalesced on insert, capacity refusal.
- **Dependencies**: none (akuma-mmap is empty-deps by design; keep it that way — its docs say
  "no dependencies at all, which is what makes those impossible").
- **Stays in the kernel**: frame alloc/free, page-table edits, the `Task` field, `munmap`'s
  free-frames effect (tx main.rs), the `[mmdbg]` event ring.
- **Host tests FIRST** (exhaustive/model, per brief): oracle test vs `BTreeMap<u64,u64>` over a
  seeded PRNG op sequence (insert/take/remove fuzz, both must agree on every `take_first_fit`);
  deterministic tests for coalesce-both-sides, capacity exhaustion, zero-length refusal, exact-fit
  take. Port the current behaviours byte-for-byte: same address chosen for the same hole layout.
- **Unsafe budget**: −N sites in `taoxichuan` (logic leaves main.rs); +0 anywhere. New tests only
  in the crate; `forbid` from first commit.

### 2.2 `akuma-syscalls-signal` (new crate — the agent log names it twice as "should become crates")
- **Owns**: the per-task sigaction **table** (store `struct sigaction` ×64, `SIG_DFL/SIG_IGN`
  decode, `oldact` readback *plan*), the sigset/mask algebra (and/or/not, `sigprocmask`'s
  how-decode, `sigismember`), `rt_sigaction`'s errno set (EINVAL for sig 0/out-of-range,
  `sigsetsize` check).
- **Dependencies**: `akuma-syscalls-linux` only (wire structs + flag bits), mirroring
  `akuma-syscalls-sync`'s deliberately-thin dep table (its Cargo.toml comment: "Deliberately NOT
  akuma-exec").
- **Stays in the kernels**: delivery (`amd64/src/signal.rs`: rt_sigframe build, FPU image,
  sigreturn); tx keeps its accepted-no-op delivery posture (gaps.md pinned exclusion — doc wins).
- **Host tests FIRST**: table round-trip (install→readback equality, the exact musl `signal()`
  regression from the agent log); mask algebra property tests (set ops commute, `how` invalid →
  EINVAL, unchanged mask on error); every errno branch enumerated. Exhaustive over sig 0..=65.
- **Consumers**: tx `sigaction_arm` (main.rs:1323) becomes decode+crate+writeback; amd64
  `signal.rs` replaces its embedded table/mask decode arms.
- **Unsafe budget**: −(tx sites in sigaction_arm) −(amd64 decode-arm sites); +0.

### 2.3 `akuma-user-tree` (new crate — the CoW fork share/free *plans*)
- **Owns**: the traversal **decisions** over an abstract 4-level user tree: `SharePlan` (which
  leaves to ref-increment + demote, in parent-then-child order, with the per-leaf `invlpg` note),
  `FreePlan` (leaves→tables→root free order, CoW refcount drop on shared leaves), the
  failed-clone partial-free rule (`free_subtree`), and the shared-span policy as a **named,
 pinned enum** (`SharedSpans::Copy` [tx today] | `SharedSpans::ShareFrames` [amd64 today]) —
  the divergence becomes an explicit choice, not an accident.
- **Dependencies**: `akuma-cow` (the break decision), `akuma-mmap` (region inheritance), 
  `akuma-user-space` (ledger plan types). **Never `akuma-exec`** (layering risk §4.1).
- **Stays in the kernels**: the raw table walkers (`leaf_entry`, the `unsafe` pointer walk), PTE
  encodings (tx bit 9 marker stays local per COW_FORK doc; amd64 paging.rs), `invlpg`/shootdown,
  frame alloc/free.
- **Host tests FIRST** (state machine ⇒ exhaustive/model per brief): a `Vec`-backed fake tree in
  the crate tests; properties: clone is zero-sum (refcounts: parent+1/child+1, no frame created);
  demote makes exactly the writable leaves RO+marker in both trees; failed clone leaves refcounts
  as before (free_subtree exactness); free plan visits leaves before tables before root and drops
  each shared ref exactly once (the host-side twin of `cow_leak_gate.py`'s delta=0); shared-span
  policy: each enum value pinned by a test that asserts the other value is *not* what happens.
- **Unsafe budget**: −(tx walk sites); amd64 unchanged this slice; +0.

### 2.4 `akuma-syscalls-net` extension: sockaddr/errno marshalling (verify-then-extend — Q4)
- **Owns**: `sockaddr_in`/`sockaddr_un` from/to-user **plans** (bounds, `addrlen` short-read
  rules, return-value convention "bytes written or 0"), the `net_err` errno mapping table (the
  three identical functions in both `sock.rs` files), `setsockopt`/`getsockopt` opt decode tables.
- **Dependencies**: `akuma-syscalls-linux` (wire structs); check existing 380 L first so we
  extend, not duplicate.
- **Host tests FIRST**: differential test — decode the *same* wire bytes both ways (old tx fn vs
  crate fn) in a test fixture ported from tx's code; errno table exhaustive vs Linux man values.
- **Unsafe budget**: −~6 sites total; +0. (Pins the divergence where tx/amd64 disagree on
  `addrlen` edge cases today — found none this session, but the differential test is the pin.)

### 2.5 `akuma-fpu-amd64` (new crate, small, amd64-suffixed — both consumers are x86_64 kernels)
- **Owns**: the save-area policy: `fxsave` 512 B vs `xsave` 832+ B area-size selection rule, 64-B
  alignment, power-on image bytes (`FCW 0x37F`, `MXCSR 0x1F80`), reset-on-slot-reuse rule, and the
  "which regions does the area cover" arithmetic.
- **Dependencies**: none. Pure byte algebra — host-testable, so it may live in `default-members`
  despite the `-amd64` name (it compiles no asm; the cfg-gate rule in §4.2 is about bins/asm).
- **Host tests FIRST**: size/alignment table tests; init-image byte equality (both kernels must
  produce the same 512-B legacy image today — the pin for the fxsave/xsave divergence).
- **Stays in the kernels**: the two `fxsave`/`xsave`/`fxrstor`/`xrstor` asm pairs and the
  per-slot statics.
- **Unsafe budget**: +0 (asm stays where it is, already budgeted).

### 2.6 Explicitly *not* proposed (and why)
- `futex`, pipes: already extracted; adopting crates is done on both kernels (inventory #5, #6).
- child-table/wait4/dispatcher/stat-glue: owned by the pm session (`switch-to-crates.md` S1–S7);
  re-planning would contradict the standing decision doc (brief rule: the doc wins; say so).
- uaccess copy loops: check `akuma-user-access` coverage first (Q5); if the check/decision
  functions are already there, this is a tx-side adoption ticket, not a new crate.

## 3. Ordered slices (each = one glm session, ≈100 tool calls; host-side cargo only per brief)

Gates named up front. **taoxichuan gate**: `scripts/kvm/goose/cow_leak_gate.py --cycles 20`
(delta=0, `cowrefs=0`) + the battery in `taoxichuan/docs/runbooks/gates.md` (F1, PK1, fs,
`apk add git`, `git --version`; runner `scripts/kvm/goose/run_gates.sh`) + the one-boot cargo
trio on `rs.img` for memory-touching slices. **amd64 gate** (the one I found — there is no
single amd64 gates.md): `scripts/box_amd64_suite.py` (live ssh regression suite, the amd64
behaviour gate) + `scripts/amd64_selfhost_build.py` (in-guest kernel-build gate) — run by the
supervisor, since glm sessions are host-cargo-only. Every slice also gates on
`cargo test -p <touched-crate> --target $(rustc -c 'print host')`-shaped host runs (the brief's
exact command) and `scripts/check_unsafe_budget.py` (new crates at 0; kernels only ratchet down).

| slice | content | size | gate |
|---|---|---|---|
| S0 | **Baseline + write tests first.** Freeze current battery outputs on both kernels (supervisor runs them once, outputs recorded in the slice doc); `cargo test` host baseline; `check_unsafe_budget.py` numbers recorded. Author the `akuma-mmap::holes` model tests against a trait stub (red) — tests FIRST per brief. | ~40 calls | host tests fail-then-pass; no kernel change |
| S1 | **`akuma-mmap::holes` + tx adoption.** Implement `HoleSet`, pass model tests; rewire tx `add_hole`/`mmap_alloc` (main.rs:1153/:1217) to call it, `Task.holes` becomes the crate's type; same-choice address pin tests. amd64: no code change, crate lands in default-members (it is host-testable, no asm). | ~60 calls | tx cow_leak_gate + battery byte-identical; amd64 suite unchanged; `-p akuma-mmap` green |
| S2 | **`akuma-syscalls-signal` + both adoptions.** Table/mask/errno crate; tx `sigaction_arm` consumes; amd64 `signal.rs` swaps its embedded decode/table arms (delivery untouched). Differential pin: musl `signal()` readback (tx) and amd64's handler-install path byte-identical. | ~80 calls | tx battery + vendored `-j4` build (the signal regression's own probe); amd64 box suite + selfhost build; `-p akuma-syscalls-signal` green |
| S3 | **`akuma-user-tree` + tx adoption.** SharePlan/FreePlan/shared-span pin enum; tx `clone_user_tree`/`free_user_root`/`free_subtree`/`in_shared_span` become plan-consumers; fake-tree model tests first. amd64 does **not** switch its walk this slice — its `SharedSpans::ShareFrames` pin is a test-only proof. | ~90 calls | tx cow_leak_gate `--cycles 20` delta=0 + one-boot cargo trio + battery; amd64 suite unchanged |
| S4 | **sockaddr/errno into `akuma-syscalls-net` + both adoptions.** Verify existing coverage, extend, differential-test old-vs-new on both kernels. | ~60 calls | tx: `curl -sS https://example.com` gate + battery; amd64: box suite |
| S5 | **`akuma-fpu-amd64`.** Policy crate + init-image pin tests; both kernels' save sites consume the size/init constants. Optional — drop if quota is short. | ~40 calls | both batteries; rustc `-j4` probe on tx |
| S6 | **uaccess decision adoption (post-Q5)** + sweep: docs (`crate-safety.md` regenerated with `cloc_akuma.py`, never hand-incremented; `unsafe-budget.json --update` ratchet *down* only; tx `reference/memory.md`+`gaps.md` staleness fixes listed in §5), crate docs cross-links. | ~50 calls | host tests + budget gate green; no behaviour change (kernels compile-only this slice) |

Coordination with pm: S1–S5 touch only files pm doesn't (`taoxichuan/src/main.rs` hole/sigaction/
walk regions vs pm's `child.rs`/`exec_twin.rs`), so the sessions can interleave; S6's dispatcher
adoption is pm's S6 — skipped here.

**Behaviour pinning rule (brief §3):** wherever the kernels disagree today, the crate carries the
divergence as a named enum/const with a host test pinning *each* value (shared-span policy,
fxsave-vs-xsave image, tx's futex group-exit purge stays kernel-side and untouched). No slice
silently picks one behaviour.

## 4. Risks

1. **Layering**: `akuma-user-tree` must never depend on `akuma-exec` (or anything that does:
   `akuma-kernel-glue`, `akuma-vfs-glue`) — same rule `akuma-syscalls-sync` states in its
   Cargo.toml ("must not know what a thread is") and `akuma-mmap` ("sits BELOW akuma-exec").
   Enforcement: empty-or-leaf dep tables, and `cargo tree -p akuma-user-tree -i akuma-exec` as a
   slice-gate line.
2. **default-members / cfg-gating**: x86_64-only *bins* stay out of `default-members`
   (taoxichuan/Cargo.toml:7 sets the precedent). The crates proposed here are host-testable and
   compile no asm, so they belong in `default-members` — *except* any future crate that grows
   `asm!`/arch headers: then it gets the `akuma-boot-amd64` treatment (cfg-gated, out of
   default-members). Watch the `akuma-el0-entry` lesson (crate-safety.md): `#[cfg(target_os =
   "none")]` matches `x86_64-unknown-none` too and selected an AArch64 arm — that cfg shape is
   "no longer a safe spelling anywhere in the tree"; grep for it before adding cfg-gates.
3. **Divergence pinning**: the shared-span and FPU divergences are load-bearing (tx copies
   MAP_SHARED spans deliberately; amd64 shares frames). Unifying naively breaks one kernel or
   leaks (COW_FORK "Still open": the BTreeMap refcount cost). Mitigation: pin-enum + per-value
   host tests (§2.3) and keeping tx's bit-9 marker local (its doc says so).
4. **Unsafe budget drift**: new crates start at 0 and take the ban from the first commit;
   a kernel that loses sites must be re-ratcheted with `--update` (decreases never fail). Risk is
   a slice leaving a half-migrated caller: gate each slice on `check_unsafe_budget.py` green.
5. **Quota/tool-call risk** (the reason this file exists in skeleton form since call ~4): hard
   cap 40 calls, stop reading at 30 — enforced; each slice above is sized under 100 calls.
6. **Doc contradiction (brief rule: doc wins — flagged)**: `taoxichuan/docs/gaps.md` still pins
   "No page-fault handler → no CoW, no lazy mmap" and "no signals"; both are stale after
   TAOXICHUAN_COW_FORK.md (2026-10-30) and the per-task sigaction table. S6 fixes the rows; until
   then, read gaps.md's pinned-exclusions as historical.
7. **amd64 has no gates.md**: its gate is a *pair* (box_amd64_suite + selfhost build), both ssh-
   based and slow under TCG; slice gates must not assume a fast amd64 loop.

## 5. Docs read; questions for the user

**Docs read** (this session, locations in §0): TAOXICHUAN_COW_FORK.md; CLAUDE.md §Layout/§Crate
naming/§Testing; docs/reference/crate-safety.md (opening sections); taoxichuan docs:
switch-to-crates.md, gaps.md, runbooks/gates.md, + the reference listing (boot, devices, memory,
pipes, processes, rust-syscalls, rust-toolchain, scheduling, syscalls — not all read, budget);
MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md head+tail (incl. the three 2026-10-30 milestones
that name the hole allocator and sigaction table as the next extractions); scripts/
check_unsafe_budget.py, box_amd64_suite.py, amd64_selfhost_build.py; workspace Cargo.toml
default-members; taoxichuan/Cargo.toml deps; crates/akuma-{mmap,cow,user-space,pipes,fdtable,
syscalls-sync,syscalls-net,exec} headers/APIs; both kernels' `futex.rs`, `sock.rs`, uaccess,
CoW and mmap symbol sites.

**Questions**:
- Q1 (holes): amd64 has no lazy mmap today — is `akuma-mmap::holes` wanted as tx-only for now
  (amd64 consumes "when it grows unmap-reuse"), or should amd64 adopt it in S1 defensively?
- Q2 (user-tree): plan-lists allocate per leaf; for a 100k-page fork that is heap churn (cf.
  COW_FORK "Still open" BTreeMap note). Acceptable to make the plan a `&mut FnMut` visitor
  callback (still effects-returned) instead of a `Vec`, or must plans stay materialised?
- Q3 (signal): scope = table+mask+errno only (this plan), or also the pending/queue set amd64
  keeps in `signal.rs`? The latter drags delivery-adjacent policy into the crate.
- Q4 (net): `akuma-syscalls-net` is 380 L — extend it vs new `akuma-net-inet`? (Default: extend;
  the family naming already exists.)
- Q5 (uaccess): `crates/akuma-user-access` was not read this session (budget). If its checks
  already cover tx's `user_ok` semantics, S6 is adoption-only — confirm, or I read it first next
  session (~6 calls).
- Q6 (ordering): pm (switch-to-crates) and this plan both touch `taoxichuan/src/main.rs` (pm S1
  init-registration, glm S1–S3). Interleave pm-first, glm-first, or strictly alternate slices?

---
**Status (2026-10-30, supervisor):** slice 1 (§2.1) DONE - `akuma_mmap::HoleSet<N>` (`crates/akuma-mmap/src/holes.rs`, 10 host tests incl. a page-bitmap model check),
taoxichuan's `add_hole`/`carve_hole`/`mmap_alloc` are thin wrappers over it; `Task::holes`/`n_holes` became one `HoleSet<64>`. Battery, leak gate unchanged.
Chose the extension of `akuma-mmap` (empty deps kept) as the plan preferred. Behaviour pinned verbatim: swap-remove order, first-fit in table order, drop-when-full.
**Slice 2 (§2.2) DONE:** `akuma-syscalls-signal` (`SigTable`, wire image, `check`, `reset_for_exec`, `procmask`; zero dependencies - errors are a closed enum the
caller maps to errno, so it needs no ABI crate; forbid(unsafe), budget 0; 10 host tests incl. an exhaustive differential of `check` against the old inline rule).
taoxichuan's `sigaction_arm`/`sigprocmask_arm`/exec reset use it. amd64's `signal.rs` has NOT adopted it yet (no amd64 gate was run).

**Slice 4 (§2.4) DONE (2026-10-30):** `akuma_syscalls_net::sockaddr` (`decode_sockaddr_in`, `encode_sockaddr_in`, `net_err`, `SockaddrError`). 7 host tests including
a differential against `akuma-net`'s `SockAddrIn` (the struct both kernels used to reinterpret with `unsafe`): the encode is byte-identical for 45 address/port
pairs and the decode agrees for families 0..=40. Both `sock.rs` files adopted it (taoxichuan's was already unsafe-free by byte code; amd64 lost its `read_unaligned` and
`from_raw_parts`: `amd64/src` 85 -> 83), and the three `net_err` copies are gone. `sockaddr_un` and `setsockopt` decode tables were not moved.
**Slice 5 (§2.5) DONE:** `akuma-fpu-amd64` (sizes, offsets, `initial_legacy`/`initial_xsave`, `effective_mxcsr_mask`, `sanitize`, `SaveFormat`). 10 host tests; the power-on image is
pinned to the exact bytes both kernels hand-wrote, and the xsave image is pinned to "legacy image + XSTATE_BV". taoxichuan's `fx_init` and amd64's `FxArea::initial`/`sanitize` consume it.
Compile-checked for amd64 (`cargo check -p akuma-amd64 --release --features no-tests`); not run on the amd64 kernel here.
**Slice 3 (§2.3) DONE (2026-10-30):** `akuma-user-tree` (`PteLayout::{plan_clone, plan_free, demote, relink}`, `ClonePlan`, `FreeStep`, `SharedSpans::{Copy, ShareFrames}`; empty `[dependencies]`, `cargo tree -i akuma-exec` finds nothing).
Q2 answered in the code: plans are returned by value, one per entry, never a `Vec`. 10 host tests run the plans through a fake tree with per-frame reference counts, including: clone is zero-sum
(no new leaf frame, each shared frame +1), demote touches exactly the writable leaves, freeing the clone and then the parent returns every reference and table (the host twin of `cow_leak_gate.py`'s
`delta=0`), and an out-of-frames failure injected at **every** allocation point leaves every reference as it was. The `SharedSpans` divergence is pinned both ways. taoxichuan's
`clone_user_tree`, `clone_user_half_into` and `free_subtree` perform the plans (counters unchanged: `shared=15520 copied=320 taken=320`, leak delta 0); amd64 does not switch its walk (as planned).
Side effect: the old PML4-level "huge page" refusal returned without releasing the subtrees already cloned; the shared failure path (`release_cloned_prefix`) now does.
All three extraction slices of this plan that were still open (3, 4, 5) are done; the remaining `Q`s are the uaccess adoption (S6) and amd64 adopting `akuma-syscalls-signal`.
