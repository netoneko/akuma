/*
 * setjmp/longjmp for x86_64 (System V ABI)
 *
 * The counterpart to setjmp.S (AArch64), used by tcc's own error handling —
 * `tcc_error_noabort` longjmps out of however deep the parser/codegen was
 * when it hit something it could not compile, back to the `setjmp` in
 * `tcc_compile_string`/`tcc_run`. Same layout convention as that file: the
 * System V callee-saved integer registers plus the return address, packed
 * into the front of musl's `jmp_buf` (`__jmp_buf[22]`, 176 bytes — this uses
 * 64 of them), which is arch-neutral and already ships with the vendored
 * musl headers on both targets.
 *
 * Deliberately NOT saved: xmm6-xmm15 (System V's callee-saved vector
 * registers). tcc's own C sources never keep a floating-point value alive
 * in one of those across a call into code that can longjmp — it is a C
 * compiler working over an AST, not numeric code — and the AArch64 sibling
 * saving d8-d15 is not proof otherwise: this target's kernel is already
 * documented as carrying no FP/SIMD save of its own on a context switch
 * (`amd64/README.md`, "What is deliberately missing"), so a setjmp that
 * *did* save vector state would be promising a guarantee nothing else here
 * keeps.
 */

// clang's integrated assembler defaults `.S` files to AT&T syntax; this is
// written Intel-style (`mov dst, src`, `[base+disp]`) to read the same as the
// AArch64 sibling's `ldp`/`stp` lines, so it needs to say so explicitly.
.intel_syntax noprefix

.global setjmp
.global longjmp
.global _setjmp
.global _longjmp
.text

// int setjmp(jmp_buf env);
// System V: rdi = env
setjmp:
_setjmp:
    mov [rdi + 0],  rbx
    mov [rdi + 8],  rbp
    mov [rdi + 16], r12
    mov [rdi + 24], r13
    mov [rdi + 32], r14
    mov [rdi + 40], r15
    // The caller's stack pointer, as of just after `setjmp` returns: `rsp`
    // right now still includes the return address `call` pushed, so the
    // caller's own rsp is 8 bytes above this one.
    lea rax, [rsp + 8]
    mov [rdi + 48], rax
    // The return address itself, off the top of the stack — this is what
    // `longjmp` resumes at, standing in for AArch64's `x30` (LR).
    mov rax, [rsp]
    mov [rdi + 56], rax

    xor eax, eax
    ret

// void longjmp(jmp_buf env, int val);
// System V: rdi = env, esi = val
longjmp:
_longjmp:
    mov rbx, [rdi + 0]
    mov rbp, [rdi + 8]
    mov r12, [rdi + 16]
    mov r13, [rdi + 24]
    mov r14, [rdi + 32]
    mov r15, [rdi + 40]
    mov rsp, [rdi + 48]
    mov rcx, [rdi + 56]     // saved return address; rcx is otherwise free here

    // longjmp's `val==0` must come back as 1 (setjmp's "we returned via
    // longjmp" contract is a *nonzero* value, and it must not be 0 — the
    // "we returned normally" value).
    mov eax, esi
    test eax, eax
    jnz 1f
    mov eax, 1
1:
    jmp rcx
