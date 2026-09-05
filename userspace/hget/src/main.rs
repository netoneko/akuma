//! `hget` — fetch a URL and write the body to stdout.
//!
//! The reason this exists rather than a staged `curl`: on the amd64 target
//! `busybox wget https://...` fails with `socketpair: Function not implemented`,
//! because busybox shells out to a separate `ssl_client` process over a
//! socketpair — a syscall this kernel does not have, and a helper binary that
//! is not on the image. `bootstrap/bin/curl` is aarch64. Alpine's `curl`
//! package is dynamically linked and this kernel has no `PT_INTERP` support.
//!
//! `libakuma-tls` does TLS in-process with `embedded-tls`, is already used by
//! `box pull` and `meow`, and builds for `x86_64-unknown-none` unchanged. So
//! the shortest path to HTTPS here is thirty lines, not a foreign binary.
//!
//! ```text
//! hget https://example.com
//! hget http://example.com          # plain HTTP works too
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use libakuma::{arg, exit, fd, print, write};

/// Cap on a response body. A fetch on this target holds the whole body in
/// memory — there is nowhere to stream it to — so an unbounded download is an
/// unbounded allocation on a kernel whose allocator failing is the thing that
/// takes the machine down.
const MAX_BODY: usize = 8 * 1024 * 1024;

#[no_mangle]
pub extern "C" fn main() {
    let Some(url) = arg(1) else {
        print("usage: hget <url>\n");
        exit(2);
    };

    match libakuma_tls::http::https_fetch(url, Some(MAX_BODY)) {
        Ok(body) => {
            write(fd::STDOUT, &body);
            exit(0);
        }
        Err(e) => {
            // The error, then the URL: on a console being read across a room,
            // the thing that went wrong should not be at the end of the line.
            // `Error` has no `Display` and several variants carry a `String`,
            // so this names them by hand rather than `{:?}` — which would pull
            // core::fmt's machinery in for a message printed at most once.
            print("hget: failed: ");
            print(match &e {
                libakuma_tls::Error::DnsError => "DNS resolution failed",
                libakuma_tls::Error::ConnectionError(m) => m.as_str(),
                libakuma_tls::Error::TlsError(_) => "TLS handshake failed",
                libakuma_tls::Error::HttpError(m) => m.as_str(),
                libakuma_tls::Error::InvalidUrl => "invalid URL",
                libakuma_tls::Error::IoError => "I/O error",
            });
            print("\n  url: ");
            print(url);
            print("\n");
            exit(1);
        }
    }
}
