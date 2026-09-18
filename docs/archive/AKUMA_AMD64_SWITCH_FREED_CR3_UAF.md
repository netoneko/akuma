# amd64: `[SWITCH FREED-CR3]` — the page-table use-after-free that killed the box

**Found:** 2026-09-18, on the bare-metal HP box, from a photograph of the
framebuffer. **Root-caused and fixed:** 2026-09-18.
**Evidence and first reading:** `proposals/AMD64_SWITCH_FREED_CR3_UAF.md`.

---

## 1. The symptom

A `kbuild -j 1` at SMP=4, running normally, then:

```
[threads] new high-water: 23 live user threads (terminated=0 free=489 ceiling=512)
[SWIT
```

— and the machine was gone. No ping, no ssh, recovery a power cycle at the
machine. `[SWIT` is `[SWITCH FREED-CR3]` truncated five characters into a
message `amd64/src/sched.rs` built from ~7 separate `serial::puts` calls.

## 2. The hole: a gate whose second arm was never asked

`akuma-mmu`'s address-space free gate has two arms, and both exist:

| arm | question | fed by |
|---|---|---|
| `any_core_on_l0` (`lib.rs:1189`) | is any core's **live CR3** on this L0? | `paging::activate`'s `publish_l0_begin`/`_end` — live on amd64 since step 5a |
| `any_saved_ctx_on_l0` (`lib.rs:161`) | does any thread's **saved** root still name it? | a `SchedHooks` table registered by `akuma_exec::init` |

Arm 2 asks through `akuma_mmu::SCHED`, a `Registered` cell that **degrades to
`None` when nothing has registered it**. And `amd64` never calls
`akuma_exec::init`: `exec_runtime::init` registers the runtime table, the
process counters and the ELF hooks and stops there, by design ("each further
hook explicitly when a step starts depending on it"). Nothing ever added this
one.

So the x86 saved-root probe added on 2026-09-12 — `sched::any_task_on_space_root`,
registered into `akuma_threading::register_saved_root_probe` — was **reachable
by nobody**. `akuma_mmu::any_saved_ctx_on_l0` kept answering `None` from its
unregistered cell, exactly as the hardcoded `None` stub it replaced did, on
*both* consumers:

* `free_or_defer_as_frames` (`lib.rs:1494`) — the free path, and
* `take_one_ready_ttbr_free` (`lib.rs:1581`) — the drain path.

That pair is what the AArch64 fix needed on both halves (`COW_PILE_AUDIT.md`
§10). amd64 had neither, while looking from the `sched.rs` side as though it had
both.

**The failure that follows:** a process dies while a sibling thread is parked
off-CPU with that address space's root still in its `Machine::space_root`.
`any_core_on_l0` truthfully answers "no core is *running* it", nobody asks about
the saved one, the L0 goes back to the PMM, the frame is reissued as something
else, PML4 slot 511 stops naming the kernel's PDPT — and the next switch-in
installs it. This is why `kill -9` on a process blocked in a syscall is the
shape that reproduces it, and why two `cargo`/`rustc` processes survived
`SIGKILL` in the same session.

## 3. The second bug, found by fixing the first

Registering the hooks turned the boot suite's `redirect: teardown leaks nothing`
red: **413 pages** parked and not returned. A/B'd three ways (baseline 776/0;
hooks registered 776/1; hooks registered with `current_thread_is_terminated`
stubbed back to `|| false` — still 776/1), which pinned it on the saved-context
arm itself rather than on the terminal-drain gate.

The cause is a divergence the probe's own doc-comment had asserted away. It
claimed "a TERMINATED slot's root is overwritten by `finish`" — true only of the
*normal* exit path. `finish()` zeroes `space_root` for the thread that runs it;
a thread that dies without running it (a fault kill, a group-fatal signal) keeps
its last root. And **this target has no slot recycler**: AArch64 zeroes the
saved context on the TERMINATED→FREE transition, where `x86_claim_slot` takes a
TERMINATED slot *directly*. So a dead slot pinned a dead process's entire frame
set until some later spawn happened to reuse it — parked rather than lost, but
on a 1 GiB box that distinction is academic.

