/*
 * execenv — what a child actually receives across `execve`, `posix_spawn` and
 * `posix_spawnp`: its environment, its `PATH`, its cwd, and what errno a failed
 * PATH search reports.
 *
 * # Why this probe exists
 *
 * Every compiler driver decides what to run from `PATH` and then reports what
 * happened through errno. On Akuma/amd64 `rustc hello.rs` fails with
 * `could not exec the linker \`cc\`: Function not implemented (os error 38)` —
 * an errno that names the kernel for what is, on the face of it, a missing
 * file. Three separate things could produce that and they need separating by
 * measurement rather than by argument:
 *
 *   1. the environment never reaches the child, so `PATH` is empty and the
 *      search looks in the wrong places (measured over ssh: a spawned child
 *      sees only `SHLVL` and `PWD`);
 *   2. the search runs correctly and a failed `execve` reports the wrong errno
 *      — and musl's `__execvpe` **abandons the search** on any errno that is
 *      not `ENOENT`/`ENOTDIR`/`EACCES`, so one wrong answer from one directory
 *      ends the whole lookup with that errno;
 *   3. the spawn itself fails before any exec.
 *
 * Each case below isolates one of those. Runs as an ordinary program — no
 * `init=`, no network — so it works over ssh in the Firecracker guest.
 *
 * The probe re-execs *itself* with `argv[1] == "child"` rather than shipping a
 * second binary: what a child receives is the measurement, so the child has to
 * be something whose expectations are written down here.
 *
 * Build: x86_64-linux-musl-gcc -static -O2 -o x86_64/execenv execenv.c
 *        aarch64-linux-musl-gcc -static -O2 -o aarch64/execenv execenv.c
 */
#include <errno.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static char self[512];

static int envc(char **e) {
    int n = 0;
    while (e && *e) { n++; e++; }
    return n;
}

/* ---- child mode: report exactly what arrived ---------------------------- */
static int child_report(void) {
    char cwd[256];
    const char *path = getenv("PATH");
    const char *mark = getenv("EXECENV_MARK");
    if (!getcwd(cwd, sizeof cwd)) strcpy(cwd, "(getcwd failed)");
    printf("      child: envc=%d MARK=%s PATH=%s cwd=%s\n",
           envc(environ),
           mark ? mark : "(unset)",
           path ? path : "(unset)",
           cwd);
    fflush(stdout);
    return 0;
}

static void reap(pid_t pid) {
    int st = 0;
    if (pid > 0) waitpid(pid, &st, 0);
}

/* ---- cases -------------------------------------------------------------- */

static void case_execve_env(void) {
    char *av[] = { self, "child", 0 };
    char *ev[] = { "EXECENV_MARK=execve", "PATH=/execve/path", "HOME=/root", 0 };
    printf("1. execve carries envp (3 entries)\n");
    pid_t pid = fork();
    if (pid == 0) {
        execve(self, av, ev);
        printf("      child: execve failed: %s\n", strerror(errno));
        _exit(1);
    }
    reap(pid);
}

static void case_spawn_env(void) {
    char *av[] = { self, "child", 0 };
    char *ev[] = { "EXECENV_MARK=posix_spawn", "PATH=/spawn/path", 0 };
    pid_t pid = -1;
    printf("2. posix_spawn carries envp (2 entries)\n");
    int r = posix_spawn(&pid, self, 0, 0, av, ev);
    if (r) printf("      posix_spawn -> %s\n", strerror(r));
    else reap(pid);
}

static void case_spawnp_found(const char *dir) {
    char pathvar[512];
    snprintf(pathvar, sizeof pathvar, "PATH=%s", dir);
    char *ev[] = { "EXECENV_MARK=spawnp", pathvar, 0 };
    char *av[] = { "execenv", "child", 0 };
    pid_t pid = -1;
    printf("3. posix_spawnp finds it through the *passed* PATH (%s)\n", dir);
    int r = posix_spawnp(&pid, "execenv", 0, 0, av, ev);
    if (r) printf("      posix_spawnp -> %d (%s)\n", r, strerror(r));
    else reap(pid);
}

/* The rustc shape: a name that is in none of the PATH directories, and one of
 * those directories does not exist at all. Linux answers ENOENT. Anything else
 * makes musl abandon the search and report that errno instead. */
static void case_spawnp_missing(void) {
    char *ev[] = {
        "PATH=/usr/local/rust/lib/rustlib/x86_64-unknown-linux-musl/bin:"
        "/usr/local/rust/lib/rustlib/x86_64-unknown-linux-musl/bin/self-contained",
        0
    };
    char *av[] = { "cc", "--version", 0 };
    pid_t pid = -1;
    printf("4. posix_spawnp of an absent name, rustc's exact PATH\n");
    int r = posix_spawnp(&pid, "cc", 0, 0, av, ev);
    printf("      posix_spawnp -> %d (%s)   [Linux: 2 (No such file or directory)]\n",
           r, r ? strerror(r) : "ok — it found one");
    if (!r) reap(pid);
}

