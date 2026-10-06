//! AES-128 (FIPS-197), both directions, and AES key wrap (RFC 3394).
//!
//! WPA2 needs AES in software for one thing: unwrapping the group key the
//! access point sends in message 3 (and in each group rekey). Data frames are
//! CCMP-protected by the card itself. This is a plain byte-oriented
//! implementation with S-box lookups — not constant-time against a local
//! cache-timing attacker, which is acceptable for a key unwrap that runs a
//! handful of times per association.
//!
//! The S-boxes are computed at compile time from their definition (inverse in
//! GF(2^8), then the affine map), so there is no 256-byte table to mistype.

const fn xtime(a: u8) -> u8 {
    (a << 1) ^ if a & 0x80 != 0 { 0x1b } else { 0 }
}

const fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    p
}

const fn ginv(a: u8) -> u8 {
    // a^254 = a^-1 in GF(2^8); 0 maps to 0.
    let mut r = 1u8;
    let mut base = a;
    let mut e = 254;
    while e > 0 {
        if e & 1 != 0 {
            r = gmul(r, base);
        }
        base = gmul(base, base);
        e >>= 1;
    }
    if a == 0 { 0 } else { r }
}

const SBOX: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let b = ginv(i as u8);
        t[i] = b ^ b.rotate_left(1) ^ b.rotate_left(2) ^ b.rotate_left(3) ^ b.rotate_left(4) ^ 0x63;
        i += 1;
    }
    t
};

const INV_SBOX: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[SBOX[i] as usize] = i as u8;
        i += 1;
    }
    t
};

/// An expanded AES-128 key.
pub struct Aes128 {
    rk: [[u8; 16]; 11],
}

impl Aes128 {
    #[must_use]
    pub fn new(key: &[u8; 16]) -> Self {
        let mut w = [[0u8; 4]; 44];
        for (i, c) in key.as_chunks::<4>().0.iter().enumerate() {
            w[i] = *c;
        }
        let mut rcon = 1u8;
        for i in 4..44 {
            let mut t = w[i - 1];
            if i % 4 == 0 {
                t = [SBOX[t[1] as usize] ^ rcon, SBOX[t[2] as usize], SBOX[t[3] as usize], SBOX[t[0] as usize]];
                rcon = xtime(rcon);
            }
            for j in 0..4 {
                w[i][j] = w[i - 4][j] ^ t[j];
            }
        }
        let mut rk = [[0u8; 16]; 11];
        for (r, k) in rk.iter_mut().enumerate() {
            for c in 0..4 {
                k[4 * c..4 * c + 4].copy_from_slice(&w[4 * r + c]);
            }
        }
        Self { rk }
    }

    pub fn encrypt(&self, s: &mut [u8; 16]) {
        add(s, &self.rk[0]);
        for rk in &self.rk[1..10] {
            sub(s, &SBOX);
            shift_rows(s);
            mix_columns(s);
            add(s, rk);
        }
        sub(s, &SBOX);
        shift_rows(s);
        add(s, &self.rk[10]);
    }

    pub fn decrypt(&self, s: &mut [u8; 16]) {
        add(s, &self.rk[10]);
        for rk in self.rk[1..10].iter().rev() {
            inv_shift_rows(s);
            sub(s, &INV_SBOX);
            add(s, rk);
            inv_mix_columns(s);
        }
        inv_shift_rows(s);
        sub(s, &INV_SBOX);
        add(s, &self.rk[0]);
    }
}

fn add(s: &mut [u8; 16], k: &[u8; 16]) {
    for (a, b) in s.iter_mut().zip(k) {
        *a ^= b;
    }
}

fn sub(s: &mut [u8; 16], t: &[u8; 256]) {
    for b in s.iter_mut() {
        *b = t[*b as usize];
    }
}

/// State byte (row r, column c) is `s[r + 4c]`.
fn shift_rows(s: &mut [u8; 16]) {
    let o = *s;
    for r in 1..4 {
        for c in 0..4 {
            s[r + 4 * c] = o[r + 4 * ((c + r) % 4)];
        }
    }
}

fn inv_shift_rows(s: &mut [u8; 16]) {
    let o = *s;
    for r in 1..4 {
        for c in 0..4 {
            s[r + 4 * ((c + r) % 4)] = o[r + 4 * c];
        }
    }
}

fn mix_columns(s: &mut [u8; 16]) {
    for c in s.as_chunks_mut::<4>().0 {
        let [a0, a1, a2, a3] = [c[0], c[1], c[2], c[3]];
        c[0] = gmul(a0, 2) ^ gmul(a1, 3) ^ a2 ^ a3;
        c[1] = a0 ^ gmul(a1, 2) ^ gmul(a2, 3) ^ a3;
        c[2] = a0 ^ a1 ^ gmul(a2, 2) ^ gmul(a3, 3);
        c[3] = gmul(a0, 3) ^ a1 ^ a2 ^ gmul(a3, 2);
    }
}

