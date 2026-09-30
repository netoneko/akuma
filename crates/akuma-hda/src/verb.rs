//! Codec verb encoding (Intel HDA 1.0a §7.1, Table 53).
//!
//! A verb is one 32-bit word: `CAd[31:28] | NID[27:20] | command[19:0]`, and the
//! command comes in two shapes — a **12-bit verb + 8-bit payload**
//! (`[19:8]`/`[7:0]`: get-parameter, connection select, power, stream/channel,
//! pin control, EAPD, ...) and a **4-bit verb + 16-bit payload**
//! (`[19:16]`/`[15:0]`: converter format, amplifier gain/mute).
//!
//! **The NID sits in bits 27:20, not 23:16.** This module exists because the
//! first HDA bring-up hand-assembled its words as hex literals and put the NID
//! one nibble too high: `0x0220_2011` was meant as "set converter 2 to
//! 48 kHz/16-bit" and decodes as *verb 0 to widget 0x22*, which no codec has,
//! so every set was silently ignored and every readback of the same phantom
//! widget read 0. Nothing here is a literal a person has to nibble-count — it
//! is a function of its fields, with known-answer tests below.
//!
//! The codec address (CAd) is left at 0 by every encoder; the bus that sends
//! the verb ORs it in with [`with_cad`].

/// Parameter ids for [`get_param`] (§7.3.4).
pub mod param {
    /// Vendor/device id (root node).
    pub const VENDOR_ID: u8 = 0x00;
    /// Subordinate node count: `[23:16]` first NID, `[7:0]` count.
    pub const NODE_COUNT: u8 = 0x04;
    /// Function group type (`[7:0]`, 1 = audio function group).
    pub const FUNCTION_TYPE: u8 = 0x05;
    /// Audio widget capabilities.
    pub const AUDIO_WIDGET_CAP: u8 = 0x09;
    /// Supported PCM sizes and rates.
    pub const PCM: u8 = 0x0A;
    /// Supported stream formats (bit 0 = PCM).
    pub const STREAM_FORMATS: u8 = 0x0B;
    /// Pin capabilities.
    pub const PIN_CAP: u8 = 0x0C;
    /// Input amplifier capabilities.
    pub const AMP_IN_CAP: u8 = 0x0D;
    /// Connection list length: `[6:0]` entries, `[7]` long form.
    pub const CONN_LIST_LEN: u8 = 0x0E;
    /// Supported power states.
    pub const POWER_STATES: u8 = 0x0F;
    /// GPIO count: `[7:0]` number of GPIO pins.
    pub const GPIO_COUNT: u8 = 0x11;
    /// Output amplifier capabilities.
    pub const AMP_OUT_CAP: u8 = 0x12;
}

/// A 12-bit-verb command word for `nid` (codec address 0).
#[must_use]
pub const fn cmd12(nid: u8, verb: u16, payload: u8) -> u32 {
    ((nid as u32) << 20) | (((verb as u32) & 0xFFF) << 8) | payload as u32
}

/// A 4-bit-verb command word for `nid` (codec address 0).
#[must_use]
pub const fn cmd4(nid: u8, verb: u8, payload: u16) -> u32 {
    ((nid as u32) << 20) | (((verb as u32) & 0xF) << 16) | payload as u32
}

/// `verb` addressed to codec `cad`.
#[must_use]
pub const fn with_cad(verb: u32, cad: u8) -> u32 {
    (verb & 0x0FFF_FFFF) | (((cad as u32) & 0xF) << 28)
}

