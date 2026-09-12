/* clockprobe — does this kernel have one clock, and does it move?
 *
 * Background: the amd64 kernel kept two. `amd64/src/clock.rs` held a private
 * `(anchor_unix, anchor_uptime)` pair that its own `gettimeofday`/`time`/
 * `clock_gettime` arms read, while `akuma-syscalls-time` — the implementation
 * the AArch64 kernel serves the same syscalls from — read `akuma_timer`'s,
 * which on x86 is CNTVCT and therefore `0` forever. Folding the family in
 * without merging the anchors would have handed ring 3 a clock nothing on the
 * target ever set. C3 merged them into `akuma_primitives::clock`
 * (`docs/archive/AKUMA_AMD64_C3_CLOCK.md`).
 *
 * That is why half the rungs here compare *two spellings against each other*
 * rather than against a known-good value: the failure this probe exists to
 * catch is two clocks that are each internally plausible and disagree. A
 * kernel with one clock passes those rungs whether or not it knows what year
 * it is, which is deliberate — an offline machine has no NTP and is not broken.
 *
 * **Every step is announced BEFORE it runs, through write(2) rather than
 * stdio.** Three rungs fail by hanging rather than by answering — an `alarm`
 * that never fires, a `pause` nothing interrupts, a `nanosleep` on a stopped
 * clock — so the last line printed has to name the operation that did not
 * return.
 *
 * The rungs, each isolating one thing the one before it does not need:
 *
 *   1 monotonic    CLOCK_MONOTONIC is non-zero and strictly increases
 *   2 agree        gettimeofday, time(2) and CLOCK_REALTIME are ONE clock
 *   3 getres       clock_getres answers for both clock ids
 *   4 nanosleep    a 300 ms sleep advances the monotonic clock by >= 250 ms
 *   5 einval       nanosleep rejects tv_nsec >= 1e9 and a negative tv_sec
 *   6 relsleep     clock_nanosleep, the spelling std::thread::sleep uses
 *   7 abssleep     ...with TIMER_ABSTIME against CLOCK_MONOTONIC
 *   8 alarm        alarm(1) + pause() reaches a SIGALRM handler
 *  8b posteintr    ...and the syscall AFTER the handler is not spuriously
 *                  EINTR, which is the defect rung 8 turned up
 *   9 alarmret     alarm(0) reports the seconds left on a pending alarm
 *  10 itimer       setitimer(ITIMER_REAL) fires, sub-second
 *  11 times        times()/getrusage() answer without an error
 *
 * It deliberately never calls `clock_settime`, `settimeofday` or `adjtimex`:
 * this binary is meant to be run on real Linux for an A/B
 * (`docs/archive/LINUX_AB_PROBE_TECHNIQUE.md`) and a probe that steps the
 * clock of the machine it is being validated on is a probe nobody runs twice.
 * The write half is checked from the boot suite instead.
 *
 * Statically linked musl, so the same binary runs on real Linux — every rung
 * must pass there, and if one does not, the probe is wrong rather than the
 * kernel.
 *
 * Exits 0 on success, or the rung number that failed.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/time.h>
#include <sys/times.h>
#include <time.h>
#include <unistd.h>

static void say(const char *s)
{
    (void)write(1, s, strlen(s));
}

static void sayn(const char *s, long long v)
{
    char buf[32];
    int i = 0;
    int neg = v < 0;
    unsigned long long u = neg ? (unsigned long long)(-v) : (unsigned long long)v;
    if (u == 0) {
        buf[i++] = '0';
    }
    while (u) {
        buf[i++] = (char)('0' + (u % 10));
        u /= 10;
    }
    if (neg) {
        buf[i++] = '-';
    }
    say(s);
    while (i--) {
        (void)write(1, &buf[i], 1);
    }
    say("\n");
}

/* Microseconds, the unit every comparison below is made in. */
static long long ts_us(const struct timespec *ts)
{
    return (long long)ts->tv_sec * 1000000 + ts->tv_nsec / 1000;
}

