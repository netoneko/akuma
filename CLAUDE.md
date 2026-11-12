# Akuma OS

Bare-metal Rust OS for AArch64 (QEMU virt). Networking, ext2 VFS, containers, JS engine, C compiler.
SSH is a userspace daemon (`userspace/sshd`); the kernel has no SSH server, no shell,
no editor and no cryptography (all removed 2026-08-10 — `docs/archive/BUILTIN_SSH_REMOVAL.md`).

## Layout

- `src/` — Kernel (no_std Rust)
- `crates/` — Host-testable extracted crates:
  `akuma-{bkl,cow,dma,dmesg,dns,exec,ext2,fdtable,firecracker,fpcache,gdt,gic,ieee80211,isolation,kacho,machine-facts,mmap,net,net-nic,net-unix,net-yarn,pmm,port,primitives,procfs,psci,rtw89,rump,svm-amd64,terminal,timer,trap-aarch64,user-space,vfs,virtio}`
  plus the `akuma-syscalls*` family below and `akuma-cpu`.
  `akuma-middleware` is the optional syscall **middleware** (B22): one table per kernel of a `pre`
  guard, a `handler` for calls the kernel does not serve, and one exec policy; a kernel wires it behind
  its `middleware` feature, and `akuma-params` (the boot command line's narrowed `Params` views) sits
  beneath it so the kernels can capture the line without it. Guide: `crates/akuma-middleware/docs/middleware.md`.
  `akuma-kernel-tests-amd64` is the amd64 boot suite (2026-11-11), the counterpart of
  `akuma-kernel-tests-aarch64`: it reaches the kernel only through `akuma_kernel_amd64::testing`
  (one re-export per item) and installs itself with `boot::register_self_tests` from the bin's
  `before_main`; a `no-tests` image does not link it. `akuma-drivers-amd64` is the amd64 driver
  wiring, **one feature per driver** (`fbcon`, `hda`, `i8042`, `kvm`, `nvme`, `touchpad`, `wifi`, `xhci`; `all` is the bin's default,
  `headless` the lean test-box set — `docs/reference/build-profiles.md` § amd64 images): each
  driver registers a per-class table (`crates/akuma-kernel-amd64/src/drivers.rs`: display, sound, pointer, wireless,
  hypervisor, usb, nvme, keyboard) whose
  forwarders read "absent" when nothing registered, and reaches the kernel only through
  `drivers::services`. **Never add a driver `cfg` to the machine half**; add a class slot.
  It is `forbid(unsafe_code)`; the hypervisor is its own crate, `akuma-kvm-amd64` (the `kvm`
  feature pulls it in), split out when entering a guest still needed `unsafe`; it forbids it too now.
  `akuma-fdtable` is the fd table's state machine — lowest-free allocation,
  dup/close/sweep/fork decisions, effects returned not performed, `forbid`,
  no locks; `SharedFdTable` in `akuma-exec` keeps the locks and performs the
  effects. **Never compute a lowest-free fd, sweep cloexec, or copy a table
  for fork outside it** (`docs/reference/subsystems/fdtable.md`).
  `akuma-port` is x86 **port I/O**, under the same rule as `akuma-cpu` below with
  one difference that is the point: no function takes a port number. `in`/`out` are only safe to execute
  against the device that owns the port, so the crate exposes devices (`COM1`,
  `CMOS`, `PIC`, `PIT`, `I8042`, `PCI_CONFIG`, `RESET_CONTROL`, `DEBUGCON`), each
  reaching only its own range, with a host-tested registry that no two overlap.
  **Never open-code `in`/`out` or add a safe `outb(port, v)`** — the second is how
  safe code gets to power the box off. `akuma_cpu::ctlreg` reads `CR0/2/3/4`
  and owns the CR0/CR4 writes the kernels need as **named functions**
  (`enable_fpu`, `enable_xsave`, `enable_smep_smap`, `set_cr0_wp` — one
  semantic each, bits stated at each); no raw `write_crN` exists, for the same
  reason no function takes a port number, and `CR3` writes stay `unsafe` at the
  call site like `TTBR0_EL1`. `akuma_cpu::interrupts::{on,off}` are `sti`/`cli`,
  `park::halt` the `cli;hlt` end state, `rng::rdrand` the hardware word
  (CPUID gate with the caller).
  `scripts/check_unsafe_budget.py` is the gate on `unsafe` growth: a component's
  production `unsafe` sites may not exceed `docs/reference/unsafe-budget.json`, and
  a new crate's budget is 0. To add `unsafe`, run it with `--update` so the budget
  diff carries the increase into review.
  **Typed addresses** (`docs/reference/typed-addresses.md`): `akuma_primitives::addr::{PhysAddr, VirtAddr, Pfn}` are
  operator-minimal newtypes (`addr - addr` is a byte count, `addr + addr` does not compile) and `akuma-phys` already takes
  them; `scripts/check_addr_params.py` is the ratchet on untyped address parameters (`--update` carries an increase into
  review, `--list <crate>` prints the sites for a conversion sweep). A pure signature change removes no `unsafe` - the
  ownership types do: a value whose constructor makes the claim once and whose methods are safe (`Frame`, `FreshFrame`,
  `akuma-stack::StackRegion`, `akuma_trap_aarch64::irq_frame::IrqFrame`). **Never add `unsafe { &mut *phys_ptr(..) }` for a
  frame; never widen `akuma_mmap::PhysFrame`.**
  `akuma-dma` is **statically allocated DMA memory** (2026-10-18): `DmaArray<E, N, Align>` —
  the `static` itself, a length and alignment in its type, a borrow word that turns a
  second overlapping `&mut` into a panic, volatile `get`/`set` for cells a *device*
  writes, and a bounds-checked cache `flush` — plus `FrameArena`/`FrameLease`, moved
  from `akuma-net-nic`. It is why `crates/akuma-kernel-amd64/src/{xhci,hda}.rs` hold no `unsafe`. **Never
  write `from_raw_parts_mut((&raw mut BUF).cast(), N)` for a DMA buffer again.** Its
  counterpart for registers is `akuma_primitives::mmio::MmioWindow` (bounds- and
  alignment-checked, one `unsafe` per mapped BAR in `pci::map_bar_window`).
  `akuma-trap-amd64` + `akuma-idt-amd64` (2026-10-18) are the amd64 **trap entry**: the
  CPU's exception frame and the saved register file as types, the naked entry stubs
  that build them, and a `Trap { frame: &mut, regs: &mut }` handed to a safe
  `fn(&mut Trap)` registered per `Kind`; and the 256-gate table as atomics with `lidt`
  its only `unsafe`. They replaced `idt.rs`'s five hand-written asm entries and
  eighteen `&mut *frame` sites (38 -> 4 sites in `idt.rs`). **Add a trap = one
  `Kind` arm + one `register` line; never `&mut *` a stub's pointer in `crates/akuma-kernel-amd64/`.**
  The AArch64 counterpart is `akuma-trap-aarch64` (2026-10-19): a `FrameHandle` over the
  EL0 `UserTrapFrame` (volatile accessors, `snapshot`/`store`), made once at
  `rust_sync_el0_handler`; `akuma-exceptions` went 83 -> 38 sites. **Never write
  `(*frame).x0` in a handler.**
  `akuma-usermode-amd64` (2026-11-12) is the full amd64 kernel's **ring-3 crossing**: `syscall_entry`,
  `enter`/`enter_forked` (safe: the stubs refuse a non-user `rip` and a null per-CPU `UserCtx`),
  `unwind`, the STAR/LSTAR/FMASK install, and the `UserCtx` layout the assembly indexes (`kernel_rsp`
  is private to it), plus `PerCpuHead`, the `gs`-read head of the per-CPU block, and the safe
  `install_percpu` that points `GS_BASE` at one. Taoxichuan's counterpart is `akuma-ring3-amd64`; the
  two are to merge. `akuma_boot_amd64::mb2` makes the multiboot2 claims (info block, framebuffer
  window, RAM-disk module) from the address `boot.s` recorded. **`crates/akuma-kernel-amd64/` has no
  `unsafe` site and forbids it in the shipped (`no-tests`) image (2026-11-13; boot-suite probes carry
  a per-function `allow`); keep it at 0 — put the operation in the crate that owns the fact.**
  `akuma_cpu::msr` is the closed set of MSRs the kernel touches (`Msr` enum): reads
  safe, `write` unsafe (so the kernel's own `GS_BASE`, which every per-CPU read
  dereferences, stays `unsafe` at its one call site), and `set_fs_base`/`set_user_gs_base`
  safe behind a canonical check - `arch_prctl(ARCH_SET_FS, non-canonical)` used to
  `#GP` ring 0 - plus `set_pat_entry_wc` for the one PAT edit the framebuffer path
  needs. Never add another `unsafe fn wrmsr(u32, u64)`. `akuma_cpu::{tsc,debugreg,
  park::{wfi_mwait,irq_window,halt}}` cover `rdtsc`, `DR6`, the idle brackets and
  the `-> !` park.
  Both are **leaf crates on purpose**: the policy is to isolate `unsafe` in small
  crates whose only job is one stated contract, and shrink the drivers to safe code.
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
  **79 of the 108 carry `#![forbid(unsafe_code)]`** (2026-10-31) — which crates, and why the
  other 29 cannot, is `docs/reference/crate-safety.md` (regenerate its numbers
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
  `akuma-syscalls-net` is a fifth (2026-09-05), and the first pulled out *for
  the amd64 port* rather than for host tests alone: the read-only
  interface-introspection surface `busybox ifconfig` reads — the `struct ifreq`
  marshalling, the 40-byte `SIOCGIFCONF` record stride (the trap
  `docs/reference/subsystems/networking.md` records), the netmask/broadcast
  derivation, the `/proc/net/dev` column format. It was inside
  `akuma-syscalls-glue`, which does not build for `x86_64-unknown-none`; now
  both `akuma-syscalls-glue::net` and `crates/akuma-kernel-amd64/src/fd.rs` consume it and cannot
  drift. Depends only on `akuma-syscalls-linux`; the user-memory copies and the
  `FileDescriptor::Socket` gate stay with each caller.
  Further families move out on the same model when a family has real
  pure logic worth testing.
  `akuma-syscalls-abi` is the **vocabulary**: which syscall a number means, *on
  which architecture*. `akuma-syscalls-linux::nr` is asm-generic/AArch64 and its
  name says so; x86_64 has its own table, and `1` is `write` there and
  `io_setup`-adjacent under asm-generic — the wrong *handler*, not a missing one.
  Since 2026-09-07 the crate is **one `syscall_table!` row per call** (80 rows)
  generating the enum, both decodes, both encodes and `ALL` together, so a
  transposed digit can only be transposed once; the x86_64 number is a literal
  and the aarch64 one is a **path into `akuma-syscalls-linux`**, which is what
  stops the tables drifting. `crates/akuma-kernel-amd64/src/usermode.rs` decodes through it and a
  folded arm hands glue `to_aarch64()`; **glue's own `match syscall_num` is
  untouched**, so the AArch64 `.text` stays byte-identical and the cost —
  asm-generic numbers inside glue on an x86_64 kernel — is pinned rather than
  hidden (`docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md`). Two rules:
  **never put x86_64 numbers in `nr`** (`cfg!(target_arch)` resolves to the
  *host* under `cargo test`, so it would silently retarget every host test), and
  **never invent an asm-generic number for an x86-only legacy spelling** —
  `open`, `stat`, `poll`, `select`, `fork`, `mkdir`, `symlink`, `arch_prctl`,
  `time` and 12 more have none. The fixed rewrites (`open` -> `openat(AT_FDCWD, ..)` and
  18 more) are `akuma_syscalls_abi::legacy::fold`, applied by both x86_64 kernels before
  dispatch so a legacy spelling reaches its modern spelling's arm; the rest stay arms in `crates/akuma-kernel-amd64/`. The
  first pass over that list found x86_64 88 (`symlink`) dispatching to
  `sys_utimensat`, which returns 0 — so `ln -s` reported success and created
  nothing for months.
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
  `akuma-user-space` is the **frame ledger** half of a user address space: which
  physical frames it holds and how many VAs map each, plus the rule joining that
  to the PMM's global CoW count (`user_frames` counts VAs *within one space*;
  `COW_REFCOUNTS` counts *spaces*, to which a frame at five VAs contributes one).
  Split out of `akuma_mmu::UserAddressSpace` 2026-09-06 because that type is 45
  methods and only 13 of them — 71 of 457 lines — are architecture-neutral; the
  page-table walker stayed. It does **not** unblock `akuma-exec` for amd64 and
  was never going to, but it makes the walker's remaining content honest and it
  is what amd64 replaced its own capped, refcount-less `FrameSet` with
  (`docs/archive/AKUMA_USER_SPACE_LEDGER.md`).
  `akuma-mmu`'s map/demote surface is **safe since 2026-10-21**
  (`docs/archive/AKUMA_MMU_SAFE_FACADE.md`): free `map_user_page`/`map_user_page_no_flush`
  are safe `#[must_use]` functions whose "held `as_lock` (or a documented no-lock fault
  path), user `va`, live `pa`, consume the return" obligations are docs, not types, and the
  raw-pointer free `demote_range_to_ro` is gone — prefer the `&mut UserAddressSpace` forms,
  which discharge the obligations by construction. **Never re-introduce a `pub unsafe fn`
  over these, and never add a fourth no-lock call site without writing down why.** The only
  `unsafe` left in the walk path is the `table`/`table_phys` contract, the TTBR0/CR3 leaves
  and linker symbols.
  `akuma-cow` is the copy-on-write **write-fault decision** and nothing else:
  `(live PTE writable?, PTE CoW-marked?, share count)` -> `Retry` / `Fault` /
  `TakeInPlace` / `Copy`. Four situations that need different answers and are
  each silent when answered wrongly — a `SIGSEGV` on a write the page table
  grants, an `mprotect(PROT_READ)` that stops working, a needless copy on the
  commonest path, two processes sharing one page. **The order of the tests is
  the substance** (`pte_writable` outranks `marked` outranks `refs`) and has its
  own test, because a reordering still passes every single-condition one. The
  ~500-line AArch64 break in `akuma-exceptions` is deliberately NOT here: it is
  welded to `akuma-exec` and nearly all its bulk is multi-thread races. Note
  `marked` is a **PTE bit**, not `refs > 0` — amd64 uses x86 bit 9; AArch64 has
  no marker and passes `refs > 0`, a pinned divergence recorded in the crate
  (`docs/archive/AKUMA_AMD64_COW.md`).
  `akuma-tty` is the **terminal stack** (2026-10-19; was `akuma-terminal` + `akuma-pty`):
  the termios vocabulary and wire layouts (`termios`), **the** N_TTY engine
  (`ld::LineDiscipline` — buffers, editing, echo, `ISIG`, read decisions; termios is a
  parameter, never stored), `TerminalState` (`session`: a session's termios/winsize/pgrp on
  the engine, allocated lazily) and `PtyPair` (`pty`: refcounts/hangup/waiters on the engine);
  pure, `forbid(unsafe)`, zero dependencies, nothing that locks. **There is one line
  discipline — do not write a second editor/echo loop anywhere.** **The raw/canonical bit has
  one owner**: `ICANON` in `c_lflag`, written only by `TerminalState::{set_termios,
  apply_mode_request}` / `PtyPair::set_termios`, each returning a `Transition` —
  `grep 'Transition::Cooked'` lists every reaction to a raw terminal going back to line mode
  (`5a577983`). **Never write `ts.lflag` in a handler; never hand-spell the 36-byte `TCGETS`
  image — use `Termios::to_wire`.** `differential.rs` feeds a session and a pty identical
  keystrokes and asserts identical echo/reads. What is left to merge (sessions apply the
  discipline lazily on `read`, ptys eagerly on write): `docs/reference/subsystems/tty.md`.
  `akuma-procfs` is the `/proc/<pid>/{stat,status,cmdline}` byte formats both
  kernels render through — the wire layer, with no process model behind it. The
  AArch64 `ProcFilesystem` (`akuma-vfs-glue`) cannot be compiled for
  `x86_64-unknown-none`, so the formats moved rather than the filesystem. `ps`
  finds `utime` by counting to field 14 of a 44-field line, which is why this is
  a crate: extraction found the line had been emitting **41** fields where its
  own comment listed 44, and a `comm` truncation that could panic the kernel on
  a UTF-8 boundary (`docs/archive/AKUMA_AMD64_STREAMLINING.md` §4b).
  `akuma-svm-amd64` is the **SVM hypervisor's vocabulary and device models** (2026-10-22;
  `forbid(unsafe_code)`, no dependencies; the instructions are `akuma-svm-switch-amd64`'s): the VMCB
  layouts each pinned to its APM table, `cpuid::filter`, NPT encodings, PVH/ELF loading, and the
  pure models a Linux guest — and, since 2026-10-22, **Akuma's own kernel** (`kvmakuma`,
  `docs/archive/AKUMA_KVM_AKUMA_GUEST.md`) — runs on: 8254 `pit`, 8259 `pic`, xAPIC `lapic`
  (register page + timer, a function of `now`), 16550 `Uart` (with its THRE interrupt), the
  `mmio` MOV/CMP/TEST decoder (LLVM folds a volatile load into a comparison — its flag
  arithmetic is checked against real x86 `cmp`/`test`), a virtio-mmio v2 block device
  (`virtio`) and `acpi`. Interrupts reach the guest through `Platform::offer`/`accept`, never
  straight into `V_INTR`. It is
  the *device* side; `akuma-virtio` is the driver side and is reused as the guest disk's backing
  store. **Never set FADT `HW_REDUCED_ACPI`** (Linux then drops the PIC/PIT and the device IRQ
  vanishes) and **no Linux/KVM source may be read** — only the APM, the virtio/ACPI specs and
  datasheets (`docs/reference/subsystems/kvm.md`, `docs/runbooks/kvm-linux-guest.md`).
  `akuma-svm-switch-amd64` is the **world switch** (2026-11-11): the only crate that executes
  `VMRUN`/`VMSAVE`/`VMLOAD` or writes `EFER.SVME`/`VM_HSAVE_PA`. `enable` takes a `HostSaveArea`
  (made only by giving up a `FreshFrame`, consumed, so no page is two cores' save area) and
  checks CPUID and `VM_CR.SVMDIS` first; `HostState::save` is `VMSAVE` into an owned page. Since
  2026-11-12 entering a guest is **safe** (`Guest::run`): the crate owns guest RAM (`GuestRam`,
  from the PMM or the `kvmmem=` `reserve`), the nested page table (`Npt`, which maps only memory
  it adopted), the `Asid` and the all-intercept permission maps, and rewrites every VMCB
  confinement field from them on each entry; `akuma-kvm-amd64` is `forbid(unsafe_code)`
  (`docs/archive/AKUMA_SAFE_GUEST_ENTRY.md`). **Never open-code an SVM instruction in the wiring,
  never map a page into a guest that its `Npt` does not own, and never add a second way into
  `VMRUN`.**
  `akuma-vm-spec` is **what a VM is, before anything runs it** (2026-10-22; `forbid(unsafe_code)`,
  no dependencies): the `Spec` (kernel, cmdline, RAM, vCPUs, disk, forwards, backend), its parser and
  `key=value` wire text, Firecracker's field names as aliases, the first-fit `arena` over the
  `kvmmem=` reserve and the `kvmcores=` list. herd builds a spec from `vm_*` keys and the kernel
  parses the same text off `/dev/kvm0` (`crates/akuma-kvm-amd64/src/ctl.rs`), so the two cannot disagree.
  **Never parse a VM description anywhere else, and never put guest RAM anywhere but the reserve
  when `kvmmem=` is set — a request that does not fit is refused**
  (`docs/reference/subsystems/kvm.md` § VMs as services).
  `akuma-dmesg` is the console history ring behind `dmesg(1)` plus the
  `syslog(2)` action decode, shared by both kernels' consoles
  (`docs/archive/AKUMA_AMD64_STREAMLINING.md` §4).
  `akuma-kacho` is the shared observe/decide/hysteresis layer every self-tuning
  policy uses (timer-tick demotion, file-page cache cap, netpoll wake rate).
  `akuma-fpcache` is that file-page cache: shared physical frames for read-only
  file-backed mappings, keyed `(inode, mount id, file offset)` and reference-counted
  through the **CoW refcount**, so teardown needed no new code (2026-09-01,
  `docs/archive/AKUMA_FPCACHE_EXTRACTION.md`). It moved for the
  `#![forbid(unsafe_code)]`-across-`src/` goal rather than for host tests — it
  held no `unsafe` to localize; it was one of the eight `crate::` clusters
  `src/exceptions.rs` must shed before **it** can leave, and the only one needing
  no hook. The four `src/config.rs` tunables arrive at `init` as an
  `FpcacheConfig`, which is the one real cost: the kill switch stopped being a
  const-folded branch and became a relaxed atomic load (+304 B `.text`, +16 B
  `.bss`, 0 B of the old 4 MB `extreme` image). **`src/config.rs` stays the single source
  of truth** — the `src/file_page_cache.rs` shim reads the consts and hands them
  over, so do not add a second copy in the crate.
  The **networking family** is four crates since 2026-08-30
  (`docs/archive/AKUMA_NET_SPLIT.md`), split so that all but one of them can
  forbid `unsafe`. `akuma-net` is the smoltcp stack (`smoltcp_net/`, 14 modules)
  plus the AF_INET socket table and DNS; it holds **no `unsafe`**.
  `akuma-net-nic` is the device — DMA frame arenas (`frames`), the virtio-net
  wrapper (`nic`), `nicstat`, the NIC MMIO doorbell
  (`irq`), the rump tap, and smoltcp's two `Device` impls. **Every `unsafe` line
  in networking is in it**, behind one stated DMA contract at the top of
  `nic.rs`: a buffer handed to `receive_begin`/`transmit_begin` is owned by the
  device until the matching completion. `frames` discharges that by owning the
  storage and handing out `FrameLease` guards, so `nic`'s safe entry points take
  an arena slot rather than a caller's buffer — if you find yourself adding a
  method that takes `&mut [u8]`, that is the seam being drawn in the wrong place.
  The `Device` impls live here *because* of those five `RxToken` sites; keeping
  the crate smoltcp-free would strand them in `akuma-net` and cost it its ban.
  Since 2026-09-05 `LoopbackAwareDevice` wraps an **`ExternalDevice` enum**
  (`Virtio` / `Rtl8169` / `Absent`) rather than a hard-wired `VirtioSmoltcpDevice`
  — the seam for the amd64 bare-metal target, which has no virtio-net (its NIC
  is a Realtek on PCI) and still needs a socket layer (`Absent` = loopback
  only). It is an enum, not a generic or `dyn`: there is exactly one in the
  system, in a `static`, and a type parameter would ripple through `NETWORK`,
  `poll()` and every socket call. `smoltcp_net::init` still probes virtio and
  errors if there is none (the VMM contract); `init_loopback_only` /
  `init_with_external` are the new entries. The `rtl8169` feature adds the
  Realtek glue (`rtl8169.rs`) over the host-tested `akuma-net-rtl8169` — MMIO on
  a mapped BAR, descriptor rings in `.bss` with `OWN` written last behind a
  fence; off for every target but amd64 bare metal.
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
  `akuma-gic` is the **whole** GICv3 driver (2026-09-01,
  `docs/archive/AKUMA_GIC_CONSOLIDATION.md`) — distributor, per-PE
  redistributor, the `ICC_*_EL1` CPU interface, and secondary bring-up. It
  replaced three files that all drove the same controller: `src/gic.rs`,
  `src/gic_v3.rs` and the redistributor half of `src/smp_shared.rs`, which
  carried a **byte-identical** second copy of `mmio_w32`/`mmio_r32` and the
  `GICR_WAKER_*` bits. 12 `unsafe` blocks left `src/`; only 5 arrived — 4 were a
  dead GICv2 backend (`feature = "gic-v2"`, never enabled by any profile and
  unable to run under HVF at all) and 3 were the duplicates. It **cannot**
  forbid `unsafe` and never will: it is MMIO and system registers end to end,
  behind one stated contract — every address passed to `mmio_w32`/`mmio_r32` is a
  device-mapped GIC register, in the L0[1] device window or the low 1 GiB
  identity block. **Do not lower those to `write_volatile`**: the optimizer may
  emit a writeback form, which sets `ESR.ISV=0` and makes QEMU's HVF backend
  assert. The `ICC_*` writes deliberately did **not** go to `akuma-cpu` — that
  crate is instructions that are *safe to execute*, and enabling a CPU interface
  is not one.
  `akuma-bkl` is the BKL protocol and the spinlocks under it — and since
  2026-09-01 also `akuma_bkl::policy`, the **decision** to take it or skip it:
  the five `no-bkl-*` phase toggles (runtime atomics; the compile-time
  `no-bkl-*` features were folded into `smp-shared`; `no-bkl-irq` and `sched_bklfree_el0`
  went with the SGI scheduler, 2026-11-11) and the per-syscall opt-out bitmap, moved
  out of `src/smp_shared.rs` (which went 1173 -> 808 lines,
  `docs/archive/AKUMA_SMP_SHARED_SPLIT.md`). Each toggle is a relaxed atomic load
  with a **paired setter that is not test-only** — every phase in
  `BKL_FINE_GRAINED_LOCKING_PLAN.md` is validated by a same-binary A/B that flips
  it at runtime, so deleting a setter because production never calls it deletes
  the ability to measure the phase. Each is read ONCE at excursion entry and
  **latched**; a guard that re-reads can close a window it never opened.
  `process_bkl_drop_enabled` is the one toggle that did NOT move — its atomic is
  in `akuma_exec::process::bkl_guard` and `akuma-exec` depends on this crate, so
  moving it would invert that edge; `src/smp_shared.rs` forwards it. The opt-out
  bitmap's tests own syscall numbers on neither the seed nor the deny list,
  because it is a `static` and `cargo test` runs threads — **`300` looks free and
  is seeded**; the free set is pinned in a comment beside them.
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
  syscall (`src/syscall/reboot.rs`) — in `default` since 2026-08-25 — **and
  since 2026-09-01 `system_reset`/`system_off` themselves**, so it decodes *and*
  performs. It keeps `#![forbid(unsafe_code)]` because the `smc`/`hvc` lives in
  the sibling `akuma-psci`, not here — absorbing it would have cost the ban, the
  same reason `akuma-net` and `akuma-net-nic` are two crates. `sc-reboot` no
  longer depends on `smp-shared`; that was a build fix for `reboot.rs` naming
  `crate::smp_shared`, never a capability choice, and dropping it means `devbox`
  no longer picks up `smp-shared` transitively. Named `-boot`, not `-reboot`: a
  natural future home for `src/boot.rs`'s logic too.
  `akuma-psci` is the conduit — `smc_call`/`hvc_call`, the `Conduit` the DTB's
  `/psci` `method` selects (default `Hvc`; both supported machines use it), and
  the `CPU_ON`/`SYSTEM_OFF`/`SYSTEM_RESET` ids. It was `src/smp_shared.rs`'s
  **last** `unsafe` block, which is now at zero
  (`docs/archive/AKUMA_SMP_SHARED_SPLIT.md`). Splitting the old
  `psci_call(use_hvc, …)` in two took 1 `unsafe` block to 2 — expected, and the
  point is that each is one fixed instruction with no branch inside the asm.
  **Not in `akuma-cpu`, and this is the third time that answer came up**: `smc`
  with `SYSTEM_OFF` halts the machine and with `CPU_ON` starts a core at an
  address you supply, so a safe wrapper there would let any safe code in the tree
  power the box off. The call functions are `#[must_use]` — dropping a PSCI
  return is how a failed `CPU_ON` goes unnoticed.
- `userspace/` — ELF binaries (musl libc); current member list + one-liners: `docs/reference/userspace-layout.md`
- `docs/` — Documentation (see below)
- `scripts/` — Build and debug helpers
- `overlays/devbox/` — Devbox distro rootfs + `run.sh` / `run-smoltcp.sh`
- `bootstrap/` — Alpine apk bootstrap assets
- `acceptance/` — Numbered acceptance test playbooks: 10, 11, 13 are the
  live set (self-host, rump, `cargo install`-built agent); `acceptance/archive/`
  holds the rest, superseded or subsumed — 05 (the 4 MB floor) since the
  `extreme` feature was removed 2026-11-11
  (`docs/archive/TRIM_FAT_PROFILES_AND_ACCEPTANCE.md`).
- `proposals/` — In-flight design proposals

Never glob or list the repo root — it has 1000+ files. Always use a specific subdirectory path.

## Crate naming

- `akuma-<thing>` for architecture-neutral code, `akuma-<thing>-<arch>` for the
  architecture-specific half of a pair (`akuma-pte-amd64` beside `akuma-mmu`'s
  AArch64 encoding). Arch-neutral crates carry no suffix.
- **We strongly prefer `amd64` over `x86`/`x86_64`** in crate names, module names,
  types and prose. The literal `x86_64` appears only where it is the toolchain's or
  the ISA's own spelling: the `x86_64-unknown-none` triple, `target_arch = "x86_64"`,
  `core::arch::x86_64`, a CPU register/instruction name. Never `akuma-x86-*`.
- Name the crate for what it *owns*, not for how it is built (`-glue` is the one
  accepted exception: the part that cannot be host-tested).

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
  boot, irq, console, rng, async-fs, config-flags, wifi, drivers/),
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

