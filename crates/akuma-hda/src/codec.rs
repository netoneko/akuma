//! The codec's widget graph: discover it, find a route from an output pin back
//! to a DAC, and compute the verbs that make that route audible.
//!
//! The first bring-up hard-coded one ALC662's node numbers and amp levels from
//! a Linux `alsa-info` dump, and every one of them was a guess about a graph it
//! had never read. This module reads the graph (the runbook's step 4, which was
//! skipped) and derives the route from it, the way Linux's generic parser does:
//! walk the connection lists from a pin toward a converter, then along that
//! route **power every node, select every connection, and unmute every
//! amplifier on it**. A single muted input amp on a mixer is enough for silence
//! with every register the driver can read looking healthy.
//!
//! Nothing here touches hardware. The bus is a trait; the tests implement it
//! with a fake codec.

use crate::verb::{self, param, pinctl, AmpSet};

/// Something that can send one verb to the codec and wait for its response.
/// `None` is a timeout.
pub trait VerbBus {
    /// Send `verb` (codec address 0 encoding) and return the 32-bit response.
    fn send(&mut self, verb: u32) -> Option<u32>;
}

/// Widgets tracked. Real codecs number their widgets contiguously from the
/// function group's first NID; the ALC662 has 0x23 of them.
pub const MAX_WIDGETS: usize = 64;
/// Connection-list entries kept per widget.
pub const MAX_CONN: usize = 16;
/// Longest route (pin, mixers/selectors, converter).
pub const MAX_PATH: usize = 6;

/// What a widget is (`caps[23:20]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Audio output converter (a DAC).
    OutputConverter,
    /// Audio input converter (an ADC).
    InputConverter,
    /// Audio mixer: sums every input.
    Mixer,
    /// Audio selector: passes one input.
    Selector,
    /// Pin complex.
    Pin,
    /// Anything else, with its raw type.
    Other(u8),
    /// A NID with no widget behind it.
    Absent,
}

impl Kind {
    fn from_caps(caps: u32) -> Self {
        match (caps >> 20) & 0xF {
            0 => Self::OutputConverter,
            1 => Self::InputConverter,
            2 => Self::Mixer,
            3 => Self::Selector,
            4 => Self::Pin,
            t => Self::Other(t as u8),
        }
    }
}

/// An amplifier's capabilities (parameter 0xD / 0x12).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AmpCaps(pub u32);

impl AmpCaps {
    /// Gain step that is 0 dB.
    #[must_use]
    pub const fn offset(self) -> u8 {
        (self.0 & 0x7F) as u8
    }
    /// Number of gain steps above step 0 (0 = fixed gain).
    #[must_use]
    pub const fn num_steps(self) -> u8 {
        ((self.0 >> 8) & 0x7F) as u8
    }
    /// The gain step for "0 dB, or as close as the amp allows".
    #[must_use]
    pub const fn zero_db(self) -> u8 {
        let (o, n) = (self.offset(), self.num_steps());
        if o < n { o } else { n }
    }
}

/// Pin roles this driver will drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinRole {
    /// Line out (default device 0x0).
    LineOut,
    /// Internal speaker (0x1).
    Speaker,
    /// Headphone out (0x2).
    Headphone,
}

/// One widget.
#[derive(Clone, Copy, Debug)]
pub struct Widget {
    /// Node id.
    pub nid: u8,
    /// Widget type.
    pub kind: Kind,
    /// Raw audio-widget capabilities.
    pub caps: u32,
    /// Raw pin capabilities (pins only).
    pub pin_caps: u32,
    /// Raw pin configuration default (pins only).
    pub pin_cfg: u32,
    /// Connection list length.
    pub nconn: u8,
    /// Connection list (NIDs).
    pub conn: [u8; MAX_CONN],
    /// Input amp capabilities (valid when [`Widget::has_in_amp`]).
    pub amp_in: AmpCaps,
    /// Output amp capabilities (valid when [`Widget::has_out_amp`]).
    pub amp_out: AmpCaps,
}

