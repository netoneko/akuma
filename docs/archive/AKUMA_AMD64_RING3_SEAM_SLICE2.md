# amd64 ring-3 entry seam, slice 2: `Process::run` gets an arch arm, and amd64 enters ring 3 through it

**Date:** 2026-09-10
**Status:** landed.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's
**ring-3 entry seam** row — the step `AKUMA_AMD64_C1_5C_SURVEY.md` named and
`proposals/NEXT_AGENT_AMD64_RING3_ENTRY_SEAM.md` scopes.
**Slice:** 2 of 4. Slice 1 is `AKUMA_AMD64_RING3_SEAM_SLICE1.md`.

`akuma_exec::process::Process::run` — the shared "activate this address space
and enter ring 3" — **is now what the amd64 kernel runs**, for every process it
starts. It ends in a registered hook rather than an `eret`, and this target
registers a function that owns the `execve` loop and the process teardown
because on x86_64 the entry *returns*. Plus the mechanical piece slice 1 left
behind: `akuma_threading::update_thread_context` has a real x86_64 arm.

The AArch64 kernel's behaviour is unchanged, measured two ways: `.text` grew by
**28 bytes** in exactly two functions, and a side-by-side KVM boot ran the same
284 self-tests with the same single pre-existing failure.

## 1. What the seam had to be, and why `-> !`

The prompt's §4 is the whole design and it was right: the mismatch is not the
register file (slice 1 settled that) but **who owns the exit path**.

- `Process::run()` is `-> !` and ends in an `eret`. A process's *exit* happens
  somewhere else entirely — `exit_group` marks the thread terminated from inside
  the syscall path — and the thread never comes back to this frame.
- `amd64::usermode::run_process` is a **loop** whose `enter_user` returns an
  exit status, followed by forty lines of teardown: close the fd table, drain
  the process's threads, publish the exit status to the `SPAWN` row, retire the
  slot, finish the task. `execve` is the loop going round again.

Option 1 was taken: `ExecRuntime` gains

```rust
pub enter_user: fn(&crate::process::UserContext) -> !,
```

and `Process::run`'s tail becomes `(runtime().enter_user)(&ctx)`. AArch64
registers `enter_user_mode_checked` — the same function the tail called
directly, so that half is a pure indirection. amd64 registers
`usermode::enter_ring3`, which *is* the old `run_process` body.

**`-> !` and not `-> u64`, and this is the load-bearing choice.** `!` is the
only return type both architectures can honour, and it forces the target whose
entry returns to say what happens afterwards **inside its own arm** rather than
handing a status back to shared code with no idea what to do with one. The two
lifecycles stay different; the difference is now one function pointer instead of
two files that do not know about each other.

The `bkl_leave()` asymmetry the prompt flagged is discharged the same way: both
arms release the kernel lock immediately before the privilege drop
(`akuma_bkl::bkl::leave_kernel` inside `enter_user_mode`,
`crate::smp::bkl_leave` inside `enter_user`), and the shared call site does not
know a lock is being dropped.

## 2. amd64 now enters ring 3 through the shared function

The prompt did not require this and it is what makes the slice *verifiable*.
Registering a hook nobody calls proves only that it compiles: on this target
`Process::run` is reached from `entry_point_trampoline`, which is slice 3's
business. So `usermode::proc_entry` — the single entry function every process
task starts at — was pointed at `Process::run` instead of at `run_process`.

Every ring-3 process on this kernel now takes the shared path: the six boot
self-tests, `run_init`, sshd, every shell, every `fork` child, every `apk`
sub-process. That is what the gates in §4 are measuring.

Three things change on this target as a result. Two are the shared code closing
a gap for free; one is a cost, and it is stated at the call site rather than
removed:

| | what `Process::run` does | this target before |
|---|---|---|
| **gain** | refuses to enter ring 3 when `THREAD_PID_MAP` says the task belongs to a *different* pid — proof it would otherwise activate a foreign address space and jump to this process's entry point inside it | no such check |
| **gain** | `state.store(ProcessState::Running)` | every registered process stayed `Ready` for its whole life; `prepare_for_execution` is not on this path, so `/proc/<pid>/stat` said `R` only by luck of the default |
| **cost** | `address_space.activate()` — one `mov cr3` | the scheduler had already installed exactly that root from the task slot's own `space_root` |

The `mov cr3` is redundant **because of a property of this target's spawn
path**, not a property of `run()`: `spawn_in_space_unpublished` recorded the
root off the same `Image` the registration owns. It costs one TLB flush per
*process launch*, not per ring-3 entry — the `execve` loop is below the hook —
and removing it would mean teaching shared code that some callers pre-activate,
which is worth less than the line of comment that says so.

The "task with no registered process" refusal moved with the resolution: it used
to be `run_process` finding `current_entry_stack()` empty, and it is now
`proc_entry` finding `current_process()` empty, which is where the lookup
happens. It has been unreachable by construction since 5b slice 4 (every process
task is registered before it is published) and is still checked rather than
assumed, because the teardown below the entry publishes an exit status a
parent's `wait4` will believe.

