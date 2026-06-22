/*
 * sp_fd_test.c — RUMP_SYSPROXY.md Step 4 feasibility test for the kernel-pipe
 * transport: prove rump_server can serve the sysproxy protocol on a PRE-CONNECTED
 * fd (no listen/accept). Single process, no fork, no kernel:
 *
 *   rump_init()
 *   socketpair(sp)                       <- stands in for the kernel pipe pair
 *   rumpuser_sp_init_fd(sp[1])           <- server serves on one end (a thread)
 *   raw sp client on sp[0]: handshake + rump_sys_socket  <- the kernel's future role
 *
 * The raw client mirrors crates/akuma-rump/src/sysproxy.rs (the Rust client the
 * kernel will use), so this also cross-checks that wire framing. PASS = the
 * socket call round-trips through the rump kernel over the connected fd.
 */
#include <sys/types.h>
#include <sys/socket.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <rump/rump.h>

extern int rumpuser_sp_init_fd(int, const char *, const char *, const char *);

#define HDRSZ 24
enum { RUMPSP_REQ = 0, RUMPSP_RESP = 1, RUMPSP_ERROR = 2 };
enum { T_HANDSHAKE = 0, T_SYSCALL = 1, T_COPYIN = 2, T_COPYOUT = 4, T_ANONMMAP = 6 };
#define HANDSHAKE_GUEST 0
#define SYS___socket30 394

static int g_fd;

static int rd(void *b, size_t n) {
	uint8_t *p = b; size_t got = 0;
	while (got < n) {
		ssize_t r = read(g_fd, p + got, n - got);
		if (r <= 0) return -1;
		got += (size_t)r;
	}
	return 0;
}
static int wr(const void *b, size_t n) {
	const uint8_t *p = b; size_t put = 0;
	while (put < n) {
		ssize_t w = write(g_fd, p + put, n - put);
		if (w <= 0) return -1;
		put += (size_t)w;
	}
	return 0;
}
static void put_hdr(uint8_t *h, uint64_t len, uint64_t reqno, uint16_t cls, uint16_t typ, uint32_t u) {
	memcpy(h + 0, &len, 8); memcpy(h + 8, &reqno, 8);
	memcpy(h + 16, &cls, 2); memcpy(h + 18, &typ, 2); memcpy(h + 20, &u, 4);
}

int
main(void)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	printf("SP_FD_TEST: rump_init...\n");
	if (rump_init() != 0) { printf("SP_FD_TEST: FAIL rump_init\n"); return 1; }

	int sp[2];
	if (socketpair(AF_UNIX, SOCK_STREAM, 0, sp) != 0) { perror("socketpair"); return 1; }
	g_fd = sp[0]; /* client end (blocking) */

	if (rumpuser_sp_init_fd(sp[1], "NetBSD", "7.99.34", "evbarm64") != 0) {
		printf("SP_FD_TEST: FAIL rumpuser_sp_init_fd\n"); return 1;
	}
	printf("SP_FD_TEST: server serving on fd %d\n", sp[1]);

	/* 1. read banner line */
	char banner[128]; int bi = 0;
	for (;;) { char c; if (rd(&c, 1)) { printf("SP_FD_TEST: FAIL banner read\n"); return 1; }
		if (c == '\n') break; if (bi < 127) banner[bi++] = c; }
	banner[bi] = 0;
	printf("SP_FD_TEST: banner = %s\n", banner);

	/* 2. handshake REQ (reqno 1): payload = progname + NUL */
	uint8_t h[HDRSZ]; const char *prog = "akuma-kernel";
	size_t plen = strlen(prog) + 1;
	put_hdr(h, HDRSZ + plen, 1, RUMPSP_REQ, T_HANDSHAKE, HANDSHAKE_GUEST);
	if (wr(h, HDRSZ) || wr(prog, plen)) { printf("SP_FD_TEST: FAIL hs send\n"); return 1; }

	/* read handshake response frame */
	if (rd(h, HDRSZ)) { printf("SP_FD_TEST: FAIL hs resp hdr\n"); return 1; }
	uint64_t rlen; memcpy(&rlen, h, 8);
	if (rlen > HDRSZ) { uint8_t tmp[256]; size_t d = rlen - HDRSZ; if (d > sizeof tmp) d = sizeof tmp; rd(tmp, d); }
	printf("SP_FD_TEST: handshake ok\n");

	/* 3. rump_sys_socket(AF_INET=2, SOCK_STREAM=1, 0) — args = 3 x register_t */
	uint64_t args[3] = { 2, 1, 0 };
	put_hdr(h, HDRSZ + sizeof(args), 2, RUMPSP_REQ, T_SYSCALL, SYS___socket30);
	if (wr(h, HDRSZ) || wr(args, sizeof(args))) { printf("SP_FD_TEST: FAIL syscall send\n"); return 1; }

	/* 4. read frames until RESP reqno 2 (socket has no copyin) */
	for (;;) {
		if (rd(h, HDRSZ)) { printf("SP_FD_TEST: FAIL resp hdr\n"); return 1; }
		uint64_t len, reqno; uint16_t cls, typ; uint32_t u;
		memcpy(&len, h, 8); memcpy(&reqno, h + 8, 8);
		memcpy(&cls, h + 16, 2); memcpy(&typ, h + 18, 2); memcpy(&u, h + 20, 4);
		size_t dlen = (size_t)(len - HDRSZ);
		uint8_t data[512]; if (dlen > sizeof data) dlen = sizeof data;
		if (dlen && rd(data, dlen)) { printf("SP_FD_TEST: FAIL resp data\n"); return 1; }
		if ((cls == RUMPSP_RESP || cls == RUMPSP_ERROR) && reqno == 2) {
			if (cls == RUMPSP_ERROR) { printf("SP_FD_TEST: FAIL server ERROR u=%u\n", u); return 1; }
			int32_t err; int64_t r0;
			memcpy(&err, data, 4); memcpy(&r0, data + 8, 8);
			printf("SP_FD_TEST: rump_sys_socket -> error=%d fd=%lld\n", err, (long long)r0);
			if (err == 0 && r0 >= 0) {
				printf("SP_FD_TEST: PASS — sysproxy served on a pre-connected fd "
				    "(socket call round-tripped through the rump kernel)\n");
				return 0;
			}
			printf("SP_FD_TEST: FAIL — socket errno %d\n", err);
			return 1;
		}
		/* any REQ callback (none expected for socket) — ignore for this test */
		printf("SP_FD_TEST: (unexpected frame cls=%u typ=%u reqno=%llu)\n",
		    cls, typ, (unsigned long long)reqno);
	}
}
