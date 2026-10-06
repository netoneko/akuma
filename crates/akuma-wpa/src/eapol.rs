//! EAPOL-Key frames and the supplicant side of the WPA2-PSK handshakes.
//!
//! IEEE 802.11-2020 12.7.2, 12.7.6, 12.7.7: CCMP with key descriptor
//! version 2 (HMAC-SHA1-128 MIC, AES key wrap) — what a WPA2 network with
//! the PSK AKM and PMF off negotiates.
//!
//! An EAPOL-Key frame, from the 802.1X header:
//!
//! | at | len | field |
//! |---|---|---|
//! | 0 | 1 | protocol version |
//! | 1 | 1 | packet type (3 = Key) |
//! | 2 | 2 | body length (big-endian, everything after these 4 bytes) |
//! | 4 | 1 | descriptor type (2 = RSN) |
//! | 5 | 2 | key information |
//! | 7 | 2 | key length |
//! | 9 | 8 | replay counter |
//! | 17 | 32 | nonce |
//! | 49 | 16 | key IV |
//! | 65 | 8 | key RSC |
//! | 73 | 8 | reserved |
//! | 81 | 16 | MIC |
//! | 97 | 2 | key data length |
//! | 99 | n | key data |
//!
//! [`Supplicant`] takes each received EAPOL frame and either writes the
//! reply into a caller's buffer or says why it dropped the frame. It never
//! touches the radio: installing the keys it hands back is the driver's job.

use crate::Addr;
use crate::aes;
use crate::sha1::{hmac, prf};

/// Bytes before the key data.
pub const HDR_LEN: usize = 99;
/// Largest key data this station accepts (msg 3: RSN element + GTK KDE,
/// wrapped; real ones are ~60 bytes).
pub const MAX_KEY_DATA: usize = 256;

const MIC_AT: usize = 81;

/// Key-information bits.
pub mod info {
    pub const VERSION_MASK: u16 = 0x0007;
    /// HMAC-SHA1 MIC, AES key wrap.
    pub const VERSION_2: u16 = 2;
    pub const PAIRWISE: u16 = 1 << 3;
    pub const INSTALL: u16 = 1 << 6;
    pub const ACK: u16 = 1 << 7;
    pub const MIC: u16 = 1 << 8;
    pub const SECURE: u16 = 1 << 9;
    pub const ERROR: u16 = 1 << 10;
    pub const REQUEST: u16 = 1 << 11;
    pub const ENCRYPTED: u16 = 1 << 12;
}

/// A received EAPOL-Key frame, read in place.
#[derive(Clone, Copy, Debug)]
pub struct Key<'a> {
    /// The whole EAPOL frame, header to the end of the key data.
    pub frame: &'a [u8],
    pub info: u16,
    pub replay: u64,
    pub nonce: &'a [u8],
    pub rsc: &'a [u8],
    pub mic: &'a [u8],
    pub data: &'a [u8],
}

impl<'a> Key<'a> {
    /// `None` for anything that is not a well-formed RSN EAPOL-Key frame.
    /// Trailing bytes past the 802.1X body length (link padding) are cut off.
    #[must_use]
    pub fn parse(f: &'a [u8]) -> Option<Self> {
        if f.len() < HDR_LEN || f[1] != 3 || f[4] != 2 {
            return None;
        }
        let body = usize::from(u16::from_be_bytes([f[2], f[3]]));
        let f = f.get(..4 + body)?;
        let dlen = usize::from(u16::from_be_bytes([f[97], f[98]]));
        let data = f.get(HDR_LEN..HDR_LEN + dlen)?;
        let mut rc = [0u8; 8];
        rc.copy_from_slice(&f[9..17]);
        Some(Self {
            frame: &f[..HDR_LEN + dlen],
            info: u16::from_be_bytes([f[5], f[6]]),
            replay: u64::from_be_bytes(rc),
            nonce: &f[17..49],
            rsc: &f[65..73],
            mic: &f[MIC_AT..MIC_AT + 16],
            data,
        })
    }
}