static void case_execvp_missing(void) {
    printf("5. fork + execvp of an absent name, PATH of one missing dir\n");
    pid_t pid = fork();
    if (pid == 0) {
        setenv("PATH", "/no/such/directory", 1);
        char *av[] = { "zzz-absent", 0 };
        execvp("zzz-absent", av);
        printf("      child: execvp -> %d (%s)   [Linux: 2 (No such file or directory)]\n",
               errno, strerror(errno));
        _exit(0);
    }
    reap(pid);
}

static volatile sig_atomic_t alarms;
static void on_alrm(int sig) { (void)sig; alarms++; }

static void case_wait_eintr(int restart) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alrm;
    sa.sa_flags = restart ? SA_RESTART : 0;
    sigaction(SIGALRM, &sa, 0);

    char *av[] = { self, "sleep", 0 };
    pid_t pid = -1;
    printf("%d. waitpid across a signal, SA_RESTART=%s\n", restart ? 7 : 6,
           restart ? "yes" : "no");
    if (posix_spawn(&pid, self, 0, 0, av, environ)) {
        printf("      spawn failed: %s\n", strerror(errno));
        return;
    }
    alarms = 0;
    alarm(1);
    int st = 0;
    errno = 0;
    pid_t got = waitpid(pid, &st, 0);
    int e = errno;
    printf("      waitpid -> %ld errno=%d (%s) alarms=%d   [Linux: %s]\n",
           (long)got, got < 0 ? e : 0, got < 0 ? strerror(e) : "-", (int)alarms,
           restart ? "restarts, returns the pid" : "-1 EINTR");
    if (got < 0) reap(pid);
    alarm(0);
}

static void case_spawn_cwd(void) {
    char *av[] = { self, "child", 0 };
    char *ev[] = { "EXECENV_MARK=cwd", 0 };
    pid_t pid = -1;
    printf("8. a spawned child inherits the spawner's cwd\n");
    if (chdir("/tmp") != 0) {
        printf("      chdir(/tmp) failed: %s\n", strerror(errno));
        return;
    }
    printf("      parent cwd=/tmp\n");
    int r = posix_spawn(&pid, self, 0, 0, av, ev);
    if (r) printf("      posix_spawn -> %s\n", strerror(r));
    else reap(pid);
    if (chdir("/") != 0) { /* best effort */ }
}

/* What Rust `std` actually does, which is none of the above.
 *
 * `Command::spawn` will not use `posix_spawnp` when the command overrides
 * `PATH` — `posix_spawnp` searches the *caller's* `PATH`, not the one being
 * handed to the child, so it would look in the wrong place (case 3 above
 * measures exactly that, and case 4 is why rustc's failure is not musl's
 * spawn). It forks instead, installs the child's environment over `environ`,
 * and calls `execvp`, whose search then reads `PATH` from the environment just
 * installed. This is that sequence, with rustc's own two directories.
 */
static void case_rust_std_shape(void) {
    printf("9. fork + install envp over environ + execvp (what Rust std does)\n");
    pid_t pid = fork();
    if (pid == 0) {
        static char *ev[] = {
            "PATH=/usr/local/rust/lib/rustlib/x86_64-unknown-linux-musl/bin:"
            "/usr/local/rust/lib/rustlib/x86_64-unknown-linux-musl/bin/self-contained",
            0
        };
        environ = ev;
        char *av[] = { "cc", "--version", 0 };
        execvp("cc", av);
        printf("      child: execvp -> %d (%s)   [Linux: 2 (No such file or directory)]\n",
               errno, strerror(errno));
        _exit(0);
    }
    reap(pid);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "child") == 0) return child_report();
    if (argc > 1 && strcmp(argv[1], "sleep") == 0) { sleep(3); return 0; }

    /* Absolute path to ourselves: `argv[0]` may be a bare name, and there is no
     * /proc/self/exe on this target to fall back to. */
    if (argv[0][0] == '/') {
        snprintf(self, sizeof self, "%s", argv[0]);
    } else {
        char cwd[256];
        if (!getcwd(cwd, sizeof cwd)) strcpy(cwd, ".");
        snprintf(self, sizeof self, "%s/%s", cwd, argv[0]);
    }

    printf("execenv: self=%s\n", self);
    printf("0. what *this* process received: envc=%d PATH=%s\n",
           envc(environ), getenv("PATH") ? getenv("PATH") : "(unset)");

    char dir[512];
    snprintf(dir, sizeof dir, "%s", self);
    char *slash = strrchr(dir, '/');
    if (slash) *slash = 0;

    case_execve_env();
    case_spawn_env();
    case_spawnp_found(dir);
    case_spawnp_missing();
    case_execvp_missing();
    case_wait_eintr(0);
    case_wait_eintr(1);
    case_spawn_cwd();
    case_rust_std_shape();
    printf("execenv: done\n");
    return 0;
}
