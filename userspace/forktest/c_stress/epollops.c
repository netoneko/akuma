/*
 * epollops.c — probe Akuma's epoll/poll/select family (src/syscall/poll.rs and
 * crates/akuma-syscalls-poll) against Linux semantics, op by op.
 *
 * Written as the correctness gate for the `akuma-syscalls-poll` extraction
 * (docs/archive/AKUMA_EXTRACT_SYSCALLS.md §8.2). Until it existed this family
 * had no in-guest probe at all: every one of the incidents below was found by
 * pointing bun, tokio, nginx or cargo at a live socket and waiting to see
 * whether it hung, which is a gate you cannot run twice in an afternoon.
 *
 * Each probe prints PASS (matches Linux) / FAIL (diverges) / SKIP (the
 * environment could not run it) / DIVERGE (a *known*, documented difference
 * from Linux — see "Known divergences" in
 * docs/reference/subsystems/syscalls/poll.md; not counted as a failure).
 *
 * Run the same static binary on Linux to confirm the probes themselves are
 * right: every FAIL here should be a PASS there, and every DIVERGE here should
 * be a PASS there.
 *
 * Static, musl, no Rust runtime. Build:
 *   aarch64-linux-musl-gcc -O2 -static -o epollops epollops.c
 */

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

static int fails = 0;
static int diverges = 0;

static void ok(const char *name, const char *detail) {
    printf("PASS %s — %s\n", name, detail);
    fflush(stdout);
}

static void bad(const char *name, const char *detail) {
    printf("FAIL %s — %s\n", name, detail);
    fflush(stdout);
    fails++;
}

static void diverge(const char *name, const char *detail) {
    printf("DIVERGE %s — %s\n", name, detail);
    fflush(stdout);
    diverges++;
}

static void skip(const char *name, const char *detail) {
    printf("SKIP %s — %s\n", name, detail);
    fflush(stdout);
}

/* epoll_wait with a bounded timeout, returning the first event's mask, 0 for
 * "nothing became ready", or -1 on error. Retries EINTR so a stray signal
 * cannot be mistaken for a lost edge — the exact confusion these probes exist
 * to remove. */
static int wait_mask(int ep, int ms) {
    struct epoll_event ev;
    for (;;) {
        int n = epoll_wait(ep, &ev, 1, ms);
        if (n < 0 && errno == EINTR) continue;
        if (n < 0) return -1;
        return n == 0 ? 0 : (int)ev.events;
    }
}

static int nonblock(int fd) {
    int fl = fcntl(fd, F_GETFL, 0);
    return fl < 0 ? -1 : fcntl(fd, F_SETFL, fl | O_NONBLOCK);
}

/* ---------------------------------------------------------------------------
 * Probe 1: an edge-triggered EPOLLIN must re-arm after the fd is drained.
 *
 * The bun HTTPS fetch hang. `epoll_pwait` recomputes readiness from scratch on
 * each pass and reports `revents & ~last_ready`, so a level transition that
 * happens *and un-happens* between two passes is invisible to it — the mask
 * still says "already reported" and the edge never fires again. The read
 * syscalls are the only code that witnesses the drain, so they have to report
 * it back (`epoll_on_fd_drained`). Without that hook the second arrival below
 * is never announced and the caller waits forever on data that is sitting in
 * the buffer.
 * ------------------------------------------------------------------------ */
static void probe_et_in_rearms_after_drain(void) {
    const char *n = "et_in_rearms_after_drain";
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0) { skip(n, "no socketpair"); return; }
    int ep = epoll_create1(0);
    if (ep < 0) { skip(n, "no epoll_create1"); close(sv[0]); close(sv[1]); return; }

    struct epoll_event reg = { .events = EPOLLIN | EPOLLET, .data = { .u64 = 1 } };
    if (epoll_ctl(ep, EPOLL_CTL_ADD, sv[0], &reg) < 0) { skip(n, "EPOLL_CTL_ADD failed"); goto out; }

    if (write(sv[1], "a", 1) != 1) { skip(n, "peer write failed"); goto out; }
    if ((wait_mask(ep, 1000) & EPOLLIN) == 0) {
        bad(n, "first arrival never reported at all — not an edge bug, readiness is broken");
        goto out;
    }
    char c;
    if (read(sv[0], &c, 1) != 1) { bad(n, "drain read failed"); goto out; }

    if (write(sv[1], "b", 1) != 1) { skip(n, "second peer write failed"); goto out; }
    int m = wait_mask(ep, 1000);
    if (m & EPOLLIN) {
        ok(n, "the second arrival re-fired the edge after the drain");
    } else {
        bad(n, "no EPOLLIN for data already in the buffer — the drained-read edge "
               "re-arm is missing; a client that reads one record at a time hangs");
    }
