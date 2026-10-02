/* groupexit_thread — `exit_group` from a thread that is NOT the leader.
 *
 * A worker calls exit(3); the whole process must end promptly with status 3
 * whatever the leader is parked in (mode 0: pthread_join's futex, 1: read(pipe),
 * 2: epoll_wait(-1)) and with a sibling parked in pause(). amd64 once left the
 * leader where it was: `goose --version` printed its version and never exited,
 * because Rust runs `main` on a spawned thread and the original thread sits in
 * the join futex. Fixed by amd64/src/thread.rs `exit_group_from_thread`
 * (docs/archive/AKUMA_AMD64_AGENT_STAGING_AND_ACCOUNTING.md § 13).
 *
 * Build: x86_64-linux-musl-gcc -O1 -static -pthread -o groupexit_thread groupexit_thread.c
 * Run:   for m in 0 1 2; do timeout 10 ./groupexit_thread $m; echo "mode $m rc=$?"; done
 *        expect rc=3 three times; "BUG: leader returned" / rc=124 is the failure. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/epoll.h>
/* argv[1]: 0 = leader joins the thread (futex), 1 = leader parked in read(pipe), 2 = leader in epoll_wait(-1).
 * A worker calls exit(3) after a moment; the whole process must end with status 3. */
static int pfd[2];
static void *w(void *a) { (void)a; usleep(200000); exit(3); return 0; }
static void *sleeper(void *a) { (void)a; for (;;) pause(); return 0; }
int main(int argc, char **argv) {
    int mode = argc > 1 ? atoi(argv[1]) : 0;
    pthread_t t, s;
    pipe(pfd);
    pthread_create(&s, 0, sleeper, 0);   /* a sibling parked in pause() must not hold the exit up either */
    pthread_create(&t, 0, w, 0);
    if (mode == 0) pthread_join(t, 0);
    else if (mode == 1) { char c; read(pfd[0], &c, 1); }
    else { int ep = epoll_create1(0); struct epoll_event o[1]; epoll_wait(ep, o, 1, -1); }
    puts("BUG: leader returned");
    return 9;
}
