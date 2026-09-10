/* consoletty — the serial console's read/poll/ioctl contract, from ring 3.
 *
 * Runs as **init**, not over ssh, and that is the whole point: over ssh a
 * process's fd 0 is a `PipeRead` and is served by `akuma-syscalls-glue`'s pipe
 * arm. Only a process on the serial line has fd 0 as a `FileDescriptor::Stdin`,
 * which is the descriptor `amd64::fd::sys_read`'s preamble claims for
 * `read_console` and `poll` answers through the `poll_console_state` hook —
 * i.e. the two paths this probe exists to check.
 *
 *   docs/archive/AMD64_CONSOLE_NONBLOCK_READ.md   (the O_NONBLOCK fix)
 *   docs/archive/AKUMA_AMD64_4B_FOLD_BATCH4B.md   (the poll/ioctl fold)
 *
 * Every check assumes an IDLE console: nothing typed while it runs. That holds
 * on every rig during a scripted boot (QEMU's serial is fed from /dev/null).
 *
 * Prints PASS/FAIL per line and a tally, in the `lazybuf`/`openflags` house
 * style, then exits non-zero if anything failed.
 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <termios.h>
#include <unistd.h>

static int passed, failed;

static void ck(const char *what, int ok) {
    if (ok) { passed++; printf("PASS %s\n", what); }
    else    { failed++; printf("FAIL %s\n", what); }
    fflush(stdout);
}

int main(void) {
    printf("consoletty: the serial console from ring 3 (fd 0 = Stdin)\n");
    fflush(stdout);

    /* ---- 1. O_NONBLOCK round-trips through ONE flag store -------------- */
    /* `fcntl` is glue's arm and writes `Process::set_nonblock`; the console
     * read reads the same set through `fd::is_nonblocking`. The first
     * diagnosis of the typing bug was that these were two stores. */
    int fl = fcntl(0, F_GETFL);
    ck("F_GETFL on the console succeeds", fl >= 0);
    ck("and does not already report O_NONBLOCK", (fl & O_NONBLOCK) == 0);
    ck("F_SETFL O_NONBLOCK succeeds", fcntl(0, F_SETFL, fl | O_NONBLOCK) == 0);
    ck("F_GETFL reads it back", (fcntl(0, F_GETFL) & O_NONBLOCK) != 0);

    /* ---- 2. the fix: a non-blocking read is EAGAIN, not a park --------- */
    /* Before the fix `read_console` was an unconditional
     * `loop { getb() else yield_now() }` and this call never returned — which
     * is exactly how the ssh client's pump starved its socket half. */
    char b[8];
    errno = 0;
    ssize_t n = read(0, b, sizeof b);
    ck("a non-blocking read of an idle console returns -1", n == -1);
    ck("with errno == EAGAIN (not a hang, not EOF, not EBADF)", errno == EAGAIN);

    /* A second one, because the first could have drained a stray byte. */
    errno = 0;
    n = read(0, b, sizeof b);
    ck("and again, so it is a state and not a one-shot", n == -1 && errno == EAGAIN);

    ck("F_SETFL clears it", fcntl(0, F_SETFL, fl) == 0);
    ck("F_GETFL agrees it is cleared", (fcntl(0, F_GETFL) & O_NONBLOCK) == 0);

    /* ---- 3. poll: the `poll_console_state` hook ------------------------ */
    /* Without the hook glue resolves fd 0 through the process fd table and
     * either finds a channel-less `Stdin` (never readable) or nothing at all
     * (`FdState::Missing` -> POLLHUP|POLLERR). The second is the loud one: a
     * shell polling its own stdin would be told the console is finished. */
    struct pollfd pf = { .fd = 0, .events = POLLIN, .revents = 0 };
    int r = poll(&pf, 1, 0);
    ck("poll(stdin, POLLIN, 0) on an idle console returns 0", r == 0);
    ck("and revents is clear -- no POLLHUP/POLLERR", pf.revents == 0);

    pf = (struct pollfd){ .fd = 1, .events = POLLOUT, .revents = 0 };
    r = poll(&pf, 1, 0);
    ck("poll(stdout, POLLOUT, 0) reports ready", r == 1);
    ck("and it is POLLOUT", (pf.revents & POLLOUT) != 0);

    /* A finite timeout must actually elapse and then report nothing. Before
     * the fold this was a lap-count approximation on a target whose lap cost
     * is not fixed. */
    pf = (struct pollfd){ .fd = 0, .events = POLLIN, .revents = 0 };
    r = poll(&pf, 1, 50);
    ck("poll(stdin, POLLIN, 50ms) times out to 0", r == 0);

    /* ---- 4. select, and the exceptfds overwrite rule ------------------- */
    /* Akuma raises no exceptional conditions, but "none" has to be WRITTEN:
     * a set the kernel received and did not write comes back as passed in,
     * which is the whole of docs/runbooks/cargo-cannot-reach-crates-io.md. */
    fd_set rs, ws, es;
    FD_ZERO(&rs); FD_ZERO(&ws); FD_ZERO(&es);
    FD_SET(0, &rs); FD_SET(1, &ws);
    FD_SET(0, &es); FD_SET(1, &es);
    struct timeval tv = { 0, 0 };
    r = select(2, &rs, &ws, &es, &tv);
    ck("select reports the writable console only", r == 1);
    ck("stdout is set in writefds", FD_ISSET(1, &ws));
    ck("stdin is NOT set in readfds", !FD_ISSET(0, &rs));
    ck("exceptfds came back cleared for fd 0", !FD_ISSET(0, &es));
    ck("exceptfds came back cleared for fd 1", !FD_ISSET(1, &es));

    /* ---- 5. ioctl: the fake tty, and the c_cc byte offsets ------------- */
    /* An interactive shell decides stdin is not a terminal if TCGETS fails,
     * and then prints no prompt and reads to EOF. */
    struct termios t;
    memset(&t, 0, sizeof t);
    ck("TCGETS on the console succeeds", ioctl(0, TCGETS, &t) == 0);
    ck("isatty(0) is true", isatty(0) == 1);
    ck("c_lflag has ICANON|ECHO", (t.c_lflag & (ICANON | ECHO)) == (ICANON | ECHO));
    /* These read c_cc[] and are what catch a one-byte offset error in the
     * struct: every index would be shifted by one. */
    ck("c_cc[VINTR] == ^C", t.c_cc[VINTR] == 3);
    ck("c_cc[VQUIT] == ^\\", t.c_cc[VQUIT] == 28);
    ck("c_cc[VERASE] == DEL", t.c_cc[VERASE] == 127);
    ck("c_cc[VKILL] == ^U", t.c_cc[VKILL] == 21);
    ck("c_cc[VEOF] == ^D", t.c_cc[VEOF] == 4);
    ck("c_cc[VMIN] == 1", t.c_cc[VMIN] == 1);
    ck("c_cc[VTIME] == 0", t.c_cc[VTIME] == 0);
    ck("c_cc[VSUSP] == ^Z", t.c_cc[VSUSP] == 26);

    struct winsize ws2;
    memset(&ws2, 0, sizeof ws2);
    ck("TIOCGWINSZ succeeds", ioctl(0, TIOCGWINSZ, &ws2) == 0);
    ck("and reports 24x80", ws2.ws_row == 24 && ws2.ws_col == 80);

    /* ---- 6. what the fold ADDED: glue's non-terminal ioctls ------------ */
    /* None of these existed on this target before batch 4b; each is a
     * delegation to `akuma_syscalls_glue::term::sys_ioctl`. */
    int avail = -1;
    ck("FIONREAD on the console succeeds", ioctl(0, FIONREAD, &avail) == 0);
    ck("and reports 0 bytes waiting on an idle console", avail == 0);
    int one = 1, zero = 0;
    ck("FIONBIO(1) succeeds", ioctl(0, FIONBIO, &one) == 0);
    ck("and shows up as O_NONBLOCK in F_GETFL", (fcntl(0, F_GETFL) & O_NONBLOCK) != 0);
    errno = 0;
    ck("so the read is EAGAIN again -- one flag store, two setters",
       read(0, b, sizeof b) == -1 && errno == EAGAIN);
    ck("FIONBIO(0) clears it", ioctl(0, FIONBIO, &zero) == 0);
    ck("and F_GETFL agrees", (fcntl(0, F_GETFL) & O_NONBLOCK) == 0);
    /* FIOASYNC: nginx's ngx_spawn_process refuses to fork if this fails. */
    ck("FIOASYNC is accepted", ioctl(0, FIOASYNC, &zero) == 0);
    /* An unknown request must be ENOTTY, never ENOSYS: a libc asking "is this
     * a tty?" treats ENOTTY as a clean no and ENOSYS as a broken kernel. */
    errno = 0;
    ck("an unknown ioctl is ENOTTY", ioctl(0, 0x5499, &zero) == -1 && errno == ENOTTY);

    printf("consoletty: %d passed, %d FAILED\n", passed, failed);
    fflush(stdout);
    return failed ? 1 : 0;
}