out:
    close(ep); close(sv[0]); close(sv[1]);
}

/* ---------------------------------------------------------------------------
 * Probe 2: an edge-triggered EPOLLOUT must re-arm after a blocked write.
 *
 * The exact mirror of probe 1, and the one whose absence was *intermittent*:
 * `epoll_pwait` drives the network poll at the top of its own loop, which
 * usually flushes the transmit buffer before `can_send()` is ever observed
 * false — so whether any pass lands while the buffer is genuinely full is a
 * race. A client that filled the buffer and waited for EPOLLOUT could wait
 * forever (nettest-reqwest, 64 KiB body, 2 runs in 3).
 * ------------------------------------------------------------------------ */
static void probe_et_out_rearms_after_blocked_write(void) {
    const char *n = "et_out_rearms_after_blocked_write";
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0) { skip(n, "no socketpair"); return; }
    int ep = epoll_create1(0);
    if (ep < 0) { skip(n, "no epoll_create1"); close(sv[0]); close(sv[1]); return; }
    if (nonblock(sv[0]) < 0) { skip(n, "O_NONBLOCK failed"); goto out; }

    struct epoll_event reg = { .events = EPOLLOUT | EPOLLET, .data = { .u64 = 2 } };
    if (epoll_ctl(ep, EPOLL_CTL_ADD, sv[0], &reg) < 0) { skip(n, "EPOLL_CTL_ADD failed"); goto out; }
    if ((wait_mask(ep, 1000) & EPOLLOUT) == 0) { bad(n, "an empty buffer was never writable"); goto out; }

    /* Fill until it blocks. Bounded, so a socket with no send-buffer limit
     * SKIPs rather than looping until the probe is killed. */
    static char buf[4096];
    long total = 0;
    const long cap = 8L << 20;
    for (;;) {
        ssize_t w = write(sv[0], buf, sizeof(buf));
        if (w < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) break;
        if (w < 0) { skip(n, "write failed for a reason other than EAGAIN"); goto out; }
        total += w;
        if (total > cap) { skip(n, "send buffer never filled (8 MB written)"); goto out; }
    }

    if (wait_mask(ep, 0) & EPOLLOUT) {
        bad(n, "reported writable with the send buffer full — readiness is wrong, "
               "and the edge test below cannot mean anything");
        goto out;
    }

    /* Drain the peer completely, so the buffer becomes writable again. */
    if (nonblock(sv[1]) < 0) { skip(n, "peer O_NONBLOCK failed"); goto out; }
    for (;;) {
        ssize_t r = read(sv[1], buf, sizeof(buf));
        if (r <= 0) break;
    }

    int m = wait_mask(ep, 1000);
    if (m & EPOLLOUT) {
        ok(n, "the write edge re-fired once the buffer drained");
    } else {
        bad(n, "no EPOLLOUT after the buffer drained — the blocked-write edge "
               "re-arm is missing; a client holding a half-written request hangs");
    }
out:
    close(ep); close(sv[0]); close(sv[1]);
}

/* ---------------------------------------------------------------------------
 * Probe 3: a pipe at EOF must produce an edge.
 *
 * `pipe_can_read` answers true both for "has bytes" and "at EOF", so those two
 * states share one EPOLLIN bit and there is no transition between them. An
 * edge-triggered reader that drained a child's stdout and went back to
 * epoll_wait for EOF therefore saw nothing new and hung — a healthy-looking
 * process that had simply stopped. EPOLLHUP, reported once the last writer is
 * gone, is the bit that makes the EOF transition an edge at all.
 * ------------------------------------------------------------------------ */