/// The MIC of an EAPOL frame: HMAC-SHA1 over it with the MIC field zeroed,
/// truncated to 16 bytes.
#[must_use]
pub fn mic(kck: &[u8; 16], frame: &[u8]) -> [u8; 16] {
    let h = hmac(kck, &[&frame[..MIC_AT], &[0; 16], &frame[MIC_AT + 16..]]);
    let mut m = [0u8; 16];
    m.copy_from_slice(&h[..16]);
    m
}

/// The pairwise transient key for CCMP: PRF-384 split into KCK, KEK, TK.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ptk {
    pub kck: [u8; 16],
    pub kek: [u8; 16],
    pub tk: [u8; 16],
}

impl core::fmt::Debug for Ptk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Ptk(..)")
    }
}

impl Ptk {
    /// 802.11-2020 12.7.1.3: `PRF-384(PMK, "Pairwise key expansion",
    /// min(AA,SPA) || max(AA,SPA) || min(ANonce,SNonce) || max(ANonce,SNonce))`.
    #[must_use]
    pub fn derive(pmk: &[u8; 32], aa: &Addr, spa: &Addr, anonce: &[u8; 32], snonce: &[u8; 32]) -> Self {
        let (a1, a2) = if aa <= spa { (aa, spa) } else { (spa, aa) };
        let (n1, n2) = if anonce <= snonce { (anonce, snonce) } else { (snonce, anonce) };
        let mut out = [0u8; 48];
        prf(pmk, b"Pairwise key expansion", &[a1, a2, n1, n2], &mut out);
        let mut p = Self { kck: [0; 16], kek: [0; 16], tk: [0; 16] };
        p.kck.copy_from_slice(&out[..16]);
        p.kek.copy_from_slice(&out[16..32]);
        p.tk.copy_from_slice(&out[32..]);
        p
    }
}

/// A group temporal key, from a GTK KDE.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Gtk {
    pub key: [u8; 16],
    /// The key id (1..=3 in practice).
    pub idx: u8,
    /// The receive sequence counter the AP is at, little-endian, from the key
    /// frame's RSC field.
    pub rsc: [u8; 8],
}

impl core::fmt::Debug for Gtk {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Gtk(idx {})", self.idx)
    }
}

/// Why a frame was dropped. None of these ends the association by itself:
/// the access point retries, and a station that answers nothing is
/// deauthenticated by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drop {
    /// Not an RSN EAPOL-Key frame.
    NotKey,
    /// Key descriptor version other than 2 (TKIP, or AES-CMAC from PMF/SHA256).
    Version,
    /// Replay counter not above the last one accepted.
    Replay,
    /// Message 3 (or a group message) before message 1 was answered.
    Order,
    /// Message 3's ANonce is not message 1's.
    Nonce,
    /// The MIC does not verify — the PSK is wrong, almost always.
    Mic,
    /// The key data did not unwrap.
    Unwrap,
    /// No GTK in the key data.
    NoGtk,
    /// A frame this station has no use for (requests, errors, odd bit sets).
    Unexpected,
    /// The reply did not fit the caller's buffer.
    Buffer,
}

/// What to do with a frame that was accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Message 1 answered: send `out[..len]` (message 2).
    Send(usize),
    /// Message 3 accepted: send `out[..len]` (message 4), **then** install
    /// both keys. Message 4 must go out unencrypted.
    Complete { len: usize, tk: [u8; 16], gtk: Gtk },
    /// A group rekey: send `out[..len]` (group message 2), install the GTK.
    Rekey { len: usize, gtk: Gtk },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    /// Message 2 sent; `ptk` holds the key it was MIC'd with.
    Msg2Sent,
    Done,
}

/// Bytes of this station's RSN element it can remember.
const IE_MAX: usize = 64;

/// The supplicant for one association.
pub struct Supplicant {
    pmk: [u8; 32],
    aa: Addr,
    spa: Addr,
    snonce: [u8; 32],
    anonce: [u8; 32],
    ptk: Option<Ptk>,
    replay: Option<u64>,
    ie: [u8; IE_MAX],
    ie_len: usize,
    state: State,
}