**A bug fix gets an `archive/` doc; `reference/` gets the rule it established.** Write the
incident (symptom, cause, fix, how it was verified, what is still open) in a new
`docs/archive/<NAME>.md`, then put only the *current* behaviour and invariant in the reference
doc, with a link to the archive doc. Do not add `(fixed 2026-..)` sections, "until <date> this
spun at N syscalls/s" narratives, or "found on the ryzen" stories to `docs/reference/` — they go
stale and bury the invariant. A dated reference heading is fine only for a design change that
describes how things are now (`Crate layout (2026-08-30)`), never for an incident. Then update
`docs/archive/BUG_FIX_LIST.md` — `docs/runbooks/update-bug-fix-list.md`, and **sweep the whole
branch, not the last commits** (the runbook has the commands).

**Runbooks are procedures, not investigation logs.** A runbook that turns into a hunt log, a
handoff prompt, or "this was fixed, the page is history" belongs in `docs/archive/` under an
`UPPER_SNAKE` name (`THREAD_SPAWN_SEGV_INVESTIGATION.md`, not `debug-thread-spawn-segv.md`).
Move it with `git mv`, then fix every reference — `git grep -n <old-name>` also finds `docs/runbooks/<name>.md`
cited from Rust, C and shell comments — and drop its row from `docs/runbooks/README.md`; a symptom row in
`docs/README.md` may stay if it points at the archive doc. Keep a runbook only while its steps still
work on the current tree.