fn inv_mix_columns(s: &mut [u8; 16]) {
    for c in s.as_chunks_mut::<4>().0 {
        let [a0, a1, a2, a3] = [c[0], c[1], c[2], c[3]];
        c[0] = gmul(a0, 14) ^ gmul(a1, 11) ^ gmul(a2, 13) ^ gmul(a3, 9);
        c[1] = gmul(a0, 9) ^ gmul(a1, 14) ^ gmul(a2, 11) ^ gmul(a3, 13);
        c[2] = gmul(a0, 13) ^ gmul(a1, 9) ^ gmul(a2, 14) ^ gmul(a3, 11);
        c[3] = gmul(a0, 11) ^ gmul(a1, 13) ^ gmul(a2, 9) ^ gmul(a3, 14);
    }
}

const IV: [u8; 8] = [0xa6; 8];

/// RFC 3394 key unwrap of `wrapped` (n+1 64-bit blocks, n >= 2) into
/// `out[..wrapped.len() - 8]`. `None` if the lengths are wrong or the
/// integrity check fails — a wrong KEK, or a corrupted frame.
#[must_use]
#[allow(clippy::many_single_char_names)] // RFC 3394's own names: A, R, B, t, n
pub fn unwrap(kek: &[u8; 16], wrapped: &[u8], out: &mut [u8]) -> Option<usize> {
    if !wrapped.len().is_multiple_of(8) || wrapped.len() < 24 || out.len() < wrapped.len() - 8 {
        return None;
    }
    let n = wrapped.len() / 8 - 1;
    let aes = Aes128::new(kek);
    let mut a = [0u8; 8];
    a.copy_from_slice(&wrapped[..8]);
    let r = &mut out[..n * 8];
    r.copy_from_slice(&wrapped[8..]);
    for j in (0..6).rev() {
        for i in (1..=n).rev() {
            let t = ((n * j + i) as u64).to_be_bytes();
            let mut b = [0u8; 16];
            for k in 0..8 {
                b[k] = a[k] ^ t[k];
            }
            b[8..].copy_from_slice(&r[(i - 1) * 8..i * 8]);
            aes.decrypt(&mut b);
            a.copy_from_slice(&b[..8]);
            r[(i - 1) * 8..i * 8].copy_from_slice(&b[8..]);
        }
    }
    (a == IV).then_some(n * 8)
}

/// RFC 3394 key wrap of `plain` (a multiple of 8 bytes, at least 16) into
/// `out[..plain.len() + 8]`. The station never wraps; the tests' access point
/// does.
#[must_use]
#[allow(clippy::many_single_char_names)] // as `unwrap`
pub fn wrap(kek: &[u8; 16], plain: &[u8], out: &mut [u8]) -> Option<usize> {
    if !plain.len().is_multiple_of(8) || plain.len() < 16 || out.len() < plain.len() + 8 {
        return None;
    }
    let n = plain.len() / 8;
    let aes = Aes128::new(kek);
    let mut a = IV;
    let (head, r) = out[..8 + n * 8].split_at_mut(8);
    r.copy_from_slice(plain);
    for j in 0..6 {
        for i in 1..=n {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&a);
            b[8..].copy_from_slice(&r[(i - 1) * 8..i * 8]);
            aes.encrypt(&mut b);
            let t = ((n * j + i) as u64).to_be_bytes();
            for k in 0..8 {
                a[k] = b[k] ^ t[k];
            }
            r[(i - 1) * 8..i * 8].copy_from_slice(&b[8..]);
        }
    }
    head.copy_from_slice(&a);
    Some(8 + n * 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h<const N: usize>(s: &str) -> [u8; N] {
        let mut b = [0u8; N];
        for (i, o) in b.iter_mut().enumerate() {
            *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        b
    }

    #[test]
    fn sbox_spot_values() {
        assert_eq!((SBOX[0x00], SBOX[0x01], SBOX[0x53], SBOX[0xff]), (0x63, 0x7c, 0xed, 0x16));
        assert_eq!(INV_SBOX[0x63], 0);
    }

    #[test]
    fn fips197_appendix_c1() {
        let aes = Aes128::new(&h("000102030405060708090a0b0c0d0e0f"));
        let mut b = h::<16>("00112233445566778899aabbccddeeff");
        aes.encrypt(&mut b);
        assert_eq!(b, h::<16>("69c4e0d86a7b0430d8cdb78070b4c55a"));
        aes.decrypt(&mut b);
        assert_eq!(b, h::<16>("00112233445566778899aabbccddeeff"));
    }

    #[test]
    fn rfc3394_4_1_wrap_and_unwrap() {
        let kek = h::<16>("000102030405060708090a0b0c0d0e0f");
        let key = h::<16>("00112233445566778899aabbccddeeff");
        let want = h::<24>("1fa68b0a8112b447aef34bd8fb5a7b829d3e862371d2cfe5");
        let mut w = [0u8; 24];
        assert_eq!(wrap(&kek, &key, &mut w), Some(24));
        assert_eq!(w, want);
        let mut p = [0u8; 16];
        assert_eq!(unwrap(&kek, &want, &mut p), Some(16));
        assert_eq!(p, key);
    }

    #[test]
    fn unwrap_refuses_a_wrong_kek_and_bad_lengths() {
        let want = h::<24>("1fa68b0a8112b447aef34bd8fb5a7b829d3e862371d2cfe5");
        let mut p = [0u8; 16];
        assert_eq!(unwrap(&[0; 16], &want, &mut p), None);
        assert_eq!(unwrap(&[0; 16], &want[..20], &mut p), None);
        assert_eq!(unwrap(&[0; 16], &want[..16], &mut p), None);
    }
}