static volatile sig_atomic_t alarms;

static void on_alarm(int sig)
{
    (void)sig;
    alarms++;
}

int main(void)
{
    struct timespec a, b;

    say("=== clockprobe\n");

    /* 1. A monotonic clock that is non-zero and moves.
     *
     * The busy loop rather than a sleep: rung 4 is where sleeping is proved,
     * and a monotonic clock that only advances when something sleeps would
     * pass this rung by accident. `volatile` so the spin survives -O2. */
    say("1 monotonic\n");
    if (clock_gettime(CLOCK_MONOTONIC, &a) != 0) {
        return 1;
    }
    if (ts_us(&a) == 0) {
        say("  monotonic reads zero\n");
        return 1;
    }
    for (volatile long i = 0; i < 20000000; i++) {
        if (clock_gettime(CLOCK_MONOTONIC, &b) != 0) {
            return 1;
        }
        if (ts_us(&b) > ts_us(&a)) {
            break;
        }
    }
    if (ts_us(&b) <= ts_us(&a)) {
        sayn("  monotonic did not advance, us=", ts_us(&a));
        return 1;
    }

    /* 2. One wall clock, read three ways. **The C3 rung.**
     *
     * `gettimeofday` and `time` are x86-only spellings that stay in the amd64
     * kernel as shims; `clock_gettime(CLOCK_REALTIME)` is `akuma-syscalls-
     * time`'s. Before C3 those were two different anchors on that target. A
     * two-second window covers the read spread and a second boundary landing
     * between them; anything larger is two clocks.
     *
     * Skipped, not failed, on a machine whose clock was never set: that is an
     * offline machine, not a broken kernel, and the rung has nothing to
     * compare. Both spellings reporting the same *absence* is still checked. */
    say("2 agree\n");
    struct timeval tv;
    struct timespec rt;
    time_t t1;
    if (gettimeofday(&tv, NULL) != 0 || clock_gettime(CLOCK_REALTIME, &rt) != 0) {
        return 2;
    }
    t1 = time(NULL);
    if (t1 == (time_t)-1) {
        return 2;
    }
    long long tv_us = (long long)tv.tv_sec * 1000000 + tv.tv_usec;
    long long rt_us = ts_us(&rt);
    if (tv_us == 0 && rt_us == 0 && t1 == 0) {
        say("  clock never set (no NTP); agreement is vacuous, skipping\n");
    } else {
        long long d = tv_us > rt_us ? tv_us - rt_us : rt_us - tv_us;
        if (d > 2000000) {
            sayn("  gettimeofday vs clock_gettime differ by us=", d);
            return 2;
        }
        long long dt = (long long)t1 - rt_us / 1000000;
        if (dt > 2 || dt < -2) {
            sayn("  time(2) vs clock_gettime differ by s=", dt);
            return 2;
        }
        /* A clock that is set at all should be this millennium. A kernel that
         * anchors against the wrong base reports a plausible-looking number
         * near the epoch, which every rung above would accept. */
        if (rt_us / 1000000 < 1700000000LL) {
            sayn("  wall clock is set but implausible, unix=", rt_us / 1000000);
            return 2;
        }
    }

    /* 3. clock_getres, for both ids. The value is not asserted: this kernel
     * reports 1 us for a clock whose tick is 10 ms, which is a divergence
     * pinned in `akuma-syscalls-time`, not something to re-litigate here. */
    say("3 getres\n");
    struct timespec res;
    if (clock_getres(CLOCK_MONOTONIC, &res) != 0 || clock_getres(CLOCK_REALTIME, &res) != 0) {
        return 3;
    }

    /* 4. nanosleep sleeps, measured on the guest's own monotonic clock.
     *
     * Self-consistent on purpose: the QEMU/TCG rig's guest clock runs several
     * times wall-clock, so a sleep measured against the *host* would look
     * wrong on a kernel that is behaving. What must hold is that the clock a
     * program steers by and the sleep it asks for agree with each other.
     *
     * 250 ms of slack under 300 ms, for a 10 ms tick and a rounding at each
     * end. This rung was `yield_now()` on amd64 until 2026-09-06 and returned
     * instantly — a `nanosleep` that does not sleep is not a coarse clock, it
     * is no clock, and every program using one to sequence against a thread
     * loses its ordering silently. */
    say("4 nanosleep\n");
    struct timespec req = { .tv_sec = 0, .tv_nsec = 300000000 };
    if (clock_gettime(CLOCK_MONOTONIC, &a) != 0) {
        return 4;
    }
    if (nanosleep(&req, NULL) != 0) {
        return 4;
    }
    if (clock_gettime(CLOCK_MONOTONIC, &b) != 0) {
        return 4;
    }
    if (ts_us(&b) - ts_us(&a) < 250000) {
        sayn("  300ms nanosleep advanced the clock by only us=", ts_us(&b) - ts_us(&a));
        return 4;
    }

    /* 5. The EINVALs. A malformed interval must be an error, not a park: an
     * unvalidated `tv_sec = -1` reinterprets to 1.8e19 microseconds and sleeps
     * for ~584 000 years, which is a hang reachable from one bad argument. The
     * shared crate had no check until C3 folded the amd64 arm's in. */
    say("5 einval\n");
    struct timespec bad = { .tv_sec = 0, .tv_nsec = 1000000000 };
    if (nanosleep(&bad, NULL) != -1 || errno != EINVAL) {
        say("  nanosleep accepted tv_nsec == 1e9\n");
        return 5;
    }
    bad.tv_sec = -1;
    bad.tv_nsec = 0;
    if (nanosleep(&bad, NULL) != -1 || errno != EINVAL) {
        say("  nanosleep accepted a negative tv_sec\n");
        return 5;
    }

    /* 6. clock_nanosleep, relative — the spelling `std::thread::sleep` emits
     * on any `target_os = "linux"` build, which plain `nanosleep` is not. Its
     * absence is an `ENOSYS` panic in every Rust binary that sleeps. */
    say("6 relsleep\n");
    req.tv_sec = 0;
    req.tv_nsec = 150000000;
    if (clock_gettime(CLOCK_MONOTONIC, &a) != 0) {
        return 6;
    }
    if (clock_nanosleep(CLOCK_MONOTONIC, 0, &req, NULL) != 0) {
        return 6;
    }
    if (clock_gettime(CLOCK_MONOTONIC, &b) != 0) {
        return 6;
    }
    if (ts_us(&b) - ts_us(&a) < 120000) {
        sayn("  relative clock_nanosleep returned early, us=", ts_us(&b) - ts_us(&a));
        return 6;
    }

    /* 7. ...and absolute. A deadline already in the past must return at once
     * rather than being treated as a relative interval — the bug the AArch64
     * kernel had in `futex(FUTEX_WAIT_BITSET)` and which the same arithmetic
     * is capable of here. */
    say("7 abssleep\n");
    if (clock_gettime(CLOCK_MONOTONIC, &a) != 0) {
        return 7;
    }
    struct timespec deadline = a;
    deadline.tv_nsec += 150000000;
    if (deadline.tv_nsec >= 1000000000) {
        deadline.tv_nsec -= 1000000000;
        deadline.tv_sec++;
    }
    if (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &deadline, NULL) != 0) {
        return 7;
    }
    if (clock_gettime(CLOCK_MONOTONIC, &b) != 0) {
        return 7;
    }
    if (ts_us(&b) < ts_us(&deadline)) {
        sayn("  absolute clock_nanosleep returned before its deadline, us=",
             ts_us(&deadline) - ts_us(&b));
        return 7;
    }
    struct timespec past = { .tv_sec = 1, .tv_nsec = 0 };
    if (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &past, NULL) != 0) {
        say("  a deadline in the past did not return 0\n");
        return 7;
    }

    /* 8. alarm(1) + pause(). **The itimer rung**, and the one that proves the
     * timer tick drives expiry: this process is blocked in `pause` when the
     * alarm comes due, so nothing it does can be the thing that notices.
     *
     * `alarm` is also the syscall-number rung. asm-generic has no `alarm`, so
     * musl spells it as `setitimer` there and as the raw syscall on x86_64 —
     * where it was ENOSYS on this kernel until C3, and `alarm(3)` therefore
     * returned -1 without arming anything. */
    say("8 alarm\n");
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm;
    if (sigaction(SIGALRM, &sa, NULL) != 0) {
        return 8;
    }
    alarms = 0;
    if (alarm(1) != 0) {
        say("  alarm(1) reported a pending alarm on a fresh process\n");
        return 8;
    }
    pause();
    if (alarms != 1) {
        sayn("  pause returned without a SIGALRM, alarms=", alarms);
        return 8;
    }

    /* 8b. The syscall after a delivered handler must succeed.
     *
     * Not a clock property, and here because this probe is what found it: the
     * flag `check_itimers` raises to break a blocking syscall was consumed by
     * nothing when the thread was *running*, so the first syscall after the
     * handler returned answered `EINTR` — `getpid()` came back `-4` on amd64
     * (2026-09-12). Any signal reached this, not only `SIGALRM`; rung 8 is
     * simply the cheapest way to get a handler to run and then ask.
     *
     * `getpid` because it is the syscall that most obviously cannot block, so
     * an `EINTR` from it cannot be anything but a leaked flag. */
    say("8b posteintr\n");
    errno = 0;
    if (getpid() <= 0) {
        sayn("  getpid after a handler failed, errno=", errno);
        return 8;
    }

    /* 9. alarm's return value: the seconds left on the alarm it replaces,
     * rounded up. `alarm(0)` cancels and reports. A kernel that always
     * returns 0 passes rung 8 and fails here. */
    say("9 alarmret\n");
    alarms = 0;
    (void)alarm(10);
    unsigned left = alarm(0);
    if (left == 0 || left > 10) {
        sayn("  alarm(0) misreported the remaining seconds, left=", (long long)left);
        return 9;
    }
    /* And the cancel took: nothing may fire while we wait out the original. */
    req.tv_sec = 0;
    req.tv_nsec = 200000000;
    (void)nanosleep(&req, NULL);
    if (alarms != 0) {
        say("  a cancelled alarm still fired\n");
        return 9;
    }

    /* 10. setitimer, sub-second — what `alarm` cannot express, and the arm the
     * amd64 kernel reached only once `akuma-syscalls-abi` had a row for x86_64
     * 38. A `nanosleep` long enough to contain it is the wait, so this also
     * says that a signal breaks a sleep rather than being swallowed by it. */
    say("10 itimer\n");
    alarms = 0;
    struct itimerval it;
    memset(&it, 0, sizeof it);
    it.it_value.tv_usec = 200000;
    if (setitimer(ITIMER_REAL, &it, NULL) != 0) {
        return 10;
    }
    req.tv_sec = 2;
    req.tv_nsec = 0;
    (void)nanosleep(&req, NULL);
    if (alarms != 1) {
        sayn("  setitimer did not fire, alarms=", alarms);
        return 10;
    }

    /* 11. times() and getrusage() answer. Neither reports per-process CPU
     * time on either kernel — the buffers come back zeroed, which is what a
     * shell reads to print `0m0.000s` rather than garbage — so this rung is
     * about the syscalls existing, which on amd64 they did not until C3. */
    say("11 times\n");
    struct tms tms;
    if (times(&tms) == (clock_t)-1) {
        return 11;
    }
    struct rusage ru;
    if (getrusage(RUSAGE_SELF, &ru) != 0) {
        return 11;
    }

    say("=== clockprobe OK\n");
    return 0;
}