### 2.1 The loop's first pass uses the context it was handed

`run_process` re-read `current_entry_stack()` at the top of every iteration,
including the first. It now takes `first: &UserContext` — what `Process::run`
read out of the registered process under the `image` lock — and re-reads only on
the way round, which is what an `execve` rewrites.

The two are the same two scalars read microseconds apart under the same lock,
and nothing can rewrite them in between: this task has not executed a user
instruction yet, so it cannot have `execve`d, and no other task writes another
process's image. Stating that is the point of the change — the authority for
where ring 3 starts is now handed *down* from shared code instead of fetched
sideways.

## 3. `update_thread_context` grows its x86_64 arm

It was `unimplemented!()`, on the argument that the AArch64 version writes a
fake-IRQ-frame trap layout x86_64 does not have. True, and beside the point:
this target has an equivalent place to put a ring-3 register file — `UserCtx`,
one per task slot, written by the `syscall_entry` assembly and read by the
`sysret` — and `amd64::sched` has been seeding a `fork` child's copy of the
parent into it since this target grew `fork`.

So the writer is shared rather than duplicated:

- `amd64::sched::seed_forked_task(slot, fs_base, gs_base, &[u64; 12])` became
  **`write_user_context(slot, &UserContext)`**. The triple *was* that type —
  `akuma-exec-core`'s x86_64 arm is exactly what `syscall_entry` saves — and
  taking it as one value is what lets the two callers be one function.
- It is registered as a new `X86ArchHooks::write_user_context`, because the
  slot table lives in `amd64/src/sched.rs` and `akuma-threading` cannot name it.
  That is the same kind of effect as `switch_to`.
- `sys_fork` calls it directly, with a `UserContext` it now builds from the
  parent's capture (`usermode::current_user_context`) instead of unpacking a
  five-tuple. When `sys_fork` folds in slice 3 that call becomes a deletion
  rather than a translation.

Two decisions are stated in the code rather than left to be rediscovered:

- **`pc`/`sp` are deliberately not copied into `UserCtx`.** A `UserContext`
  carries them; `UserCtx::user_rip`/`user_rsp` are the *syscall entry* capture,
  and the authority for where a task enters ring 3 is `ProcessImage::context`,
  which `enter_ring3` is handed. Writing them here would put a second copy of
  that in a second structure — the staleness bug
  `UserContext::set_address_space_root`'s doc describes, one field along.
- **`rax` has no home**, because both entry points hard-code the value ring 3
  resumes with (`enter_user_mode_forked` does `xor eax, eax`; `enter_user_mode`
  takes `entry_rax` and every caller passes 0). That matches every context
  shared code builds — `set_child_return_zero` is the field's only writer — so
  a non-zero `rax` reaching the writer prints rather than being dropped
  silently.

