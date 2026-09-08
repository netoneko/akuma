/* grandfork — does a forked process forking AGAIN wedge this kernel?
 *
 * Background: on amd64, `( ls /bin; ls /bin )` over ssh hangs and poisons the
 * kernel — afterwards sshd still accepts connections and still runs commands
 * (their output arrives in full), but no session ever tears down, and on the
 * bare-metal box the machine needed a power cycle. `( ls /bin )` is fine, a
 * plain `ls | wc -l` pipe is fine, and `( echo a )` is fine
 * (`docs/archive/AKUMA_SELF_HOSTING_AMD64.md` issue 5).
 *
 * The shell explains why those three differ: ash execs the LAST command of a
 * subshell in place, and forks for anything earlier. So `( ls )` never creates
 * a grandchild and `( ls; ls )` does — confirmed directly, `( true; ls )`
 * passes and `( ls; true )` hangs. This probe removes the shell from the
 * picture and asks the kernel the question in four rungs.
 *
 * **Every step is announced BEFORE it runs, through write(2) rather than
 * stdio.** The failure mode is a hang, not a wrong answer: there is no exit
 * status to read and a buffered line is a line you never see, so the last
 * thing printed has to name the operation that did not return.
 *
 * The rungs separate three things a "grandchild fork" bundles together:
 *
 *   1 control      fork + _exit + wait, at the top level          (known good)
 *   2 gfork_nowait a forked child forks, and does NOT wait        (the 2nd fork alone)
 *   3 gfork_wait   a forked child forks and waits for it          (+ wait4 in a child)
 *   4 gfork_exec   the grandchild execs before exiting            (+ exec in a grandchild)
 *   5 gfork_echild after reaping, the child's wait4(-1) must ECHILD (the bug)
 *
 * Statically linked musl, so the same binary runs on real Linux for an A/B —
 * every rung must pass there, and if one does not, the probe is wrong rather
 * than the kernel (`docs/archive/LINUX_AB_PROBE_TECHNIQUE.md`).
 *
 * Exits 0 on success, or the rung number that failed. A hang is the interesting
 * outcome and has no exit code at all — read the last line printed.
 */
#include <sys/wait.h>
#include <errno.h>
#include <string.h>
#include <unistd.h>

static void say(const char *s)
{
    (void)write(1, s, strlen(s));
}

/* wait for exactly `pid`; returns its exit status, or -1 on any other outcome */
static int wait_status(pid_t pid)
{
    int st = 0;
    if (waitpid(pid, &st, 0) != pid)
        return -1;
    if (!WIFEXITED(st))
        return -1;
    return WEXITSTATUS(st);
}

int main(void)
{
    pid_t child;

    say("step 1 control: fork + _exit + wait\n");
    child = fork();
    if (child == 0)
        _exit(3);
    if (child < 0 || wait_status(child) != 3) {
        say("step 1 FAILED\n");
        return 1;
    }
    say("  step 1 ok\n");

    say("step 2 gfork_nowait: a forked child forks and does NOT wait\n");
    child = fork();
    if (child == 0) {
        pid_t g = fork();          /* <- the second fork, in a forked process */
        if (g == 0)
            _exit(0);
        _exit(g < 0 ? 1 : 4);      /* leaves the grandchild to be reparented */
    }
    if (child < 0 || wait_status(child) != 4) {
        say("step 2 FAILED\n");
        return 2;
    }
    say("  step 2 ok\n");

    say("step 3 gfork_wait: a forked child forks and waits for the grandchild\n");
    child = fork();
    if (child == 0) {
        pid_t g = fork();
        if (g == 0)
            _exit(7);
        _exit(wait_status(g) == 7 ? 5 : 1);
    }
    if (child < 0 || wait_status(child) != 5) {
        say("step 3 FAILED\n");
        return 3;
    }
    say("  step 3 ok\n");

    say("step 4 gfork_exec: the grandchild execs, the child waits\n");
    child = fork();
    if (child == 0) {
        pid_t g = fork();
        if (g == 0) {
            /* `true` is a busybox applet on both guests; argv[0] selects it. */
            char *const argv[] = { "true", 0 };
            execv("/bin/busybox", argv);
            _exit(9);              /* exec failed */
        }
        _exit(wait_status(g) == 0 ? 6 : 1);
    }
    if (child < 0 || wait_status(child) != 6) {
        say("step 4 FAILED\n");
        return 4;
    }
    say("  step 4 ok\n");

    /* Rung 5 is the one that fails on a pre-fix kernel, and it fails by
     * HANGING rather than by returning a wrong answer — which is why every step
     * announces itself first. The spawn table is global, so a `wait4(-1)` that
     * does not filter by parent sees rows that are not this process's children,
     * including this process's OWN row. A forked child that has just reaped its
     * only grandchild then gets "a child exists, none has exited" forever.
     *
     * It has to run in a forked child: the top-level process here is init's
     * child, and only a process that is itself a table row can see itself. That
     * is also why the boot suite cannot pin this half — it runs as init. */
    say("step 5 gfork_echild: after reaping, a forked child's wait4(-1) must ECHILD\n");
    child = fork();
    if (child == 0) {
        pid_t g = fork();
        if (g == 0)
            _exit(0);
        if (wait_status(g) != 0)
            _exit(1);
        /* No children left. This must fail with ECHILD, not block. */
        if (wait(0) != -1)
            _exit(2);
        _exit(errno == ECHILD ? 8 : 3);
    }
    if (child < 0 || wait_status(child) != 8) {
        say("step 5 FAILED\n");
        return 5;
    }
    say("  step 5 ok\n");

    say("grandfork: ALL PASS\n");
    return 0;
}
