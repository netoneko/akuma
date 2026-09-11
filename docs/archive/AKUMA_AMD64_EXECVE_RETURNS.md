# amd64: `execve` returns into the new image

> 2026-09-11, the same day as the console `ProcessChannel`. Piece **B** of
> `proposals/NEXT_AGENT_AMD64_PROCESSCHANNEL_AND_LIFECYCLE.md` — the last
> structural item in C1.
>
> **No shared crate changed**, again: `git status -- crates/ src/` is clean, so
> the AArch64 kernel is untouched.

## 1. The prompt's evidence was wrong, and it changed the cost

The handoff proposed: "`sys_execve` rewrites `user_rip`/`user_rsp` in its own
`UserCtx` instead of asking to leave ring 3, and the syscall returns straight
into the new image." Its argument was that `syscall_entry` writes `user_rip` at
`[rax + 32]` **and the return path reads it**, so this would be "making the two
agree rather than adding a third copy".

The return path does not read it. It pops the `rcx`/`r11` that the `syscall`
instruction itself delivered, off the kernel stack:

```
    pop r11                         /* user rflags */
    pop rcx                         /* user rip    */
```

`[rax + 32]` is written for `vfork`'s benefit alone — so a child task can be
handed the exact point the parent will resume from — and **nothing read it on
the way out**. Writing it from `execve` would therefore have changed nothing at
all: the syscall would have returned to the instruction after the old image's
`syscall`, in an address space where that instruction no longer exists.

So the work is not "make two fields agree". It is a **second return path**, and
that path has two obligations the ordinary one does not.

## 2. `.Lexec_return`

```
    cmp qword ptr [rcx + 168], 0    /* uctx.exec_pending */
    jne .Lexec_return
```

checked after `leave` and before the ordinary return. The path abandons the
pushed rip/rflags and the whole kernel frame — they describe a program that no
longer exists, and `syscall_entry` re-establishes `rsp` from `uctx.kernel_rsp`
on every entry, so nothing leaks — then:

- **zeroes every register.** Not hygiene. System V says `%rdx` at process entry
  is a function pointer to register with `atexit`, and musl's `_start` passes it
  straight to `__libc_start_main` as `rtld_fini`. Left holding the old image's
  third syscall argument, the new program would *call* it on the way out.
  `enter_user_mode` got this for free — it is reached from Rust, so `rdx` held
  its own third argument, which is 0 — and that is exactly the kind of accident
  a second entry path does not inherit.
- **forces `r11 = 0x202`**: `IF` set and `DF` **clear**, which `execve`
  guarantees a fresh image. The ordinary return hands back the caller's saved
  flags; here the caller is the program being replaced.

`sys_execve` then writes `user_rip = new_entry`, `user_rsp = new_stack`,
`exec_pending = 1`, and — the line that matters — **does not set `leave`**.

### The BKL accounting is unchanged, which is why this is safe

`syscall_handler` releases the BKL at the bottom **unless `leave` is set**; the
old `execve` set it, so the lock stayed held into `run_process`'s loop and was
released by `enter_user`'s own `bkl_leave()` one instruction before `sysret`.
Now `leave` is 0, so `syscall_handler` drops it on the ordinary line and
`.Lexec_return` runs lock-free. One enter, one leave, either way.

The new shape is marginally *safer*: the old window between `bkl_leave()` and
`sysretq` ran with interrupts on (it was ordinary kernel code) and could be
preempted; this one runs under `IA32_FMASK`'s cleared `IF` throughout.

## 3. What collapsed, and what did not

`run_process` is one entry and one exit:

```rust
let status = enter_user(first.pc, first.sp, forked_child);
```

Gone with the loop: `UserCtx::exec_pending`'s Rust consumer (it is spent in
assembly now, where nothing in Rust is running), `current_entry_stack`, and the
`mut` on `forked_child`/`entry`/`stack`. What the loop really cost was never its
four lines — it was that one task could be below ring 3 more than once for one
process, so the teardown, the exit status and the reap ordering all had to be
written as "the last time round". There is no last time round.

**The two-way `enter_ring3` did not collapse, and the prompt's expectation that
it might was wrong for the same reason its evidence was.** `run_thread` versus
`run_process` is not about re-entry; it is about **teardown**. `run_process`
closes the fd table, drains the thread group, publishes an exit status a
parent's `wait4` will believe and retires the process, and running that when one
thread of several returns would report the whole process dead while its siblings
execute. Deleting the loop does not touch that, and nothing should: the split is
`akuma-exec`'s own distinction, arriving here through `UserCtx::thread_slot`.

## 4. The offsets are pinned now

`syscall_entry` indexes `UserCtx` by hand, and this change added a **sixth**
such offset (168) whose job is to select a return path that then takes a program
counter from a seventh (32). A reordered field would not fail to compile, would
not fail a boot check, and would send `execve` into whatever word had moved — on
a target where that is the entire mechanism by which any program is replaced.

So there is a `const _: () = { assert!(core::mem::offset_of!(..) == ..) }` block
over all six. It should have existed before this change; it is load-bearing
after it.

## 5. Verification

| gate | result |
|---|---|
| Firecracker/KVM `SMP=4` | **619 passed, 0 failed** — the baseline exactly |
| `amd64_ring3_check --smp 1 -n 60` | **RING-3 CHECK: OK** — 60/60 sessions, `free` unmoved, heap drift +132 kB (tolerance 8192) |
| ↳ `grandfork` | **ALL PASS**, including step 4 (*the grandchild execs, the child waits*) |
| clippy, amd64 | clean |
| AArch64 | untouched |

`amd64_ring3_check` is the gate this change had to pass rather than the boot
suite: each of its 60 ssh sessions is `sshd` → `fork` → `execve` → `busybox`, so
the new return path runs hundreds of times against a real musl `_start` — which
is the only thing that would notice a garbage `%rdx` or an inherited `DF`.

### Bare metal could not be measured, and the A/B says why

The box booted and served ssh, and reported **634 passed, 3 FAILED**:

```
  FAILED: xhci: read the MBR at LBA 0
  FAILED: xhci: read the sda1 ext2 superblock
  FAILED: xhci: WRITE(10) to a scratch LBA in sda2
```

with `[xhci] transfer timeout: data` after a successful `READ CAPACITY`, and
`fs: ext2 mounted on module` — the RAM-image fallback, not `root=/dev/sda1`.

**Not this change.** The kernel that scored 641/0 on the same box ninety minutes
earlier (the console `ProcessChannel` build, `md5 f80b82b4…`) was restored from
`/boot/akuma/akuma-amd64.bak-20260911-051038`, booted, and reported **the same
634 passed, 3 FAILED with the same three xhci lines**. Two different binaries,
one result: the USB disk has stalled, which is the box's documented failure mode
(`docs/runbooks/amd64-bare-metal-loop.md`; the usual symptom is the harsher one,
an ssh lockout, because `sshd` reads `authorized_keys` off `sda1`). It needs a
power cycle, which is a hand on the machine.

Everything the boot suite can reach without the disk passed, on both binaries.
The metal number for this change is owed and should be taken on the next boot
after a power cycle.

## Background

`proposals/NEXT_AGENT_AMD64_PROCESSCHANNEL_AND_LIFECYCLE.md` §4 (the prompt, and
the hypothesis this corrects), `AKUMA_AMD64_RING3_SEAM_SLICE{5,6,7}.md` (the
entry seam and the two-way split), `AKUMA_AMD64_EXECVE_INSTALL_IMAGE.md` (the
image swap this return path now follows),
`AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md` (piece A, the same day).
