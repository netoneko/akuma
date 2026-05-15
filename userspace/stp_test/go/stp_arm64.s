#include "textflag.h"

// func stpXzrXzr(dst uintptr)
// Emits: stp xzr, xzr, [x0]  (0xa9007c1f)
TEXT ·stpXzrXzr(SB),NOSPLIT,$0-8
    MOVD dst+0(FP), R0
    STP (ZR, ZR), (R0)
    RET

// func stpXzrXzrOff16(dst uintptr)
// Emits: stp xzr, xzr, [x0, #16]  (0xa9017c1f)
TEXT ·stpXzrXzrOff16(SB),NOSPLIT,$0-8
    MOVD dst+0(FP), R0
    STP (ZR, ZR), 16(R0)
    RET

// func stpXzrXzrOff32(dst uintptr)
// Emits: stp xzr, xzr, [x0, #32]  (0xa9027c1f)
TEXT ·stpXzrXzrOff32(SB),NOSPLIT,$0-8
    MOVD dst+0(FP), R0
    STP (ZR, ZR), 32(R0)
    RET
