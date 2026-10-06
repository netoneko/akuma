//! SHA-1 (FIPS 180-4), HMAC-SHA1 (RFC 2104), the 802.11 PRF and PBKDF2.
//!
//! SHA-1 is broken for collision resistance; WPA2 uses it only inside HMAC,
//! where that does not matter, and the protocol fixes the choice.

/// Bytes of a SHA-1 digest.
pub const LEN: usize = 20;
const BLOCK: usize = 64;

/// An incremental SHA-1.
#[derive(Clone)]
pub struct Sha1 {
    h: [u32; 5],
    buf: [u8; BLOCK],
    fill: usize,
    total: u64,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha1 {
    #[must_use]
    pub const fn new() -> Self {
        Self { h: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476, 0xc3d2_e1f0], buf: [0; BLOCK], fill: 0, total: 0 }
    }

    pub fn update(&mut self, mut d: &[u8]) {
        self.total += d.len() as u64;
        while !d.is_empty() {
            let n = (BLOCK - self.fill).min(d.len());
            self.buf[self.fill..self.fill + n].copy_from_slice(&d[..n]);
            self.fill += n;
            d = &d[n..];
            if self.fill == BLOCK {
                let b = self.buf;
                self.compress(&b);
                self.fill = 0;
            }
        }
    }

    #[must_use]
    pub fn finish(mut self) -> [u8; LEN] {
        let bits = self.total * 8;
        self.update(&[0x80]);
        while self.fill != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0; LEN];
        for (o, h) in out.as_chunks_mut::<4>().0.iter_mut().zip(self.h) {
            *o = h.to_be_bytes();
        }
        out
    }

    #[allow(clippy::many_single_char_names)] // FIPS 180-4's own names
    fn compress(&mut self, block: &[u8; BLOCK]) {
        let mut w = [0u32; 80];
        for (i, c) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*c);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = self.h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (h, v) in self.h.iter_mut().zip([a, b, c, d, e]) {
            *h = h.wrapping_add(v);
        }
    }
}

/// SHA-1 of the concatenation of `parts`.
#[must_use]
pub fn sha1(parts: &[&[u8]]) -> [u8; LEN] {
    let mut s = Sha1::new();
    for p in parts {
        s.update(p);
    }
    s.finish()
}

/// HMAC-SHA1 of the concatenation of `parts` (RFC 2104) — several parts so
/// callers never assemble a message in a buffer.
#[must_use]
pub fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; LEN] {
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..LEN].copy_from_slice(&sha1(&[key]));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha1::new();
    inner.update(&ipad);
    for p in parts {
        inner.update(p);
    }
    let ih = inner.finish();
    sha1(&[&opad, &ih])
}

/// The 802.11 PRF (IEEE 802.11-2020 12.7.1.2): `HMAC-SHA1(K, A || 0 || B || i)`
/// for i = 0, 1, ..., truncated to `out.len()`.
pub fn prf(key: &[u8], label: &[u8], data: &[&[u8]], out: &mut [u8]) {
    for (i, chunk) in out.chunks_mut(LEN).enumerate() {
        let mut parts: [&[u8]; 8] = [&[]; 8];
        parts[0] = label;
        parts[1] = &[0];
        let n = data.len().min(5);
        parts[2..2 + n].copy_from_slice(&data[..n]);
        let ctr = [i as u8];
        parts[2 + n] = &ctr;
        let h = hmac(key, &parts[..3 + n]);
        chunk.copy_from_slice(&h[..chunk.len()]);
    }
}

/// PBKDF2-HMAC-SHA1 (RFC 8018), the WPA2 passphrase-to-PSK function with
/// `iterations` 4096 and `out` 32 bytes. The kernel never runs it — the PSK is
/// staged — but the tests use it, and so can the `wifi` tool.
pub fn pbkdf2(pass: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    for (i, chunk) in out.chunks_mut(LEN).enumerate() {
        let idx = (i as u32 + 1).to_be_bytes();
        let mut u = hmac(pass, &[salt, &idx]);
        let mut t = u;
        for _ in 1..iterations {
            u = hmac(pass, &[&u]);
            for (a, b) in t.iter_mut().zip(u) {
                *a ^= b;
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> [u8; 64] {
        let mut b = [0u8; 64];
        for i in 0..s.len() / 2 {
            b[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        b
    }

    #[test]
    fn sha1_known_answers() {
        assert_eq!(sha1(&[b"abc"]), hex("a9993e364706816aba3e25717850c26c9cd0d89d")[..20]);
        // Split across many updates and blocks.
        let mut s = Sha1::new();
        for _ in 0..1000 {
            s.update(b"a");
        }
        assert_eq!(s.finish(), hex("291e9a6c66994949b57ba5e650361e98fc36b1ba")[..20]);
    }

    #[test]
    fn hmac_rfc2202_case_1() {
        assert_eq!(hmac(&[0x0b; 20], &[b"Hi ", b"There"]), hex("b617318655057264e28bc0b6fb378c8ef146be00")[..20]);
    }

    #[test]
    fn prf_80211_test_case_1() {
        // IEEE 802.11-2020 J.3.2 (PRF-512, "prefix", "Hi There").
        let mut out = [0u8; 64];
        prf(&[0x0b; 20], b"prefix", &[b"Hi There"], &mut out);
        assert_eq!(
            out,
            hex("bcd4c650b30b9684951829e0d75f9d54b862175ed9f00606e17d8da35402ffee75df78c3d31e0f889f012120c0862beb67753e7439ae242edb8373698356cf5a")
        );
    }

    #[test]
    fn pbkdf2_wpa_passphrase_vector() {
        // IEEE 802.11-2020 J.4: passphrase "password", SSID "IEEE".
        let mut psk = [0u8; 32];
        pbkdf2(b"password", b"IEEE", 4096, &mut psk);
        assert_eq!(psk, hex("f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e")[..32]);
    }
}
