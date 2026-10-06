//! 802.11 frames for Akuma's in-kernel wifi station.
//!
//! The rtw89 driver (`amd64/src/rtw89.rs`) moves frames; this crate decides
//! what they say. It is the station half of what Linux splits between
//! mac80211 and wpa_supplicant, cut down to one case: a station joining one
//! access point, open or WPA2-PSK (CCMP), on a 2.4 or 5 GHz channel.
//!
//! | module | |
//! |---|---|
//! | [`frame`] | frame control, the management header, information elements |
//! | [`beacon`] | what a beacon or probe response advertises: SSID, channel, RSN |
//! | [`sta`] | authentication, association, data frames (LLC/SNAP, CCMP header) |
//!
//! `no_std`, no allocation, no dependencies, `forbid(unsafe_code)`.

#![no_std]
#![forbid(unsafe_code)]

pub mod beacon;
pub mod frame;
pub mod sta;

/// A MAC address.
pub type Addr = [u8; 6];

/// The broadcast address.
pub const BROADCAST: Addr = [0xff; 6];
