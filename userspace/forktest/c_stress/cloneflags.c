// cloneflags — which clone(2) flag words make a thread, and which are EINVAL.
//
// Every line is `name result`, for diffing against Linux (the oracle; run this
// static musl binary there first). A thread case prints `ran` when the child
// stored its marker in the shared address space and exited.
//
//   go_threads         CLONE_VM|FS|FILES|SIGHAND|SYSVSEM|THREAD and no
//                      CLONE_SETTLS: the flag word Go's runtime passes (it sets
//                      TLS itself). Linux runs it.
//   musl_threads       the same plus CLONE_SETTLS|PARENT_SETTID|CHILD_CLEARTID,
//                      what pthread_create passes, with the parent's TLS pointer.
//   thread_no_sighand  CLONE_VM|CLONE_THREAD: EINVAL.
//   sighand_no_vm      CLONE_SIGHAND alone: EINVAL.
//
// The call is a raw syscall, not musl's clone(): musl refuses CLONE_VM without
// CLONE_VFORK itself, with EINVAL, before the kernel sees it. The child runs
// only the few instructions in raw_clone (store the marker, exit), so it needs
// no TLS and never returns into C.
//
// `docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`, "Slice 6: threads".

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define CLONE_VM             0x00000100
#define CLONE_FS             0x00000200
#define CLONE_FILES          0x00000400
#define CLONE_SIGHAND        0x00000800
#define CLONE_THREAD         0x00010000
#define CLONE_SYSVSEM        0x00040000
#define CLONE_SETTLS         0x00080000
#define CLONE_PARENT_SETTID  0x00100000
#define CLONE_CHILD_CLEARTID 0x00200000

volatile int marker;

// long raw_clone(flags, stack, ptid, ctid, tls, val): the child stores `val`
// into `marker` and exits; the parent gets the tid or -errno.
long raw_clone(long flags, void *stack, int *ptid, int *ctid, void *tls, long val);
#if defined(__x86_64__)
__asm__(
    ".text\n.globl raw_clone\nraw_clone:\n"
    "  mov %rcx, %r10\n"          // ctid is the 4th syscall argument
    "  mov $56, %eax\n"           // clone(flags, stack, ptid, ctid, tls)
    "  syscall\n"
    "  test %rax, %rax\n"
    "  jnz 1f\n"
    "  mov %r9d, marker(%rip)\n"  // r9 survives `syscall`
    "  mov $60, %eax\n"           // exit(0): this thread only
    "  xor %edi, %edi\n"
    "  syscall\n"
    "  hlt\n"
    "1: ret\n");
#elif defined(__aarch64__)
__asm__(
    ".text\n.globl raw_clone\nraw_clone:\n"
    "  mov x6, x3\n"              // clone(flags, stack, ptid, tls, ctid)
    "  mov x3, x4\n"
    "  mov x4, x6\n"
    "  mov x8, #220\n"
    "  svc #0\n"
    "  cbnz x0, 1f\n"
    "  adrp x9, marker\n"
    "  str w5, [x9, :lo12:marker]\n"
    "  mov x8, #93\n"             // exit(0): this thread only
    "  mov x0, #0\n"
    "  svc #0\n"
    "1: ret\n");
#else
#error "x86_64 or aarch64"
#endif

static void run(const char *name, long flags, void *stack, void *tls, int want) {
    static int ptid, ctid;
    marker = 0;
    long r = raw_clone(flags, stack, &ptid, &ctid, tls, want);
    if (r < 0) {
        printf("%-18s -%ld (%s)\n", name, -r, strerror((int)-r));
        return;
    }
    struct timespec ts = {0, 1000000};
    for (int i = 0; i < 2000 && marker != want; i++) nanosleep(&ts, 0);
    printf("%-18s %s\n", name, marker == want ? "ran" : "child did not run");
    // Let the child leave its stack before the next case reuses it.
    ts.tv_nsec = 50000000;
    nanosleep(&ts, 0);
}

int main(void) {
    const long base = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_SYSVSEM | CLONE_THREAD;
    char *stack = malloc(1 << 16);
    void *top = stack + (1 << 16) - 64;
    void *tp;
#if defined(__x86_64__)
    __asm__("mov %%fs:0, %0" : "=r"(tp));
#else
    __asm__("mrs %0, tpidr_el0" : "=r"(tp));
#endif
    run("go_threads", base, top, 0, 1);
    run("musl_threads", base | CLONE_SETTLS | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID, top, tp, 2);
    run("thread_no_sighand", CLONE_VM | CLONE_THREAD, top, 0, 3);
    run("sighand_no_vm", CLONE_SIGHAND, top, 0, 4);
    return 0;
}