The fix is a carve-out that is a proof rather than a guess. A slot that is
TERMINATED or FREE **and not `ON_CPU`** cannot install its `space_root`:

* `x86_pick_next` picks only READY and RUNNING slots, and skips any slot with
  `ON_CPU` set — so it cannot be switched in; and
* the only route out of TERMINATED/FREE is `x86_claim_slot`, and every caller
  writes `space_root` before publishing — `prepare_task_slot` zeroes it, then
  `spawn_unpublished` / `set_task_space_root` / `register_idle_task` supply the
  real one.

Every other state still blocks, as the AArch64 scan documents: the cost of a
wrong "no" is the machine, the cost of a wrong "yes" is a deferral.

## 4. Making the tripwire survivable

As written the tripwire destroyed its own evidence twice over, and both halves
cost a walk to the machine:

* **The message was not atomic.** `serial::LOCK` is per call, so seven `puts`
  can be shredded by a peer core and truncated mid-word by a dying one. Now one
  `StackWriter::<192>` and a single `flush()` — the rule
  `idt::dump_user_registers_and_memory` already states for the same reason.
* **It then fell through into the `mov cr3`.** That is the fatal step. It no
  longer does: the slot is **demoted to the kernel root** and its `space_root`
  zeroed. The kernel root maps everything ring 0 touches and, since
  `paging::drop_identity_map` cleared PML4 slot 0, nothing at all in the lower
  half — so a user thread resuming on it takes an ordinary not-present fault at
  its first ring-3 instruction and dies with a `SIGSEGV`. One process for the
  machine, and the next occurrence is readable over ssh instead of on a camera.

`sched::freed_cr3_trips()` counts them and the boot suite asserts it is zero
(`debug: no switch installed a freed page-table root`), beside the
`switches_without_bkl` check it is modelled on. A survivable failure is one a
boot can otherwise scroll past, which is why it is an assertion and not a print.

## 5. What changed

| file | change |
|---|---|
| `amd64/src/sched.rs` | `register_hooks` now registers `akuma_mmu::SchedHooks` (all three fields); `any_task_on_space_root` gained the unschedulable-slot carve-out (`slot_can_install`); the tripwire composes one line and demotes instead of installing; `FREED_CR3_TRIPS` + `freed_cr3_trips()` |
| `amd64/src/idt.rs` | boot-suite assertion on `freed_cr3_trips()` |
| `scripts/utils/hpbox.py` | `akuma_push()` — the missing Akuma-side file transport (there is no `scp` here) |

The third `SchedHooks` field is not a freebie either.
`current_thread_is_terminated` unregistered reads as "no thread is terminal",
which lets a *dying* thread run a multi-thousand-page drain it can be reaped out
of mid-loop, orphaning the entry it had already removed from the list — the
self-host heap leak (`SELFHOST_KERNEL_HEAP_LEAK.md`). The collectors that keep
the parked list short without it are the ones AArch64 uses and this target
already has: the idle loop's reclaim, and every address-space drop not itself on
a terminal thread.

## 6. Verification

`scripts/utils/amd64_trials.py --local-only` (the box's Ubuntu side was down, so
the Firecracker arm could not run):

| arm | SMP=4 | note |
|---|---|---|
| baseline (`308580a2`) | 776 passed, 0 failed | |
| hooks registered, no carve-out | 776 passed, **1 failed** | `redirect: teardown leaks nothing`, −413 pages |
| same, `current_thread_is_terminated: \|\| false` | 776 passed, **1 failed** | identical — exonerates the terminal gate |
| **final** | **777 passed, 0 failed** | the extra check is the new assertion |

SMP=1: 767 passed, 0 failed. Host tests: 80 suites, 0 failures. Clippy clean for
`x86_64-unknown-none`.

### On the metal

Installed the same day (`kbuild -j 1` incremental, 1 m 33 s, two crates;
`install_kernel_amd64.sh`; `reboot -f`). The box came back in 33 s and the
self-test ran **775 passed, 0 failed** at SMP=4 on real silicon, with
`debug: no switch installed a freed page-table root [OK]`.

