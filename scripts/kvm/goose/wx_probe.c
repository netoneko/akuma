#include <sys/mman.h>
#include <stdio.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
static int fails;
static void t(const char *n, int got, int want) {
    printf("%s %s (got %d want %d)\n", got == want ? "PASS" : "FAIL", n, got, want);
    if (got != want) fails++;
}
static void *rw(void) { return mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0); }
static int mp(void *p, int prot) { return mprotect(p, 4096, prot) == 0 ? 0 : errno; }
int main(void) {
    void *p = mmap(0, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    t("mmap RWX refused", p == MAP_FAILED ? errno : 0, EACCES);
    p = rw();
    t("untouched RW -> RX allowed", mp(p, PROT_READ | PROT_EXEC), 0);
    p = rw(); *(volatile char *)p = 1;
    t("user-written -> RX refused", mp(p, PROT_READ | PROT_EXEC), EACCES);
    t("user-written still fine as data", mp(p, PROT_READ | PROT_WRITE), 0);
    p = rw(); { int f = open("/tmp/wx.c", O_RDONLY); read(f, p, 16); close(f); }
    t("kernel-written (read) -> RX refused", mp(p, PROT_READ | PROT_EXEC), EACCES);
    p = rw(); *(volatile char *)p = 1;
    t("written -> R allowed", mp(p, PROT_READ), 0);
    t("written, then R, then RX refused (written bit survives the remap)", mp(p, PROT_READ | PROT_EXEC), EACCES);
    p = rw();
    t("mprotect RWX refused", mp(p, PROT_READ | PROT_WRITE | PROT_EXEC), EACCES);
    p = mmap(0, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    t("mmap RX anonymous allowed", p == MAP_FAILED ? errno : 0, 0);
    p = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    *((volatile char *)p + 4096) = 1; /* only the second page is written */
    t("range with one written page -> RX refused", mprotect(p, 8192, PROT_READ | PROT_EXEC) ? errno : 0, EACCES);
    *(volatile char *)p = 7; /* the first page must still be writable: the refusal changed nothing */
    t("...and the first page is still writable", 0, 0);
    printf("%s: %d failure(s)\n", fails ? "WX FAIL" : "WX OK", fails);
    return fails != 0;
}
