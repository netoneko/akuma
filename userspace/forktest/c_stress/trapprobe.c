/* trapprobe — does a ring-3 CPU exception other than #PF/#GP/#UD kill only the
 * program that took it?
 *
 * Background (2026-09-25): on amd64, `#DE`, `#NP`, `#SS`, `#AC`, `#MF` and
 * `#XM` were `x86-interrupt` stubs that went straight to the kernel's `fatal`,
 * so a *program* taking one halted every core. That was photographed on the HP
 * box: kot's musl `free` read a garbage `meta` pointer out of a corrupted heap
 * group and executed `cmp rcx, [rbp+0x10]` with it. A non-canonical address
 * with `rbp`/`rsp` as the base register is `#SS`, not `#GP`, and the whole
 * machine went down for one process's heap bug.
 *
 * Linux (`arch/x86/kernel/traps.c`) turns `#DE` into SIGFPE/FPE_INTDIV and
 * `#SS` into SIGBUS/SI_KERNEL. The rungs pin both halves of that, for the two
 * vectors a program can raise at will:
 *
 *   1 de-caught    idiv by zero reaches a SIGFPE handler, si_code FPE_INTDIV,
 *                  escaped by siglongjmp
 *   2 ss-caught    a load through a non-canonical rbp reaches a SIGBUS handler
 *                  (kot's exact instruction shape), escaped by siglongjmp
 *   3 de-default   with no handler, a child dies WIFSIGNALED/WTERMSIG==SIGFPE
 *   4 ss-default   with no handler, a child dies WIFSIGNALED/WTERMSIG==SIGBUS
 *
 * Rungs 3 and 4 are the ones the photograph failed: before the fix the machine
 * stopped, so the probe's last line is the evidence, and a parent that prints
 * `trapprobe: OK` is a kernel that survived.
 *
 * **QEMU TCG cannot run rungs 2 and 4 as written.** Its x86 emulation raises
 * `#GP` for a non-canonical stack-segment reference, not `#SS`, so under TCG the
 * instruction arrives as SIGSEGV and the kernel's `#SS` path never runs. Real
 * silicon (the metal box, KVM on the Ryzen) raises `#SS`. Those rungs therefore
 * accept SIGSEGV too, and say `NOT EXERCISED` when that is what arrived, so a
 * green TCG run is not mistaken for a proof of the `#SS` path. Only a run under
 * KVM or on the metal proves it.
 *
 * Every step is announced before it runs, through write(2): the failure mode
 * being pinned is a machine that stops, so the last line printed names the
 * instruction that stopped it.
 *
 * Statically linked musl, so the same binary runs on real Linux for an A/B —
 * every rung must pass there, and if one does not, the probe is wrong rather
 * than the kernel (the same A/B rule `sigprobe.c` follows).
 *
 * Exits 0 on success, or the rung number that failed.
 */
#define _GNU_SOURCE
#include <setjmp.h>
#include <signal.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void say(const char *s)
{
    (void)write(1, s, strlen(s));
}

static sigjmp_buf escape;
static volatile sig_atomic_t got_sig;
static volatile sig_atomic_t got_code;

static void on_fault(int sig, siginfo_t *info, void *uc)
{
    (void)uc;
    got_sig = sig;
    got_code = info->si_code;
    siglongjmp(escape, 1);
}

/* `idiv` by a zero the compiler cannot see: `#DE`. */
static void raise_de(void)
{
    long zero = 0;
    __asm__ volatile(
        "mov $1, %%rax\n\t"
        "cqo\n\t"
        "idivq %0\n\t"
        : : "r"(zero) : "rax", "rdx", "cc");
}

/* A load through a non-canonical `rbp`: `#SS`, because `rbp` is the base.
 * `cmp rcx, [rbp+0x10]` is the instruction kot faulted on, byte for byte in
 * shape; the value is the kind a corrupted mallocng group header yields. */
static void raise_ss(void)
{
    __asm__ volatile(
        "push %%rbp\n\t"
        "movabs $0x8badf00ddeadbeef, %%rbp\n\t"
        "cmp 0x10(%%rbp), %%rcx\n\t"
        "pop %%rbp\n\t"
        : : : "rcx", "cc", "memory");
}

/* Say so when an emulator delivered #GP's SIGSEGV where silicon raises #SS. */
static void note_emulated(int sig, int got)
{
    if (sig == SIGBUS && got == SIGSEGV)
        say("  NOT EXERCISED: SIGSEGV arrived, i.e. the CPU raised #GP, not #SS "
            "(QEMU TCG does this; real silicon does not)\n");
}

static int caught(int rung, int sig, int alt, int want_code, void (*raise_it)(void))
{
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_fault;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    if (sigaction(sig, &sa, 0) != 0)
        return rung;
    if (alt && sigaction(alt, &sa, 0) != 0)
        return rung;
    got_sig = 0;
    got_code = -1;
    if (sigsetjmp(escape, 1) == 0) {
        raise_it();
        say("  (the instruction did not fault)\n");
        return rung;
    }
    signal(sig, SIG_DFL);
    if (alt)
        signal(alt, SIG_DFL);
    if (alt && got_sig == alt) {
        note_emulated(sig, got_sig);
        return 0;
    }
    if (got_sig != sig)
        return rung;
    if (want_code >= 0 && got_code != want_code)
        return rung;
    return 0;
}

static int dies_of(int rung, int sig, int alt, void (*raise_it)(void))
{
    pid_t pid = fork();
    if (pid < 0)
        return rung;
    if (pid == 0) {
        signal(sig, SIG_DFL);
        raise_it();
        _exit(100);
    }
    int st = 0;
    if (waitpid(pid, &st, 0) != pid)
        return rung;
    if (!WIFSIGNALED(st))
        return rung;
    if (alt && WTERMSIG(st) == alt) {
        note_emulated(sig, alt);
        return 0;
    }
    if (WTERMSIG(st) != sig)
        return rung;
    return 0;
}

int main(void)
{
    int r;
    say("trapprobe: 1 de-caught (idiv 0 -> SIGFPE/FPE_INTDIV)\n");
    if ((r = caught(1, SIGFPE, 0, FPE_INTDIV, raise_de)))
        return r;
    say("trapprobe: 2 ss-caught (non-canonical rbp -> SIGBUS)\n");
    if ((r = caught(2, SIGBUS, SIGSEGV, -1, raise_ss)))
        return r;
    say("trapprobe: 3 de-default (child dies of SIGFPE)\n");
    if ((r = dies_of(3, SIGFPE, 0, raise_de)))
        return r;
    say("trapprobe: 4 ss-default (child dies of SIGBUS)\n");
    if ((r = dies_of(4, SIGBUS, SIGSEGV, raise_ss)))
        return r;
    say("trapprobe: OK\n");
    return 0;
}
