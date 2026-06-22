/*
 * csupport.c — the one rumpuser hypercall that needs C: rumpuser_dprintf.
 *
 * It's a printf-style variadic, which is painful in stable Rust. Everything
 * else in the rumpuser layer is Rust (src/lib.rs); this is just the variadic
 * console diagnostic, forwarded to stderr via vfprintf.
 */
#include <stdarg.h>
#include <stdio.h>

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

