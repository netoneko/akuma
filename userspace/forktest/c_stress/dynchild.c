/* dynchild — the spawnee for `dynspawn`. Deliberately DYNAMICALLY linked, so
 * running it exercises the whole ld-musl startup path: self-relocation
 * (R_AARCH64_RELATIVE), library mapping, and PLT/GOT resolution.
 *
 * Exits 42 on success. Any other outcome — a signal, a different status — means
 * the loader did not get the process to `main` correctly. */
#include <string.h>
#include <stdlib.h>

/* A relocated pointer: its initializer needs an R_AARCH64_RELATIVE fixup, so if
 * the loader applies relocations twice this no longer points at `msg`. */
static const char msg[] = "akuma-dynchild";
static const char *const relocated = msg;

int main(void)
{
    if (relocated != msg)
        return 10; /* relocation applied wrong number of times */
    if (strlen(relocated) != 14)
        return 11; /* PLT call went somewhere unexpected */
    return 42;
}
