/* auxvprobe.c — what the kernel puts on a new program's initial stack.
 *
 * Why: until 2026-11-12 the two kernels built that stack with two builders.
 * AArch64's (`akuma_elf::attach_stack`) wrote sixteen zero bytes as AT_RANDOM,
 * which musl and glibc seed the stack protector and pointer guard from, and
 * left AT_BASE out of a static image's vector; amd64's own builder had
 * neither problem but lacked AT_HWCAP2. Both run `attach_stack` now, and its
 * first run on amd64 entered programs with SP 8 bytes off whenever
 * argc + envc was odd (the alignment check below), a defect AArch64 shared.
 *
 * Checks (raw auxv walk past envp, not getauxval, so a missing key is seen):
 *   AT_RANDOM   present, 16 bytes, not all zero, and differs across two execs
 *   AT_BASE     present (0 for a static image, as on Linux)
 *   AT_PAGESZ   4096
 *   AT_SECURE   present and 0
 *   AT_PHDR/AT_PHNUM/AT_ENTRY present
 *   alignment   the initial sp (argc's address) is 16-byte aligned
 *
 * Build: <arch>-linux-musl-gcc -O2 -static -o auxvprobe auxvprobe.c
 * Run:   ./auxvprobe        (PASS/FAIL per case, exit 0 iff all pass)
 */
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static int fails;

static void check(const char *what, int ok) {
    printf("auxvprobe: %-26s %s\n", what, ok ? "PASS" : "FAIL");
    fails |= !ok;
}

/* The auxv follows envp's NULL. `environ` is still the kernel's array here:
 * nothing in main() has called setenv. */
static const uint64_t *auxv(void) {
    char **e = environ;
    while (*e) e++;
    return (const uint64_t *)(e + 1);
}

static int find(uint64_t key, uint64_t *val) {
    for (const uint64_t *a = auxv(); a[0]; a += 2)
        if (a[0] == key) { *val = a[1]; return 1; }
    return 0;
}

int main(int argc, char **argv) {
    uint64_t v = 0;
    if (argc > 1 && !strcmp(argv[1], "--random")) {
        /* Child mode: print our AT_RANDOM for the parent to compare. */
        if (!find(25, &v)) return 1;
        fwrite((const void *)(uintptr_t)v, 1, 16, stdout);
        return 0;
    }

    /* argv[0]'s address minus the argc word is the initial sp. */
    uintptr_t sp = (uintptr_t)argv - sizeof(long);
    check("initial sp 16-aligned", (sp & 15) == 0);

    int have = find(25, &v);
    const unsigned char *r = (const unsigned char *)(uintptr_t)v;
    int nonzero = 0;
    for (int i = 0; have && i < 16; i++) nonzero |= r[i];
    check("AT_RANDOM not all zero", have && nonzero);

    check("AT_BASE present", find(7, &v));
    check("AT_PAGESZ is 4096", find(6, &v) && v == 4096);
    check("AT_SECURE present, 0", find(23, &v) && v == 0);
    uint64_t e = 0, n = 0;
    check("AT_PHDR/PHNUM/ENTRY", find(3, &v) && find(5, &n) && find(9, &e) && v && n && e);

    /* Two execs of ourselves must not see the same AT_RANDOM. */
    unsigned char a[16] = {0}, b[16] = {0};
    for (int k = 0; k < 2; k++) {
        int p[2];
        if (pipe(p)) break;
        pid_t pid = fork();
        if (pid == 0) {
            dup2(p[1], 1);
            execl("/proc/self/exe", argv[0], "--random", (char *)0);
            execl(argv[0], argv[0], "--random", (char *)0);
            _exit(127);
        }
        close(p[1]);
        ssize_t got = read(p[0], k ? b : a, 16);
        close(p[0]);
        waitpid(pid, 0, 0);
        if (got != 16) { memset(a, 0, 16); memset(b, 0, 16); break; }
    }
    check("AT_RANDOM differs per exec", memcmp(a, b, 16) != 0);

    puts(fails ? "auxvprobe: FAIL" : "auxvprobe: all PASS");
    return fails;
}
