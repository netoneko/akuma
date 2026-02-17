.global _start

_start:
    mov x0, #0
    mov x1, #0
    bl main
    bl exit
    b .
