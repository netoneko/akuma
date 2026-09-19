//! embed-tls-host — `libakuma-tls`'s handshake, on the development host.
//!
//! One question: **when our TLS stack cannot reach a server, is that our TLS
//! stack or is it Akuma?** This runs the same `embedded-tls` version, the same
//! single cipher suite (`Aes128GcmSha256`) and the same `NoVerify` that
//! `libakuma-tls::TlsStream::connect` does, over an ordinary
//! `std::net::TcpStream`. Nothing of Akuma is in the path.
//!
//! So a failure here is a `libakuma-tls` (or server-compatibility) bug and the
//! kernel is exonerated; a success here against a host that fails on the box is
//! the opposite, and localises it to the guest's sockets.
//!
//! ```sh
//! cargo run --release -- api.z.ai api.github.com          # one line per host
//! RUST_LOG=trace cargo run --release -- api.z.ai           # where it dies
//! ```
//!
//! Exit status is the number of hosts that failed, so it works as a gate.

use std::env;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use embedded_tls::blocking::{TlsConfig, TlsConnection, TlsContext};
use embedded_tls::{Aes128GcmSha256, TlsError, UnsecureProvider};

/// Matches `libakuma_tls::TLS_RECORD_SIZE` exactly. It is load-bearing: at
/// 16384 this probe would reproduce `TLS_BUFFER_TRUNCATION_FIX.md` instead of
/// whatever is actually being investigated, and the two look identical from
/// outside (`InsufficientSpace` both times).
const TLS_RECORD_SIZE: usize = 17408;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let hosts: Vec<String> = {
        let a: Vec<String> = env::args().skip(1).collect();
        if a.is_empty() {
            // Hosts that MUST handshake, so a clean run means exit 0 and this
            // works as a gate. `api.z.ai` is the regression this probe was
            // written for — a server whose advisory `supported_groups` hint
            // contains `curveSM2` — and `api.github.com` is the control that
            // sends no such hint and has always worked.
            //
            // **`z.ai` (the apex) is deliberately NOT here.** It fails with a
            // genuine `protocol_version` alert from the peer: that host will not
            // do TLS 1.3 on our offer at all, which is a different defect from
            // the one this probe guards and is not fixed by the group patch. It
            // is a real specimen, so run it explicitly — `./run.sh z.ai` — but
            // keeping a permanent failure in the default set would make the exit
            // status useless.
            vec!["api.z.ai".into(), "api.github.com".into()]
        } else {
            a
        }
    };

    let mut failed = 0;
    for host in &hosts {
        match probe(host) {
            Ok(ms) => println!("[embtls] {host:<20} OK    handshake_ms={ms}"),
            Err(e) => {
                failed += 1;
                println!("[embtls] {host:<20} FAIL  {}  ({e:?})", name(&e));
            }
        }
    }
    println!("[embtls] {} host(s), {failed} failed", hosts.len());
    std::process::exit(failed);
}

/// Handshake only — no request, no body. The handshake is the whole question,
/// and stopping there keeps a server's application-layer behaviour (a 301 to a
/// slow marketing page, say) out of the measurement.
fn probe(host: &str) -> Result<u128, TlsError> {
    let t0 = Instant::now();
    let sock = TcpStream::connect((host, 443)).map_err(|_| TlsError::Io(embedded_io::ErrorKind::Other))?;
    // Without a timeout a stalled handshake hangs the whole run, and "it hung"
    // is the one result this probe exists to replace with a named error.
    let _ = sock.set_read_timeout(Some(Duration::from_secs(20)));
    let _ = sock.set_write_timeout(Some(Duration::from_secs(20)));
    let sock = embedded_io_adapters::std::FromStd::new(sock);

    let mut read_buf = vec![0u8; TLS_RECORD_SIZE];
    let mut write_buf = vec![0u8; TLS_RECORD_SIZE];

    let config = TlsConfig::new().with_server_name(host);
    let mut conn: TlsConnection<_, Aes128GcmSha256> =
        TlsConnection::new(sock, &mut read_buf, &mut write_buf);
    // `UnsecureProvider` is 0.19's spelling of what used to be the `NoVerify`
    // type argument — verification moved into the `CryptoProvider`. Same
    // don't-verify behaviour, and the same call `libakuma-tls` now makes.
    conn.open(TlsContext::new(
        &config,
        UnsecureProvider::new::<Aes128GcmSha256>(rand::rngs::OsRng),
    ))?;
    Ok(t0.elapsed().as_millis())
}

/// The same vocabulary `libakuma_tls::tls_error_name` prints, so a line from
/// this probe and a line from `hget` can be compared without translation.
fn name(e: &TlsError) -> &'static str {
    match e {
        TlsError::DecodeError => "decode error",
        TlsError::InsufficientSpace => "insufficient buffer space",
        TlsError::InvalidCipherSuite => "cipher suite refused",
        TlsError::InvalidKeyShare => "invalid key_share (HelloRetryRequest?)",
        TlsError::UnknownExtensionType => "unknown extension type",
        TlsError::InvalidSupportedVersions => "invalid supported_versions",
        TlsError::HandshakeAborted(_, _) => "handshake aborted by peer",
        TlsError::AbortHandshake(_, _) => "handshake aborted by us",
        TlsError::ConnectionClosed => "peer closed the connection",
        TlsError::Io(_) => "transport I/O error",
        _ => "other",
    }
}
