# Akuma OS

Bare-metal Rust OS for AArch64 (QEMU virt). Networking, ext2 VFS, containers, JS engine, C compiler.
SSH is a userspace daemon (`userspace/sshd`); the kernel has no SSH server, no shell,
no editor and no cryptography (all removed 2026-08-10 — `docs/archive/BUILTIN_SSH_REMOVAL.md`).

## Layout

- `src/` — Kernel (no_std Rust)
- `crates/` — Host-testable extracted crates:
  `akuma-{exec,ext2,firecracker,isolation,kacho,mmap,net,net-nic,net-unix,net-yarn,pmm,primitives,rump,terminal,timer,vfs,virtio}`
  plus the `akuma-syscalls*` family below and `akuma-cpu`.
  `akuma-cpu` holds every AArch64 instruction that is **safe to execute** —
  barriers, cache/TLB maintenance, core parking, `DAIF`, the virtual-timer
  comparator and read-only system registers — behind safe `#[inline(always)]`
  functions. `asm!` is unconditionally `unsafe`, so a `dsb ish` used to need the
  same ceremony as an `msr ttbr0_el1`; the tree was migrated onto it 2026-08-31
  and **218 `asm!` sites outside it became 35** (`docs/archive/INLINE_ASM_CLEANUP.md`),
  then 34 (below).
  **Never open-code one of those instructions again.** What is deliberately
  absent: writes to `ttbr0_el1`/`elr_el1`/`vbar_el1`/`tpidr_el1`/`tpidrro_el0`,
  `mov sp,x`, `dc zva`, raw `ldr`/`str` and the GIC `ICC_*` writes stay `unsafe`
  at their call site. Reading `TTBR0_EL1` is in the crate; writing it is not, and
  that asymmetry is the design. The list read `tpidr*` until 2026-08-31; the
  wildcard was wrong. `TPIDRRO_EL0` holds the thread id that `current_tid`
  indexes every per-slot static with, and `TPIDR_EL1` is the kernel's own
  per-thread base — writes to either re-point state the kernel *dereferences*.
  `TPIDR_EL0` is userspace's TLS base, read in exactly one place in the tree (to
  save it into the trap frame) and never dereferenced, so a garbage value can
  only fault EL0's own accesses; it moved **into** the crate as
  `sysreg::set_tpidr_el0` (`docs/archive/SYSCALL_UNSAFE_CLEANUP.md` §6). To time a code path use
  `sysreg::cntvct_el0_ordered()` — a bare counter read is unordered against the
  work it measures and once made an 8 KB copy measure as 0 ns.
  **22 of the 32 carry `#![forbid(unsafe_code)]`** — which crates, and why the
  other ten cannot, is `docs/reference/crate-safety.md` (regenerate its numbers
  with `python3 scripts/cloc_akuma.py src crates`, never increment them by hand).
  **`src/syscall/` carries the ban too** (2026-08-31), as a module attribute in
  its `mod.rs` — the first one outside `crates/`, and the reason the crate tally
  and the ban tally differ. It went 17 `unsafe` blocks to 0: the genuinely-unsafe
  operations moved into `akuma-cpu`/`akuma-mmu`/`akuma-pmm`/`akuma-exec`, three
  of them gaining a real runtime check on the way. **Do not reach for
  `#[allow(unsafe_code)]` there** — `forbid` cannot be switched off locally,
  which is the point; put the operation behind a named function in the crate that
  owns what it pokes, and state the obligation there
  (`docs/archive/SYSCALL_UNSAFE_CLEANUP.md`).
  The **syscall family** is three layers, leaf-first and named so the
  layering is visible — an ABI crate, a shape crate, and one crate per syscall
  family (four of those so far): `akuma-syscalls-linux` is the ABI (numbers, `repr(C)`
  wire structs and their layout assertions, flag tables — zero dependencies);
  `akuma-syscalls` is the *shape* of an excursion (which counter bucket, which
  hooks run, where the epilogue's identity comes from) and depends only on the
  ABI crate; `akuma-syscalls-time` is one syscall *family* implementation —
  `clock_gettime`/`clock_settime`/`adjtimex`/itimers/`nanosleep` plus the
  boot-time SNTP client for platforms with no RTC (Firecracker) —
  `docs/reference/subsystems/syscalls/time.md`. It was named `akuma-time`
  until 2026-08-28; the rename is what stops it reading as a sibling of
  `akuma-timer`, the hardware CNTV/PL031 + tick-policy crate below, which it
  is not. `akuma-syscalls-sync` is the second family (2026-08-29): the futex
  op decode, the `(tgid, uaddr)` waiter table, the deadline algebra, the
  `WAKE_OP` opcode and the wait loop's outcome decision — chosen because every
  futex bug in `docs/archive/` is a property of one of those four things and
  each previously cost a `-j4` self-host build to find. The crate decides; the
  lock, the IRQ masking, the in-hold user read and every wake stay in
  `src/syscall/sync.rs`. Gates: `scripts/futex_suite.py` (correctness) and
  `userspace/futexprobe/` + `scripts/benchmarks/futex_ab_run.sh` (cost, run
  A/B/A — `docs/reference/subsystems/syscalls/sync.md`).
  `akuma-syscalls-poll` is the third family (2026-08-29): the fd-state ->
  event-bits **readiness map**, the interest list and `epoll_ctl`'s errno set,
  the `EPOLLET` armed-state decision, and the `ppoll`/`pselect6` wire
  marshalling — chosen because every epoll incident in `docs/archive/` except
  the lock inversion is one of those, and each previously cost a live VM and a
  network client (bun, tokio, nginx, cargo's libcurl) to find. The seam is
  inside what was one function: `epoll_check_fd_readiness` **probed** an fd and
  **mapped** the result in one `match`, and only the mapping was testable.
  Every probe, waker registration, `EPOLL_TABLE` hold and user copy stays in
  `src/syscall/poll.rs`. **The wait loop is NOT in it** — that is
  `akuma-net-yarn`, load-bearing for four syscalls; this extraction stops at
  the readiness edge. Seven known divergences from Linux are preserved and
  pinned rather than fixed. Gates: `scripts/epoll_suite.py` (correctness;
  `--linux` runs the same static binary on real Linux to prove the probe) and
  `userspace/epollprobe/` + `scripts/benchmarks/epoll_ab_run.sh` (cost, run
  A/B/A — `docs/reference/subsystems/syscalls/poll.md`).
  `akuma-syscalls-mem` is the fourth family (2026-08-29): `sys_mmap`'s
  **mapping-kind plan** (lazy vs eager, file-backed, shared-writable,
  `shared_anon`) and `MAP_FIXED` validation, `sys_mremap`'s move-vs-expand,
  `sys_madvise`'s advice decode and `MADV_DONTNEED`'s range + per-page rule,
  `munmap`'s sizing, and `membarrier`'s command decode. It depends on
  `{akuma-syscalls-linux, akuma-primitives}` and **deliberately NOT on
  `akuma-mmap`** — the mapping-kind decision is a function of the argument bits
  and never sees a region; if a change makes it want `MmapRegion`, the seam is
  drawn in the wrong place, so move the seam rather than add the dependency.
  Seven known divergences from Linux are preserved and pinned. Two of the moved
  functions failed their first host test with an **overflow reachable from
  unprivileged userspace** — `madvise(addr, -1, MADV_DONTNEED)` computed ~4.5e15
  pages and looped unbounded in an `MmBklGuard` window, and `MAP_FIXED`'s
  kernel-VA overlap guard wrapped to "no overlap"; both are fixed by validating
  `len` at the syscall boundary (`docs/archive/AKUMA_EXTRACT_MMAP.md` §10.1).
  Gates: `scripts/mem_suite.py` (correctness — runs the ten `c_stress` memory
  probes, refuses to score a *silent* probe as a pass, treats `DIVERGE` as green)
  and `userspace/memprobe/` + `scripts/benchmarks/mem_ab_run.sh` (cost, run
  A/B/A — `docs/reference/subsystems/syscalls/mem.md`). `mem_op_cost` takes a
  third argument `hostile`; `hostile=0` skips the two arms a pre-fix kernel
  cannot survive, which is what lets a baseline arm run at all.
  Further families move out on the same model when a family has real
  pure logic worth testing.
  `akuma-mmap` is virtual-memory **region bookkeeping**: `MmapRegion`,
  `PhysFrame`, CoW-fork inheritance, `munmap`'s clip-and-split, and the PTE
  permission vocabulary (`user_flags`, including `is_write`). Its
  `[dependencies]` table is **empty** (one of six such crates, with `-boot`,
  `-kacho`, `-net-yarn`, `-primitives`, `-rump`), and here that emptiness is the
  enforcement rather than a coincidence — it cannot lock, allocate a frame, edit
  a page table or name a `Process`, so `Process::vm_lock` and the
  `vm_with_regions` discipline stay in `akuma-exec` by construction. It sits
  BELOW `akuma-exec`, which re-exports everything it owns, so no call site
  changed when it moved. `is_write` is what tells an `mprotect(PROT_READ)` page
  from a CoW-demoted one — both are read-only in the PTE, and the EL0 write-fault
  handler used to break CoW on either, silently defeating `mprotect` across a
  fork. It also owns `mprotect_eager_regions_in_range`, which **splits** a region
  rather than recording a sub-range `mprotect` against the whole of it, and
  `MmapRegion::prot_recorded`, which separates "this region states a protection"
  from the unrecorded `NONE` default. Those three exist together for one reason:
  `flags` had only ever been read to GRANT a write, so every writer was allowed to
  under-state, and reading it to DENY one turned each of those into a false
  refusal that killed `rustc` mid-build. **Before reading any permission record to
  refuse something, enumerate every writer** —
  `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md`.
  `akuma-kacho` is the shared observe/decide/hysteresis layer every self-tuning
  policy uses (timer-tick demotion, file-page cache cap, netpoll wake rate).
  The **networking family** is four crates since 2026-08-30
  (`docs/archive/AKUMA_NET_SPLIT.md`), split so that all but one of them can
  forbid `unsafe`. `akuma-net` is the smoltcp stack (`smoltcp_net/`, 14 modules)
  plus the AF_INET socket table and DNS; it holds **no `unsafe`**.
  `akuma-net-nic` is the device — DMA frame arenas (`frames`), the virtio-net
  wrapper (`nic`), the `net-noalloc` rings, `nicstat`, the NIC MMIO doorbell
  (`irq`), the rump tap, and smoltcp's two `Device` impls. **Every `unsafe` line
  in networking is in it**, behind one stated DMA contract at the top of
  `nic.rs`: a buffer handed to `receive_begin`/`transmit_begin` is owned by the
  device until the matching completion. `frames` discharges that by owning the
  storage and handing out `FrameLease` guards, so `nic`'s safe entry points take
  an arena slot rather than a caller's buffer — if you find yourself adding a
  method that takes `&mut [u8]`, that is the seam being drawn in the wrong place.
  The `Device` impls live here *because* of those five `RxToken` sites; keeping
  the crate smoltcp-free would strand them in `akuma-net` and cost it its ban.
  `akuma-net-unix` is the AF_UNIX state machine — IPC over pipes, no NIC, no IP,
  no smoltcp — a separate crate because the rump-only devbox (`--no-default-features`,
  smoltcp compiled out) needs AF_UNIX for box 0's `rump_server` at fd 3 and
  should not pull the TCP/IP stack to get it. It is NOT re-exported from
  `akuma-net`: reaching AF_UNIX through the TCP/IP crate is the coupling the
  split removes.
  `akuma-net-yarn` is the readiness wait loop as a pure state machine, driven by
  **all four** blocking-wait paths: `akuma_net::socket::wait_until` and
  `src/syscall/poll.rs`'s `sys_epoll_pwait` / `sys_pselect6` / `sys_ppoll`.
  Callers supply only the effects, so the drain budget, fruitless-progress
  escape, epoch guard, timeout comparison, interrupt precedence and park kind
  are `WaitPolicy` fields with host tests instead of a devbox boot. **The two
  families differ in six of those fields and every difference is a real
  divergence** — don't "unify" one without measuring it (`docs/reference/
  subsystems/syscalls/poll.md` § "The wait loop is one machine"). The crate also
  carries a differential test against the pre-extraction `wait_until`; keep that
  oracle in the shipped loop's shape rather than tidying it.
  `akuma-locks-rw` is the recoverable reader/writer lock for orphaned-lock
  recovery (2026-08-30, `docs/archive/AKUMA_EXT2_CLEANUP.md` §4): release **is**
  abandon — the sweep for a dead holder performs the same CAS-guarded operation
  a legitimate release performs, so a `panic = "abort"` kill can never wedge a
  mount permanently. Carries no value and no global registry; host-tested by an
  exhaustive model checker on the `akuma-bkl` `bkl_model.rs` pattern.
  `akuma-locks-rw-cell` is the value half — the `UnsafeCell<T>` derefs stable
  Rust cannot express under `forbid`, **generic over `T`** so it never names a
  consumer's state type. That parametricity is the whole point: it is what let
  `akuma-ext2` adopt the lock and take `forbid` itself (§5 step 4, 2026-08-31)
  while `Ext2State` stayed `pub(crate)`. Do not reach for `dyn` here — it works,
  but costs a `Box` per mount, a downcast per acquire and `const fn new`, and
  buys no encapsulation the generic does not already give.
  **The reap is wired at one point only**: `akuma_exec::threading`'s
  `set_slot_reap_callback`, invoked at the TERMINATED→FREE transition, where the
  tid is genuinely dead and its slot cannot yet be reissued. Deliberately NOT
  `set_slot_purge_callback` (the futex one), which also fires from
  `mark_thread_terminated` while the thread may still be executing inside the
  critical section — dropping a futex queue entry that early is safe, releasing
  a lock is not. The sweep reaches the mounts through `src/vfs/ext2.rs`'s own
  registry rather than the VFS mount table, because `MountTable::resolve` hands
  out a borrowed `&dyn Filesystem` and its callers therefore hold `MOUNT_TABLE`
  across filesystem calls — a reaper taking that lock would invert against them.
  Cost of the swap, measured with `scripts/benchmarks/locks_rw_ab.sh`: +1.2 ns
  per uncontended write acquire, +0.8 ns per read.
  `akuma-scheduler` models scheduler placement / netpoll wake policies so a
  candidate can be ranked in a second instead of a devbox boot
  (`docs/archive/AKUMA_SCHEDULING_EXTRACTION.md`). The simulator core (`lib.rs`)
  is `no_std` and in `default-members` like its siblings; only its CLI report
  (`src/main.rs`, `--bin sched-sim`) stays plain `std` and is gated behind
  `required-features = ["cli"]` so a bare `cargo build`/`test` at the repo root
  never tries to cross-compile it for the kernel's own target.
  `akuma-boot` holds the Linux `reboot(2)` ABI decode for the `sc-reboot`
  syscall (`src/syscall/reboot.rs`) — in `default` since 2026-08-25 (only
  `extreme-size`, which builds `--no-default-features`, excludes it) — the
  PSCI SMC call itself stays in `src/smp_shared.rs`, which already owns
  SMC/HVC conduit selection for `CPU_ON`; `sc-reboot` depends on
  `smp-shared` for exactly that reason. Named `-boot`, not `-reboot`: a
  natural future home for `src/boot.rs`'s logic too.
- `userspace/` — ELF binaries (musl libc); current member list + one-liners: `docs/reference/userspace-layout.md`
- `docs/` — Documentation (see below)
- `scripts/` — Build and debug helpers
- `overlays/devbox/` — Devbox distro rootfs + `run.sh` / `run-smoltcp.sh`
- `bootstrap/` — Alpine apk bootstrap assets
- `acceptance/` — Numbered acceptance test playbooks: 05, 10, 11, 13 are the
  live set (extreme-size 4 MB floor, self-host, rump, `cargo install`-built
  agent); `acceptance/archive/` holds the rest, superseded or subsumed
  (`docs/archive/TRIM_FAT_PROFILES_AND_ACCEPTANCE.md`).
- `proposals/` — In-flight design proposals

Never glob or list the repo root — it has 1000+ files. Always use a specific subdirectory path.

## Documentation

`docs/README.md` is the front door: it has a symptom matrix ("I see X, what do I
read?"), a task list, and the subsystem index. Read it before searching docs by hand.

```
docs/runbooks/     Action-first procedures. "Do X, expect to see Y." Debugging + building.
docs/reference/    Current-state architecture and invariants. No history.
docs/userspace/    Per-binary docs (pointers to source-co-located docs).
docs/archive/      200+ historical investigation docs. Linked from new docs. Correct a
                   doc whose findings a later measurement disproves — a stale "FIXED"
                   is worse than an edited record; date the correction.
```

- Reference subsystem docs live in `docs/reference/subsystems/` (memory, scheduler,
  smp, smp-shared, networking, rump-stack, ssh, vfs, containers, exceptions,
  boot, irq, console, rng, async-fs, config-flags, drivers/),
  with 17 per-family syscall docs under `docs/reference/subsystems/syscalls/`.
- Linux ABI / musl notes: `docs/reference/abi/`.
- Build targets: `docs/reference/build-profiles.md`; every feature and env knob:
  `docs/reference/subsystems/config-flags.md`.
- Reference docs carry a **stability grade** — A (stable, trust it), B (verify
  behaviour), C (active risk, expect surprises). Check the grade before relying on a doc.

When writing docs: runbooks are named after the task/symptom and end with a
**Verify** section; new docs get a "Background" footer linking the `archive/`
originals; add a row to the relevant triage matrix (`docs/README.md`,
`docs/runbooks/README.md`).

## Build & Run

```bash
cargo build --release
cargo run --release                     # QEMU via scripts/cargo_runner.sh
MEMORY=2048 cargo run --release         # Override RAM
GDB=1 cargo run --release               # QEMU gdbstub on :1234
scripts/create_disk.sh                  # (Re)create ext2 disk image
scripts/populate_disk.sh                # Populate disk with userspace binaries
userspace/build.sh                      # Build all userspace binaries
userspace/build.sh --apk-tools-only     # Build apk bootstrap assets only
userspace/build.sh --meow-only          # Build a single member (--<name>-only)
cargo check                             # Fast diagnostics
```

### Other build targets

A build target is **profile + feature set**, always selected together. Details
and tradeoffs in `docs/reference/build-profiles.md`.

```bash
scripts/build_extreme_size.sh                             # extreme-size (4.0 MB floor, userspace sshd + paws, single-core)
scripts/build_devbox_smoltcp.sh && overlays/devbox/run-smoltcp.sh  # default devbox (userspace sshd)
scripts/build_devbox.sh && overlays/devbox/run.sh         # rump devbox (deferred; needs RUMP_NIC=1)
```

**`cargo build --release` is real SMP.** `smp-shared` — one shared kernel across
all cores under real locks — is in the default feature set; run it with `SMP=N`.
That is *the* SMP. The `smp` feature is the separate, experimental
**multikernel** (one whole kernel per core); the two are mutually exclusive
(build.rs panics), so building it needs `--no-default-features`.

Profiles are only `release`, `extreme-size` and `release-debug` — a build target
is `--release` plus a feature set.

## VM Access

SSH on port 2222: `ssh -o StrictHostKeyChecking=no root@localhost -p 2222`

The `ssh` CLI command is blocked by security policy. Use Python to run SSH commands:
```python
import subprocess
subprocess.run(["ssh", "-o", "StrictHostKeyChecking=no", "-p", "2222", "root@localhost", "<cmd>"])
```

### Waiting for a VM

**Poll the guest with an ssh round-trip. Never grep the boot log for a marker,
and NEVER** call `job_output` with `wait: true` on the QEMU process (it runs
forever).

```bash
scripts/vm_ready.py 2222            # exits 0 once the guest answers ssh
# or, in Python:  import vm_ready; vm_ready.wait_ready(port=2222, proc=qemu)
```

The old `grep -aqE "sshd started|Started sshd"` recipe is wrong in **both**
directions and cost real time in both:

- **False negative.** At SMP>1 the cores interleave console output, so the line
  arrives torn (`[herd] Starting service: sshd` / `sshd (pid= 2)`). Worse, some
  builds never print either spelling: measured 2026-08-28, a VM served ssh for
  570 s with `bind=1 listen=1 accept=371380` in `[PSTATS]` and **zero** marker
  matches, so a 10-minute wait timed out against a VM that was ready in seconds.
- **False positive.** The marker means sshd *started*, not that it can accept —
  and it never expires, so a stale log from the previous run reads as ready.

Connecting to the forwarded TCP port is not readiness either: QEMU opens the
`hostfwd` listener on the host the moment it starts, long before the guest is up.
Only a completed command counts.

`scripts/vm_ready.py` is the one implementation; harnesses import it rather than
re-deriving the check. Marker greps that remain in `scripts/` are fallbacks for
the "ssh never came up at all" diagnostic path only.

If you *are* reading a boot log for something else, `-a` is mandatory — QEMU
emits a control byte that makes plain `grep` treat the log as binary and print
nothing. And note there are two startup paths for sshd: `extreme-size` has the
kernel spawn it, every other profile lets herd do it.

**Never `pkill -f qemu-system-aarch64`.** Other VMs belonging to the user may be
running (they use `INSTANCE=N`, which maps ssh to `2222 + 100*N`). Kill only the
instance you started, matched on its own forward, e.g.
`hostfwd=tcp::2222-:22` for INSTANCE=0.

If the VM wedges (100% CPU, unresponsive), see `docs/runbooks/recover-wedged-vm.md`.

## Kernel conventions

**Analyze every new or changed path for allocations.** The best code allocates
nothing; every kernel allocation is a potential problem — it can fail, it can
fragment, it can recurse into the allocator on the path that is trying to report
the allocator broke, and it can turn a bounded operation into an unbounded one.
Before finishing a change, look at what it allocates and justify each one: prefer
fixed arrays, `static` per-slot state, stack buffers and borrows over `Vec`,
`String`, `Box` and `format!`. Fallible allocation beats infallible on any path
that can run under memory pressure.

**Console output must use `safe_print!` / `tprint!`.** No heap allocation on
any path that ends at the console — no `format!`, no `String`, no hand-rolled
stack writer, and no `-> String` helper feeding a print. The console is what
survives when the allocator is what broke. Exemptions (variable-cardinality
loops, no-runtime-registered paths) and the reasoning are in
`docs/reference/subsystems/console.md` § "Printing rules"; the violations that
motivated the rule are in `docs/archive/ALLOC_PRINT_AUDIT.md`.

## Testing

Host unit tests (crates only):
```bash
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
# akuma-ssh-crypto lives under userspace/ (its only consumer is userspace/sshd):
(cd userspace && cargo test -p akuma-ssh-crypto --target $(rustc -vV | grep '^host:' | cut -d' ' -f2))
```

Two userspace binaries split their pure logic into a library half so it can be
host-tested; both need `--no-default-features` to drop `libakuma`, whose
`#[panic_handler]`/`#[global_allocator]` collide with std's, and `--lib` so the
`no_main` binary is not built:
```bash
cd userspace && HOST=$(rustc -vV | grep '^host:' | cut -d' ' -f2)
cargo test -p sshd --lib --no-default-features --target $HOST   # wire.rs
cargo test -p box  --lib --no-default-features --target $HOST   # boxlib: json, oci, paths, spec
```

Acceptance playbooks live in `acceptance/` — run them end-to-end for
integration coverage. `scripts/` has targeted harnesses too
(`ssh_harness.py`, `forktest_smp_matrix.py`, `run_selfhost_kernelbuild.py`,
`test_sched_bklfree_ticket_fix.py`, …).

Pre-commit hook runs clippy + tests automatically.

## Working with Claude Code in this repo

Never use the `fork` subagent type (or any multi-agent fan-out) in this repo — do the
work directly instead. Forking copies the whole conversation context into a background
agent, which costs far more tokens than doing it inline.
