//! CCMP receive replay protection (802.11-2020 12.5.3.4.4).
//!
//! The card decrypts and verifies the MIC but leaves the CCMP header in the
//! frame; nothing in it checks that the packet number (PN) is *new*, so an
//! attacker who records a valid frame can play it back. mac80211 keeps, per
//! key, the highest PN seen per TID and drops any frame that does not exceed
//! it; this is that, for one association.
//!
//! Pairwise and group frames have separate counters: the pairwise key is
//! key id 0, the group key may use ids 0..=3 and each has its own space.
//! Non-QoS data counts as TID 0, as in mac80211.

use crate::frame::{HDR_LEN, Hdr, TYPE_DATA};
use crate::sta::{CCMP_HDR_LEN, QOS_HDR_LEN};

const TIDS: usize = 16;
const KEYIDS: usize = 4;

/// The security fields of a received protected data frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rx {
    /// Addressed to a group (group-key frame) rather than to us.
    pub group: bool,
    pub tid: u8,
    pub keyid: u8,
    /// The 48-bit packet number.
    pub pn: u64,
}

/// Read the CCMP header of a protected from-DS data frame (QoS or not, with
/// or without HT control). `None` if the frame is not protected data or is
/// too short for the header, the Ext IV bit being clear included.
#[must_use]
pub fn header(f: &[u8], fcs: bool) -> Option<Rx> {
    let f = if fcs { f.get(..f.len().checked_sub(4)?)? } else { f };
    let h = Hdr::parse(f)?;
    if h.fc.ty() != TYPE_DATA || !h.fc.protected() {
        return None;
    }
    let st = h.fc.subtype();
    let (mut at, tid) = if st & 8 != 0 {
        (QOS_HDR_LEN, *f.get(24)? & 0x0f)
    } else {
        (HDR_LEN, 0)
    };
    if st & 8 != 0 && h.fc.0 & (1 << 15) != 0 {
        at += 4; // HT control
    }
    let c = f.get(at..at + CCMP_HDR_LEN)?;
    if c[3] & 0x20 == 0 {
        return None; // no Ext IV: not CCMP
    }
    let pn = u64::from(c[0])
        | u64::from(c[1]) << 8
        | u64::from(c[4]) << 16
        | u64::from(c[5]) << 24
        | u64::from(c[6]) << 32
        | u64::from(c[7]) << 40;
    Some(Rx { group: h.addr1[0] & 1 != 0, tid, keyid: c[3] >> 6, pn })
}

/// The highest PN accepted so far, per key and TID.
#[derive(Clone, Debug)]
pub struct Replay {
    pairwise: [Option<u64>; TIDS],
    group: [[Option<u64>; TIDS]; KEYIDS],
    /// Frames refused as replays since creation.
    pub replays: u32,
}

impl Default for Replay {
    fn default() -> Self {
        Self::new()
    }
}

impl Replay {
    #[must_use]
    pub const fn new() -> Self {
        Self { pairwise: [None; TIDS], group: [[None; TIDS]; KEYIDS], replays: 0 }
    }

    /// Accept `rx` if its PN is above everything seen on its key and TID, and
    /// remember it; otherwise count a replay and refuse.
    pub fn accept(&mut self, rx: &Rx) -> bool {
        let tid = usize::from(rx.tid) & (TIDS - 1);
        let slot = if rx.group {
            &mut self.group[usize::from(rx.keyid) & (KEYIDS - 1)][tid]
        } else {
            &mut self.pairwise[tid]
        };
        if slot.is_some_and(|last| rx.pn <= last) {
            self.replays = self.replays.wrapping_add(1);
            return false;
        }
        *slot = Some(rx.pn);
        true
    }

