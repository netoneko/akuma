// thrchurn — create and join N threads one after another, optionally pausing
// between rounds, and report how many pthread_create calls failed.
//
//   thrchurn [rounds=1000] [pause_us=0] [batches=1]
//
// A thread that has exited still holds kernel resources until its row is
// reclaimed. With no pause, the rounds outrun that reclaim and measure how many
// threads may be in flight; with a pause, any failure that remains is a leak.
// Prints `thrchurn: R rounds x B batches, F failed` and PASS when F is 0.

#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static void *body(void *arg) { return arg; }

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 1000;
    long pause_us = argc > 2 ? atol(argv[2]) : 0;
    int batches = argc > 3 ? atoi(argv[3]) : 1;
    int failed = 0, first_err = 0, first_at = -1;
    for (int b = 0; b < batches; b++) {
        for (int i = 0; i < rounds; i++) {
            pthread_t t;
            int e = pthread_create(&t, NULL, body, NULL);
            if (e != 0) {
                if (!failed++) { first_err = e; first_at = b * rounds + i; }
            } else {
                pthread_join(t, NULL);
            }
            if (pause_us) {
                struct timespec ts = {pause_us / 1000000, (pause_us % 1000000) * 1000};
                nanosleep(&ts, NULL);
            }
        }
    }
    printf("thrchurn: %d rounds x %d batches, pause %ldus, %d failed", rounds, batches, pause_us, failed);
    if (failed) printf(" (first at %d: %s)", first_at, strerror(first_err));
    printf("\nthrchurn: %s\n", failed ? "FAIL" : "PASS");
    return failed != 0;
}
