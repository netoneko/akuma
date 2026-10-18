/*
 * run_all — run a list of probes as `init`, with banners, on a guest that has
 * no network and no working shell scripting.
 *
 * # Why this exists rather than a shell script
 *
 * The obvious way to run ten probes in one boot is `init=/bin/busybox
 * initargs=sh,/probes/run.sh`. Two things on this target stop that, and both
 * are silent:
 *
 * 1. **`sh <script>` cannot spawn anything.** Running a *script file* — as
 *    opposed to `sh -c <one command>`, which busybox ash execs in place without
 *    forking — fails at the first line that runs a program, with
 *    `sh: <line>: Invalid argument`. Nothing else is printed, so the run looks
 *    like it did not happen.
 * 2. **The init shell's own stdout goes nowhere.** `echo` from inside such a
 *    script produces no console output at all, while its *errors* appear — so
 *    the banners a harness splits the log on would be missing even if the
 *    probes ran.
 *
 * Both are shell/fd-layer gaps rather than memory ones and are recorded in
 * `docs/runbooks/amd64-bare-metal-loop.md`. This program routes around them
 * with the three things that demonstrably do work here: `fork`, `execve` and
 * `wait4`, all of which the boot self-test suite exercises.
 *
 * # Usage
 *
 *   init=/probes/run_all initargs=mmap_stress,mmapsum:/probes/mem_suite_data,…
 *
 * The kernel command line is split on whitespace, so `initargs=` is **one
 * token** and no argument may contain a space — that is why the per-probe
 * argument is attached with a colon rather than passed as a separate word. A
 * `name:arg` element runs `/probes/name arg`; a bare `name` runs it with none.
 *
 * Output is framed so a harness can split it even at SMP>1, where cores
 * interleave console writes: each marker is one short line of its own, so a torn
 * line damages a probe's output rather than the framing that finds it.
 *
 * Build (any x86_64 static toolchain):
 *   x86_64-linux-musl-gcc -O2 -static -o run_all run_all.c
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define PROBE_DIR "/probes/"

/* Written with `write(2)` on fd 1 rather than printf, and flushed by
 * construction: a marker that is still sitting in a stdio buffer when a probe
 * SIGSEGVs — which several probes do on purpose — is a marker the harness never
 * sees, and the run then reads as "stopped before this probe". */
static void say(const char *a, const char *b, const char *c)
{
    char buf[256];
    size_t n = 0;
    const char *parts[3];
    parts[0] = a;
    parts[1] = b;
    parts[2] = c;
    for (int i = 0; i < 3; i++) {
        if (!parts[i]) {
            continue;
        }
        size_t len = strlen(parts[i]);
        if (n + len >= sizeof(buf) - 2) {
            break;
        }
        memcpy(buf + n, parts[i], len);
        n += len;
    }
    buf[n++] = '\n';
    (void)!write(1, buf, n);
}

static void say_rc(const char *name, int status)
{
    char rc[16];
    int v;
    if (WIFEXITED(status)) {
        v = WEXITSTATUS(status);
    } else if (WIFSIGNALED(status)) {
        /* Reported the way a shell would, so a harness sees one number whether
         * the guest can produce a signalled status or not. This target cannot
         * — a killed process exits `128 + signal` — but the aarch64 one can, and
         * the marker should read the same on both. */
        v = 128 + WTERMSIG(status);
    } else {
        v = -1;
    }
    snprintf(rc, sizeof(rc), "%d", v);
    say("=== END ", name, NULL);
    say("=== RC ", rc, " ===");
}

int main(int argc, char **argv)
{
    for (int i = 1; i < argc; i++) {
        char spec[128];
        snprintf(spec, sizeof(spec), "%s", argv[i]);
        char *arg = strchr(spec, ':');
        if (arg) {
            *arg++ = '\0';
        }

        char path[192];
        snprintf(path, sizeof(path), "%s%s", PROBE_DIR, spec);

        say("=== PROBE ", spec, " ===");
        pid_t pid = fork();
        if (pid < 0) {
            say("=== FORK FAILED ", spec, " ===");
            say_rc(spec, -1);
            continue;
        }
        if (pid == 0) {
            char *av[3];
            av[0] = path;
            av[1] = arg;
            av[2] = NULL;
            execve(path, av, (char *[]){NULL});
            /* Only reachable if execve failed; say so rather than letting the
             * child fall through into the parent's loop and run every remaining
             * probe a second time. */
            say("=== EXEC FAILED ", spec, " ===");
            _exit(127);
        }
        int status = 0;
        while (waitpid(pid, &status, 0) < 0) {
            /* Retry: an interrupted wait must not be read as "the probe is
             * gone", which would leave the child unreaped and the next probe
             * racing it. */
        }
        say_rc(spec, status);
    }
    say("=== PROBES DONE ===", NULL, NULL);
    return 0;
}