impl Supplicant {
    /// `pmk` is the PSK; `aa` the access point's address, `spa` this
    /// station's; `snonce` 32 fresh random bytes; `rsn_ie` the RSN element
    /// (id and length included) exactly as sent in the association request —
    /// message 2 must carry the same bytes.
    #[must_use]
    pub fn new(pmk: &[u8; 32], aa: Addr, spa: Addr, snonce: [u8; 32], rsn_ie: &[u8]) -> Self {
        let mut ie = [0u8; IE_MAX];
        let ie_len = rsn_ie.len().min(IE_MAX);
        ie[..ie_len].copy_from_slice(&rsn_ie[..ie_len]);
        Self { pmk: *pmk, aa, spa, snonce, anonce: [0; 32], ptk: None, replay: None, ie, ie_len, state: State::Idle }
    }

    /// The handshake finished (keys handed out at least once).
    #[must_use]
    pub fn done(&self) -> bool {
        self.state == State::Done
    }

    /// Take one received EAPOL frame (the LLC payload of a data frame with
    /// ethertype 0x888e); write the reply, if any, into `out`.
    pub fn handle(&mut self, frame: &[u8], out: &mut [u8]) -> Result<Action, Drop> {
        let k = Key::parse(frame).ok_or(Drop::NotKey)?;
        if k.info & info::VERSION_MASK != info::VERSION_2 {
            return Err(Drop::Version);
        }
        if k.info & (info::REQUEST | info::ERROR) != 0 || k.info & info::ACK == 0 {
            return Err(Drop::Unexpected);
        }
        if self.replay.is_some_and(|r| k.replay <= r) {
            return Err(Drop::Replay);
        }
        let pairwise = k.info & info::PAIRWISE != 0;
        let has_mic = k.info & info::MIC != 0;
        match (pairwise, has_mic) {
            (true, false) => self.msg1(&k, out),
            (true, true) => self.msg3(&k, out),
            (false, true) => self.group1(&k, out),
            (false, false) => Err(Drop::Unexpected),
        }
    }

    fn msg1(&mut self, k: &Key<'_>, out: &mut [u8]) -> Result<Action, Drop> {
        self.anonce.copy_from_slice(k.nonce);
        let ptk = Ptk::derive(&self.pmk, &self.aa, &self.spa, &self.anonce, &self.snonce);
        let ie = self.ie;
        let len = write_key(out, &ptk.kck, info::VERSION_2 | info::PAIRWISE | info::MIC, k.replay, &self.snonce, &ie[..self.ie_len])?;
        self.ptk = Some(ptk);
        self.replay = Some(k.replay);
        self.state = State::Msg2Sent;
        Ok(Action::Send(len))
    }

    fn msg3(&mut self, k: &Key<'_>, out: &mut [u8]) -> Result<Action, Drop> {
        if self.state == State::Idle {
            return Err(Drop::Order);
        }
        if k.nonce != self.anonce {
            return Err(Drop::Nonce);
        }
        let ptk = self.ptk.ok_or(Drop::Order)?;
        if mic(&ptk.kck, k.frame) != k.mic {
            return Err(Drop::Mic);
        }
        if k.info & (info::INSTALL | info::SECURE | info::ENCRYPTED) != (info::INSTALL | info::SECURE | info::ENCRYPTED) {
            return Err(Drop::Unexpected);
        }
        let gtk = Self::gtk(k, &ptk)?;
        let len = write_key(out, &ptk.kck, info::VERSION_2 | info::PAIRWISE | info::MIC | info::SECURE, k.replay, &[0; 32], &[])?;
        self.replay = Some(k.replay);
        self.state = State::Done;
        Ok(Action::Complete { len, tk: ptk.tk, gtk })
    }