static void probe_pipe_eof_is_an_edge(void) {
    const char *n = "pipe_eof_is_an_edge";
    int p[2];
    if (pipe(p) < 0) { skip(n, "no pipe"); return; }
    int ep = epoll_create1(0);
    if (ep < 0) { skip(n, "no epoll_create1"); close(p[0]); close(p[1]); return; }

    struct epoll_event reg = { .events = EPOLLIN | EPOLLET, .data = { .u64 = 3 } };
    if (epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &reg) < 0) { skip(n, "EPOLL_CTL_ADD failed"); goto out; }

    if (write(p[1], "x", 1) != 1) { skip(n, "pipe write failed"); goto out; }
    if ((wait_mask(ep, 1000) & EPOLLIN) == 0) { bad(n, "pipe data never reported"); goto out; }
    char c;
    if (read(p[0], &c, 1) != 1) { bad(n, "drain read failed"); goto out; }

    close(p[1]);
    p[1] = -1;
    int m = wait_mask(ep, 1000);
    if (m == 0) {
        bad(n, "nothing reported after the last writer closed — an edge-triggered "
               "reader waiting for EOF hangs on a pipe that is already at EOF");
    } else if (m & EPOLLHUP) {
        ok(n, "EPOLLHUP made the EOF transition an edge");
    } else {
        ok(n, "an event was delivered for the EOF transition");
    }
out:
    close(ep); close(p[0]); if (p[1] >= 0) close(p[1]);
}

/* ---------------------------------------------------------------------------
 * Probe 4: epoll_ctl's errno set.
 *
 * MOD/DEL on an fd that was never added is ENOENT, an unknown op is EINVAL, and
 * ADD on an fd already in the interest list is EEXIST *on Linux*. Akuma treats
 * that last one as a MOD and answers 0 — a known, documented divergence, so it
 * is reported as DIVERGE rather than counted as a failure. Running this binary
 * on Linux is what proves the probe is asking the right question.
 * ------------------------------------------------------------------------ */
static void probe_epoll_ctl_errno_set(void) {
    int ep = epoll_create1(0);
    int p[2];
    if (ep < 0 || pipe(p) < 0) { skip("epoll_ctl_errno_set", "no epoll/pipe"); return; }
    struct epoll_event reg = { .events = EPOLLIN, .data = { .u64 = 4 } };

    if (epoll_ctl(ep, EPOLL_CTL_MOD, p[0], &reg) == 0)
        bad("epoll_ctl_mod_absent", "MOD on an unregistered fd succeeded");
    else if (errno == ENOENT) ok("epoll_ctl_mod_absent", "ENOENT");
    else bad("epoll_ctl_mod_absent", "wrong errno for MOD on an unregistered fd");

    if (epoll_ctl(ep, EPOLL_CTL_DEL, p[0], &reg) == 0)
        bad("epoll_ctl_del_absent", "DEL on an unregistered fd succeeded");
    else if (errno == ENOENT) ok("epoll_ctl_del_absent", "ENOENT");
    else bad("epoll_ctl_del_absent", "wrong errno for DEL on an unregistered fd");

    if (epoll_ctl(ep, 99, p[0], &reg) == 0)
        bad("epoll_ctl_unknown_op", "an unknown op succeeded");
    else if (errno == EINVAL) ok("epoll_ctl_unknown_op", "EINVAL");
    else bad("epoll_ctl_unknown_op", "wrong errno for an unknown op");

    if (epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &reg) < 0) {
        bad("epoll_ctl_add_twice", "the first ADD failed");
    } else if (epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &reg) == 0) {
        diverge("epoll_ctl_add_twice",
                "a second ADD returned 0 (overwrote the registration); Linux answers "
                "EEXIST — a caller that tests for EEXIST concludes it never registered");
    } else if (errno == EEXIST) {
        ok("epoll_ctl_add_twice", "EEXIST");
    } else {
        bad("epoll_ctl_add_twice", "a second ADD failed with something other than EEXIST");
    }

    /* DEL must ignore its fourth argument: Linux has allowed NULL there since
     * 2.6.9 and real callers pass it. Reading it would turn a correct program
     * into EFAULT. */
    if (epoll_ctl(ep, EPOLL_CTL_DEL, p[0], NULL) == 0) ok("epoll_ctl_del_null_event", "NULL accepted");
    else bad("epoll_ctl_del_null_event", "DEL with a NULL event failed");

    close(ep); close(p[0]); close(p[1]);
}

