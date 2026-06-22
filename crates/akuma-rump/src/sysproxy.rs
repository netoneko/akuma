//! rumpsp client protocol — RUMP_SYSPROXY.md Step 4 (kernel-as-client), the
//! host-testable core.
//!
//! The Akuma kernel, acting as the sysproxy *client* for a `stack=rump` box,
//! forwards that box's AF_INET syscalls to the box's `rump_server` over a unix
//! socket. This module implements the wire protocol (the same one NetBSD's
//! `lib/librumpclient/rumpclient.c` speaks, framed by `sp_common.c`) independent
//! of the kernel's real socket + user-memory plumbing, so it can be exercised on
//! the host with mocks. The kernel supplies:
//!
//! - a [`Transport`] over the real AF_UNIX connection to the box server, and
//! - a [`ClientMem`] that copies in/out of the *calling box process's* user
//!   memory (where the syscall's pointer args live).
//!
//! Protocol (little-endian, NetBSD/aarch64 LP64 layout; see `sp_common.c`):
//! 24-byte header `{ len:u64, reqno:u64, class:u16, type:u16, u:u32 }`, then
//! `len-24` data bytes. A SYSCALL request may trigger server→client COPYIN /
//! COPYOUT / ANONMMAP callbacks before the final RESP — exactly the loop in
//! `rumpclient.c:cliwaitresp` + `handlereq`.
//!
//! SECURITY (RUMP_SYSPROXY.md "Security / hardening TODOs"): every length/addr in
//! a server callback is server-supplied. The kernel's [`ClientMem`] impl MUST
//! bounds-check `addr`/`len` against the box process's mappings — never trust
//! these values. This module caps allocation sizes defensively but cannot know
//! the address space; that check lives in the impl.

extern crate alloc;
use alloc::vec::Vec;

/// Header size on the wire (`sizeof(struct rsp_hdr)`).
pub const HDRSZ: usize = 24;

// rsp_class
const RUMPSP_REQ: u16 = 0;
const RUMPSP_RESP: u16 = 1;
const RUMPSP_ERROR: u16 = 2;

// rsp_type
const RUMPSP_HANDSHAKE: u16 = 0;
const RUMPSP_SYSCALL: u16 = 1;
const RUMPSP_COPYIN: u16 = 2;
const RUMPSP_COPYINSTR: u16 = 3;
const RUMPSP_COPYOUT: u16 = 4;
const RUMPSP_COPYOUTSTR: u16 = 5;
const RUMPSP_ANONMMAP: u16 = 6;
const RUMPSP_RAISE: u16 = 8;

// handshake subtype (enum { HANDSHAKE_GUEST, HANDSHAKE_AUTH, ... })
const HANDSHAKE_GUEST: u32 = 0;

/// Defensive ceiling on any single server-requested copy/alloc (16 MiB).
///
/// A well-behaved server never asks for more for a socket syscall; this bounds a
/// malformed/hostile server before the [`ClientMem`] bounds check runs.
pub const MAX_XFER: usize = 16 * 1024 * 1024;

/// Transport-level failure (connection dead / short read). Distinct from a
/// NetBSD errno, which rides in the protocol payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportErr;

/// Byte-stream transport to the box's rump_server (the kernel wraps its AF_UNIX
/// client socket; tests use an in-memory script).
pub trait Transport {
    /// Fill `buf` completely or fail (a closed connection is a failure).
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), TransportErr>;
    /// Write all of `buf` or fail.
    fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportErr>;
}

/// Access to the calling box process's user memory, for the copyin/copyout
/// callbacks the server issues against the syscall's pointer args.
///
/// Implementations MUST validate `addr`/`len` against the process's address
/// space (the server is not trusted — see the module security note).
pub trait ClientMem {
    /// Read `len` bytes from user `addr` into `out` (cleared first). Err = a
    /// NetBSD errno (e.g. `EFAULT`) to fail the copyin with.
    fn copyin(&mut self, addr: u64, len: usize, out: &mut Vec<u8>) -> Result<(), i32>;
    /// Read a NUL-terminated string at `addr`, at most `max` bytes incl. NUL.
    fn copyinstr(&mut self, addr: u64, max: usize, out: &mut Vec<u8>) -> Result<(), i32>;
    /// Write `data` to user `addr`. Err = a NetBSD errno.
    fn copyout(&mut self, addr: u64, data: &[u8]) -> Result<(), i32>;
    /// Anonymously map `len` bytes in the process; return the address, or 0 on
    /// failure (the server tolerates a NULL map address).
    fn anonmmap(&mut self, len: usize) -> u64;
}

