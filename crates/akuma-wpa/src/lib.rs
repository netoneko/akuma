//! WPA2-PSK for Akuma's in-kernel wifi station (wifi stage W4,
//! `docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md` § 5).
//!
//! Akuma has no supplicant: the kernel runs the EAPOL handshake itself
//! (`proposals/AKUMA_WIFI_CONTROL.md`). This crate is everything about that
//! handshake that is not moving frames: the crypto it needs, and the
//! supplicant state machine. The card does CCMP on data frames in hardware,
//! so software AES is only for unwrapping group keys.
//!
//! | module | |
//! |---|---|
//! | [`sha1`] | SHA-1, HMAC-SHA1, the 802.11 PRF, PBKDF2 (tests and tools only) |
//! | [`aes`] | AES-128 and RFC 3394 key wrap/unwrap |
//! | [`eapol`] | EAPOL-Key frames, PTK derivation, the 4-way and group-key handshakes |
//!
//! The PSK arrives already derived (`/etc/wifi/<network>` holds it; the
//! kernel never runs PBKDF2). Every primitive is checked against a published
//! vector (RFC 3174/2202/3394, FIPS-197, IEEE 802.11 annex J); the handshake
//! is checked against an access point built from those same primitives.
//!
//! `no_std`, no allocation, no dependencies, `forbid(unsafe_code)`.

#![no_std]
#![forbid(unsafe_code)]

pub mod aes;
pub mod eapol;
pub mod sha1;

/// A MAC address.
pub type Addr = [u8; 6];