/* ---------------------------------------------------------------------------
 * Probe 5: a zero timeout is a single non-blocking pass.
 *
 * The `>=` timeout comparison in the wait policy is what makes this so; a `>`
 * would park for one backstop interval on every non-blocking poll a busy event
 * loop makes.
 * ------------------------------------------------------------------------ */
static void probe_zero_timeout_is_nonblocking(void) {
    const char *n = "zero_timeout_is_nonblocking";
    int ep = epoll_create1(0);
    int p[2];
    if (ep < 0 || pipe(p) < 0) { skip(n, "no epoll/pipe"); return; }
    struct epoll_event reg = { .events = EPOLLIN, .data = { .u64 = 5 } };
    if (epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &reg) < 0) { skip(n, "ADD failed"); goto out; }

    struct timeval a, b;
    gettimeofday(&a, NULL);
    for (int i = 0; i < 20; i++) (void)wait_mask(ep, 0);
    gettimeofday(&b, NULL);
    long us = (b.tv_sec - a.tv_sec) * 1000000L + (b.tv_usec - a.tv_usec);
    /* 20 passes of a genuinely non-blocking call are microseconds. One 10 ms
     * backstop park per pass would be 200 ms. */
    if (us < 100000) ok(n, "20 zero-timeout passes returned promptly");
    else bad(n, "zero-timeout epoll_wait parked — a busy event loop pays a "
                "backstop interval per non-blocking poll");
out:
    close(ep); close(p[0]); close(p[1]);
}

/* ---------------------------------------------------------------------------
 * Probe 6: level-triggered means "tell me again".
 *
 * The complement of probes 1-3: an entry without EPOLLET must report the same
 * readiness on every pass, whether or not the caller drained anything. A
 * kernel that applied edge bookkeeping to level-triggered entries would look
 * fine to an edge-triggered client and starve everyone else.
 * ------------------------------------------------------------------------ */
static void probe_level_triggered_repeats(void) {
    const char *n = "level_triggered_repeats";
    int ep = epoll_create1(0);
    int p[2];
    if (ep < 0 || pipe(p) < 0) { skip(n, "no epoll/pipe"); return; }
    struct epoll_event reg = { .events = EPOLLIN, .data = { .u64 = 6 } };
    if (epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &reg) < 0) { skip(n, "ADD failed"); goto out; }
    if (write(p[1], "xy", 2) != 2) { skip(n, "pipe write failed"); goto out; }

    int seen = 0;
    for (int i = 0; i < 3; i++) if (wait_mask(ep, 500) & EPOLLIN) seen++;
    if (seen == 3) ok(n, "3 of 3 passes reported the undrained data");
    else bad(n, "a level-triggered fd stopped reporting data it still holds");
out:
    close(ep); close(p[0]); close(p[1]);
}

/* ---------------------------------------------------------------------------
 * Probe 7: poll(2) reports POLLHUP without being asked.
 *
 * POLLHUP cannot appear in `events` and is always possible in `revents`. A
 * poll() that reported a hangup only when asked leaves a caller waiting on a
 * dead fd forever. This also exercises the ppoll `POLL*`/`EPOLL*` marshalling
 * in both directions.
 * ------------------------------------------------------------------------ */
static void probe_poll_reports_hup_unasked(void) {
    const char *n = "poll_reports_hup_unasked";
    int p[2];
    if (pipe(p) < 0) { skip(n, "no pipe"); return; }
    close(p[1]);

    struct pollfd pf = { .fd = p[0], .events = POLLIN, .revents = 0 };
    int r = poll(&pf, 1, 1000);
    if (r <= 0) {
        bad(n, "poll() on a pipe whose last writer closed reported nothing");
    } else if (pf.revents & (POLLHUP | POLLIN)) {
        ok(n, (pf.revents & POLLHUP) ? "POLLHUP reported unrequested"
                                     : "the EOF was reported (as POLLIN)");
    } else {
        bad(n, "poll() returned but set no usable revents bit");
    }
    close(p[0]);
}