    /// A new group key was installed: its counters start over.
    pub fn group_rekeyed(&mut self) {
        self.group = [[None; TIDS]; KEYIDS];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rx(group: bool, tid: u8, keyid: u8, pn: u64) -> Rx {
        Rx { group, tid, keyid, pn }
    }

    #[test]
    fn rejects_equal_and_lower() {
        let mut r = Replay::new();
        assert!(r.accept(&rx(false, 0, 0, 5)));
        assert!(!r.accept(&rx(false, 0, 0, 5)));
        assert!(!r.accept(&rx(false, 0, 0, 4)));
        assert!(r.accept(&rx(false, 0, 0, 6)));
        assert_eq!(r.replays, 2);
    }

    #[test]
    fn first_frame_may_have_pn_zero() {
        let mut r = Replay::new();
        assert!(r.accept(&rx(false, 3, 0, 0)));
        assert!(!r.accept(&rx(false, 3, 0, 0)));
    }

    #[test]
    fn tids_and_keys_are_independent() {
        let mut r = Replay::new();
        assert!(r.accept(&rx(false, 0, 0, 100)));
        assert!(r.accept(&rx(false, 1, 0, 1)));
        assert!(r.accept(&rx(true, 0, 2, 1)));
        assert!(r.accept(&rx(true, 0, 1, 1)));
        assert!(!r.accept(&rx(true, 0, 2, 1)));
    }

    #[test]
    fn group_rekey_resets_group_only() {
        let mut r = Replay::new();
        assert!(r.accept(&rx(true, 0, 2, 50)));
        assert!(r.accept(&rx(false, 0, 0, 50)));
        r.group_rekeyed();
        assert!(r.accept(&rx(true, 0, 2, 1)));
        assert!(!r.accept(&rx(false, 0, 0, 50)));
    }

    /// A QoS data frame, from-DS, protected, with `pn` and `keyid`.
    fn frame(qos: bool, htc: bool, da0: u8, tid: u8, keyid: u8, pn: [u8; 6]) -> [u8; 48] {
        let mut f = [0u8; 48];
        f[0] = if qos { 0x88 } else { 0x08 };
        f[1] = 0x02 | 0x40 | if htc { 0x80 } else { 0 };
        f[4] = da0;
        let mut at = 24;
        if qos {
            f[24] = tid;
            at = 26;
        }
        if htc && qos {
            at += 4;
        }
        f[at..at + 8].copy_from_slice(&[pn[0], pn[1], 0, 0x20 | (keyid << 6), pn[2], pn[3], pn[4], pn[5]]);
        f
    }

    #[test]
    fn header_reads_the_pn_in_wire_order() {
        let f = frame(true, false, 0x02, 5, 0, [1, 2, 3, 4, 5, 6]);
        let h = header(&f, false).unwrap();
        assert_eq!(h, rx(false, 5, 0, 0x0605_0403_0201));
    }

    #[test]
    fn header_sees_group_keyid_non_qos_and_htc() {
        let g = frame(true, false, 0x01, 0, 2, [9, 0, 0, 0, 0, 0]);
        assert_eq!(header(&g, false).unwrap(), rx(true, 0, 2, 9));
        let n = frame(false, false, 0x02, 0, 0, [7, 0, 0, 0, 0, 0]);
        assert_eq!(header(&n, false).unwrap(), rx(false, 0, 0, 7));
        let h = frame(true, true, 0x02, 1, 0, [8, 0, 0, 0, 0, 0]);
        assert_eq!(header(&h, false).unwrap().pn, 8);
    }

    #[test]
    fn header_refuses_clear_and_short_frames() {
        let mut f = frame(true, false, 0x02, 0, 0, [1, 0, 0, 0, 0, 0]);
        f[1] &= !0x40;
        assert!(header(&f, false).is_none());
        let f = frame(true, false, 0x02, 0, 0, [1, 0, 0, 0, 0, 0]);
        assert!(header(&f[..30], false).is_none());
        // Ext IV clear.
        let mut f = frame(true, false, 0x02, 0, 0, [1, 0, 0, 0, 0, 0]);
        f[29] = 0;
        assert!(header(&f, false).is_none());
    }
}
