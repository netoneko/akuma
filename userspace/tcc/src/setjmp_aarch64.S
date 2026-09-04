/*
 * setjmp/longjmp for AArch64
 */

.global setjmp
.global longjmp
.global _setjmp
.global _longjmp

// int setjmp(jmp_buf env);
// x0 = env
setjmp:
_setjmp:
    stp x19, x20, [x0, #0]
    stp x21, x22, [x0, #16]
    stp x23, x24, [x0, #32]
    stp x25, x26, [x0, #48]
    stp x27, x28, [x0, #64]
    stp x29, x30, [x0, #80]  // x29=FP, x30=LR
    mov x2, sp
    str x2, [x0, #96]
    
    // Save floating point registers (d8-d15 are callee-saved)
    stp d8, d9, [x0, #104]
    stp d10, d11, [x0, #120]
    stp d12, d13, [x0, #136]
    stp d14, d15, [x0, #152]

    mov x0, #0
    ret

// void longjmp(jmp_buf env, int val);
// x0 = env, x1 = val
longjmp:
_longjmp:
    ldp x19, x20, [x0, #0]
    ldp x21, x22, [x0, #16]
    ldp x23, x24, [x0, #32]
    ldp x25, x26, [x0, #48]
    ldp x27, x28, [x0, #64]
    ldp x29, x30, [x0, #80]
    ldr x2, [x0, #96]
    mov sp, x2
    
    ldp d8, d9, [x0, #104]
    ldp d10, d11, [x0, #120]
    ldp d12, d13, [x0, #136]
    ldp d14, d15, [x0, #152]

    cmp x1, #0
    cinc x0, x1, eq  // if val==0, return 1
    ret