Off both kernels (a host `cargo test`, where `target_arch` is the host's) the
function is a **no-op rather than a panic**: nothing on a host has a slot to
write. That is the same trap slice 1 §2.3 hit from the other side.

### 3.1 It gets a boot test, because it is the half that is still unreachable

`enter_ring3` is exercised by every process this kernel starts. The other half
is the opposite: `update_thread_context` is called by the **shared**
`spawn_child_thread_and_publish`, which this target does not reach until
`sys_fork` folds. Wired but unreachable is the shape that rots, and the failure
it rots into is a child resuming on a register file that is subtly not its
parent's — which reads as a userspace bug a long way from here.

`sched::user_context_smoke_test` (5 checks) claims one unpublished slot, calls
`akuma_threading::update_thread_context` **by thread id, the way the shared code
will**, with a `UserContext` whose every field is distinct and non-zero, reads
the slot's `UserCtx` back and abandons the slot. The last check pins the
*decision* rather than a mapping: `user_rip`/`user_rsp` must still be 0.

## 4. Verification

| gate | before | after |
|---|---|---|
| QEMU/TCG `SMP=1` | 616/0 | **621/0** |
| QEMU/TCG `SMP=4` | 626/0 | **631/0** |
| amd64 `--features no-tests` | OK | **OK** |
| host tests | 1373 | **1373** |
| clippy — aarch64 `release` + `extreme-size`, amd64 with and without `no-tests` | clean | **clean** |

`+5` on both QEMU rows, all of them `user_context_smoke_test`; 616/626 minus the
new checks either side.

**Pre-existing, not introduced:** the per-crate *host* clippy loop the
pre-commit hook runs fails in `akuma-primitives/src/preempt.rs:153`
(`too_long_first_doc_paragraph`), a file this slice does not touch. The three
crates it edits carry no warning of their own.

### 4.1 The ring-3 gates

| gate | result |
|---|---|
| `amd64_ring3_check --smp 1 -n 40` | **40/40 sessions**, `free` unmoved (1563224 → 1563224), `ps` 5 → 5, heap +37 kB, `grandfork` ALL PASS |
| `amd64_ring3_check --smp 1 -n 60` | **60/60**, `free` unmoved, `ps` 5 → 5, heap +21 kB, `grandfork` ALL PASS |
| `lazybuf` (over ssh) | **8/8** |
| `openflags` (over ssh) | **20/20**, 0 known divergences |
| `busybox sh` pipeline over ssh (`ls /bin \| sort \| head`, `ls /bin \| wc -l`) | **OK** |
| `apk update` | **OK** — 28641 packages |
| `apk add file`, then `file /bin/busybox` | **OK** — 3 packages installed, and the *dynamically linked* `file(1)` it installed runs and identifies busybox correctly |

`apk` reports `3 errors` and `WARNING: … failed to preserve …: owner`. That is
pre-existing and unrelated — amd64 dispatches no `chown`/`fchownat` at all
(`AMD64_CONSOLE_NONBLOCK_READ.md` §"gates"); the same three errors appear on a
run that installs nothing.

### 4.2 The AArch64 side, both ways

This slice edits code the AArch64 kernel *executes*, so slice 1's
byte-identical check could not apply and the prompt's §7 boot was owed.

**The binary.** `.text` grew **28 bytes**, and exactly two symbols changed size:

| symbol | before | after |
|---|---|---|
| `akuma_exec::process::Process::run` | 728 | **744** (+16) |
| `akuma_kernel_glue::kernel_main` | 55620 | **55632** (+12) |

The first is the load of the hook out of the registered table plus an indirect
branch, where a direct `bl` stood; the second is one more field stored into the
`ExecRuntime` literal. `.rodata` differs only by address shifts (583 of 860
differing words are exactly `+28`), and 15 `.data` statics moved by 8 bytes with
the section's total size unchanged — a layout reshuffle, not a content change.
`.bss` and `.text.boot` are byte-identical. The build is deterministic: a forced
rebuild of the same tree reproduced every section exactly.

**The boot.** `scripts/lima_aarch64_run.sh` (KVM inside Lima, `INSTANCE=6`,
`SMP=1`), committed HEAD and the change side by side, each run to a quiet log:

| | HEAD | change |
|---|---|---|
| distinct `[Test] … PASSED` | 284 | **284**, identical set (`diff` empty) |
| `PASSED` occurrences | 306 | **306** |
| failures | 1 | **1**, the same one |

The one failure is `test_mmap_file_oom_survives` ("PMM not reclaimed after kill
(500 polls)"), present in both arms and therefore this rig's, not this slice's.

The pre-existing TCG panic slice 1 warned about (`test_spawn_ext_passes_env`)
does not occur under KVM in Lima; the whole suite runs, which makes this a
stronger check than the one the prompt budgeted for.

### 4.3 What was **not** run

The **Firecracker/KVM** and **bare metal** lanes on the trashcan box. The box
was booted into its Akuma personality when this landed; the Firecracker lane
needs it on Ubuntu (`hpbox.deploy` + `amd64_trials.py --remote-only`) and the
metal lane needs a `stage` + `reboot_to("akuma")` on top of that. The prompt
already flagged both numbers as extrapolated from 4b rather than measured, so
they are still owed — by this slice and by 4b.

## 5. What slice 2 deliberately did not do

- **Call `fork_process`.** `sys_fork` still runs, and still builds the child
  itself; what changed is that it describes the child with a `UserContext` and
  seeds it through the function the shared path will use. Folding it is slice 3,
  and the `CHILD_CHANNELS` decision (prompt §5) bites there and nowhere earlier.
- **Unify the lifecycles** (prompt §4 option 2). Still the attractive follow-on,
  and still its own step with its own baseline — more attractive now that the
  register file and this hook have both been proved.
- **The `SPAWN` row and `proc_slot`** (prompt §5, third bullet). `enter_ring3`
  reads the slot out of the running task's own `UserCtx`, exactly as
  `proc_entry` did, so nothing regressed — but a child arriving through the
  shared `entry_point_trampoline` in slice 3 still needs `seed_proc_slot` called
  for it, and that is where the question the prompt asks ("does `Spawn` + `wait4`
  first make this smaller?") has to be answered.

## Background

- `proposals/NEXT_AGENT_AMD64_RING3_ENTRY_SEAM.md` — the step's prompt. §4 is
  this slice's design, including the two options it deliberately did not take.
- `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE1.md` — the `UserContext` split this
  builds on, and the §3.1 ELF-comparison recipe §4.2 above uses.
- `docs/archive/AKUMA_AMD64_C1_5C_SURVEY.md` — why this is a step and not a fold.
- `docs/archive/AKUMA_THREADING_X86_SWITCH.md` — the kernel-side `Context` split
  and the `X86ArchHooks` table this slice adds a field to.
- `docs/archive/AMD64_CONSOLE_NONBLOCK_READ.md` — the previous change to this
  target, and where the `apk` chown residue is written down.
