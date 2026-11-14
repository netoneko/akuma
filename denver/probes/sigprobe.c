/* denver/probes/sigprobe.c - PHASE-3-JAVAC arm 1 (D75): does the kernel deliver a
 * SYNCHRONOUS fault (a ring-3 page fault it cannot resolve) to this process's own
 * SIGSEGV handler, with siginfo + a real ucontext, and does rt_sigreturn restore the
 * EDITED context (HotSpot's trap-and-handle is exactly this: patch uc_mcontext and
 * return)?
 *
 * The faulting site is hand-written asm of known length:
 *     fault_site:
 *       xor  %rax, %rax        ; +0  (3 bytes)  rax = 0
 *       mov  %rax, (%rax)      ; +3  (3 bytes)  write through NULL -> #PF, cr2=0
 *       ret                    ; +6  (1 byte)
 * The handler records si_signo/si_code/si_addr and the interrupted
 * rip/rsp/rax/cr2/err/fpstate read at the KERNEL's fixed offsets inside the ucontext
 * (uc_mcontext at byte 40; rip 40+128, rsp 40+120, rax 40+104, err 40+152, cr2 40+176,
 * fpstate 40+184 — the kernel ABI field order, verified against a real Linux host
 * (D76); no libc struct layout is trusted), then patches the saved
 * rip -> fault_site+6 (skip the dead instruction) and rax -> 99, and returns.
 * PASS needs: delivered (si_code=1 MAPERR, si_addr=0), ripdelta==3, cr2==0, AND
 * fault_site() returning 99 — the rax edit proves the edited ucontext is what
 * rt_sigreturn restored (the rip edit proves delivery itself: unpatched, the mov
 * re-executes and the kernel kills us).
 *
 * Expected on the unfeatured kernel: no output at all — SIGSEGV kills the task and the
 * console shows `[Fault] ... task killed`; the probe's silence plus that line is the
 * decisive FAIL datum.
 *
 * Build: gcc -static -O2 -o sigprobe sigprobe.c   (same recipe as jvmprobe.c)
 */
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>

extern unsigned char fault_site[];
asm(
    ".text\n"
    ".globl fault_site\n"
    "fault_site:\n"
    "  xor %rax, %rax\n"
    "  mov %rax, (%rax)\n"
    "  ret\n"
);

static volatile unsigned long g_rip, g_rsp, g_rax, g_cr2, g_err, g_fp;
static volatile int g_signo, g_code;
static volatile unsigned long g_addr;
static volatile int g_delivered;

static void handler(int signo, siginfo_t *info, void *ucp) {
    unsigned char *uc = (unsigned char *)ucp;
    /* The kernel's sigcontext lives at ucontext byte 40; the fields a handler
     * patches are read at the fixed ABI offsets (see header). */
    g_rip  = *(unsigned long *)(uc + 40 + 128);
    g_rsp  = *(unsigned long *)(uc + 40 + 120);
    g_rax  = *(unsigned long *)(uc + 40 + 104);
    g_err  = *(unsigned long *)(uc + 40 + 152);
    g_cr2  = *(unsigned long *)(uc + 40 + 176);
    g_fp   = *(unsigned long *)(uc + 40 + 184);
    g_signo = info->si_signo;
    g_code  = info->si_code;
    g_addr  = (unsigned long)info->si_addr;
    g_delivered = 1;
    /* Patch the saved context: resume at the `ret`, carrying rax = 99. */
    unsigned long site = (unsigned long)fault_site;
    *(unsigned long *)(uc + 40 + 128) = site + 6;
    *(unsigned long *)(uc + 40 + 104) = 99;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO;
    if (sigaction(SIGSEGV, &sa, NULL) != 0) {
        printf("SIGPROBE-FAIL sigaction errno=%d\n", errno);
        fflush(stdout);
        return 3;
    }
    printf("SIGPROBE-INFO handler installed\n");
    fflush(stdout);

    long rc = ((long (*)(void))fault_site)();
    if (!g_delivered) {
        printf("SIGPROBE-FAIL not delivered rc=%ld\n", rc);
        fflush(stdout);
        return 2;
    }
    unsigned long site = (unsigned long)fault_site;
    long ripdelta = (long)(g_rip - site);
    printf("SIGPROBE-DELIVERED si_signo=%d si_code=%d si_addr=%#lx ripdelta=%ld rsp=%#lx rax=%#lx cr2=%#lx err=%#lx fp=%#lx\n",
           g_signo, g_code, g_addr, ripdelta, g_rsp, g_rax, g_cr2, g_err, g_fp);
    int ok = g_signo == 11 && g_code == 1 && g_addr == 0 && ripdelta == 3
             && g_cr2 == 0 && rc == 99;
    if (ok) {
        printf("SIGPROVE rax-after=%ld (the edited ucontext is what ran)\n", rc);
        printf("SIGPROBE PASS\n");
    } else {
        printf("SIGPROBE FAIL rc=%ld (want 99) sig fields above\n", rc);
    }
    fflush(stdout);
    return ok ? 0 : 1;
}
