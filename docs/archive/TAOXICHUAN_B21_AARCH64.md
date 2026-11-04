# B21: Taoxichuan on aarch64 - progress (from 2026-11-04)

The record of [`proposals/B21_TOKIO_KERNEL_AARCH64.md`](../../proposals/B21_TOKIO_KERNEL_AARCH64.md): one `main.rs`
for both architectures, and a smaller audit surface. One row per step, newest last; each row says what moved,
what it cost, and which gates it passed.

## Gates on the Mac (TCG)

- **Battery**: `tcg_rig.py battery` on the goose image (`img/goose.img`, key `keys/k`): quick, F1, PK1, fs,
  append, dup2-sock, pipeline, apk, git, console faults.
- **Probes** at `smp=1` and `smp=4` on the probe image (`spawn.img`, key `verify/key`): `tokioprobe`,
  `killtree 8`, `pidfdprobe`, `sh -c "sleep 1 & sleep 2 & wait"`.
- The HEAD kernel is run beside the candidate whenever a probe fails, because TCG at `smp=4` has a flake of its
  own (below).

## Baseline (`e80e6cd9` + the rig fix `1077b20f`)

- Battery 10/10, once the rig served its own Alpine mirror (below).
- Probes: `smp=1` PASS 3/3. `smp=4` PASS 2/3; the third run lost its ssh session during `killtree` (`rc=255`)
  and printed 4 fault lines. That is a pre-existing TCG `smp=4` flake, not investigated here; a candidate is
  compared against the same count of HEAD runs.
- Audit (`cloc_akuma.py --audit taoxichuan`): amd64 image **78,823** lines; aarch64 track does not build.

### Found on the way: the battery's Alpine mirror (`1077b20f`)

The battery's `apk` and `git` lines failed on an unchanged kernel. The images' apk repository is
`http://10.0.2.2:8130/main`, and nothing in the tree served it: an `http.server` started by hand in an earlier
session was still listening on 8130 after its directory under `/private/tmp` had been emptied, so every fetch
was a 404. `scripts/kvm/taoxichuan/apk_mirror.py` is a caching proxy to Alpine v3.24 (the `git 2.54.0` the
battery expects) that never refreshes its index, and `tcg_rig.py battery` starts it when nothing listens on the
port. The `apk` line's expected text moved from the old mirror's snapshot (`9 errors; 15 MiB in 19 packages`)
to v3.24's (`OK: 20 MiB in 28 packages`).

## A0: census of the machine half

`crates/akuma-tokio-kernel-amd64` at `e80e6cd9`, 7,013 lines (`wc -l`). Each item was read and sorted into
**arch** (names the x86 machine: an instruction, a register, an x86 structure, an x86 encoding) or **neutral**
(policy over an address space, a process table, a file system; it reaches the machine only through calls that
an aarch64 half can answer differently).

| file | lines | arch | neutral | the arch part |
|---|---|---|---|---|
| `lib.rs` | 3,286 | ~1,350 | ~1,880 | PVH start info, GDT/TSS/trap stacks, IDT and stubs, the fault and timer traps' x86 decode (CR2, error code), LAPIC, CR0/CR4, the x86 4-level walks (`leaf_entry`, clone/free subtrees, `free_user_root`), the PTE bits the CoW and W^X code reads (`RW`, `D`, bit 9), the syscall frame (`dispatch`), `halt`, panic/OOM, `boot`'s order |
| `child.rs` | 1,322 | ~270 | ~1,050 | the register capture for fork, the x86 signal frame (`push_handler`, `sigreturn_arm`, `FxArea`), the exec entry frame and FS base, the x86 clone argument order |
| `exec_twin.rs` | 501 | ~50 | ~450 | `enter_user_on` (CR3 + `sysretq`), `hlt` in the idle lap |
| `sched.rs` | 156 | 156 | 0 | `akuma-sched-amd64`'s `Platform`: CR3, TSS `rsp0`, FS base, FP state |
| `smp.rs` | 560 | ~410 | ~150 | INIT-SIPI trampoline, LAPIC IPIs, TLB shootdown vector; the BKL is neutral |
| `net.rs` | 402 | ~180 | ~220 | TSC calibration against the PIT, RDRAND, `sti; hlt` wait lap; virtio-net + DHCP + netpoll are neutral |
| `lazy.rs` | 434 | (format) | 434 | the lazy record is `akuma_pte_amd64::lazy`, an x86 not-present PTE; the fill/reserve/move logic is neutral given an entry format |
| `shmpages.rs` | 245 | 0 | 245 | |
| `console.rs` | 69 | 69 | 0 | COM1 |
| `ring.rs` | 38 | 0 | 38 | |
| **total** | **7,013** | **~2,500** | **~4,500** | |