Then the repro the proposal asks for, for ~10 minutes: a clean 95-crate
`kbuild -c -j 1` as load, and alongside it 24 rounds of `kill -9` against
processes blocked inside a syscall — two `sleep 600` parked in `nanosleep` and
two `find` blocked in `read(2)` on the USB root, so there is a real address
space with real mappings being torn down at a point no thread chose. The probe
ran to `DONE`.

**Result: 0 `[SWITCH FREED-CR3]`, 0 kernel faults, 0 exceptions, no panic, and
the machine stayed up.** (312 `[BKL] stuck` lines — the known contention storm
under load, not new.)

## 7. The `rc=139` is a different bug, and this run says what it is

The proposal allowed that this might be "the same corruption that produced
`rc=139` in ~half of this box's builds". **This run separates them.** The clean
build above did die `rc=139` — and the tripwire stayed at zero while it did.

The kill was a **ring-3 `#GP`**, no kernel fault anywhere:

```
[Fault] #GP general protection in ring 3 on cpu 2 err=0x0 rip=0x300469c6 task=13 pid=74
  [regs] rax=0x0032004e4f495450 …  rdi=0x0000000101a5e000 …
  [mem] rdi=0x101a5e000: 0032004e4f495450 0000ff0000000007 646465626d65203a 6961006f692d6465 …
  [mem] rcx=0x300469c6: … 3948f04f8d48f047 48b60ff401741048 …
```

Read it in three steps, because each one is checkable:

1. **The instruction.** The `rcx` dump starts at `(rip & !0xf) - 16` =
   `0x300469b0`, so `rip` is the byte at `+22`: `48 39 48 10` —
   `cmp QWORD PTR [rax+0x10], rcx`.
2. **The operand.** `rax = 0x0032004e4f495450`, whose bits 63:48 are `0x0032` —
   **non-canonical**, which is precisely what a `#GP` with `err=0` on a memory
   reference means. Its bytes little-endian are `b"PTION 2 "`.
3. **Where it came from.** `rdi` points into a string table — the six qwords
   read as `": embedd"`, `"ed-io ia"`, `"ts _f16 "`, i.e. crate names — and
   `rax`'s value is exactly the qword at `rdi-16`.

So `rustc` loaded what should have been a pointer and got **string-table bytes**,
then dereferenced them. That is the self-host data-corruption family
(`AKUMA_AMD64_ANON_FAULT_DOUBLE_POPULATE` / `fpcache` — note `[FPCACHE]
entries=61904 hits=1333063 misses=63854 inval=1950` at the moment of the fault),
**not** a page-table use-after-free: a freed-root UAF faults in *ring 0* on a
kernel address, which is the whole reason it takes the machine.

Worth noting it is also **not** the `hlt`/mallocng-assert shape that earlier
ld-musl `#GP`s here turned out to be (`AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md`
§13) — the byte at `rip` is `0x48`, not `0xf4`. Two different ring-3 `#GP`s in
the same address range, and the instruction byte is what tells them apart.

**One occurrence, so this is evidence and not proof** that the two can never
coincide. What it does retire is the assumption that fixing the UAF would fix
`rc=139`. It did not, and the next attack on `rc=139` should start from the
corrupted-pointer evidence above — `userspace/amd64/fbstress` (concurrent demand
faults on file-backed pages shared via `fpcache`) is still unrun and is the
closest probe to this shape.

Three `[TRAMP-MISMATCH] tid=7 THREAD_PID_MAP=1174 but table scan found 64` lines
also appeared during the build. Unexplained, unrelated to this fix, and noted
here because it is the kind of line that is cheaper to have written down than to
rediscover.

## 8. Background

- `proposals/AMD64_SWITCH_FREED_CR3_UAF.md` — the evidence, the photograph, and
  the reading that pointed at the right pair of arms.
- `docs/archive/COW_PILE_AUDIT.md` §10 — the AArch64 freed-L0 hazard this is the
  counterpart of.
- `docs/archive/AKUMA_AMD64_SSH_WEDGE_CONTEXT_SWITCH_PF.md` — the ring-0 fault
  inside `akuma_threading_x86_switch_context` that is this bug's earlier costume.
- `docs/archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md` §3 — the `rc=139` SMP
  corruption.
- `docs/runbooks/amd64-bare-metal-loop.md` — the rig.