impl Widget {
    const ABSENT: Self = Self {
        nid: 0,
        kind: Kind::Absent,
        caps: 0,
        pin_caps: 0,
        pin_cfg: 0,
        nconn: 0,
        conn: [0; MAX_CONN],
        amp_in: AmpCaps(0),
        amp_out: AmpCaps(0),
    };
    /// Has an input amplifier.
    #[must_use]
    pub const fn has_in_amp(&self) -> bool {
        self.caps & (1 << 1) != 0
    }
    /// Has an output amplifier.
    #[must_use]
    pub const fn has_out_amp(&self) -> bool {
        self.caps & (1 << 2) != 0
    }
    /// Supports the power-state verbs.
    #[must_use]
    pub const fn has_power_ctl(&self) -> bool {
        self.caps & (1 << 10) != 0
    }
    /// The pin's role, if it is an output pin worth driving: output-capable,
    /// physically connected (a jack or a fixed device), and a line-out,
    /// speaker or headphone.
    #[must_use]
    pub fn output_role(&self) -> Option<PinRole> {
        if self.kind != Kind::Pin || self.pin_caps & (1 << 4) == 0 {
            return None;
        }
        if (self.pin_cfg >> 30) == 1 {
            return None; // port connectivity: no physical connection
        }
        match (self.pin_cfg >> 20) & 0xF {
            0 => Some(PinRole::LineOut),
            1 => Some(PinRole::Speaker),
            2 => Some(PinRole::Headphone),
            _ => None,
        }
    }
    /// Pin caps: EAPD-capable.
    #[must_use]
    pub const fn eapd_capable(&self) -> bool {
        self.pin_caps & (1 << 16) != 0
    }
    /// Pin caps: headphone-drive capable.
    #[must_use]
    pub const fn hp_capable(&self) -> bool {
        self.pin_caps & (1 << 3) != 0
    }
}

/// The audio function group's widgets.
pub struct Graph {
    /// NID of the audio function group.
    pub afg: u8,
    /// NID of `widgets[0]`.
    pub first: u8,
    /// Widgets found.
    pub count: u8,
    /// The widgets, indexed by `nid - first`.
    pub widgets: [Widget; MAX_WIDGETS],
}

impl Graph {
    /// The widget with node id `nid`.
    #[must_use]
    pub fn get(&self, nid: u8) -> Option<&Widget> {
        let i = usize::from(nid.checked_sub(self.first)?);
        if i < usize::from(self.count) && self.widgets[i].kind != Kind::Absent {
            Some(&self.widgets[i])
        } else {
            None
        }
    }
    /// Iterate the widgets that exist.
    pub fn iter(&self) -> impl Iterator<Item = &Widget> {
        self.widgets[..usize::from(self.count)].iter().filter(|w| w.kind != Kind::Absent)
    }
}

fn node_range(r: u32) -> (u8, u8) {
    (((r >> 16) & 0xFF) as u8, (r & 0xFF) as u8)
}

/// Read one widget's connection list. Short form (four 8-bit entries per
/// response) and long form (two 16-bit entries) are both handled; a short-form
/// entry with bit 7 set is the end of a range that starts one past the entry
/// before it. Returns the number of entries stored.
fn read_conn_list<B: VerbBus + ?Sized>(bus: &mut B, nid: u8, out: &mut [u8; MAX_CONN]) -> u8 {
    let Some(len_r) = bus.send(verb::get_param(nid, param::CONN_LIST_LEN)) else { return 0 };
    let long = len_r & 0x80 != 0;
    let total = (len_r & 0x7F) as usize;
    let (per, bits) = if long { (2usize, 16u32) } else { (4usize, 8u32) };
    let mask = (1u32 << bits) - 1;
    let range_bit = 1u32 << (bits - 1);
    let mut stored = 0usize;
    let mut prev: Option<u32> = None;
    let mut done = 0usize;
    while done < total {
        let Some(resp) = bus.send(verb::get_conn_list(nid, done as u8)) else { break };
        for slot in 0..per {
            if done + slot >= total {
                break;
            }
            let entry = (resp >> (bits * slot as u32)) & mask;
            if entry & range_bit != 0 {
                if let Some(before) = prev {
                    let end = entry & !range_bit;
                    let mut next = before + 1;
                    while next <= end && stored < MAX_CONN {
                        out[stored] = next as u8;
                        stored += 1;
                        next += 1;
                    }
                }
            } else if stored < MAX_CONN {
                out[stored] = entry as u8;
                stored += 1;
            }
            prev = Some(entry & !range_bit);
        }
        done += per;
    }
    stored as u8
}