So about two thirds of the machine half is policy that an aarch64 half must not copy. The census also changes
two steps of the proposal:

1. **The scheduler (A4) does not need B12.** The tokio kernel never touches AArch64's SGI scheduler: it runs on
   `akuma-threading`'s *hooked* path (`x86_claim_slot`, `x86_seed_entry`, `x86_publish`, `x86_yield`,
   `x86_finish_current`, `X86ArchHooks`). That path is cooperative, picks and parks through the same
   `THREAD_STATES`/`ON_CPU`/`WAKE_TIMES` as everything else, and reaches the machine only through the hooks
   struct, plus three pieces of code: the switch (`akuma_threading_x86_switch_context`, push callee-saved +
   flags, swap `sp`), the first-run frame (`x86_build_closure_context`), and `Context`'s layout (one `rsp` word
   on x86; AArch64's `Context` already has `sp`). It is behind `cfg(target_arch = "x86_64")` because amd64 was
   its only user, not because the logic is x86. A4 becomes: the hooked path builds on aarch64 behind a feature,
   with an aarch64 switch (`x19`-`x30`, `DAIF`, `sp`) and first-run frame, and the full AArch64 kernel keeps its
   SGI scheduler untouched. B12 stays its own proposal.
2. **`akuma-exec` is already arch-neutral where the kernel meets it.** It launches user code only through
   `ExecRuntime::enter_user` and builds children through `spawn_user_thread_initializing` /
   `update_thread_context`, which on a hooked-path build route to the hooks. Its `UserContext` has an aarch64
   layout already (`x0`-`x30`, `sp`, `pc`, `spsr`, `tpidr`).

What A2 has to give the core, from the third column: a page-table trait (map, unmap, translate, read/write a
leaf, the writable/dirty/CoW/user bits as named queries, a lazy-record codec, TLB invalidate, the kernel half of
a new root, clone and free of a user half), the syscall frame as named argument and return accessors, a signal
frame builder and restorer, the user-register capture, `enter_user`, the clock and RNG, the console, and boot.

## Steps