## Build & Run

```bash
cargo build --release
cargo run --release                     # QEMU via scripts/cargo_runner.sh
MEMORY=2048 cargo run --release         # Override RAM
GDB=1 cargo run --release               # QEMU gdbstub on :1234
INIT=/bin/sshd INITARGS=--port,22 cargo run --release  # init= other than herd (both kernels: boot.md § Init)
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
scripts/build_devbox_smoltcp.sh && overlays/devbox/run-smoltcp.sh  # default devbox (userspace sshd)
scripts/build_devbox.sh && overlays/devbox/run.sh         # rump devbox (deferred; needs RUMP_NIC=1)
```

**Every Akuma build is real SMP**: one shared kernel across all cores under
`akuma-bkl` (run it with `SMP=N`; `SMP=1` is the same image on one core). It is
not a feature any more: the `smp-shared` feature and the single-core build
without it were removed 2026-11-11. The shared crates still carry the switch as
`bkl-smp` (cfg `kernel_bkl_smp`) because Taoxichuan builds them without it and
serializes under its own lock — `proposals/ONE_BIG_KERNEL_LOCK.md`. The
multikernel (`smp`, one whole kernel per core) was removed 2026-08-10.

Profiles are only `release`, `extreme-size` and `release-debug` — a build target
is `--release` plus a feature set. `extreme-size` is **codegen only**
(`opt-level = "z"`, fat LTO): no cfg reads the profile, so it builds the same
kernel as `--release` for a given feature set. The 4 MB-floor `extreme` feature,
`userspace-sshd` and `fs-cache` (the ext2 block cache, now always on) were
removed 2026-11-11 (`docs/archive/AKUMA_FEATURE_AUDIT.md`).