/// Read the codec's audio function group: the root's function groups, the
/// first of type 1 (audio), and every widget under it. `None` if the codec does
/// not answer or has no audio function group.
pub fn discover<B: VerbBus + ?Sized>(bus: &mut B) -> Option<Graph> {
    let (fg_first, fg_count) = node_range(bus.send(verb::get_param(0, param::NODE_COUNT))?);
    let mut afg = None;
    for nid in fg_first..fg_first.saturating_add(fg_count) {
        if bus.send(verb::get_param(nid, param::FUNCTION_TYPE))? & 0xFF == 1 {
            afg = Some(nid);
            break;
        }
    }
    let afg = afg?;
    let (first, count) = node_range(bus.send(verb::get_param(afg, param::NODE_COUNT))?);
    let count = count.min(MAX_WIDGETS as u8);
    // The group's default amp capabilities; a widget only carries its own when
    // its "amp param override" bit says so.
    let afg_in = AmpCaps(bus.send(verb::get_param(afg, param::AMP_IN_CAP)).unwrap_or(0));
    let afg_out = AmpCaps(bus.send(verb::get_param(afg, param::AMP_OUT_CAP)).unwrap_or(0));

    let mut g = Graph { afg, first, count, widgets: [Widget::ABSENT; MAX_WIDGETS] };
    for i in 0..count {
        let nid = first + i;
        let Some(caps) = bus.send(verb::get_param(nid, param::AUDIO_WIDGET_CAP)) else { continue };
        if caps == 0 {
            continue; // an unimplemented NID answers 0, which would decode as a DAC
        }
        let mut w = Widget { nid, kind: Kind::from_caps(caps), caps, ..Widget::ABSENT };
        if w.kind == Kind::Pin {
            w.pin_caps = bus.send(verb::get_param(nid, param::PIN_CAP)).unwrap_or(0);
            w.pin_cfg = bus.send(verb::get_cfg_default(nid)).unwrap_or(0);
        }
        let over = caps & (1 << 3) != 0;
        if w.has_in_amp() {
            w.amp_in = if over {
                AmpCaps(bus.send(verb::get_param(nid, param::AMP_IN_CAP)).unwrap_or(0))
            } else {
                afg_in
            };
        }
        if w.has_out_amp() {
            w.amp_out = if over {
                AmpCaps(bus.send(verb::get_param(nid, param::AMP_OUT_CAP)).unwrap_or(0))
            } else {
                afg_out
            };
        }
        if caps & (1 << 8) != 0 {
            w.nconn = read_conn_list(bus, nid, &mut w.conn);
        }
        g.widgets[usize::from(i)] = w;
    }
    Some(g)
}

/// A route from an output pin back to a converter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Path {
    /// Nodes in the route.
    pub len: u8,
    /// `nid[0]` is the pin, `nid[len-1]` the converter.
    pub nid: [u8; MAX_PATH],
    /// `idx[i]` is the connection index of `nid[i]` that `nid[i+1]` feeds.
    pub idx: [u8; MAX_PATH],
}

impl Path {
    /// The output pin.
    #[must_use]
    pub fn pin(&self) -> u8 {
        self.nid[0]
    }
    /// The output converter.
    #[must_use]
    pub fn converter(&self) -> u8 {
        self.nid[usize::from(self.len) - 1]
    }
}