| step | commit | what | gates |
|---|---|---|---|
| A0 | `17b13896` | Census above; the proposal's A4 revised | - |
| A1 | `17b13896` | **`akuma-runtime-tokio` is arch-neutral.** `serve(call: Syscall, args)` and `serve_shared(nr, args)` replace `dispatch(nr, args)`; the x86 front (decode by x86_64 number, then `akuma-syscalls-glue-amd64` for the fd family) is `tokio_surface` in the amd64 half, and the crate no longer depends on the x86 ABI layer. Same routing order as before. A host test pins that every allow-listed call round-trips through its asm-generic number too | host tests 7/7; battery 10/10; probes `smp=1` 3/3, `smp=4` 2/3 (the HEAD flake, also 2/3 on HEAD) |
| cloc | `17b13896` | **`--audit` reports per-arch tracks.** `taoxichuan` is now the track pair `taoxichuan-amd64` + `taoxichuan-aarch64`; a track that does not build is reported as "no track yet" instead of failing the run, and when both build, a table gives each track's arch-only lines, the shared lines and the union, per origin | `--self-test` PASS; `track_split` checked against a hand-built case; amd64 still 78,823 |
| A2.1 | `309c3199` | **`akuma-tokio-kernel-core` exists.** The first pieces that never touched the machine moved in from the amd64 half: the command line's `token=` words (`cmdline::token`; the half still finds the line in PVH start info), the init program (`init::Prog::parse`, with `store_slot`/`str_until_nul`), `Identity` and its once-cell, the unserved-syscall note (`nosys`), and the evidence `ring`. `no_std`, `forbid(unsafe_code)`, in `default-members`, 5 host tests (none of these pieces had one before). No behaviour change | host tests 5/5; battery 10/10; probes `smp=1` PASS; `smp=4` lost its ssh session after `killtree` (`rc=255`, 0 faults), the HEAD flake |
| A2.2 | `1ad69051` | **`shmpages` moved to core**, behind a `shmpages` feature (`disk` adds `inode_freed`). It reaches the machine only through the PMM's share count and `akuma_mmu::copy_to_phys`, both arch-neutral, so it builds for `aarch64-unknown-none` unchanged; host tests build core without it | core clippy clean on aarch64 with `disk`; amd64 build + clippy clean; boot smoke (ssh, file write/read, `uname`). Battery and probes deferred |
| A2.3 | `14ed9ac8` | **Demand paging is core's, over a page-table trait.** (a) The lazy-record codec moved from `akuma-pte-amd64::lazy` to `akuma-tokio-kernel-core::lazy_record`: it only needs bit 0 clear, and bit 0 is `P` on amd64 and the descriptor's valid bit on AArch64, with every other bit of an invalid leaf ignored by both CPUs, so one layout serves both. (b) `mmu::UserPages` (`leaf`, `write_leaf`, `set_leaf`, `map_user`, `invalidate`, `anon_like`, and a `FreshPage` frame type) is the page-table seam; `lazy::Kernel` adds `root_fs`/`task_root`. `lazy.rs` (fill, shared fill, reserve, reprotect, clear, move, prefault, inode pins) moved to core, generic over the machine; the amd64 half keeps a shim (`Amd64` implements the traits; `Frame` newtypes `FreshFrame` for the orphan rule), so no call site in `lib.rs` changed. Core builds for `aarch64-unknown-none` with `lazy,disk` | host tests 10/10 (core) + 14/14 (`akuma-pte-amd64`); clippy clean on both targets; battery 10/10 after `boot fresh`. Probes deferred |
| A2.4 | `94cc73a1` | **The task record is core's.** `core::task`: `Task` (root page table by physical address, the `akuma-user-layout` layout, the exit code), `Tasks<N, C>` (the records in a `BorrowCell` plus each core's current record), and the address-space layout constants (stack top, mmap base and slab, brk slab, hole and shared-mapping counts), all under 2^47 so the same layout serves amd64's canonical user half and AArch64's 48-bit `TTBR0`. The amd64 half's `with_task`/`task_root`/`set_task_slot` are one-line forwards passing its CPU index | host tests 17/17 (core: 3 new, incl. the nested-borrow panic); clippy clean on both targets; battery 10/10. Probes deferred |
| A2.5 | `79bc221f` | **The memory arms are core's.** `core::mm`: `mprotect`, `munmap`, `msync`, `madvise` and the release and write-back paths under them (`release_pages`, `shared_writeback`, `shared_flush_all`, `free_dropped`), generic over `mm::Mm`, which extends `lazy::Kernel` with the task record, translate/unmap/map with a user bit, `frame_of(pte)`, the TLB shootdown, the W^X written record, the CoW privatize, and the `wx=`/`mmdbg=` switches. `USER_TOP` (2^47) joined the layout constants. The amd64 half forwards under the old names; `Amd64` implements `Mm` from its existing helpers, which stay (the CoW break and the W^X `D` bit are still x86 code, and the next to go behind the trait) | host tests 17/17; clippy clean on both targets; battery 10/10 |
| A2.6 | (this commit) | **The CoW break and the W^X record are core's.** `core::cow`: `resolve` (the one break path: re-read the live leaf, `akuma_cow`'s decision, take in place or copy, invalidate, shoot down before the old frame's count drops), `prepare_write` (the kernel-store gate), `privatize` (`mprotect` to writable), `page_written`/`mark_written`, the `wx=` switch and the CoW counters. `UserPages` gained the bit vocabulary they need: `writable`, `cow_marked`, `written`, and the three edits `take`, `take_at(pa)`, `share_marked`. amd64 answers with `RW`, bit 9 and `D`; an AArch64 half answers with its own AP and software bits. The copy now allocates an unzeroed frame and overwrites all of it through `akuma_mmu::copy_from_phys` (was: zeroed, then copied) | host tests 17/17; clippy clean on both targets; battery 10/10. A2.5 probes (run after its commit): `smp=1` PASS, `smp=4` the HEAD flake (`rc=255` after `killtree`, 0 faults) |

### Rig note

`tcg_rig.py battery` runs against a guest that is already up; it does not boot one. The first A2.1 battery ran
without a `boot fresh` before it and failed every line with `rc=255 got=''`, though the guest's console shows each
command executing with exit status 0. The saved HEAD kernel failed the same way in the same state. After
`boot fresh`, a hand-run `ssh` returned normally and the battery passed 10/10. Why the client returned 255 was not
pinned down. Boot fresh before every battery.

## Background

[`B15_TOKIO_KERNEL_KIT.md`](../../proposals/B15_TOKIO_KERNEL_KIT.md),
[`B12_AARCH64_SLOT_TABLE.md`](../../proposals/B12_AARCH64_SLOT_TABLE.md),
[`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md),
[`AUDIT_SURFACE_METHODOLOGY.md`](AUDIT_SURFACE_METHODOLOGY.md).