    fn group1(&mut self, k: &Key<'_>, out: &mut [u8]) -> Result<Action, Drop> {
        let ptk = match (self.state, self.ptk) {
            (State::Done, Some(p)) => p,
            _ => return Err(Drop::Order),
        };
        if mic(&ptk.kck, k.frame) != k.mic {
            return Err(Drop::Mic);
        }
        if k.info & (info::SECURE | info::ENCRYPTED) != (info::SECURE | info::ENCRYPTED) {
            return Err(Drop::Unexpected);
        }
        let gtk = Self::gtk(k, &ptk)?;
        let len = write_key(out, &ptk.kck, info::VERSION_2 | info::MIC | info::SECURE, k.replay, &[0; 32], &[])?;
        self.replay = Some(k.replay);
        Ok(Action::Rekey { len, gtk })
    }

    /// Unwrap the key data and find the GTK KDE in it.
    fn gtk(k: &Key<'_>, ptk: &Ptk) -> Result<Gtk, Drop> {
        let mut plain = [0u8; MAX_KEY_DATA];
        if k.data.len() > MAX_KEY_DATA {
            return Err(Drop::Unwrap);
        }
        let n = aes::unwrap(&ptk.kek, k.data, &mut plain).ok_or(Drop::Unwrap)?;
        let mut g = find_gtk(&plain[..n]).ok_or(Drop::NoGtk)?;
        g.rsc.copy_from_slice(k.rsc);
        Ok(g)
    }
}

/// Find the GTK KDE (`dd len 00-0f-ac 01 keyid rsvd gtk`) among the
/// elements of unwrapped key data. The padding (`dd 00 ...`) ends the walk.
#[must_use]
pub fn find_gtk(d: &[u8]) -> Option<Gtk> {
    let mut at = 0;
    while at + 2 <= d.len() {
        let (id, len) = (d[at], usize::from(d[at + 1]));
        if id == 0xdd && len == 0 {
            break;
        }
        let body = d.get(at + 2..at + 2 + len)?;
        if id == 0xdd && len == 6 + 16 && body[..4] == [0x00, 0x0f, 0xac, 0x01] {
            let mut key = [0u8; 16];
            key.copy_from_slice(&body[6..22]);
            return Some(Gtk { key, idx: body[4] & 3, rsc: [0; 8] });
        }
        at += 2 + len;
    }
    None
}

