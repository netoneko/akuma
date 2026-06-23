/*
 * akctx_smoke.c — minimal HAND-ROLLED aarch64 stackful context switch,
 *
 * Attribution: the fiber USAGE pattern (64KB mmap stack, makecontext-style entry
 * with a single pointer arg, A/B ping-pong) mirrors NetBSD's rumpfiber.c
 * (src-netbsd/lib/librumpuser/rumpfiber.c, (c) Antti Kantee / Justin Cormack).
 * The aarch64 asm switch is original, adapted from Akuma's EL1 switch_context
 * (crates/akuma-exec/src/threading/mod.rs) down to an EL0 cooperative subset.
 *
 * Validated 2026-06-23: Linux (static aarch64-musl in arm64 Alpine) + Akuma EL0
 * (/bin/asmctx_smoke over SSH) — both print the full 10-hop ping-pong + OK.
 *
 * the real candidate for the rumpfiber primitive on Akuma (musl ships no
 * ucontext implementation, only headers — see swapctx_smoke.c link failure).
 *
 * akctx_switch(prev, next): save callee-saved regs (x19-x28, fp, lr) + sp into
 * *prev, restore them from *next, ret. A pure register/stack swap — NO syscall,
 * NO signal-mask touch (unlike musl/glibc swapcontext), so it does not depend on
 * Akuma's rt_sigprocmask at all.
 *
 * akctx_make() seeds a fresh context so the first switch into it lands on a
 * trampoline that calls entry(arg) on the fiber's own stack.
 *
 * Modeled on Akuma's own EL1 trap-frame switch (src/exceptions.rs): that saves
 * all q0-q31 + tpidr_el0 because it is PREEMPTIVE. A COOPERATIVE switch (this
 * file) only needs the AAPCS callee-saved set: x19-x30, sp, the low 64b of
 * v8-v15 (d8-d15), and tpidr_el0 (so each fiber keeps its own TLS/errno). All
 * register moves, no syscall, no signal-mask touch.
 *
 * Proves on Akuma if it prints the full A/B ping-pong + "main resumed ... OK":
 *   stack switching works end-to-end as a normal static aarch64 userspace binary,
 *   with zero libc-ucontext / signal-mask dependency.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <sys/mman.h>

#define STACKSIZE 65536
#define ROUNDS 5

/*
 * slots: x19..x30 (12) + sp (1) + d8..d15 (8) + tpidr_el0 (1) = 22
 * byte offsets: x19..x30 @0..95, sp @96, d8..d15 @104..167, tpidr @168
 */
typedef struct { uint64_t reg[22]; } akctx_t;

/* x0=prev, x1=next ; module-level asm so it's a real symbol we can call */
__asm__(
"	.text\n"
"	.globl akctx_switch\n"
"	.type akctx_switch,%function\n"
"akctx_switch:\n"
"	stp x19, x20, [x0, #0]\n"
"	stp x21, x22, [x0, #16]\n"
"	stp x23, x24, [x0, #32]\n"
"	stp x25, x26, [x0, #48]\n"
"	stp x27, x28, [x0, #64]\n"
"	stp x29, x30, [x0, #80]\n"
"	mov x2, sp\n"
"	str x2, [x0, #96]\n"
"	stp d8, d9,   [x0, #104]\n"
"	stp d10, d11, [x0, #120]\n"
"	stp d12, d13, [x0, #136]\n"
"	stp d14, d15, [x0, #152]\n"
"	mrs x2, tpidr_el0\n"
"	str x2, [x0, #168]\n"
"	ldp x19, x20, [x1, #0]\n"
"	ldp x21, x22, [x1, #16]\n"
"	ldp x23, x24, [x1, #32]\n"
"	ldp x25, x26, [x1, #48]\n"
"	ldp x27, x28, [x1, #64]\n"
"	ldp x29, x30, [x1, #80]\n"
"	ldp d8, d9,   [x1, #104]\n"
"	ldp d10, d11, [x1, #120]\n"
"	ldp d12, d13, [x1, #136]\n"
"	ldp d14, d15, [x1, #152]\n"
"	ldr x2, [x1, #168]\n"
"	msr tpidr_el0, x2\n"
"	ldr x2, [x1, #96]\n"
"	mov sp, x2\n"
"	ret\n"
/* trampoline: x19=entry fn, x20=arg (seeded by akctx_make) */
"	.globl akctx_tramp\n"
"	.type akctx_tramp,%function\n"
"akctx_tramp:\n"
"	mov x0, x20\n"
"	blr x19\n"
"	bl  abort\n"   /* entry must never return in this test */
);

extern void akctx_switch(akctx_t *prev, akctx_t *next);
extern void akctx_tramp(void);

static inline uint64_t
rd_tpidr(void)
{
	uint64_t v;
	__asm__ volatile("mrs %0, tpidr_el0" : "=r"(v));
	return v;
}

static void
akctx_make(akctx_t *c, void *stack_base, void (*entry)(void *), void *arg)
{
	memset(c, 0, sizeof(*c));
	uintptr_t sp = ((uintptr_t)stack_base + STACKSIZE) & ~(uintptr_t)15;
	c->reg[0] = (uint64_t)(uintptr_t)entry;        /* x19 */
	c->reg[1] = (uint64_t)(uintptr_t)arg;          /* x20 */
	c->reg[11] = (uint64_t)(uintptr_t)akctx_tramp; /* x30 -> tramp on first ret */
	c->reg[12] = (uint64_t)sp;                     /* sp  */
	c->reg[21] = rd_tpidr();   /* inherit creator's TLS base so libc works */
}

static akctx_t ctx_main, ctx_a, ctx_b;
static int hops;

static void *
fiber_stack(void)
{
	void *s = mmap(NULL, STACKSIZE, PROT_READ | PROT_WRITE,
	    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (s == MAP_FAILED) { perror("mmap"); exit(2); }
	return s;
}

static void
fiber_a(void *arg)
{
	const char *name = arg;
	for (int i = 0; i < ROUNDS; i++) {
		printf("[%s] hop %d\n", name, hops++);
		fflush(stdout);
		akctx_switch(&ctx_a, &ctx_b);
	}
	akctx_switch(&ctx_a, &ctx_main); /* done: hand control back to main */
}

static void
fiber_b(void *arg)
{
	const char *name = arg;
	for (;;) {
		printf("[%s] hop %d\n", name, hops++);
		fflush(stdout);
		akctx_switch(&ctx_b, &ctx_a);
	}
}

int
main(void)
{
	akctx_make(&ctx_a, fiber_stack(), fiber_a, (void *)"A");
	akctx_make(&ctx_b, fiber_stack(), fiber_b, (void *)"B");

	printf("asmctx_smoke: start\n");
	fflush(stdout);
	akctx_switch(&ctx_main, &ctx_a); /* returns here when fiber_a finishes */

	printf("main resumed after %d hops, %s\n", hops,
	    hops == 2 * ROUNDS ? "OK" : "WRONG");
	fflush(stdout);
	return hops == 2 * ROUNDS ? 0 : 1;
}