/// GET_PARAMETER (0xF00).
#[must_use]
pub const fn get_param(nid: u8, p: u8) -> u32 {
    cmd12(nid, 0xF00, p)
}
/// GET_CONNECTION_SELECT_CONTROL (0xF01).
#[must_use]
pub const fn get_conn_sel(nid: u8) -> u32 {
    cmd12(nid, 0xF01, 0)
}
/// GET_CONNECTION_LIST_ENTRY (0xF02), starting at entry `index`.
#[must_use]
pub const fn get_conn_list(nid: u8, index: u8) -> u32 {
    cmd12(nid, 0xF02, index)
}
/// GET_PIN_SENSE (0xF09): `[31]` presence detect (a plug is in the jack).
#[must_use]
pub const fn get_pin_sense(nid: u8) -> u32 {
    cmd12(nid, 0xF09, 0)
}
/// GET_GPIO_DATA (0xF15).
#[must_use]
pub const fn get_gpio_data(nid: u8) -> u32 {
    cmd12(nid, 0xF15, 0)
}
/// GET_GPIO_ENABLE_MASK (0xF16).
#[must_use]
pub const fn get_gpio_enable(nid: u8) -> u32 {
    cmd12(nid, 0xF16, 0)
}
/// GET_GPIO_DIRECTION (0xF17).
#[must_use]
pub const fn get_gpio_dir(nid: u8) -> u32 {
    cmd12(nid, 0xF17, 0)
}
/// GET_POWER_STATE (0xF05).
#[must_use]
pub const fn get_power(nid: u8) -> u32 {
    cmd12(nid, 0xF05, 0)
}
/// GET_CONVERTER_STREAM_CHANNEL (0xF06): `[7:4]` stream tag, `[3:0]` channel.
#[must_use]
pub const fn get_stream(nid: u8) -> u32 {
    cmd12(nid, 0xF06, 0)
}
/// GET_PIN_WIDGET_CONTROL (0xF07).
#[must_use]
pub const fn get_pin_ctrl(nid: u8) -> u32 {
    cmd12(nid, 0xF07, 0)
}
/// GET_EAPD/BTL_ENABLE (0xF0C).
#[must_use]
pub const fn get_eapd(nid: u8) -> u32 {
    cmd12(nid, 0xF0C, 0)
}
/// GET_CONFIGURATION_DEFAULT (0xF1C).
#[must_use]
pub const fn get_cfg_default(nid: u8) -> u32 {
    cmd12(nid, 0xF1C, 0)
}

/// SET_CONNECTION_SELECT_CONTROL (0x701).
///
/// **Not** a VREF control — pin VREF is `[2:0]` of [`set_pin_ctrl`]. The first
/// bring-up sent `0x701` with `0xC3` meaning "VREF"; it selected a connection
/// index that does not exist.
#[must_use]
pub const fn set_conn_sel(nid: u8, index: u8) -> u32 {
    cmd12(nid, 0x701, index)
}
/// SET_POWER_STATE (0x705); `state` 0 = D0 (fully on).
#[must_use]
pub const fn set_power(nid: u8, state: u8) -> u32 {
    cmd12(nid, 0x705, state)
}
/// SET_CONVERTER_STREAM_CHANNEL (0x706): bind a converter to a stream tag.
#[must_use]
pub const fn set_stream(nid: u8, tag: u8, channel: u8) -> u32 {
    cmd12(nid, 0x706, ((tag & 0xF) << 4) | (channel & 0xF))
}
/// SET_PIN_WIDGET_CONTROL (0x707).
#[must_use]
pub const fn set_pin_ctrl(nid: u8, ctrl: u8) -> u32 {
    cmd12(nid, 0x707, ctrl)
}
/// SET_EAPD/BTL_ENABLE (0x70C).
#[must_use]
pub const fn set_eapd(nid: u8, v: u8) -> u32 {
    cmd12(nid, 0x70C, v)
}
/// SET_CONVERTER_FORMAT (verb 0x2, 16-bit payload — see [`crate::stream::format_word`]).
#[must_use]
pub const fn set_conv_fmt(nid: u8, fmt: u16) -> u32 {
    cmd4(nid, 0x2, fmt)
}
/// GET_CONVERTER_FORMAT (verb 0xA).
#[must_use]
pub const fn get_conv_fmt(nid: u8) -> u32 {
    cmd4(nid, 0xA, 0)
}

/// `SET_PIN_WIDGET_CONTROL` bits.
pub mod pinctl {
    /// Output enable.
    pub const OUT_EN: u8 = 0x40;
    /// Headphone amplifier enable.
    pub const HP_EN: u8 = 0x80;
}

/// `SET_EAPD` bit: external amplifier powered.
pub const EAPD_ON: u8 = 0x02;

/// Which amplifier(s) and channel(s) a SET_AMP touches (§7.3.3.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // the wire format is four independent bits
pub struct AmpSet {
    /// Set the output amp.
    pub output: bool,
    /// Set the input amp (at `index`).
    pub input: bool,
    /// Left channel.
    pub left: bool,
    /// Right channel.
    pub right: bool,
    /// Input-amp index (which connection); 0 for an output amp.
    pub index: u8,
    /// Mute.
    pub mute: bool,
    /// Gain step, 7 bits.
    pub gain: u8,
}