### amd64 builds: default to `headless`

```bash
cargo build -p akuma --target x86_64-unknown-none --release \
    --no-default-features --features headless      # build/check, Firecracker/TCG smoke boots
```

**One kernel package for both architectures** (2026-11-12): the root `akuma` is the AArch64
kernel by default and the amd64 one with `--target x86_64-unknown-none` (artifact
`target/x86_64-unknown-none/release/akuma`). Its dependencies are per-target tables and its
`[features]` table serves both; `src/main.rs` is the root, `src/{aarch64,amd64}.rs` each half's
bin items. The old `amd64/bin` package `akuma-amd64` is gone; installed kernels on the boxes
keep their file names (`/boot/akuma-amd64`, the ESP's `EFI/akuma/akuma-amd64`). The amd64
machine half is the crate `crates/akuma-kernel-amd64` (it was `amd64/` until 2026-11-12);
`amd64/` keeps only its run, disk and staging scripts (`run.sh`, `mkdisk.sh`,
`run-firecracker.sh`, ...).

**Build `headless` unless the kernel actually has to run on ryzen or trashcan as
the host.** The plain `--release` default pulls in `akuma-fbcon` (`fbcon` is in
`all`), which needs the `crates/akuma-fbcon/vendor/spleen` git submodule — a
clone over the network the first time it's fetched. `headless`
(`storage-and-input`: NVMe, xHCI, i8042) never touches that crate, so a
compile check, a clippy pass or a Firecracker/TCG boot gate neither needs the
submodule nor pays for cloning it.

Use the plain default (every driver: `fbcon hda i8042 kvm nvme touchpad wifi
xhci`) only when the point of the build is to actually boot on real hardware —
ryzen (the machine with the SVM hypervisor and a real GRUB framebuffer) or
trashcan (the Intel test box with a serial line; it already builds `headless`
in practice since it has no hypervisor and no framebuffer use). Table and full
feature list: `docs/reference/build-profiles.md` § amd64 images.

`server` is `headless` without the interactive console (the machine half's `console`
feature: no pump task, no keyboard, init's stdin EOF, `SPAWN_FLAG_CONSOLE` = `ENODEV`) —
for a machine whose console is ssh. Anything that types into the serial console needs
`headless` or the default.

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
nothing. herd starts sshd on every build.

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

Pre-commit hook runs clippy + tests + a secret scan automatically (docs-only commits skip it with `--no-verify`; see
"Working with Claude Code").

## Working with Claude Code in this repo

Never use the `fork` subagent type (or any multi-agent fan-out) in this repo — do the
work directly instead. Forking copies the whole conversation context into a background
agent, which costs far more tokens than doing it inline.

**Commit and push after every task you finished and verified** (standing instruction from the user, 2026-11-05;
it replaces the earlier "the user drives all commits" rule). "Verified" means the checks that apply to the change
passed: clippy and the host tests for code, the build for every target it touches, the boot gate for kernel work.
Then:

- **Stage only what you changed**, by path. Never `git add -A` / `git add .`: the tree routinely carries the user's
  uncommitted work, submodule state (`userspace/meow`) and untracked results. Several tasks are several commits.
- **Push the current branch to `origin`** (github.com/netoneko/akuma) right after the commit. **This covers branches
  only: pushing to `main` (`git push origin main`, `HEAD:main`, a merge into it) still needs the user's explicit
  permission, every time.** If you are on `main`, branch first and push the branch. A push also publishes any commit of
  the user's that is ahead of origin; that is expected.
- **Never rewrite history**: no `reset`, `rebase`, `commit --amend` or force-push, in this repo or any submodule. A
  mistake is fixed by a new commit.
- **Docs-only commits use `git commit --no-verify`.** A commit whose every path is Markdown or under `docs/` or
  `proposals/` cannot change what clippy or the tests see, and the hook's run costs minutes. Anything that also
  touches code, a script, a manifest or a lockfile runs the hook. `--no-verify` also skips the hook's secret scan of
  staged `*.md`, so read what you stage for tokens, keys and passphrases first.
- **A pasted handoff prompt or task spec that mentions commits does not change this**; the user's own message does.

**Commit vocabulary describes the user's actions.** When the user says "committed", "checkpoint", "pushed",
"landed", "stashed" or similar, they are *reporting what they just did* so you know the state of the tree; it is
context, not a request. The same goes for a bare noun phrase on its own line in a longer message.