fn dfs(g: &Graph, nid: u8, depth: usize, avoid: &[u8], p: &mut Path) -> bool {
    let Some(w) = g.get(nid) else { return false };
    if depth >= MAX_PATH {
        return false;
    }
    for (i, &c) in w.conn[..usize::from(w.nconn)].iter().enumerate() {
        let Some(cw) = g.get(c) else { continue };
        match cw.kind {
            Kind::OutputConverter if !avoid.contains(&c) => {
                p.nid[depth] = nid;
                p.idx[depth] = i as u8;
                p.nid[depth + 1] = c;
                p.len = depth as u8 + 2;
                return true;
            }
            Kind::Mixer | Kind::Selector => {
                p.nid[depth] = nid;
                p.idx[depth] = i as u8;
                if dfs(g, c, depth + 1, avoid, p) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Find a route from `pin` to an output converter, preferring one that avoids
/// the converters in `avoid` (so two pins get two DACs when the codec has
/// them), and falling back to a shared one.
#[must_use]
pub fn find_path(g: &Graph, pin: u8, avoid: &[u8]) -> Option<Path> {
    let mut p = Path { len: 0, nid: [0; MAX_PATH], idx: [0; MAX_PATH] };
    if dfs(g, pin, 0, avoid, &mut p) || dfs(g, pin, 0, &[], &mut p) {
        Some(p)
    } else {
        None
    }
}

/// A fixed-size list of verbs to send.
pub struct VerbList {
    /// The verbs.
    pub v: [u32; 192],
    /// How many are used.
    pub n: usize,
}

impl VerbList {
    /// An empty list.
    #[must_use]
    pub const fn new() -> Self {
        Self { v: [0; 192], n: 0 }
    }
    /// Append; `false` if full.
    pub fn push(&mut self, verb: u32) -> bool {
        if self.n == self.v.len() {
            return false;
        }
        self.v[self.n] = verb;
        self.n += 1;
        true
    }
    /// The verbs, in order.
    #[must_use]
    pub fn as_slice(&self) -> &[u32] {
        &self.v[..self.n]
    }
}

impl Default for VerbList {
    fn default() -> Self {
        Self::new()
    }
}

/// The DAC amplifier's gain: `pct` percent of its range from 0 dB downward
/// would be wrong (steps above `offset` are gain, below are attenuation), so
/// this is `pct` percent of the 0 dB step. 100 is 0 dB.
#[must_use]
pub fn dac_gain(a: AmpCaps, pct: u8) -> u8 {
    if a.num_steps() == 0 {
        return 0;
    }
    (u32::from(a.zero_db()) * u32::from(pct.min(100)) / 100) as u8
}

/// Verbs that power the audio function group.
pub fn afg_verbs(g: &Graph, out: &mut VerbList) {
    out.push(verb::set_power(g.afg, 0));
}

/// Verbs that make `path` audible: power, connection selects, every amplifier
/// on the route unmuted, then the pin switched to output. `dac_pct` is the
/// converter amp's level as a percentage of its 0 dB step.
pub fn path_verbs(g: &Graph, path: &Path, dac_pct: u8, out: &mut VerbList) {
    for i in (0..usize::from(path.len)).rev() {
        let Some(w) = g.get(path.nid[i]) else { continue };
        let is_dac = i == usize::from(path.len) - 1;
        if w.has_power_ctl() {
            out.push(verb::set_power(w.nid, 0));
        }
        if !is_dac {
            if w.nconn > 1 && w.kind != Kind::Mixer {
                out.push(verb::set_conn_sel(w.nid, path.idx[i]));
            }
            if w.has_in_amp() {
                out.push(verb::set_amp(w.nid, AmpSet::inp(path.idx[i], false, w.amp_in.zero_db())));
            }
        }
        if w.has_out_amp() {
            let gain = if is_dac { dac_gain(w.amp_out, dac_pct) } else { w.amp_out.zero_db() };
            out.push(verb::set_amp(w.nid, AmpSet::out(false, gain)));
        }
    }
    if let Some(pin) = g.get(path.pin()) {
        let mut ctl = pinctl::OUT_EN;
        if pin.output_role() == Some(PinRole::Headphone) && pin.hp_capable() {
            ctl |= pinctl::HP_EN;
        }
        out.push(verb::set_pin_ctrl(pin.nid, ctl));
        if pin.eapd_capable() {
            out.push(verb::set_eapd(pin.nid, verb::EAPD_ON));
        }
    }
}

/// Verbs that bind converter `dac` to stream `tag` at format `fmt`.
pub fn stream_verbs(dac: u8, tag: u8, fmt: u16, out: &mut VerbList) {
    out.push(verb::set_conv_fmt(dac, fmt));
    out.push(verb::set_stream(dac, tag, 0));
}

#[cfg(test)]
#[allow(clippy::many_single_char_names)] // g/c/p/q/l: graph, codec, path, path, list
mod tests {
    use super::*;

    /// A codec shaped like the trashcan's Realtek ALC662, as far as the
    /// boot-log census showed it: DACs 2..4, mixers 0x0b/0x0c, a line-out pin
    /// 0x14 and a headphone pin 0x1b, a mic and a line-in, and a pin with no
    /// physical connection. The connection lists are a plausible ALC662 wiring,
    /// not a dump — the point of the tests is that the *code* follows whatever
    /// the lists say.
    struct FakeCodec {
        widgets: [(u8, u32, u32, u32, &'static [u8]); 12],
    }

    const PIN_CAPS_OUT: u32 = (1 << 4) | (1 << 3) | (1 << 16);

    impl FakeCodec {
        fn alc662() -> Self {
            // (nid, widget caps, pin caps, pin cfg, connections)
            let dac = 0x0000_0001 | (1 << 2) | (1 << 10); // stereo, out amp, power
            let mixer = (2 << 20) | 1 | (1 << 1) | (1 << 8) | (1 << 10); // in amp, conn list
            let pin = (4 << 20) | 1 | (1 << 8) | (1 << 10);
            Self {
                widgets: [
                    (2, dac, 0, 0, &[]),
                    (3, dac, 0, 0, &[]),
                    (4, dac, 0, 0, &[]),
                    (0x0b, mixer, 0, 0, &[0x02, 0x03]),
                    (0x0c, mixer, 0, 0, &[0x02, 0x03, 0x04]),
                    (0x12, pin, PIN_CAPS_OUT, 0x4000_0000, &[0x0c]),
                    (0x14, pin, PIN_CAPS_OUT, 0x0101_4010, &[0x0c, 0x0b]),
                    (0x18, pin | (1 << 1), 1 << 5, 0x02A1_9020, &[]),
                    (0x1b, pin | (1 << 1) | (1 << 2), PIN_CAPS_OUT, 0x0221_401F, &[0x0c, 0x0b]),
                    (0x1c, pin, PIN_CAPS_OUT, 0x4111_11F0, &[0x0c]),
                    (0x0d, 0, 0, 0, &[]),
                    (0x0e, 0, 0, 0, &[]),
                ],
            }
        }
        fn find(&self, nid: u8) -> Option<&(u8, u32, u32, u32, &'static [u8])> {
            self.widgets.iter().find(|w| w.0 == nid && w.1 != 0)
        }
    }

    impl VerbBus for FakeCodec {
        fn send(&mut self, v: u32) -> Option<u32> {
            let nid = ((v >> 20) & 0xFF) as u8;
            let verb12 = (v >> 8) & 0xFFF;
            let p = (v & 0xFF) as u8;
            match (nid, verb12, p) {
                (0, 0xF00, 0x04) => Some(0x0001_0001), // one function group at nid 1
                (1, 0xF00, 0x05) => Some(1),
                (1, 0xF00, 0x04) => Some((2 << 16) | 0x1c), // widgets 2..0x1d
                (1, 0xF00, 0x0D) => Some((0x1F << 8) | 0x17), // in amps: 0 dB at step 0x17
                (1, 0xF00, 0x12) => Some((0x57 << 8) | 0x57), // out amp: 87 steps, 0dB at 87
                (n, 0xF00, 0x09) => Some(self.find(n).map_or(0, |w| w.1)),
                (n, 0xF00, 0x0C) => Some(self.find(n).map_or(0, |w| w.2)),
                (n, 0xF1C, _) => Some(self.find(n).map_or(0, |w| w.3)),
                (n, 0xF00, 0x0E) => Some(self.find(n).map_or(0, |w| w.4.len() as u32)),
                (n, 0xF02, i) => {
                    let c = self.find(n)?.4;
                    let mut r = 0u32;
                    for k in 0..4 {
                        if let Some(&e) = c.get(usize::from(i) + k) {
                            r |= u32::from(e) << (8 * k);
                        }
                    }
                    Some(r)
                }
                _ => Some(0),
            }
        }
    }

    #[test]
    fn discovers_the_graph() {
        let mut c = FakeCodec::alc662();
        let g = discover(&mut c).expect("graph");
        assert_eq!((g.afg, g.first, g.count), (1, 2, 0x1c));
        assert_eq!(g.get(0x1b).unwrap().kind, Kind::Pin);
        assert_eq!(g.get(0x0c).unwrap().nconn, 3);
        assert_eq!(&g.get(0x0c).unwrap().conn[..3], &[2, 3, 4]);
        assert_eq!(g.get(2).unwrap().kind, Kind::OutputConverter);
        // The AFG's out-amp caps are inherited by a widget that does not override.
        assert_eq!(g.get(2).unwrap().amp_out.num_steps(), 0x57);
        assert!(g.get(0x05).is_none()); // a NID with no widget
    }

    #[test]
    fn output_pins_are_the_connected_output_capable_ones() {
        let mut c = FakeCodec::alc662();
        let g = discover(&mut c).unwrap();
        let roles: Vec<(u8, PinRole)> = g.iter().filter_map(|w| w.output_role().map(|r| (w.nid, r))).collect();
        // 0x12 and 0x1c are "no physical connection"; 0x18 is a mic (input only).
        assert_eq!(roles, vec![(0x14, PinRole::LineOut), (0x1b, PinRole::Headphone)]);
    }

    #[test]
    fn paths_follow_the_connection_lists() {
        let mut c = FakeCodec::alc662();
        let g = discover(&mut c).unwrap();
        let p = find_path(&g, 0x1b, &[]).unwrap();
        assert_eq!(p.len, 3);
        assert_eq!(&p.nid[..3], &[0x1b, 0x0c, 0x02]);
        assert_eq!(&p.idx[..2], &[0, 0]);
        assert_eq!((p.pin(), p.converter()), (0x1b, 2));
        // A second pin avoids the DAC the first took, when the wiring allows.
        let q = find_path(&g, 0x14, &[p.converter()]).unwrap();
        assert_eq!(q.converter(), 3);
        assert_eq!(&q.nid[..3], &[0x14, 0x0c, 0x03]);
        assert_eq!(q.idx[1], 1);
        // ...and falls back to sharing when it does not.
        let r = find_path(&g, 0x14, &[2, 3, 4]).unwrap();
        assert_eq!(r.converter(), 2);
        assert!(find_path(&g, 0x18, &[]).is_none());
    }

    #[test]
    fn path_verbs_unmute_every_amp_and_enable_the_pin() {
        let mut c = FakeCodec::alc662();
        let g = discover(&mut c).unwrap();
        let p = find_path(&g, 0x1b, &[]).unwrap();
        let mut l = VerbList::new();
        path_verbs(&g, &p, 64, &mut l);
        let v = l.as_slice();
        // DAC 2: power, then output amp at 64% of 0 dB (0x57 * 64 / 100 = 55).
        assert_eq!(v[0], verb::set_power(2, 0));
        assert_eq!(v[1], verb::set_amp(2, AmpSet::out(false, 55)));
        // Mixer 0x0c: power, input amp for connection 0 unmuted. No connection
        // select — a mixer sums its inputs.
        assert!(v.contains(&verb::set_power(0x0c, 0)));
        assert!(v.contains(&verb::set_amp(0x0c, AmpSet::inp(0, false, 0x17))));
        assert!(!v.contains(&verb::set_conn_sel(0x0c, 0)));
        // Pin 0x1b: headphone output enabled, EAPD on.
        assert!(v.contains(&verb::set_pin_ctrl(0x1b, 0xC0)));
        assert_eq!(*v.last().unwrap(), verb::set_eapd(0x1b, 2));
        // The pin's own amps: input at connection 0, output at 0 dB.
        assert!(v.contains(&verb::set_amp(0x1b, AmpSet::inp(0, false, 0x17))));
        assert!(v.contains(&verb::set_amp(0x1b, AmpSet::out(false, 0x57))));
        // Nothing addressed to a widget the graph does not have.
        for w in v {
            let nid = ((w >> 20) & 0xFF) as u8;
            assert!(g.get(nid).is_some(), "verb {w:#x} to phantom nid {nid:#x}");
        }
    }

    #[test]
    fn line_out_pin_does_not_get_the_headphone_bit() {
        let mut c = FakeCodec::alc662();
        let g = discover(&mut c).unwrap();
        let p = find_path(&g, 0x14, &[]).unwrap();
        let mut l = VerbList::new();
        path_verbs(&g, &p, 100, &mut l);
        assert!(l.as_slice().contains(&verb::set_pin_ctrl(0x14, 0x40)));
        assert!(!l.as_slice().contains(&verb::set_pin_ctrl(0x14, 0xC0)));
    }

    #[test]
    fn selectors_get_a_connection_select() {
        // pin 0x20 <- selector 0x21 (inputs: dac 2, dac 3) ; take input 1.
        let mut g = Graph { afg: 1, first: 2, count: 0x20, widgets: [Widget::ABSENT; MAX_WIDGETS] };
        let mk = |nid: u8, kind: Kind, conn: &[u8]| {
            let mut w = Widget { nid, kind, caps: 1 << 10, ..Widget::ABSENT };
            w.nconn = conn.len() as u8;
            w.conn[..conn.len()].copy_from_slice(conn);
            w
        };
        let put = |g: &mut Graph, w: Widget| g.widgets[usize::from(w.nid - 2)] = w;
        put(&mut g, mk(2, Kind::OutputConverter, &[]));
        put(&mut g, mk(3, Kind::OutputConverter, &[]));
        put(&mut g, mk(0x21, Kind::Selector, &[2, 3]));
        let mut pin = mk(0x20, Kind::Pin, &[0x21]);
        pin.pin_caps = PIN_CAPS_OUT;
        pin.pin_cfg = 0x0221_401F;
        put(&mut g, pin);
        let p = find_path(&g, 0x20, &[2]).unwrap();
        assert_eq!(p.converter(), 3);
        let mut l = VerbList::new();
        path_verbs(&g, &p, 100, &mut l);
        assert!(l.as_slice().contains(&verb::set_conn_sel(0x21, 1)));
    }

    #[test]
    fn stream_verbs_bind_tag_and_format() {
        let mut l = VerbList::new();
        stream_verbs(2, 1, 0x4011, &mut l);
        assert_eq!(l.as_slice(), &[0x0022_4011, 0x0027_0610]);
    }

    #[test]
    fn dac_gain_scales_the_zero_db_step() {
        let a = AmpCaps((0x57 << 8) | 0x57);
        assert_eq!(dac_gain(a, 100), 0x57);
        assert_eq!(dac_gain(a, 64), 55);
        assert_eq!(dac_gain(a, 0), 0);
        assert_eq!(dac_gain(a, 200), 0x57);
        assert_eq!(dac_gain(AmpCaps(0), 64), 0); // fixed-gain amp
    }

    #[test]
    fn range_entries_in_a_connection_list_expand() {
        // Two entries: 0x02, then 0x84 = "through 4" (bit 7 marks a range end).
        struct B;
        impl VerbBus for B {
            fn send(&mut self, v: u32) -> Option<u32> {
                match (v >> 8) & 0xFFF {
                    0xF00 => Some(2),
                    0xF02 => Some(0x0000_8402),
                    _ => None,
                }
            }
        }
        let mut out = [0u8; MAX_CONN];
        let n = read_conn_list(&mut B, 0x0c, &mut out);
        assert_eq!(&out[..usize::from(n)], &[2, 3, 4]);
    }
}