/// Write an EAPOL-Key frame (version 1, RSN descriptor, key length 0) with
/// its MIC.
fn write_key(out: &mut [u8], kck: &[u8; 16], key_info: u16, replay: u64, nonce: &[u8; 32], data: &[u8]) -> Result<usize, Drop> {
    let len = HDR_LEN + data.len();
    let f = out.get_mut(..len).ok_or(Drop::Buffer)?;
    f.fill(0);
    f[0] = 1;
    f[1] = 3;
    f[2..4].copy_from_slice(&((len - 4) as u16).to_be_bytes());
    f[4] = 2;
    f[5..7].copy_from_slice(&key_info.to_be_bytes());
    f[9..17].copy_from_slice(&replay.to_be_bytes());
    f[17..49].copy_from_slice(nonce);
    f[97..99].copy_from_slice(&(data.len() as u16).to_be_bytes());
    f[HDR_LEN..].copy_from_slice(data);
    let m = mic(kck, f);
    f[MIC_AT..MIC_AT + 16].copy_from_slice(&m);
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha1::pbkdf2;

    const AA: Addr = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
    const SPA: Addr = [0x02, 0x66, 0x77, 0x88, 0x99, 0xaa];
    const RSN: [u8; 22] = [
        48, 20, 1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 2, 0, 0,
    ];

    fn pmk() -> [u8; 32] {
        let mut p = [0u8; 32];
        pbkdf2(b"password", b"IEEE", 4096, &mut p);
        p
    }

    /// The access point's half, built from the same primitives the tests
    /// check against published vectors (`sha1`, `aes`).
    struct Ap {
        anonce: [u8; 32],
        replay: u64,
        ptk: Option<Ptk>,
        gtk: [u8; 16],
    }

    impl Ap {
        fn frame(&self, out: &mut [u8], key_info: u16, data: &[u8], with_mic: bool) -> usize {
            let len = HDR_LEN + data.len();
            out[..len].fill(0);
            out[0] = 2;
            out[1] = 3;
            out[2..4].copy_from_slice(&((len - 4) as u16).to_be_bytes());
            out[4] = 2;
            out[5..7].copy_from_slice(&key_info.to_be_bytes());
            out[7..9].copy_from_slice(&16u16.to_be_bytes());
            out[9..17].copy_from_slice(&self.replay.to_be_bytes());
            out[17..49].copy_from_slice(&self.anonce);
            out[65] = 0x2a; // RSC
            out[97..99].copy_from_slice(&(data.len() as u16).to_be_bytes());
            out[HDR_LEN..len].copy_from_slice(data);
            if with_mic {
                let m = mic(&self.ptk.unwrap().kck, &out[..len]);
                out[MIC_AT..MIC_AT + 16].copy_from_slice(&m);
            }
            len
        }

        fn msg1(&mut self, out: &mut [u8]) -> usize {
            self.replay += 1;
            self.frame(out, info::VERSION_2 | info::PAIRWISE | info::ACK, &[], false)
        }

        /// Wrapped `RSN element || GTK KDE || padding`.
        fn key_data(&self, kek: &[u8; 16], out: &mut [u8]) -> usize {
            let mut plain = [0u8; 56];
            plain[..22].copy_from_slice(&RSN);
            plain[22..28].copy_from_slice(&[0xdd, 22, 0x00, 0x0f, 0xac, 0x01]);
            plain[28] = 1; // key id 1
            plain[30..46].copy_from_slice(&self.gtk);
            plain[46] = 0xdd; // padding to a multiple of 8: dd 00 ...
            aes::wrap(kek, &plain[..48], out).unwrap()
        }

        fn msg3(&mut self, out: &mut [u8]) -> usize {
            self.replay += 1;
            let mut kd = [0u8; 64];
            let n = self.key_data(&self.ptk.unwrap().kek, &mut kd);
            let ki = info::VERSION_2 | info::PAIRWISE | info::INSTALL | info::ACK | info::MIC | info::SECURE | info::ENCRYPTED;
            self.frame(out, ki, &kd[..n], true)
        }
    }

    fn handshake() -> (Supplicant, Ap, [u8; 512]) {
        let mut sta = Supplicant::new(&pmk(), AA, SPA, [0x5a; 32], &RSN);
        let mut ap = Ap { anonce: [0xa5; 32], replay: 0, ptk: None, gtk: [0x77; 16] };
        let mut f = [0u8; 512];
        let mut r = [0u8; 512];
        let n = ap.msg1(&mut f);
        let Ok(Action::Send(m2)) = sta.handle(&f[..n], &mut r) else { panic!("msg1") };
        // The AP reads SNonce from msg 2, derives the PTK, checks msg 2's MIC.
        let k2 = Key::parse(&r[..m2]).unwrap();
        let mut sn = [0u8; 32];
        sn.copy_from_slice(k2.nonce);
        let ptk = Ptk::derive(&pmk(), &AA, &SPA, &ap.anonce, &sn);
        assert_eq!(mic(&ptk.kck, k2.frame), k2.mic);
        assert_eq!(k2.data, &RSN, "msg 2 carries the association's RSN element");
        assert_eq!(k2.info, 0x010a);
        ap.ptk = Some(ptk);
        (sta, ap, f)
    }

    #[test]
    fn ptk_matches_an_independent_derivation() {
        // Python's hashlib/hmac over the same inputs (min/max ordering by hand).
        let p = Ptk::derive(&pmk(), &AA, &SPA, &[0xa5; 32], &[0x5a; 32]);
        let want = "c949d295a4d39f41174ab9ab5040513699c6175e887303fa7df6f7cf501d0446c031f36d06d7dfdd3e7ae4e40ad3e9cd";
        let mut all = [0u8; 48];
        all[..16].copy_from_slice(&p.kck);
        all[16..32].copy_from_slice(&p.kek);
        all[32..].copy_from_slice(&p.tk);
        for (i, b) in all.iter().enumerate() {
            assert_eq!(*b, u8::from_str_radix(&want[2 * i..2 * i + 2], 16).unwrap(), "byte {i}");
        }
        // Swapping the roles of the two parties gives the same key.
        assert_eq!(Ptk::derive(&pmk(), &SPA, &AA, &[0x5a; 32], &[0xa5; 32]), p);
    }

    #[test]
    fn four_way_handshake_completes_and_hands_out_both_keys() {
        let (mut sta, mut ap, mut f) = handshake();
        let mut r = [0u8; 512];
        let n = ap.msg3(&mut f);
        let Ok(Action::Complete { len, tk, gtk }) = sta.handle(&f[..n], &mut r) else { panic!("msg3") };
        assert_eq!(tk, ap.ptk.unwrap().tk);
        assert_eq!((gtk.key, gtk.idx, gtk.rsc[0]), ([0x77; 16], 1, 0x2a));
        let k4 = Key::parse(&r[..len]).unwrap();
        assert_eq!(k4.info, 0x030a);
        assert_eq!(k4.replay, 2);
        assert_eq!(mic(&ap.ptk.unwrap().kck, k4.frame), k4.mic);
        assert!(sta.done());
        // A replayed msg 3 is refused.
        assert_eq!(sta.handle(&f[..n], &mut r), Err(Drop::Replay));
    }

    #[test]
    fn a_wrong_psk_fails_at_message_3s_mic() {
        let (_, mut ap, mut f) = handshake();
        let mut sta = Supplicant::new(&[0x42; 32], AA, SPA, [0x5a; 32], &RSN);
        let mut r = [0u8; 512];
        let mut f1 = [0u8; 512];
        let n1 = ap.frame(&mut f1, info::VERSION_2 | info::PAIRWISE | info::ACK, &[], false);
        assert!(matches!(sta.handle(&f1[..n1], &mut r), Ok(Action::Send(_))));
        let n = ap.msg3(&mut f);
        assert_eq!(sta.handle(&f[..n], &mut r), Err(Drop::Mic));
    }

    #[test]
    fn group_rekey_after_the_handshake() {
        let (mut sta, mut ap, mut f) = handshake();
        let mut r = [0u8; 512];
        let n = ap.msg3(&mut f);
        assert!(matches!(sta.handle(&f[..n], &mut r), Ok(Action::Complete { .. })));
        ap.gtk = [0x88; 16];
        ap.replay += 1;
        let mut kd = [0u8; 64];
        let kn = ap.key_data(&ap.ptk.unwrap().kek, &mut kd);
        let n = ap.frame(&mut f, info::VERSION_2 | info::ACK | info::MIC | info::SECURE | info::ENCRYPTED, &kd[..kn], true);
        let Ok(Action::Rekey { len, gtk }) = sta.handle(&f[..n], &mut r) else { panic!("rekey") };
        assert_eq!(gtk.key, [0x88; 16]);
        assert_eq!(Key::parse(&r[..len]).unwrap().info, 0x0302);
    }

    #[test]
    fn message_3_before_message_1_and_garbage_are_dropped() {
        let mut sta = Supplicant::new(&pmk(), AA, SPA, [0x5a; 32], &RSN);
        let mut ap = Ap { anonce: [0xa5; 32], replay: 0, ptk: Some(Ptk::derive(&pmk(), &AA, &SPA, &[0xa5; 32], &[0x5a; 32])), gtk: [0; 16] };
        let mut f = [0u8; 512];
        let mut r = [0u8; 512];
        let n = ap.msg3(&mut f);
        assert_eq!(sta.handle(&f[..n], &mut r), Err(Drop::Order));
        assert_eq!(sta.handle(&[0u8; 50], &mut r), Err(Drop::NotKey));
        // Version 1 (TKIP) descriptor.
        let n = ap.frame(&mut f, 1 | info::PAIRWISE | info::ACK, &[], false);
        assert_eq!(sta.handle(&f[..n], &mut r), Err(Drop::Version));
    }

    #[test]
    fn padding_after_the_body_length_is_ignored() {
        let (_, mut ap, mut f) = handshake();
        let n = ap.msg1(&mut f);
        // Ethernet-style padding past the 802.1X length.
        let k = Key::parse(&f[..n + 7]).unwrap();
        assert_eq!(k.frame.len(), n);
    }
}