/// Outcome of one proxied syscall: NetBSD `(retval[0], retval[1])` on success,
/// or a NetBSD errno on failure.
pub type SyscallResult = Result<[i64; 2], i32>;

fn enc_hdr(len: u64, reqno: u64, class: u16, typ: u16, u: u32) -> [u8; HDRSZ] {
    let mut h = [0u8; HDRSZ];
    h[0..8].copy_from_slice(&len.to_le_bytes());
    h[8..16].copy_from_slice(&reqno.to_le_bytes());
    h[16..18].copy_from_slice(&class.to_le_bytes());
    h[18..20].copy_from_slice(&typ.to_le_bytes());
    h[20..24].copy_from_slice(&u.to_le_bytes());
    h
}

struct DecHdr {
    len: u64,
    reqno: u64,
    class: u16,
    typ: u16,
    u: u32,
}

fn dec_hdr(h: &[u8; HDRSZ]) -> DecHdr {
    DecHdr {
        len: u64::from_le_bytes(h[0..8].try_into().unwrap()),
        reqno: u64::from_le_bytes(h[8..16].try_into().unwrap()),
        class: u16::from_le_bytes(h[16..18].try_into().unwrap()),
        typ: u16::from_le_bytes(h[18..20].try_into().unwrap()),
        u: u32::from_le_bytes(h[20..24].try_into().unwrap()),
    }
}

// NetBSD errnos we may synthesize locally.
const EFAULT: i32 = 14;
const EIO: i32 = 5;
const EINVAL: i32 = 22;

/// A sysproxy client bound to one rump_server connection (one per box).
pub struct Client<T: Transport> {
    t: T,
    reqno: u64,
}

impl<T: Transport> Client<T> {
    /// Wrap a freshly-connected transport and perform the guest handshake:
    /// read the server banner (a `\n`-terminated line), then send a
    /// `HANDSHAKE_GUEST` request carrying `progname` and await its RESP.
    pub fn connect(mut t: T, progname: &[u8]) -> Result<Self, i32> {
        // 1. Banner: read bytes until '\n' (bounded by MAXBANNER=96).
        let mut got_nl = false;
        for _ in 0..96 {
            let mut b = [0u8; 1];
            t.read_exact(&mut b).map_err(|_| EIO)?;
            if b[0] == b'\n' {
                got_nl = true;
                break;
            }
        }
        if !got_nl {
            return Err(EIO);
        }

        let mut c = Self { t, reqno: 1 };
        // 2. Handshake request: hdr + progname + NUL.
        let reqno = c.next_reqno();
        let mut payload = Vec::with_capacity(progname.len() + 1);
        payload.extend_from_slice(progname);
        payload.push(0);
        let hdr = enc_hdr(
            (HDRSZ + payload.len()) as u64,
            reqno,
            RUMPSP_REQ,
            RUMPSP_HANDSHAKE,
            HANDSHAKE_GUEST,
        );
        c.t.write_all(&hdr).map_err(|_| EIO)?;
        c.t.write_all(&payload).map_err(|_| EIO)?;

        // 3. Await the handshake response (no copyin/out for handshake).
        c.await_response(reqno, &mut NoMem)?;
        Ok(c)
    }

    fn next_reqno(&mut self) -> u64 {
        let r = self.reqno;
        self.reqno = self.reqno.wrapping_add(1);
        r
    }

    /// Proxy one syscall: send `(sysnum, args)` (args = the marshaled
    /// `register_t` argument block, already in NetBSD layout) and run the
    /// copyin/copyout callback loop against `mem` until the final RESP.
    pub fn syscall(&mut self, sysnum: u32, args: &[u8], mem: &mut dyn ClientMem) -> SyscallResult {
        let reqno = self.next_reqno();
        let hdr = enc_hdr(
            (HDRSZ + args.len()) as u64,
            reqno,
            RUMPSP_REQ,
            RUMPSP_SYSCALL,
            sysnum,
        );
        self.t.write_all(&hdr).map_err(|_| EIO)?;
        self.t.write_all(args).map_err(|_| EIO)?;
        self.await_response(reqno, mem)
    }

