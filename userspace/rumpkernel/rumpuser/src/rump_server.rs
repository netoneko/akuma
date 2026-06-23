//! Rust port of our hand-written `rump_server.c` wrapper (FIBER_HANDOFF.md
//! "PLANNED" port; RUMP_SYSPROXY Step 2). This is the per-box rump SERVER
//! payload: a long-lived process that owns one NetBSD rump TCP/IP stack and
//! exposes it over a sysproxy channel (an inherited kernel-pipe fd, or a listen
//! URL) so other processes can run `rump_sys_*` against this stack.
//!
//! This file is ORIGINAL Akuma code. It only *calls* the rump public API
//! (`rump_init`, `rump_pub_netconfig_*`), our sysproxy bridge `rumpuser_sp_init_fd`
//! (from `sp_serve_fd.c`, which `#include`s NetBSD's UNMODIFIED `rumpuser_sp.c`),
//! and libc — so it ports cleanly to Rust while the sysproxy protocol server
//! itself stays NetBSD C. See docs/FIBER_HANDOFF.md.
//!
//! Feature-gated behind `rump_server_main` so the `#[no_mangle] main` never
//! collides with the `main` of the OTHER consumers of `librumpuser_akuma.a`
//! (rumphttp, sic, the test harnesses). The shipped `rump_server` binary builds
//! the staticlib WITH this feature and drops `rump_server.c` from the gcc link
//! line (crt0 calls this Rust `main`); see docker-build-rump-server.sh.

use core::ffi::{c_char, c_int, c_uint, c_void};
use core::ptr;

// ── libc (resolved by the musl libc the final program links) ──────────────────
extern "C" {
    fn printf(fmt: *const c_char, ...) -> c_int;
    fn open(path: *const c_char, oflag: c_int, ...) -> c_int;
    fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn mkdir(path: *const c_char, mode: c_uint) -> c_int;
    fn sleep(seconds: c_uint) -> c_uint;
    fn atoi(s: *const c_char) -> c_int;
    fn strcmp(a: *const c_char, b: *const c_char) -> c_int;
    fn strlen(s: *const c_char) -> usize;
    fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void;
    fn setvbuf(stream: *mut c_void, buf: *mut c_char, mode: c_int, size: usize) -> c_int;
    // musl exports a real `stdout` symbol (a `FILE *const`); we only need the
    // pointer value to hand to setvbuf.
    static stdout: *mut c_void;
}

// ── rump public API + our sysproxy bridge ─────────────────────────────────────
extern "C" {
    fn rump_init() -> c_int;
    fn rump_pub_netconfig_ifcreate(ifname: *const c_char) -> c_int;
    fn rump_pub_netconfig_dhcp_ipv4_oneshot(ifname: *const c_char) -> c_int;
    fn rump_init_server(url: *const c_char) -> c_int;
    // serve sysproxy on a pre-connected fd (kernel-pipe transport); from sp_serve_fd.c.
    fn rumpuser_sp_init_fd(fd: c_int, host: *const c_char, vers: *const c_char, arch: *const c_char) -> c_int;
    // rumpuser backend introspection (defined in this crate): 1 if rump kthreads
    // are cooperative fibers on one OS thread (else 0 = pthread). _yield runs the
    // fiber scheduler. Declared extern so we resolve the symbol regardless of which
    // backend module (fiber.rs / lib.rs) defines it under the active cfg.
    fn rumpuser_akuma_cooperative() -> c_int;
    fn rumpuser_akuma_yield();
}

// open(2) flags / setvbuf mode (Linux/musl aarch64; asm-generic).
const O_WRONLY: c_int = 1;
const O_CREAT: c_int = 0o100;
const O_TRUNC: c_int = 0o1000;
const _IONBF: c_int = 2;

/// 1 if `--net` was given (for the `(net=%s)` log fields), else 0 → "up"/"off".
unsafe fn netstr(do_net: c_int) -> *const c_char {
    if do_net != 0 {
        c"up".as_ptr()
    } else {
        c"off".as_ptr()
    }
}