impl AmpSet {
    /// Both channels of the output amp.
    #[must_use]
    pub const fn out(mute: bool, gain: u8) -> Self {
        Self { output: true, input: false, left: true, right: true, index: 0, mute, gain }
    }
    /// Both channels of the input amp for connection `index`.
    #[must_use]
    pub const fn inp(index: u8, mute: bool, gain: u8) -> Self {
        Self { output: false, input: true, left: true, right: true, index, mute, gain }
    }
    const fn payload(self) -> u16 {
        ((self.output as u16) << 15)
            | ((self.input as u16) << 14)
            | ((self.left as u16) << 13)
            | ((self.right as u16) << 12)
            | (((self.index & 0xF) as u16) << 8)
            | ((self.mute as u16) << 7)
            | (self.gain & 0x7F) as u16
    }
}

/// SET_AMPLIFIER_GAIN_MUTE (verb 0x3).
#[must_use]
pub const fn set_amp(nid: u8, a: AmpSet) -> u32 {
    cmd4(nid, 0x3, a.payload())
}

/// GET_AMPLIFIER_GAIN_MUTE (verb 0xB). `output` picks the output amp,
/// `left` the channel; `index` is the input-amp connection.
#[must_use]
pub const fn get_amp(nid: u8, output: bool, left: bool, index: u8) -> u32 {
    cmd4(nid, 0xB, ((output as u16) << 15) | ((left as u16) << 13) | (index & 0xF) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answers_from_the_spec_and_the_metal() {
        // Root vendor id: the one verb the first bring-up got right.
        assert_eq!(get_param(0, param::VENDOR_ID), 0x000F_0000);
        assert_eq!(get_param(0, param::NODE_COUNT), 0x000F_0004);
        // Widget 0x1b, configuration default: 0x01B_F1C_00.
        assert_eq!(get_cfg_default(0x1b), 0x01BF_1C00);
        assert_eq!(get_pin_sense(0x1b), 0x01BF_0900);
        assert_eq!(get_gpio_data(1), 0x001F_1500);
    }

    #[test]
    fn nid_is_in_bits_27_to_20() {
        // The bug this module exists to make unrepresentable.
        for nid in [1u8, 2, 0x0c, 0x14, 0x1b, 0x22] {
            assert_eq!((set_power(nid, 0) >> 20) & 0xFF, u32::from(nid));
            assert_eq!((set_conv_fmt(nid, 0x4011) >> 20) & 0xFF, u32::from(nid));
            assert_eq!((set_amp(nid, AmpSet::out(false, 0)) >> 20) & 0xFF, u32::from(nid));
        }
        // "Set converter 2 to 44.1k/16/stereo": the old literal 0x0220_4011 is
        // verb 0 to widget 0x22.
        assert_eq!(set_conv_fmt(2, 0x4011), 0x0022_4011);
        assert_ne!(set_conv_fmt(2, 0x4011), 0x0220_4011);
        assert_eq!((0x0220_4011u32 >> 20) & 0xFF, 0x22);
    }

    #[test]
    fn stream_binding_words() {
        assert_eq!(set_stream(2, 1, 0), 0x0027_0610);
        assert_eq!(get_stream(2), 0x002F_0600);
    }

    #[test]
    fn pin_words() {
        assert_eq!(set_pin_ctrl(0x1b, pinctl::OUT_EN | pinctl::HP_EN), 0x01B7_07C0);
        assert_eq!(set_eapd(0x14, EAPD_ON), 0x0147_0C02);
        // The SET the old code called "PIN_VREF" is a connection select.
        assert_eq!(set_conn_sel(0x1b, 0xC3), 0x01B7_01C3);
    }

    #[test]
    fn amp_payload_bits() {
        // Output amp, both channels, unmuted, gain 0x38: out|left|right = 0xB000.
        assert_eq!(set_amp(2, AmpSet::out(false, 0x38)), 0x0023_B038);
        // Input amp index 1, both channels, muted: in|left|right|idx|mute.
        assert_eq!(set_amp(0x0c, AmpSet::inp(1, true, 0)), 0x00C3_7180);
        // Gain is 7 bits.
        assert_eq!(set_amp(2, AmpSet::out(false, 0xFF)) & 0xFF, 0x7F);
        assert_eq!(get_amp(2, true, true, 0), 0x002B_A000);
        assert_eq!(get_amp(0x0c, false, false, 3), 0x00CB_0003);
    }

    #[test]
    fn cad_is_the_top_nibble() {
        assert_eq!(with_cad(0x000F_0000, 0), 0x000F_0000);
        assert_eq!(with_cad(0x000F_0000, 2), 0x200F_0000);
        assert_eq!(with_cad(0xF000_0000, 1), 0x1000_0000);
    }
}