    /// Read frames, servicing server callbacks, until the RESP/ERROR for
    /// `want_reqno` arrives.
    fn await_response(&mut self, want_reqno: u64, mem: &mut dyn ClientMem) -> SyscallResult {
        loop {
            let mut hbuf = [0u8; HDRSZ];
            self.t.read_exact(&mut hbuf).map_err(|_| EIO)?;
            let h = dec_hdr(&hbuf);
            if (h.len as usize) < HDRSZ {
                return Err(EIO);
            }
            let dlen = h.len as usize - HDRSZ;
            if dlen > MAX_XFER {
                return Err(EIO);
            }
            let mut data = Vec::new();
            if dlen > 0 {
                data.resize(dlen, 0);
                self.t.read_exact(&mut data).map_err(|_| EIO)?;
            }

            match h.class {
                RUMPSP_RESP | RUMPSP_ERROR if h.reqno == want_reqno => {
                    if h.class == RUMPSP_ERROR {
                        return Err(rumpsp_err_to_errno(h.u));
                    }
                    return parse_sysresp(&data);
                }
                RUMPSP_REQ => self.handle_req(&h, &data, mem)?,
                // A RESP for some other reqno (shouldn't happen on our
                // serialized per-box connection): ignore and keep reading.
                _ => {}
            }
        }
    }

    /// Service one server→client callback (copyin/copyout/anonmmap/raise).
    fn handle_req(&mut self, h: &DecHdr, data: &[u8], mem: &mut dyn ClientMem) -> Result<(), i32> {
        match h.typ {
            RUMPSP_COPYIN | RUMPSP_COPYINSTR => {
                let (len, addr) = parse_copydata_head(data)?;
                let mut out = Vec::new();
                let res = if h.typ == RUMPSP_COPYINSTR {
                    mem.copyinstr(addr, len, &mut out)
                } else {
                    mem.copyin(addr, len, &mut out)
                };
                // On a copyin fault we still must answer (empty payload) so the
                // server's syscall completes with its own EFAULT rather than
                // wedging the connection.
                if res.is_err() {
                    out.clear();
                }
                let hdr = enc_hdr(
                    (HDRSZ + out.len()) as u64,
                    h.reqno,
                    RUMPSP_RESP,
                    RUMPSP_COPYIN,
                    0,
                );
                self.t.write_all(&hdr).map_err(|_| EIO)?;
                self.t.write_all(&out).map_err(|_| EIO)?;
                Ok(())
            }
            RUMPSP_COPYOUT | RUMPSP_COPYOUTSTR => {
                let (len, addr) = parse_copydata_head(data)?;
                let body = data.get(16..16 + len).ok_or(EINVAL)?;
                // copyout has no response frame (see rumpclient.c handlereq).
                let _ = mem.copyout(addr, body);
                Ok(())
            }
            RUMPSP_ANONMMAP => {
                let len = data.get(0..8).ok_or(EINVAL)?;
                let len = u64::from_le_bytes(len.try_into().unwrap()) as usize;
                let addr = if len <= MAX_XFER { mem.anonmmap(len) } else { 0 };
                let hdr = enc_hdr(
                    (HDRSZ + 8) as u64,
                    h.reqno,
                    RUMPSP_RESP,
                    RUMPSP_ANONMMAP,
                    0,
                );
                self.t.write_all(&hdr).map_err(|_| EIO)?;
                self.t.write_all(&addr.to_le_bytes()).map_err(|_| EIO)?;
                Ok(())
            }
            RUMPSP_RAISE => Ok(()), // no host signal in the kernel client
            _ => Ok(()),
        }
    }
}

/// `struct rsp_copydata { size_t rcp_len; void *rcp_addr; u8 rcp_data[]; }`.
fn parse_copydata_head(data: &[u8]) -> Result<(usize, u64), i32> {
    if data.len() < 16 {
        return Err(EINVAL);
    }
    let len = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let addr = u64::from_le_bytes(data[8..16].try_into().unwrap());
    if len as usize > MAX_XFER {
        return Err(EINVAL);
    }
    Ok((len as usize, addr))
}

