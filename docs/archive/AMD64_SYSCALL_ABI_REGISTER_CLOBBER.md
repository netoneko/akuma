# The amd64 syscall path clobbered six registers the Linux ABI preserves

**Found** 2026-09-04, during Stage L of the amd64 port (the ELF loader).
**Fixed** the same day, in `amd64/src/usermode.rs`'s `syscall_entry`.
**Scope:** `amd64/` only. The AArch64 kernel does not have this bug — see §5.

This is the first defect the amd64 port found in kernel code rather than in its
own scaffolding, and it is worth its own document for one reason: it was
*invisible* for five stages, and it became visible the moment a program was
compiled by a compiler instead of assembled by the kernel.

## 1. The contract

The Linux x86_64 syscall ABI clobbers **three** registers and no more:

| register | why |
|---|---|
| `rax` | carries the syscall number in, the result out |
| `rcx` | the `syscall` instruction puts the return address here |
| `r11` | the `syscall` instruction puts `RFLAGS` here |

Everything else survives, **argument registers included**: `rdi`, `rsi`, `rdx`,
`r10`, `r8`, `r9` go in and come back unchanged. This is not a courtesy. It is
the contract every compiler targeting Linux/x86_64 generates code against, and
`entry_SYSCALL_64` in Linux honours it by saving a full `pt_regs` and restoring
it on the way out.

The AArch64 ABI is the same shape with different names: `x0` carries the number
in and the result out, `x1`-`x30` are preserved.

## 2. What this kernel did

`syscall_entry` pushed three values — the user `rip`, the user `RFLAGS`, and the
syscall number — switched to the task's kernel stack, shuffled the Linux
argument registers into System V positions, and `call`ed a Rust
`extern "C"` handler:

```asm
    push rcx                        /* user rip    */
    push r11                        /* user rflags */
    push rax                        /* nr */
    sub  rsp, 8
    mov  rcx, rdx                   /* Linux a3 -> SysV 4 */
    mov  rdx, rsi
    mov  rsi, rdi
    mov  rdi, rax
    call syscall_handler
```

`syscall_handler` is an ordinary `extern "C"` function. Under System V, `rdi`,
`rsi`, `rdx`, `rcx`, `r8`, `r9`, `r10` and `r11` are **caller-saved** — the
callee may destroy them freely, and rustc does. So every one of the six
registers the syscall ABI promises to preserve came back holding whatever the
handler had left in it.

The kernel was, in effect, implementing a syscall ABI that clobbered nine
registers instead of three, and had never written the contract down anywhere.

## 3. Why five stages did not notice

Stages F through J ran programs emitted by `usermode::build_user_program` — a
byte-by-byte assembler inside the kernel. Look at what it emits:

```text
  mov r12, <rounds>          ; loop counter
loop:
  mov rax, <write> ...  syscall
  mov r13, <delay>           ; spin counter
  ...
  dec r12
  jnz loop
```

Its live state across a syscall is in **`r12` and `r13`** — which are *callee*-saved
under System V, so `syscall_handler` preserved them for free. The hand-written
program happened to use exactly the registers the bug did not touch.

That is the general shape of the hazard, and it is worth stating plainly: **a
test program written by the same author as the kernel tends to make the same
assumptions the kernel makes.** The value of a program produced by an
independent toolchain is not that it is more complex; it is that its choices are
not correlated with the kernel's.

## 4. How it surfaced

Stage L's guest program (`userspace/amd64/hello/hello.rs`) accumulates a bitmask
of six checks and reports it as its exit status. It exited with `0x401000`.

`0x401000` is not a plausible bitmask; it is the address of the program's
`.rodata`. `rust-objdump` on the image showed why in two instructions:

```asm
400137: mov  eax, 1                 ; SYS_write
40013c: mov  edi, 1
400141: mov  esi, 0x401000          ; the message
400146: mov  edx, 0x27
40014b: syscall
40014d: mov  eax, 0xe7              ; SYS_exit_group
400152: mov  rdi, r8                ; <-- status has been in r8 all along
400155: syscall
```

