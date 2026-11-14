# PHASE-2-PROCFS — research (R1, written before any code)

**Gate:** PHASE-2-PROCFS — G4 (`denver/docs/REQUIREMENTS.md`): `/proc/<pid>/auxv` (and
`/proc/<pid>/maps`) are not served, so a fault cannot be *named* once the JVM gets
further. G9's NULL-store death (`rip=0x000000003005fc41 cr2=0x0 code=0x6`, user-mode
write, observed first-hand in `/root/denver/merge-verify/gate-vdso.log`) dies unnamed:
a bare rip cannot be fixed. Diagnostics before the unknown (R8), the runbook rule's
third application.

**Evidence the gate flips:** jvmprobe's auxv arm prints `AUXV INFO /proc/self/auxv
unreadable errno=2 (No such file or directory)` (phase 1, both kernels); the attempt
doc's "`/proc/<pid>/maps` is empty on this kernel" (taoxichuan). On the *tokio* kernel
the truth is sharper than the attempt doc's: `maps` is not empty but **absent** — the
tree lists it, the serving is stubbed.

## Finding 0 — the /proc tree already exists in the tokio kernel; only the address-space files are stubbed

`akuma-tokio-kernel-amd64` mounts the shared procfs at boot
(`crates/akuma-tokio-kernel-amd64/src/lib.rs:990`,
`akuma_vfs_glue::mount_with("/proc", Some("proc"), 0, proc_fs)`). `stat`, `status`,
`cmdline`, `statm`'s sibling `mounts`, `fd/…` all serve today (libcap-ng/redis
precedent in the crate doc). Two files are missing, for two different reasons:

1. **`maps` — the walk is stubbed, not missing.** `PidFile::Maps` exists in the tree
   (`crates/akuma-procfs/src/tree.rs`, `exists` = `visible && has_address_space`),
   the format renderer exists (`akuma_procfs::render_maps_line`, host-tested), the
   glue renderer exists (`crates/akuma-vfs-glue/src/proc.rs:42 render_pid_maps`) —
   but the rows come from `VfsGlueHooks::pid_map_rows`
   (`crates/akuma-vfs-glue/src/lib.rs:1707`), and **`akuma-runtime-tokio::install`
   registers the stub** (`crates/akuma-runtime-tokio/src/lib.rs:71`,
   `pid_map_rows: |_| None`, comment: "Nothing renders `/proc/<pid>/maps` rows from
   here"). None ⇒ the file is *absent* (`has_address_space` =
   `pid_map_rows(pid).is_some()`, proc.rs:240) — the glue's own rule: "absent rather
   than empty — an empty `maps` is a confident 'this process has no mappings', which
   no process has".
2. **`auxv` — absent at every layer.** No `PidFile::Auxv` variant, no renderer, no
   hook, no retention: the loader builds the auxv table in
   `crates/akuma-tokio-kernel-core/src/loader.rs` (~L370: `AT_BASE…AT_RANDOM` 14
   pairs, +`AT_SYSINFO_EHDR` under `vsyscall` — D21/D25), hands it to `build_stack`
   (`crates/akuma-elf-exec/src/stack.rs`), and **drops it** — `TaskImage` carries
   only `{root_pa, entry, sp}` (loader.rs:27).

## Finding 1 — the metal kernel is the walk's precedent, and it is *exactly* reusable

`crates/akuma-kernel-amd64/src/fd.rs:1595 pub fn pid_map_rows(pid)` walks:
(a) the recorded `mmap_regions` of the address-space **owner** (a `CLONE_VM` thread
reaches the leader's tables — and the leader's lock), (b) `collect_leaf_runs` —
present user leaves *outside* the recorded extents, coalesced into runs by identical
`w/x` ("that is what the hardware says … for pages the loader placed"), with the
l0-generation re-check under the address-space lock (the freed-tables hazard:
`docs/archive/PROC_MAPS_WALK_FREED_TABLES.md`) and the `covered_by` binary search
(its own oracle-tested replacement note). The leaf iterator it uses —
`UserAddressSpace::for_each_user_leaf` — is **shared** (`crates/akuma-mmu/src/lib.rs`),
and `mmap_regions`, `with_process`, `address_space_owner_of` live on the shared
`akuma-exec::process::Process` (both kernels register into that table; the tokio
kernel's spawn comment: "a ring row, a registered `Process`").

So the walk's only metal-specific parts are its *location* (fd.rs) and nothing else.
The tokio kernel's JVM mappings are reachable the same way: the JVM's CodeCache/heap
reservations come through `mmap` (the tokio kernel serves it via
`mm::mmap_arm::<lazy::Amd64>`, machine lib.rs:1598, into the shared glue decisions in
`crates/akuma-syscalls-glue/src/mem.rs`, which records "the neutral record and the
page-table encoding" — regions land on `Process::mmap_regions`), and the loader's own
mappings (image text, stack) show up as leaf runs — which is what makes *self*-maps
of a static-PIE binary meaningful (regions alone would render it near-empty).

## Finding 2 — no /proc-auxv precedent exists anywhere in the tree (R9 absence)

`rg -il auxv` over `shanghai/`, `taoxichuan/`, `crates/akuma-kernel-amd64/`: the metal
only *names* auxv in a loader doc comment; taoxichuan's `docs/reference/memory.md`
mentions the word; **no kernel serves `/proc/<pid>/auxv`**. Same shape as VDSO's
Finding-0 absence: there is nothing to copy, only an ABI to serve. The byte format is
Linux's raw `Elf64_auxv_t` array — native-endian `(key, value)` u64 pairs, every pair
present including the trailing `AT_NULL` — and the vocabulary (`AT_*` consts) already
lives host-tested in `crates/akuma-ring3-amd64/src/lib.rs` (~L56–90) with a second
copy in `akuma-elf-exec/src/stack.rs`; the *renderer* belongs in `akuma-procfs` (the
crate's stated job: "the `/proc/<pid>` virtual-file byte formats `ps` and `top`
parse" — it must stay dependency-free, so it takes plain `(u64, u64)` pairs).

## Crates, files, functions found (named)

| piece | path | decides |
|---|---|---|
| tree membership | `crates/akuma-procfs/src/tree.rs` `PidFile` (`ALL`, `name`, `exists`) | whether `maps`/`auxv` resolve; `exists` is model-gated |
| wire format | `crates/akuma-procfs/src/lib.rs` `render_maps_line`, `MAPS_LINE_MAX` | the maps line; a sibling `render_auxv` is the R2 home for auxv bytes |
| glue renderer | `crates/akuma-vfs-glue/src/proc.rs:42` `render_pid_maps`, `:84 pid_address_space_file`, `:240 has_address_space` | rows→bytes; absence rule |
| hook table | `crates/akuma-vfs-glue/src/lib.rs:1674-1716` `VfsGlueHooks`, `MapRow = (usize,usize,bool,bool,bool,bool)` (:1714), `set_hooks` | the registration seam (pre-existing, not new middleware) |
| the walk | `crates/akuma-kernel-amd64/src/fd.rs:1595` `pid_map_rows`, `:1659 collect_leaf_runs`, `:1694 covered_by`, `:1712+` oracle test | what maps must contain; moves to the shared home (D27) |
| process facts | `crates/akuma-exec/src/process/mod.rs:727` `Process` (`address_space: ProcAddressSpace` :751, `mmap_regions` :810), `address_space.rs` (`lock`, `l0_phys`, `is_shared`), `fresh.rs:68 fresh()` | where auxv retention attaches (D28) |
| auxv construction | `crates/akuma-tokio-kernel-core/src/loader.rs` `load_task` (~L370 aux table, ~L393 vsyscall growth), `TaskImage` (:27) | the pairs exist here and are dropped today |
| exec paths | `akuma-tokio-kernel-core/src/proc.rs:209 bind_init`, `:577 exec`; machine `child.rs:104/193` | the two retention points (init + execve); fork inherits via `fresh`/`inherit_mmap_regions_for_cow_child` precedent |
| registration | `crates/akuma-runtime-tokio/src/lib.rs:62-76 install()` | where the stubs become real, behind the feature |
| probe vocabulary | `denver/probes/jvmprobe.c` (arms `auxv` with the errno=2 diagnostic, `java` with fork/exec/wait + pipe capture) | the guest-side arms this gate extends |

## Precedents (R9) — searched docs/archive, docs/reference, the sibling kernels, git history

1. **The walk** (`akuma-kernel-amd64/src/fd.rs` + `docs/archive/PROC_MAPS_WALK_FREED_TABLES.md`):
   owner-walk, extent-skip, leaf runs, l0 re-check. Moved verbatim, not re-derived —
   the oracle test moves with it (R2/R3: add the decision to the crate that owns the
   type; `MapRow`'s new home is `akuma-exec`, which `akuma-vfs-glue` already depends
   on, so its public alias re-exports and the hook signature does not change).
2. **Absence for auxv** (Finding 2): no sibling serves it; decided from the consumer
   need (G4's evidence line + HotSpot's report path), not copied. Renderer format
   checked against Linux `fs/proc/base.c` semantics as documented in the crate docs
   (termination, native endianness, `st_size`-lessness).
3. **Feature + rig + probe shapes**: D25 (feature relayed 1:1, off by default, gate
   kernel rides the proven policies), D15/D18 (second binary + spec; proven binaries
   byte-put), D26 (probe shape: stamps, console window, `-a`-tolerant greps, verdict
   arms in order), the FAULTNAME lesson (a probe constant that can lie is a broken
   probe — the rip constant is python-checked against the capture before the run).
4. **Auxv vocabulary**: `akuma-ring3-amd64` (host-tested `AT_*` consts + pin test) and
   the metal loader's fourteen-entry table (`STACK_WORDS_MAX` warning — auxv growth
   is budgeted, which is why retention is a copy, not a second build).
5. **`akuma-syscalls-glue/src/mem.rs`**: mmap records land on `Process::mmap_regions`
   for both kernels — the tokio JVM's mappings are already recorded; no new recording
   is added by this pass.

## Decisions

- **D27 — the maps walk moves into `akuma-exec`, both kernels use it.** New
  `crates/akuma-exec/src/process/maps_rows.rs`: `MapRow`, `collect_leaf_runs`,
  `covered_by`, and `pub fn pid_map_rows(pid)` (the fd.rs body verbatim, now using
  the crate's own `Process`). The metal's `fd::pid_map_rows` becomes a delegation
  (its registration line unchanged); its oracle test moves into `akuma-exec`'s host
  tests. Rationale: the walk reads only shared `Process` facts; keeping one
  implementation is the crate-header's own law ("this file supplies the process
  facts"); a second copy in `akuma-runtime-tokio` would be a fork (R2).
- **D28 — auxv is retained on `Process`, served through a hook, rendered in
  `akuma-procfs`.** (a) `akuma-procfs`: `PidFile::Auxv` ("auxv", exists = visible &&
  model has one — absent rather than empty, the maps rule) + `render_auxv(entries,
  buf) -> usize` writing native-endian pairs, appending the `AT_NULL` pair, + 
  `AUXV_MAX`; host tests (round-trip, truncation, termination). (b) `akuma-vfs-glue`:
  `VfsGlueHooks` gains `pid_auxv: fn(u32) -> Option<Vec<(u64, u64)>>` (default `|_| 
  None` — every existing `set_hooks` caller updated with the stub; default behavior
  byte-identical); proc.rs serves `auxv` mirroring `render_pid_maps`'s
  visibility-checked shape. (c) `akuma-exec` behind `procfs-diag`:
  `Process.auxv: Option<Vec<(u64, u64)>>`, set at `bind_init` + execve completion,
  copied at fork (Linux: a child's auxv is the parent's until exec); (d)
  `akuma-tokio-kernel-core`: `TaskImage` carries the pairs (fixed array, stays
  `Copy`), the loader fills it. Known limits, recorded honestly: threads serve the
  thread-group leader's table; a process that never exec'd through the loader (none
  exist here) has no table.
- **D29 — feature `procfs-diag`, off by default, relayed 1:1** down
  `denver → akuma-jvm-kernel → akuma-tokio-kernel → akuma-runtime-tokio`
  (`wx-relaxed`'s shape, R6/R7). It gates: the runtime's real hook registrations
  (maps walk + auxv), the auxv retention path, nothing else. The default build
  serves the same /proc tree it serves today.
- **D30 — the rig is `denver-procfs` + `vm-procfs.spec`** (`--features
  wx-relaxed,vsyscall,procfs-diag` — it rides the proven policies, D25;
  `vm-vdso.spec` with `kernel=` swapped; the proven binaries stay byte-put).
- **D31 — the probe is `denver/probe-procfs.sh`** (D26 shape), verdict arms in
  order: (1) `uname` says `denver` and **not** the WX hash `8a658723` nor the merged
  tree `3531cf69` (staging proof — the vdso binary does not serve procfs-diag);
  (2) tokioprobe `RESULT PASS`; (3) `RWX PASS rw_to_rx=42` (rides, free);
  (4) stack-auxv arm still `AUXV PASS AT_SYSINFO_EHDR=` (the disclosure survives the
  retention change); (5) **MAPS PASS**: `jvmprobe maps` — `/proc/self/maps` ≥1 line,
  every line matches `%lx-%lx [r-][w-][x-][ps] …`, and an executable line exists
  (the G4 flip: errno=2 must be gone); (6) **AUXV PASS**: `jvmprobe auxv` reads
  `/proc/self/auxv`, bytes ≡ 0 (mod 16), contains `AT_PAGESZ=4096`, and its
  `AT_SYSINFO_EHDR` **equals** `getauxval(33)` (stack vs procfs agreement,
  python-checked constant `18446744073699061760`); (7) **NAME arm** (the payoff):
  `jvmprobe name /opt/jdk/bin/java 3005fc41` — fork+exec `java -version`, poll
  `/proc/<pid>/maps` every 2 ms until death, keep the last snapshot, dump it, and
  attribute the expected rip: `NAME PASS rip=0x3005fc41 in <start>-<end> <perms> ` /
  `NAME SURPRISE` (captured, unattributed — the constant is the suspect, recorded
  for the re-rank) / `NAME MISSING` (no snapshot before death — the diagnostic
  cannot do its job ⇒ gate fails). (8) console window: the G9 fault may recur
  (expected rip, recorded); a *different* rip with a `NAME SURPRISE` is the
  re-rank's data. **`RESULT PASS` = 1-6 ∧ (NAME PASS ∨ NAME SURPRISE)**.

## Composition plan and new-LOC estimate (R5 budget)

1. this artifact (~160 LOC).
2. `crates/akuma-exec`: `process/maps_rows.rs` (moved walk ~70 LOC + host tests ~40),
   cfg'd `auxv` field + set/inherit sites (~30 LOC); Cargo feature line.
3. `crates/akuma-kernel-amd64/src/fd.rs`: delete moved fns, delegate (~-60/+6 LOC).
4. `crates/akuma-procfs`: `PidFile::Auxv` + `render_auxv` + `AUXV_MAX` + tests (~70 LOC).
5. `crates/akuma-vfs-glue`: hook field + `render_pid_auxv` + dispatch + re-export (~40 LOC);
   stub lines at the other `set_hooks` callers (~4 LOC).
6. `crates/akuma-tokio-kernel-core`: `TaskImage` aux pairs + loader fill (~25 LOC).
7. `crates/akuma-runtime-tokio`: cfg'd registrations + feature (~35 LOC).
8. Relay manifests: `akuma-tokio-kernel`, `akuma-jvm-kernel`, `denver` (~9 lines).
9. `denver/probes/jvmprobe.c`: `maps` arm, procfs-auxv arm, `name` arm (~130 LOC C).
10. `denver/probe-procfs.sh` (~150 LOC, D26 shape).

Total ≈ 450 LOC, of which kernel-side ≈ 190 behind the feature. R5 stop line: kernel
delta > 400 LOC means the shape went fork-shaped — re-research before writing more.

Verification order: `cargo test -p akuma-exec -p akuma-procfs` (host), metal still
builds (`cargo build -p akuma-kernel-amd64 --target x86_64-unknown-none`), default
kernels unchanged (`cargo build -p denver`, `-p taoxichuan`), clippy on touched
crates, then the gate kernel build + stage + run (D30/D31).
