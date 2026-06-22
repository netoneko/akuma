/*
 * csupport.c — the one rumpuser hypercall that needs C: rumpuser_dprintf.
 *
 * It's a printf-style variadic, which is painful in stable Rust. Everything
 * else in the rumpuser layer is Rust (src/lib.rs); this is just the variadic
 * console diagnostic, forwarded to stderr via vfprintf.
 */
#include <stdarg.h>
#include <stdio.h>
#include <stddef.h>

/*
 * DIAGNOSTIC: override rump's optimized aarch64 memset (rumpns_memset), which
 * miscomputes its loop bound on a small zero-fill in our environment and walks
 * off the allocation (SIGSEGV in early uvm_init). A trivial byte loop lets us
 * confirm that's the sole blocker. Linked with -Wl,--allow-multiple-definition
 * so this strong definition wins over the one in librump.a. (Proper fix: build
 * librump with the generic libkern memset, or carry this override.)
 */
void *
rumpns_memset(void *b, int c, size_t len)
{
	unsigned char *p = b;
	while (len--)
		*p++ = (unsigned char)c;
	return b;
}

void *
rumpns_memcpy(void *d, const void *s, size_t n)
{
	unsigned char *dp = d;
	const unsigned char *sp = s;
	while (n--)
		*dp++ = *sp++;
	return d;
}

void *
rumpns_memmove(void *d, const void *s, size_t n)
{
	unsigned char *dp = d;
	const unsigned char *sp = s;
	if (dp < sp) {
		while (n--)
			*dp++ = *sp++;
	} else {
		dp += n;
		sp += n;
		while (n--)
			*--dp = *--sp;
	}
	return d;
}

size_t
rumpns_strlen(const char *s)
{
	const char *p = s;
	while (*p)
		p++;
	return (size_t)(p - s);
}

int
rumpns_strcmp(const char *a, const char *b)
{
	while (*a && (*a == *b)) {
		a++;
		b++;
	}
	return (int)(unsigned char)*a - (int)(unsigned char)*b;
}

int
rumpns_strncmp(const char *a, const char *b, size_t n)
{
	while (n && *a && (*a == *b)) {
		a++;
		b++;
		n--;
	}
	if (n == 0)
		return 0;
	return (int)(unsigned char)*a - (int)(unsigned char)*b;
}

void
rumpuser_dprintf(const char *fmt, ...)
{
	va_list ap;
	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
}

/*
 * The prebuilt Rust `core` carries an unwinding-table reference to
 * rust_eh_personality even though our rumpuser staticlib is panic=abort and
 * never unwinds. Provide a no-op so the final link resolves; it is never called.
 * (The alternative is rebuilding core with -Cpanic=immediate-abort via nightly
 * build-std, like Akuma's other userspace — done when we link on Akuma proper.)
 */
void rust_eh_personality(void) {}