rustc had parked the running `status` value in `r8` across the `write` syscall,
because the ABI says it may. The kernel's handler had overwritten `r8` with the
message pointer it was iterating, and `exit_group` therefore received `0x401000`
as its status.

The self-test named the symptom precisely — `got 0x401000 want 0x3f`, followed
by all six sub-checks failing at once — which is what pointed at "the status
never arrived" rather than at "the loader mapped something wrong". A bare
boolean assertion would have said `[FAIL]` and left the loader as the suspect,
which is the wrong place to look for two hours. That is the argument for
`check_eq` over `check`, made for the fourth time
(`AKUMA_FIRECRACKER_AMD64.md` §3.14.2).

## 5. Why the AArch64 kernel is not affected

Checked rather than assumed. `crates/akuma-exceptions/src/lib.rs`'s
`sync_el0_handler` allocates an 832-byte frame and saves `x0`-`x30`, `SP_EL0`,
`ELR_EL1`, `SPSR_EL1`, `TPIDR_EL0` and all 32 NEON registers; the epilogue
restores `x1`-`x30` and loads only `x0` from the syscall result slot before
`eret`. That is exactly the arm64 contract — `x0` clobbered, everything else
preserved — so nothing there needs changing.

The asymmetry is structural rather than lucky. The AArch64 side saves a full
trap frame because it has to: a signal frame, `ptrace`, `fork`'s child return
and `rt_sigreturn` all need every register of the interrupted context. The amd64
target has none of those yet, so it hand-rolled the minimum its own test programs
appeared to need — and "appeared to need" was decided by programs that shared the
kernel's blind spot.

## 6. The fix

Six pushes and six pops around the handler call:

```asm
    push rdi
    push rsi
    push rdx
    push r8
    push r9
    push r10
    sub  rsp, 8
    ...
    call syscall_handler
    add  rsp, 8
    pop  r10
    pop  r9
    pop  r8
    pop  rdx
    pop  rsi
    pop  rdi
```

48 bytes is a multiple of 16, so the stack alignment the existing `sub rsp, 8`
establishes is unchanged — worth checking rather than assuming, because a
16-byte misalignment at a `call` is another bug that only shows up under a
compiler that emits aligned SSE spills.

`rbx`, `rbp` and `r12`-`r15` need nothing: they are callee-saved, so
`syscall_handler` preserves them, and a context switch taken inside it (the
`sched_yield` path) saves them too.

## 7. The regression test

The accidental discovery is **not** the regression test. Which register rustc
picks for a live value is its business and can change with any release, so a test
that depends on it having picked `r8` tests the compiler.

`hello.rs` therefore asks the question directly: `regs_preserved()` loads six
distinct sentinels into `rdi`, `rsi`, `rdx`, `r8`, `r9`, `r10`, executes
`getpid` — the cheapest syscall that touches nothing — and compares all six on
the way out. It reports bit 6 of the exit status, and
`usermode::elf_test` names it individually when the mask comes back short:

```
  elf:   a syscall preserved the ABI's registers   [FAIL]
```

## 8. What is still not covered

The fix restores the six registers the ABI names. It does not:

* **Zero anything.** Linux scrubs several registers on the return path to avoid
  leaking kernel values to userspace. This kernel returns `rcx` and `r11` with
  whatever `sysret` requires and leaves the rest as the user set them, which is
  correct but not hardened.
* **Preserve the vector registers.** There is no FP/SIMD state save at all on
  this target — no `xsave`, no lazy-FPU. `syscall_handler` currently touches no
  vector register, which is a property of today's handler rather than a
  guarantee. The moment one does (a `memcpy` intrinsic would be enough),
  userspace SSE state is destroyed silently. This is the same class of bug and it
  is still open; the AArch64 side saves `q0`-`q31` for exactly this reason.
* **Cover interrupt entry.** The LAPIC timer handler goes through rustc's
  `x86-interrupt` calling convention, which preserves the full register set
  itself — so preemption was never affected. Only the `syscall` path hand-rolled
  its frame, and only the `syscall` path was wrong.

---

**Background:** `docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.18 is the stage this
was found in; §3.12 is the syscall path as originally built.
`amd64/README.md` is the current-state doc for the target.