/* ---------------------------------------------------------------------------
 * Probe 8: select(2) must WRITE all three fd sets, exceptfds included.
 *
 * `select` reports by overwriting, so a set the kernel never writes comes back
 * exactly as the caller passed it in — every fd in it still flagged. That was a
 * live bug until 2026-08-20 and it broke cargo completely: the nightly
 * toolchain's libcurl compiles Curl_poll()'s select() branch and asks for
 * POLLPRI on a connecting socket, which that branch puts in exceptfds. The
 * stale set made libcurl synthesise POLLPRI, map it to CURL_CSELECT_ERR, and
 * abandon a socket that had just reached Established with SO_ERROR == 0.
 * ------------------------------------------------------------------------ */
static void probe_select_clears_exceptfds(void) {
    const char *n = "select_clears_exceptfds";
    int p[2];
    if (pipe(p) < 0) { skip(n, "no pipe"); return; }
    if (write(p[1], "z", 1) != 1) { skip(n, "pipe write failed"); goto out; }

    fd_set rd, ex;
    FD_ZERO(&rd); FD_ZERO(&ex);
    FD_SET(p[0], &rd);
    FD_SET(p[0], &ex);   /* the caller's stale flag */
    struct timeval tv = { .tv_sec = 1, .tv_usec = 0 };
    int r = select(p[0] + 1, &rd, NULL, &ex, &tv);
    if (r <= 0) { bad(n, "select() did not report readable data"); goto out; }
    if (FD_ISSET(p[0], &ex))
        bad(n, "exceptfds came back with the caller's flag still set — a libcurl "
               "caller reads that as POLLPRI and abandons a healthy socket");
    else if (!FD_ISSET(p[0], &rd))
        bad(n, "readfds did not report the readable fd");
    else
        ok(n, "readfds written, exceptfds cleared");
out:
    close(p[0]); close(p[1]);
}

/* ---------------------------------------------------------------------------
 * Probe 9: select(2) counts BITS, not fds.
 *
 * An fd ready for both reading and writing contributes two to the return value.
 * A caller that sized a loop by the return value and got one per fd stops
 * early.
 * ------------------------------------------------------------------------ */
static void probe_select_counts_bits_not_fds(void) {
    const char *n = "select_counts_bits_not_fds";
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0) { skip(n, "no socketpair"); return; }
    if (write(sv[1], "q", 1) != 1) { skip(n, "peer write failed"); goto out; }

    fd_set rd, wr;
    FD_ZERO(&rd); FD_ZERO(&wr);
    FD_SET(sv[0], &rd);
    FD_SET(sv[0], &wr);
    struct timeval tv = { .tv_sec = 1, .tv_usec = 0 };
    int r = select(sv[0] + 1, &rd, &wr, NULL, &tv);
    if (r == 2 && FD_ISSET(sv[0], &rd) && FD_ISSET(sv[0], &wr))
        ok(n, "returned 2 for one fd ready in both directions");
    else if (r == 1)
        bad(n, "returned 1 for an fd ready in both directions — the count is per "
               "bit, not per fd");
    else
        bad(n, "neither 1 nor 2: the fd was not reported ready in both directions");
out:
    close(sv[0]); close(sv[1]);
}

/* ---------------------------------------------------------------------------
 * The TCP group. Everything below needs a loopback connection; if the stack
 * cannot make one, the whole group SKIPs rather than reporting failures that
 * are about the network and not about readiness.
 * ------------------------------------------------------------------------ */
static int tcp_pair(int *cli, int *srv, int port) {
    int ln = socket(AF_INET, SOCK_STREAM, 0);
    if (ln < 0) return -1;
    int one = 1;
    setsockopt(ln, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in a;
    memset(&a, 0, sizeof(a));
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(ln, (struct sockaddr *)&a, sizeof(a)) < 0 || listen(ln, 8) < 0) { close(ln); return -1; }

    int c = socket(AF_INET, SOCK_STREAM, 0);
    if (c < 0) { close(ln); return -1; }
    if (connect(c, (struct sockaddr *)&a, sizeof(a)) < 0) { close(c); close(ln); return -1; }
    int s = accept(ln, NULL, NULL);
    if (s < 0) { close(c); close(ln); return -1; }
    close(ln);
    *cli = c;
    *srv = s;
    return 0;
}

/* Bug 1: EPOLLIN was never reported for a *listening* TCP socket, so no
 * epoll-driven server could accept at all. */