/// `struct rsp_sysresp { int rsys_error; register_t rsys_retval[2]; }` —
/// error at 0, retval[0] at 8, retval[1] at 16 (LP64, 8-byte aligned).
fn parse_sysresp(data: &[u8]) -> SyscallResult {
    if data.len() < 24 {
        return Err(EIO);
    }
    let err = i32::from_le_bytes(data[0..4].try_into().unwrap());
    if err != 0 {
        return Err(err);
    }
    let r0 = i64::from_le_bytes(data[8..16].try_into().unwrap());
    let r1 = i64::from_le_bytes(data[16..24].try_into().unwrap());
    Ok([r0, r1])
}

/// Map an `enum rumpsp_err` (RUMPSP_ERROR header `u`) to a NetBSD errno, per
/// `sp_common.c:errmap`.
fn rumpsp_err_to_errno(e: u32) -> i32 {
    match e {
        0 => 0,           // RUMPSP_ERR_NONE
        1 => 35,          // ERR_TRYAGAIN -> EAGAIN (NetBSD EAGAIN=35)
        2 => 1,           // ERR_AUTH -> EPERM
        3 => 3,           // ERR_INVALID_PREFORK -> ESRCH
        4 => EIO,         // ERR_RFORK_FAILED
        5 => 16,          // ERR_INEXEC -> EBUSY
        6 => 12,          // ERR_NOMEM -> ENOMEM
        7 => EINVAL,      // ERR_MALFORMED_REQUEST
        _ => EIO,
    }
}

// ── kernel pipe transport (injected IO, host-testable) ────────────────────

/// Injected kernel I/O for [`PipeTransport`], so the blocking read loop
/// (poll + yield + timeout) is host-testable with a mock instead of only at boot.
///
/// The kernel impl wraps `pipe_write`/`pipe_read` + the scheduler yield + the
/// monotonic clock; non-kernel callers (tests) supply a scripted mock.
///
/// NOTE (why not reuse `akuma-net`'s injected `runtime()`, which already exposes
/// `yield_now`/`uptime_us`): `akuma-rump` is a **leaf** crate that `akuma-net`
/// depends on, so importing `akuma-net` here would be a **circular dependency**.
/// `PipeIo` is therefore defined locally and implemented by the *kernel* (which
/// depends on both crates), delegating to the same `threading::yield_now` /
/// `timer::uptime_us` that feed akuma-net's runtime — one source of truth at the
/// kernel, the trait duplicated only at the crate boundary to stay acyclic. The
/// `read`/`write` here have no runtime equivalent anyway (the kernel pipe API).
pub trait PipeIo {
    /// Non-blocking read of one chunk into `buf`; returns `(bytes_read, eof)`.
    fn read(&mut self, id: u32, buf: &mut [u8]) -> (usize, bool);
    /// Write all of `buf` (the kernel pipe buffer is unbounded). `Err` = broken pipe.
    fn write(&mut self, id: u32, buf: &[u8]) -> Result<(), ()>;
    /// Cooperatively yield the CPU so the server thread can run.
    fn yield_now(&mut self);
    /// Monotonic microseconds (for the read timeout).
    fn now_us(&mut self) -> u64;
}

/// A [`Transport`] over a kernel pipe pair: write to `wr` (→ the server's `rx`),
/// read from `rd` (← the server's `tx`). Reads poll [`PipeIo::read`] +
/// `yield_now` until the buffer is full, EOF, or `timeout_us` elapses (so a
/// wedged server fails the request instead of hanging the caller forever).
pub struct PipeTransport<P: PipeIo> {
    /// Injected pipe I/O.
    pub io: P,
    /// Pipe the kernel writes (server reads via its `rx`).
    pub wr: u32,
    /// Pipe the kernel reads (server writes via its `tx`).
    pub rd: u32,
    /// Per-`read_exact` timeout in microseconds.
    pub timeout_us: u64,
}

impl<P: PipeIo> Transport for PipeTransport<P> {
    fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportErr> {
        self.io.write(self.wr, buf).map_err(|()| TransportErr)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), TransportErr> {
        let start = self.io.now_us();
        let mut got = 0;
        while got < buf.len() {
            let (n, eof) = self.io.read(self.rd, &mut buf[got..]);
            if n > 0 {
                got += n;
                continue;
            }
            if eof {
                return Err(TransportErr);
            }
            if self.io.now_us().wrapping_sub(start) > self.timeout_us {
                return Err(TransportErr);
            }
            self.io.yield_now();
        }
        Ok(())
    }
}

