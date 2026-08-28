#!/usr/bin/env python3
"""Generate rodatacheck.c — 256 KiB of .rodata whose word i is a pure function of i.

The generated file is ~800 KB of initializers, which is why the GENERATOR is in
the tree and the output is not. Verifies, as the first act of main(), that a
lazily-faulted file-backed .rodata page reads back correctly on FIRST touch.

Written for docs/archive/BUSYBOX_HASH_MISCOMPUTE.md, where it ruled out md5's
constant table being faulted in wrong (80 fresh execs, 0 mismatches). Kept
because that hypothesis is the natural one for any "wrong constants" symptom.

  ./gen_rodatacheck.py > rodatacheck.c
  aarch64-linux-musl-gcc -static -O2 -o rodatacheck rodatacheck.c
"""
N = 65536
print('#include <stdint.h>\n#include <stdio.h>\n')
print('static uint32_t expect(uint32_t i) { return i * 2654435761u + 12345u; }\n')
print('static const uint32_t g_table[%d] = {' % N)
vals = ["%uu" % ((i * 2654435761 + 12345) & 0xFFFFFFFF) for i in range(N)]
for k in range(0, N, 8):
    print("    " + ",".join(vals[k:k+8]) + ",")
print('};\n')
print('''int main(void)
{
    long bad = 0;
    uint32_t first_i = 0, first_want = 0, first_got = 0;
    for (uint32_t i = 0; i < (uint32_t)(sizeof g_table / sizeof g_table[0]); i++) {
        uint32_t got = g_table[i];
        if (got != expect(i)) {
            if (!bad) { first_i = i; first_want = expect(i); first_got = got; }
            bad++;
        }
    }
    if (bad) {
        printf("RODATA WRONG: %ld word(s); first at index %u (byte %zu)\\\\n",
               bad, first_i, (size_t)first_i * 4);
        printf("  want %#x got %#x\\\\n", first_want, first_got);
        printf("RESULT: FAIL\\\\n");
        return 1;
    }
    printf("RESULT: PASS\\\\n");
    return 0;
}''')