static void probe_listener_is_readable(int port) {
    const char *n = "listener_reports_epollin";
    int ln = socket(AF_INET, SOCK_STREAM, 0);
    if (ln < 0) { skip(n, "no AF_INET socket"); return; }
    int one = 1;
    setsockopt(ln, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in a;
    memset(&a, 0, sizeof(a));
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (bind(ln, (struct sockaddr *)&a, sizeof(a)) < 0 || listen(ln, 8) < 0) {
        skip(n, "bind/listen on loopback failed"); close(ln); return;
    }

    int ep = epoll_create1(0);
    struct epoll_event reg = { .events = EPOLLIN, .data = { .u64 = 10 } };
    if (ep < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, ln, &reg) < 0) { skip(n, "epoll setup failed"); goto out; }

    if (wait_mask(ep, 0) & EPOLLIN) { bad(n, "an idle listener reported readable"); goto out; }

    int c = socket(AF_INET, SOCK_STREAM, 0);
    if (c < 0 || connect(c, (struct sockaddr *)&a, sizeof(a)) < 0) {
        skip(n, "loopback connect failed"); if (c >= 0) close(c); goto out;
    }
    if (wait_mask(ep, 2000) & EPOLLIN)
        ok(n, "a pending connection made the listener readable");
    else
        bad(n, "a listener with a pending connection never reported EPOLLIN — an "
               "epoll-driven server can never accept");
    close(c);
out:
    if (ep >= 0) close(ep);
    close(ln);
}

/* Bug 4 / Bug 5: after the peer closes, the socket must report readable (there
 * may be buffered bytes) *and* eventually a hangup — never nothing, which is
 * the epoll spin. */
static void probe_tcp_peer_close(int port) {
    const char *n = "tcp_peer_close_reports_in_then_hup";
    int cli = -1, srv = -1;
    if (tcp_pair(&cli, &srv, port) < 0) { skip(n, "no loopback TCP pair"); return; }

    int ep = epoll_create1(0);
    struct epoll_event reg = { .events = EPOLLIN, .data = { .u64 = 11 } };
    if (ep < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, cli, &reg) < 0) { skip(n, "epoll setup failed"); goto out; }

    if (write(srv, "hi", 2) != 2) { skip(n, "server write failed"); goto out; }
    close(srv);
    srv = -1;

    int m = wait_mask(ep, 3000);
    if (m == 0) {
        bad(n, "nothing reported after the peer sent and closed — the caller spins "
               "or sleeps forever on a connection that is finished");
        goto out;
    }
    if ((m & (EPOLLIN | EPOLLHUP | EPOLLRDHUP)) == 0) {
        bad(n, "an event was delivered but none of IN/HUP/RDHUP was in it");
        goto out;
    }
    char b[8];
    ssize_t r = read(cli, b, sizeof(b));
    if (r != 2 || memcmp(b, "hi", 2) != 0) {
        bad(n, "the bytes the peer sent before closing were not readable — a client "
               "told only 'read-closed' discards the last response it was sent");
        goto out;
    }
    /* Now genuinely at EOF: the next pass must still report something. */
    if (wait_mask(ep, 3000) == 0)
        bad(n, "after draining, a fully-closed socket reported nothing — this is "
               "the epoll spin EPOLLHUP was added to end");
    else
        ok(n, "buffered data readable, then the closed socket kept reporting");
out:
    if (ep >= 0) close(ep);
    if (cli >= 0) close(cli);
    if (srv >= 0) close(srv);
}

int main(void) {
    printf("epollops: epoll/poll/select semantics probe\n");
    fflush(stdout);

    probe_et_in_rearms_after_drain();
    probe_et_out_rearms_after_blocked_write();
    probe_pipe_eof_is_an_edge();
    probe_epoll_ctl_errno_set();
    probe_zero_timeout_is_nonblocking();
    probe_level_triggered_repeats();
    probe_poll_reports_hup_unasked();
    probe_select_clears_exceptfds();
    probe_select_counts_bits_not_fds();
    probe_listener_is_readable(34561);
    probe_tcp_peer_close(34562);

    printf("\nepollops: %d FAIL, %d known DIVERGE\n", fails, diverges);
    fflush(stdout);
    return fails ? 1 : 0;
}