/// A [`ClientMem`] that faults everything — used for the handshake (which has
/// no copyin/out) and as a safe default.
struct NoMem;
impl ClientMem for NoMem {
    fn copyin(&mut self, _a: u64, _l: usize, _o: &mut Vec<u8>) -> Result<(), i32> {
        Err(EFAULT)
    }
    fn copyinstr(&mut self, _a: u64, _m: usize, _o: &mut Vec<u8>) -> Result<(), i32> {
        Err(EFAULT)
    }
    fn copyout(&mut self, _a: u64, _d: &[u8]) -> Result<(), i32> {
        Err(EFAULT)
    }
    fn anonmmap(&mut self, _l: usize) -> u64 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;

    /// A scripted transport: `inbox` is the bytes the "server" will deliver to
    /// the client (concatenated frames); `outbox` records what the client sent.
    struct MockT {
        inbox: Vec<u8>,
        pos: usize,
        outbox: Vec<u8>,
    }
    impl MockT {
        fn new(inbox: Vec<u8>) -> Self {
            MockT { inbox, pos: 0, outbox: Vec::new() }
        }
    }
    impl Transport for MockT {
        fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), TransportErr> {
            if self.pos + buf.len() > self.inbox.len() {
                return Err(TransportErr);
            }
            buf.copy_from_slice(&self.inbox[self.pos..self.pos + buf.len()]);
            self.pos += buf.len();
            Ok(())
        }
        fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportErr> {
            self.outbox.extend_from_slice(buf);
            Ok(())
        }
    }

    /// A flat keyed memory: addr -> bytes.
    struct MockMem {
        regions: BTreeMap<u64, Vec<u8>>,
        last_mmap: u64,
    }
    impl MockMem {
        fn new() -> Self {
            MockMem { regions: BTreeMap::new(), last_mmap: 0 }
        }
    }
    impl ClientMem for MockMem {
        fn copyin(&mut self, addr: u64, len: usize, out: &mut Vec<u8>) -> Result<(), i32> {
            let r = self.regions.get(&addr).ok_or(EFAULT)?;
            if r.len() < len {
                return Err(EFAULT);
            }
            out.clear();
            out.extend_from_slice(&r[..len]);
            Ok(())
        }
        fn copyinstr(&mut self, addr: u64, max: usize, out: &mut Vec<u8>) -> Result<(), i32> {
            let r = self.regions.get(&addr).ok_or(EFAULT)?;
            let end = r.iter().position(|&b| b == 0).map(|p| p + 1).unwrap_or(r.len());
            let n = end.min(max);
            out.clear();
            out.extend_from_slice(&r[..n]);
            Ok(())
        }
        fn copyout(&mut self, addr: u64, data: &[u8]) -> Result<(), i32> {
            self.regions.insert(addr, data.to_vec());
            Ok(())
        }
        fn anonmmap(&mut self, _len: usize) -> u64 {
            self.last_mmap += 0x1000;
            self.last_mmap
        }
    }

    fn frame(reqno: u64, class: u16, typ: u16, u: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = enc_hdr((HDRSZ + payload.len()) as u64, reqno, class, typ, u).to_vec();
        v.extend_from_slice(payload);
        v
    }
    fn sysresp(err: i32, r0: i64, r1: i64) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&err.to_le_bytes());
        p.extend_from_slice(&[0u8; 4]); // pad
        p.extend_from_slice(&r0.to_le_bytes());
        p.extend_from_slice(&r1.to_le_bytes());
        p
    }
    fn copydata_head(len: u64, addr: u64) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&len.to_le_bytes());
        p.extend_from_slice(&addr.to_le_bytes());
        p
    }

    // Header round-trips exactly (byte layout matches sp_common.c rsp_hdr).
    #[test]
    fn header_roundtrip() {
        let h = enc_hdr(0x1122334455667788, 0x99aabbccddeeff00, RUMPSP_REQ, RUMPSP_SYSCALL, 0xdeadbeef);
        let d = dec_hdr(&h);
        assert_eq!(d.len, 0x1122334455667788);
        assert_eq!(d.reqno, 0x99aabbccddeeff00);
        assert_eq!(d.class, RUMPSP_REQ);
        assert_eq!(d.typ, RUMPSP_SYSCALL);
        assert_eq!(d.u, 0xdeadbeef);
        // First two LE bytes of len.
        assert_eq!(h[0], 0x88);
        assert_eq!(&h[16..18], &[0u8, 0u8]); // class REQ=0
    }

    // Banner + guest handshake: client consumes the banner line and emits a
    // well-formed HANDSHAKE_GUEST request carrying progname+NUL.
    #[test]
    fn handshake_sends_guest_request() {
        let mut inbox = b"NetBSD rump\n".to_vec();
        inbox.extend_from_slice(&frame(1, RUMPSP_RESP, RUMPSP_HANDSHAKE, 0, &sysresp(0, 0, 0)));
        let t = MockT::new(inbox);
        let c = Client::connect(t, b"kernel").expect("handshake ok");
        let out = &c.t.outbox;
        let h = dec_hdr(&out[0..HDRSZ].try_into().unwrap());
        assert_eq!(h.class, RUMPSP_REQ);
        assert_eq!(h.typ, RUMPSP_HANDSHAKE);
        assert_eq!(h.u, HANDSHAKE_GUEST);
        assert_eq!(&out[HDRSZ..], b"kernel\0");
    }

    // The headline case: a syscall whose response is preceded by a COPYIN
    // callback. The client must read its user memory and answer, then return
    // the final retval. (Mirrors connect()/bind() pulling the sockaddr.)
    #[test]
    fn syscall_with_copyin_then_resp() {
        let sin: [u8; 8] = [0x10, 2, 0, 80, 10, 0, 2, 2]; // bogus sockaddr-ish
        let mut inbox = b"banner\n".to_vec();
        inbox.extend_from_slice(&frame(1, RUMPSP_RESP, RUMPSP_HANDSHAKE, 0, &sysresp(0, 0, 0)));
        // syscall reqno will be 2; server first asks COPYIN of 8 bytes @0x4000.
        inbox.extend_from_slice(&frame(2, RUMPSP_REQ, RUMPSP_COPYIN, 0, &copydata_head(8, 0x4000)));
        // then the syscall result: fd 5.
        inbox.extend_from_slice(&frame(2, RUMPSP_RESP, RUMPSP_SYSCALL, 0, &sysresp(0, 5, 0)));

        let mut mem = MockMem::new();
        mem.regions.insert(0x4000, sin.to_vec());

        let mut c = Client::connect(MockT::new(inbox), b"k").expect("hs");
        let n_after_hs = c.t.outbox.len();
        let r = c.syscall(98 /*connect-ish*/, &[0u8; 24], &mut mem).expect("syscall ok");
        assert_eq!(r, [5, 0]);

        // The client must have written a COPYIN response carrying the 8 bytes.
        let sent = &c.t.outbox[n_after_hs..];
        // sent = [SYSCALL req hdr+args][COPYIN resp hdr+8 bytes]
        let sys_hdr = dec_hdr(&sent[0..HDRSZ].try_into().unwrap());
        assert_eq!(sys_hdr.typ, RUMPSP_SYSCALL);
        let copyin_off = HDRSZ + 24; // sys hdr + 24-byte args
        let ci_hdr = dec_hdr(&sent[copyin_off..copyin_off + HDRSZ].try_into().unwrap());
        assert_eq!(ci_hdr.class, RUMPSP_RESP);
        assert_eq!(ci_hdr.typ, RUMPSP_COPYIN);
        assert_eq!(ci_hdr.reqno, 2);
        assert_eq!(&sent[copyin_off + HDRSZ..copyin_off + HDRSZ + 8], &sin);
    }

    // A COPYOUT callback writes into the client's user memory and is NOT
    // answered with a frame (per rumpclient.c). Then the RESP arrives.
    #[test]
    fn syscall_with_copyout() {
        let payload = b"resultdata";
        let mut inbox = b"b\n".to_vec();
        inbox.extend_from_slice(&frame(1, RUMPSP_RESP, RUMPSP_HANDSHAKE, 0, &sysresp(0, 0, 0)));
        let mut cod = copydata_head(payload.len() as u64, 0x8000);
        cod.extend_from_slice(payload);
        inbox.extend_from_slice(&frame(2, RUMPSP_REQ, RUMPSP_COPYOUT, 0, &cod));
        inbox.extend_from_slice(&frame(2, RUMPSP_RESP, RUMPSP_SYSCALL, 0, &sysresp(0, 10, 0)));

        let mut mem = MockMem::new();
        let mut c = Client::connect(MockT::new(inbox), b"k").expect("hs");
        let before = c.t.outbox.len();
        let r = c.syscall(4 /*read-ish*/, &[0u8; 24], &mut mem).expect("ok");
        assert_eq!(r, [10, 0]);
        // memory got the copyout
        assert_eq!(mem.regions.get(&0x8000).unwrap().as_slice(), payload);
        // only the SYSCALL request was sent (no copyout response frame)
        let sent = &c.t.outbox[before..];
        assert_eq!(sent.len(), HDRSZ + 24);
    }

    // An ERROR-class reply maps to a NetBSD errno.
    #[test]
    fn error_class_maps_errno() {
        let mut inbox = b"b\n".to_vec();
        inbox.extend_from_slice(&frame(1, RUMPSP_RESP, RUMPSP_HANDSHAKE, 0, &sysresp(0, 0, 0)));
        inbox.extend_from_slice(&frame(2, RUMPSP_ERROR, RUMPSP_SYSCALL, 6 /*NOMEM*/, &[]));
        let mut mem = MockMem::new();
        let mut c = Client::connect(MockT::new(inbox), b"k").expect("hs");
        let r = c.syscall(1, &[0u8; 24], &mut mem);
        assert_eq!(r, Err(12)); // ENOMEM
    }

    // A non-zero rsys_error in a normal RESP propagates as that errno.
    #[test]
    fn syscall_errno_propagates() {
        let mut inbox = b"b\n".to_vec();
        inbox.extend_from_slice(&frame(1, RUMPSP_RESP, RUMPSP_HANDSHAKE, 0, &sysresp(0, 0, 0)));
        inbox.extend_from_slice(&frame(2, RUMPSP_RESP, RUMPSP_SYSCALL, 0, &sysresp(61 /*ECONNREFUSED*/, -1, 0)));
        let mut mem = MockMem::new();
        let mut c = Client::connect(MockT::new(inbox), b"k").expect("hs");
        assert_eq!(c.syscall(98, &[0u8; 24], &mut mem), Err(61));
    }

    // A malformed (too-large) frame length is rejected, not allocated.
    #[test]
    fn oversize_frame_rejected() {
        let mut inbox = b"b\n".to_vec();
        inbox.extend_from_slice(&frame(1, RUMPSP_RESP, RUMPSP_HANDSHAKE, 0, &sysresp(0, 0, 0)));
        // claim a huge len with no body
        let bad = enc_hdr((HDRSZ + MAX_XFER + 1) as u64, 2, RUMPSP_RESP, RUMPSP_SYSCALL, 0);
        inbox.extend_from_slice(&bad);
        let mut mem = MockMem::new();
        let mut c = Client::connect(MockT::new(inbox), b"k").expect("hs");
        assert_eq!(c.syscall(1, &[0u8; 24], &mut mem), Err(EIO));
    }

    // anonmmap callback returns an address chosen by ClientMem.
    #[test]
    fn anonmmap_callback() {
        let mut inbox = b"b\n".to_vec();
        inbox.extend_from_slice(&frame(1, RUMPSP_RESP, RUMPSP_HANDSHAKE, 0, &sysresp(0, 0, 0)));
        inbox.extend_from_slice(&frame(2, RUMPSP_REQ, RUMPSP_ANONMMAP, 0, &0x2000u64.to_le_bytes()));
        inbox.extend_from_slice(&frame(2, RUMPSP_RESP, RUMPSP_SYSCALL, 0, &sysresp(0, 0, 0)));
        let mut mem = MockMem::new();
        let mut c = Client::connect(MockT::new(inbox), b"k").expect("hs");
        let before = c.t.outbox.len();
        c.syscall(197, &[0u8; 24], &mut mem).expect("ok");
        // a RESP/ANONMMAP carrying a non-zero addr was sent
        let sent = &c.t.outbox[before + HDRSZ + 24..];
        let mm = dec_hdr(&sent[0..HDRSZ].try_into().unwrap());
        assert_eq!(mm.typ, RUMPSP_ANONMMAP);
        let addr = u64::from_le_bytes(sent[HDRSZ..HDRSZ + 8].try_into().unwrap());
        assert_eq!(addr, 0x1000);
    }

    // ── PipeTransport (the kernel-pipe blocking read loop) ──────────────────

    /// Scripted PipeIo: `reads` is a queue of (bytes, eof) chunks delivered on
    /// successive read() calls (empty Vec with eof=false = "nothing yet, yield");
    /// `clock` advances by `tick` each now_us() call; writes are captured.
    struct MockIo {
        reads: alloc::collections::VecDeque<(Vec<u8>, bool)>,
        writes: Vec<u8>,
        clock: u64,
        tick: u64,
        yields: u32,
    }
    impl MockIo {
        fn new(reads: &[(&[u8], bool)], tick: u64) -> Self {
            let mut q = alloc::collections::VecDeque::new();
            for (b, eof) in reads {
                q.push_back((b.to_vec(), *eof));
            }
            MockIo { reads: q, writes: Vec::new(), clock: 0, tick, yields: 0 }
        }
    }
    impl PipeIo for MockIo {
        fn read(&mut self, _id: u32, buf: &mut [u8]) -> (usize, bool) {
            match self.reads.pop_front() {
                Some((bytes, eof)) => {
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    // requeue any remainder of this chunk
                    if n < bytes.len() {
                        self.reads.push_front((bytes[n..].to_vec(), eof));
                        (n, false)
                    } else {
                        (n, eof)
                    }
                }
                None => (0, false), // nothing yet → caller yields/times out
            }
        }
        fn write(&mut self, _id: u32, buf: &[u8]) -> Result<(), ()> {
            self.writes.extend_from_slice(buf);
            Ok(())
        }
        fn yield_now(&mut self) {
            self.yields += 1;
        }
        fn now_us(&mut self) -> u64 {
            let t = self.clock;
            self.clock += self.tick;
            t
        }
    }

    fn pt(io: MockIo) -> PipeTransport<MockIo> {
        PipeTransport { io, wr: 1, rd: 2, timeout_us: 1000 }
    }

    // read_exact assembles a value from multiple partial chunks (with empty
    // "nothing yet" gaps that force yields).
    #[test]
    fn pipe_read_exact_assembles_partial_chunks() {
        let io = MockIo::new(
            &[(&[0xAA, 0xBB], false), (&[], false), (&[0xCC], false), (&[0xDD], false)],
            1,
        );
        let mut t = pt(io);
        let mut buf = [0u8; 4];
        t.read_exact(&mut buf).expect("assembled");
        assert_eq!(buf, [0xAA, 0xBB, 0xCC, 0xDD]);
        assert!(t.io.yields >= 1); // the empty gap forced a yield
    }

    // a single read() returning more than the request is split across calls.
    #[test]
    fn pipe_read_exact_splits_oversized_chunk() {
        let io = MockIo::new(&[(&[1, 2, 3, 4, 5, 6], false)], 1);
        let mut t = pt(io);
        let mut a = [0u8; 2];
        let mut b = [0u8; 4];
        t.read_exact(&mut a).expect("a");
        t.read_exact(&mut b).expect("b");
        assert_eq!(a, [1, 2]);
        assert_eq!(b, [3, 4, 5, 6]);
    }

    // EOF mid-read is a transport error (server closed the channel).
    #[test]
    fn pipe_read_exact_eof_errors() {
        let io = MockIo::new(&[(&[9], false), (&[], true)], 1);
        let mut t = pt(io);
        let mut buf = [0u8; 4];
        assert_eq!(t.read_exact(&mut buf), Err(TransportErr));
    }

    // a server that never sends times out instead of looping forever.
    #[test]
    fn pipe_read_exact_times_out() {
        // every now_us() advances 600us; timeout 1000us → fails within ~2 polls.
        let io = MockIo::new(&[], 600);
        let mut t = pt(io);
        let mut buf = [0u8; 1];
        assert_eq!(t.read_exact(&mut buf), Err(TransportErr));
    }

    // write_all goes to the wr pipe verbatim.
    #[test]
    fn pipe_write_all_passthrough() {
        let mut t = pt(MockIo::new(&[], 1));
        t.write_all(&[1, 2, 3]).expect("w");
        assert_eq!(t.io.writes, alloc::vec![1, 2, 3]);
    }
}