/// Redirect stdout+stderr to `path` (creating parent dirs best-effort) so all of
/// rump_server's output — including rump_init's verbose dprintf on fd 2 — lands
/// in the box log instead of the (undrained) kernel ProcessChannel. Faithful port
/// of the C `redirect_log` (used by `--log /var/log/box/<id>/rump_server.log`).
unsafe fn redirect_log(path: *const c_char) {
    let n = strlen(path);
    if n >= 256 {
        return;
    }
    let mut tmp = [0u8; 256];
    // copy path including its NUL terminator
    memcpy(tmp.as_mut_ptr() as *mut c_void, path as *const c_void, n + 1);
    // mkdir each parent component (ignore EEXIST/errors); skip index 0 so a
    // leading '/' isn't a mkdir("").
    let mut p = 1usize;
    while p < 256 && tmp[p] != 0 {
        if tmp[p] == b'/' {
            tmp[p] = 0;
            mkdir(tmp.as_ptr() as *const c_char, 0o755);
            tmp[p] = b'/';
        }
        p += 1;
    }
    let lf = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0o644 as c_int);
    if lf >= 0 {
        dup2(lf, 1); // stdout (printf)
        dup2(lf, 2); // stderr (rumpuser dprintf)
        if lf > 2 {
            close(lf);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: c_int, argv: *const *const c_char) -> c_int {
    let mut url: *const c_char = c"unix:///tmp/rump_server.sock".as_ptr();
    let mut ifname: *const c_char = c"virt0".as_ptr();
    let mut logpath: *const c_char = ptr::null(); // --log: redirect stdout/stderr here
    let mut serve_fd: c_int = -1; // >=0: serve sysproxy on this inherited fd
    let mut do_net: c_int = 0; // --net: bring up virt0 + DHCP over /dev/net/tap0
    let mut url_given: c_int = 0; // a positional URL was passed (legacy listen mode)

    // Modes:
    //   rump_server --fd N [--net]     serve on inherited fd N (Akuma kernel-pipe)
    //   rump_server [url] [--net]      listen on a URL (container/path tests)
    // --net brings the rump stack online (needs RUMP_NIC=1 / a tap); without it
    // the stack still serves control-plane syscalls (e.g. socket()).
    let mut i: c_int = 1;
    while i < argc {
        let arg = *argv.add(i as usize);
        if strcmp(arg, c"--fd".as_ptr()) == 0 && i + 1 < argc {
            i += 1;
            serve_fd = atoi(*argv.add(i as usize));
        } else if strcmp(arg, c"--net".as_ptr()) == 0 {
            do_net = 1;
        } else if strcmp(arg, c"--if".as_ptr()) == 0 && i + 1 < argc {
            i += 1;
            ifname = *argv.add(i as usize);
        } else if strcmp(arg, c"--log".as_ptr()) == 0 && i + 1 < argc {
            i += 1;
            logpath = *argv.add(i as usize);
        } else if *arg as u8 != b'-' {
            url = arg;
            url_given = 1;
        }
        i += 1;
    }

    if !logpath.is_null() {
        redirect_log(logpath);
    }

    setvbuf(stdout, ptr::null_mut(), _IONBF, 0);

    printf(c"RUMP_SERVER: rump_init...\n".as_ptr());
    let mut rv = rump_init();
    if rv != 0 {
        printf(c"RUMP_SERVER: FAIL rump_init=%d\n".as_ptr(), rv);
        return 1;
    }

    if do_net != 0 {
        rv = rump_pub_netconfig_ifcreate(ifname);
        printf(c"RUMP_SERVER: ifcreate %s -> %d\n".as_ptr(), ifname, rv);
        rv = rump_pub_netconfig_dhcp_ipv4_oneshot(ifname);
        printf(c"RUMP_SERVER: dhcp_ipv4_oneshot %s -> %d\n".as_ptr(), ifname, rv);
        if rv != 0 {
            printf(c"RUMP_SERVER: WARN — DHCP rv=%d (continuing)\n".as_ptr(), rv);
        }
    }

    if serve_fd >= 0 {
        rv = rumpuser_sp_init_fd(serve_fd, c"NetBSD".as_ptr(), c"7.99.34".as_ptr(), c"evbarm64".as_ptr());
        printf(c"RUMP_SERVER: rumpuser_sp_init_fd(%d) -> %d\n".as_ptr(), serve_fd, rv);
        if rv != 0 {
            printf(c"RUMP_SERVER: FAIL — sp_init_fd rv=%d\n".as_ptr(), rv);
            return 1;
        }
        printf(c"RUMP_SERVER: SERVING sysproxy on fd %d (net=%s)\n".as_ptr(), serve_fd, netstr(do_net));
    } else if url_given != 0 {
        rv = rump_init_server(url);
        printf(c"RUMP_SERVER: rump_init_server(%s) -> %d\n".as_ptr(), url, rv);
        if rv != 0 {
            printf(c"RUMP_SERVER: FAIL — rump_init_server rv=%d\n".as_ptr(), rv);
            return 1;
        }
        printf(c"RUMP_SERVER: LISTENING — sysproxy on %s (iface %s)\n".as_ptr(), url, ifname);
    } else {
        // No fd handed in and no URL: just keep the NetBSD stack alive in this box
        // (e.g. herd-managed `--net` service). The kernel-as-client wires the
        // sysproxy channel separately; this proves the stack boots boxed.
        printf(c"RUMP_SERVER: stack up, no sysproxy channel (net=%s) — staying alive\n".as_ptr(), netstr(do_net));
    }

    // The sp server / rump kthreads do the work; the main thread just parks.
    //
    // Under the FIBER (cooperative) backend the sp server's receiver and its
    // per-request workers are FIBERS on this one OS thread. The main thread is
    // itself the initial fiber, so it MUST cooperatively yield to let them run. A
    // real sleep()/nanosleep() here is an Akuma kernel syscall that blocks the
    // single OS thread (it is NOT the cooperative rumpuser_clock_sleep hypercall),
    // starving every fiber — the receiver never sends its banner and the client
    // handshake times out (the EIO we hit). So spin on the fiber yield.
    //
    // Under the PTHREAD backend the receiver runs on its own OS thread, so the
    // main thread can just block-sleep (cheaper than a busy-yield).
    // NOTE: do NOT use pause() — on Akuma musl pause() compiles to ppoll(NULL,0)
    // which returns immediately (sys_ppoll: nfds==0 -> 0), a CPU-pegging busy-loop.
    if rumpuser_akuma_cooperative() != 0 {
        loop {
            rumpuser_akuma_yield();
        }
    } else {
        loop {
            sleep(3600);
        }
    }
}
