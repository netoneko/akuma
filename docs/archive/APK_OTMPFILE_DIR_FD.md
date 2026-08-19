# apk "installation succeeded but errored": O_TMPFILE answered with a directory fd

**Status: FIXED 2026-08-19.** Found while reproducing the `apk add` failure
from `BENCHMARK_PERFORMANCE_ATTEMPT_0.md` § "Prerequisite" (2026-08-19) and
`DEVBOX_ISSUES.md` Issue 18's aside ("apk on the same image fails at *its*
write step too"). Recorded as Issue 20 there.

## Symptom

On a devbox-smoltcp guest (any fresh disk; reproduced on a cold-start image at
SMP=2, MEMORY=2048, `release-smp-shared`):

```
# apk update
WARNING: updating and opening https://dl-cdn.alpinelinux.org/.../APKINDEX.tar.gz: Is a directory
2 unavailable, 0 stale; 0 distinct packages available          # rc=2

# apk add tar
(1/3) Installing musl (1.2.6-r2)
(2/3) Installing acl-libs (2.3.2-r1)
(3/3) Installing tar (1.35-r5)
ERROR: System state may be inconsistent: failed to write database: Is a directory
```

The package **files install fine** — `tar --version` works right after — but
every apk database/cache write fails with `EISDIR`, so apk exits non-zero and
the installed-state database is not updated. Superficially resembles Issue 8
("1 error from busybox's own trigger"), but the post-install scripts are
innocent this time: `apk add busybox` runs `busybox-*.post-install` and
`.trigger` cleanly once this is fixed. The suspect class is "apk's atomic
writes", not script execution.

## Root cause

apk-tools 3 writes every database/index file **atomically**
(`io.c: __apk_ostream_to_file`). When `/proc/self/fd` resolves — which it has
on Akuma since the 2026-08-16 `resolve_self` fix — it prefers
`O_TMPFILE`:

```c
if (is_proc_fd_ok()) {
    tmpfile = true;
    fd = openat(atfd, path, O_RDWR | O_TMPFILE | O_CLOEXEC, mode);   // path = "."
}
```

Akuma's `sys_openat` neither implemented `O_TMPFILE` nor rejected write-mode
opens of directories. So the call **succeeded**, returning a writable fd *on
the directory itself* (`/var/cache/apk`, `/lib/apk/db`). Every subsequent
`write()` then failed `FsError::NotAFile → EISDIR` — the same errno, but at
the wrong syscall, after which apk's tidy error reporting blamed the database.

Measured in-guest with a static musl probe (source below), before the fix:

| call | Linux aarch64 | Akuma (before) | Akuma (after) |
|---|---|---|---|
| `access("/proc/self/fd", F_OK)` | 0 | 0 (→ apk takes the O_TMPFILE path) | 0 |
| `openat(dfd, ".", O_RDWR\|O_TMPFILE)` | tmpfile fd | **dir fd** | EINVAL |
| `write()` on that fd | 5 | **-1 EISDIR** (apk's error) | — |
| `openat(dfd, ".", O_RDWR)` plain | -1 EISDIR | **dir fd** | EISDIR |
| fallback: `.tmp.<pid>` + `renameat` | works | works (unreached) | works |

The whole failure needs three things to line up, which is why it looks
intermittent across images: apk-tools **3.x** (2.x did not use O_TMPFILE), a
kernel where `/proc/self/fd` resolves (so apk prefers the tmpfile route), and
this kernel's missing flag handling. Images populated by
`populate_disk.sh`/`bootstrap.sh` on the host never see it because host-side
apk runs on Linux.

## Fix

Two guards in `src/syscall/fs.rs::sys_openat`, plus the flag constant
(`crates/akuma-exec/src/process/types.rs::open_flags::O_TMPFILE`):

1. `O_TMPFILE` is answered with `EINVAL` — what Linux kernels predating
   tmpfiles returned. Portable callers (apk-tools 3 `__apk_ostream_to_file`)
   treat *any* failure as "no tmpfiles here" and fall back to a named
   `.tmp.<pid>` + `renameat`, which works. **Arch detail that bit once during
   the fix: arm64 keeps the 32-bit ARM fcntl values — `O_DIRECTORY = 0o40000`,
   `O_TMPFILE = 0o20040000` — *not* the asm-generic `0o200000`/`0o20200000`
   that x86_64/riscv use.** An `O_TMPFILE` mask-check written against the
   asm-generic numbers never matches real musl/glibc/Go binaries on this
   target; the dir-write guard catches them with EISDIR (still makes apk work)
   but the errno is wrong.
2. Write-mode (`O_WRONLY|O_RDWR`) opens of an existing **directory** return
   `EISDIR` at open() time, matching Linux `fs/namei.c: may_open`. Read-only
   directory opens (getdents, `ls`) are unaffected. Before, the kernel handed
   out a writable directory fd whose every write failed EISDIR anyway — no
   working behaviour could depend on the old semantics.

Regression coverage: `test_openat` (boot suite) grew cases 9-10
(`dir-write-EISDIR`, both absolute and dirfd-relative `.`, with a read-only
control, and `O_TMPFILE-EINVAL`).

## Verify

```
INSTANCE=1 SMP=2 MEMORY=2048 DISK=<fresh small disk> scripts/cargo_runner.sh target/.../akuma
# in-guest:
apk update                       # -> OK: N distinct packages available
apk add tar                      # -> OK: ... in N packages, rc=0
apk add busybox                  # -> post-install + trigger execute, OK, rc=0
tar --version; apk info -e tar   # both answer from the written db
ls -la /lib/apk/db/installed     # non-zero size (database actually written)
```

Boot suite: `[Test] openat PASSED (10 cases: .../dir-write-EISDIR/O_TMPFILE-EINVAL)`.

## The probe

Static musl aarch64, built with `docker run --platform linux/arm64 alpine
apk add musl-dev gcc && gcc -static`; shipped in-guest via base64 over ssh
stdin (scp needs a guest-side `scp -t` the cold image lacks):

```c
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdio.h>
#include <sys/syscall.h>
int main(void) {
    printf("O_TMPFILE=0%o O_DIRECTORY=0%o\n", O_TMPFILE, O_DIRECTORY);
    int dfd = open("/var/cache/apk", O_RDONLY | O_DIRECTORY);
    errno = 0;
    long r = syscall(SYS_openat, dfd, ".", O_RDWR | O_TMPFILE | O_CLOEXEC, 0644);
    printf("tmpfile open: ret=%ld errno=%d (%s)\n", r, errno, strerror(errno));
    errno = 0;
    r = syscall(SYS_openat, dfd, ".", O_RDWR, 0);
    printf("plain dir O_RDWR: ret=%ld errno=%d (%s)\n", r, errno, strerror(errno));
    return 0;
}
```

(Control it under the same docker invocation — on real Linux the tmpfile open
returns a fd and the plain dir open returns EISDIR.)

## Background

- [`DEVBOX_ISSUES.md`](DEVBOX_ISSUES.md) — Issue 20 (this bug), Issue 18 (the
  `box pull` failure whose aside surfaced it; open), Issue 8 (the *earlier*
  "1 error", root-caused 2026-08-16 to the shebang argv[0] bug — different
  disease, similar-looking symptom).
- [`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`](BENCHMARK_PERFORMANCE_ATTEMPT_0.md) §
  "Prerequisite to clear first" — the live report this investigation started
  from (`apk add tar`, 2026-08-19).
- [`REDIS_END_TO_END.md`](REDIS_END_TO_END.md) §4 — the `/proc/self` resolution
  fix that, as a side effect, made apk prefer the O_TMPFILE path.
- apk-tools 3 `src/io.c::__apk_ostream_to_file` and `src/database.c:
  open_repository` ("updating and opening %s" is printed when *both* the
  cache download and the cache open fail; the printed errno is the download's).
