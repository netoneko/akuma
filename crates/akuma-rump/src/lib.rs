#![cfg_attr(not(test), no_std)]
//! Device-independent orchestration for the kernel `rump` raw L2 tap path.
//!
//! The hardware-bound bits — `VirtIONetRaw` + `NetHal` (real DMA/MMU), and the
//! volatile MMIO probing — live in `akuma-net`, because they can't run under a
//! host `cargo test`. This crate holds the logic that *can*, behind the
//! [`RawNic`] trait, so a mock NIC can exercise it on the host:
//!
//! - [`select_second_net_addr`] — the NIC-selection ordering (skip the first
//!   virtio-net, which smoltcp owns; claim the second).
//! - [`TapNic`] — the RX two-phase state machine (post a buffer once, poll,
//!   complete) and its **malformed-length bounds guard**, plus frame TX.
//!
//! `akuma-net::rump_tap` implements `RawNic` over `VirtIONetRaw` and owns the
//! global instance; the kernel syscall layer talks to that. Nothing about
//! `NetHal`/`VirtIONetRaw` needs to be exported — the trait is the seam.

extern crate alloc;

/// Opaque error from a raw NIC backend. The orchestration only branches on
/// success vs. failure, so the cause is intentionally not modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NicError;

/// Minimal raw L2 NIC backend the tap orchestration drives.
///
/// Buffer/token semantics mirror virtio-drivers' `VirtIONetRaw`: the driver
/// posts a receive buffer (`receive_begin` → token), polls for completion, then
/// `receive_complete`s with the same token to learn the header/packet split.
/// The real impl (akuma-net) wraps the `unsafe` virtio calls; tests use a mock.
pub trait RawNic {
    /// Post `buf` to the device to receive into; returns a token identifying it.
    fn receive_begin(&mut self, buf: &mut [u8]) -> Result<u16, NicError>;
    /// Whether the device has filled a previously posted buffer.
    fn poll_receive(&mut self) -> bool;
    /// Complete a receive started with `token`. Returns `(header_len, packet_len)`
    /// — the frame occupies `buf[header_len .. header_len + packet_len]`.
    fn receive_complete(&mut self, token: u16, buf: &mut [u8]) -> Result<(usize, usize), NicError>;
    /// Transmit a bare L2 (Ethernet) frame; the backend prepends any device header.
    fn send(&mut self, frame: &[u8]) -> Result<(), NicError>;
}

/// Staging buffer size: Ethernet MTU (1500) + headers + virtio-net header slack,
/// rounded up. Matches `akuma-net`'s buffers.
pub const FRAME_BUF: usize = 2048;

/// virtio device id for a network device (per the virtio spec).
pub const VIRTIO_DEVICE_ID_NET: u32 = 1;

/// Device-independent tap state over an arbitrary [`RawNic`]: one staging buffer
/// and an optional posted-RX token.
pub struct TapNic<N: RawNic> {
    nic: N,
    rx_buffer: [u8; FRAME_BUF],
    rx_token: Option<u16>,
}

impl<N: RawNic> TapNic<N> {
    /// Wrap a raw NIC backend.
    pub fn new(nic: N) -> Self {
        Self { nic, rx_buffer: [0u8; FRAME_BUF], rx_token: None }
    }

    /// Pull one received L2 frame into `out`.
    ///
    /// Returns `Some(n)` if a frame was available — copied into `out`, truncated
    /// to `out.len()` — or `None` if no frame is ready (the caller maps `None` to
    /// `EAGAIN`). Guards against a malformed device response reporting a length
    /// past the staging buffer (which would otherwise be an out-of-bounds slice).
    pub fn read_frame(&mut self, out: &mut [u8]) -> Option<usize> {
        // Phase 1: ensure a receive buffer is posted.
        if self.rx_token.is_none() {
            match self.nic.receive_begin(&mut self.rx_buffer) {
                Ok(token) => self.rx_token = Some(token),
                Err(_) => return None,
            }
        }

        // Phase 2: has the device filled it?
        if !self.nic.poll_receive() {
            return None;
        }
        let token = self.rx_token.take()?;
        match self.nic.receive_complete(token, &mut self.rx_buffer) {
            Ok((hdr_len, pkt_len)) => {
                if hdr_len.saturating_add(pkt_len) > self.rx_buffer.len() {
                    return None;
                }
                let n = pkt_len.min(out.len());
                out[..n].copy_from_slice(&self.rx_buffer[hdr_len..hdr_len + n]);
                Some(n)
            }
            Err(_) => None,
        }
    }

    /// Transmit one bare L2 frame. Returns the bytes accepted (`frame.len()`).
    pub fn write_frame(&mut self, frame: &[u8]) -> Result<usize, NicError> {
        self.nic.send(frame)?;
        Ok(frame.len())
    }

    /// Borrow the backend (e.g. to read its MAC at init).
    pub fn nic(&self) -> &N {
        &self.nic
    }
}

/// Pick the MMIO address of the **second** virtio-net device from a scan list of
/// `(address, device_id)` pairs.
///
/// The first virtio-net is owned by the native smoltcp stack; the rump tap path
/// claims the second (the plan's §4 option A — dedicated second NIC). Non-net
/// devices in the list are ignored. Returns `None` if fewer than two virtio-net
/// devices are present.
#[must_use]
pub fn select_second_net_addr(slots: &[(usize, u32)]) -> Option<usize> {
    slots
        .iter()
        .filter(|(_, id)| *id == VIRTIO_DEVICE_ID_NET)
        .nth(1)
        .map(|(addr, _)| *addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::vec;
    use alloc::vec::Vec;

    // ── NIC selection ────────────────────────────────────────────────────────

    #[test]
    fn select_skips_first_net_takes_second() {
        // slots: net0 @0x100, blk @0x200, rng @0x300, net1 @0x500
        let slots = [(0x100, 1u32), (0x200, 2), (0x300, 4), (0x500, 1)];
        assert_eq!(select_second_net_addr(&slots), Some(0x500));
    }

    #[test]
    fn select_none_when_only_one_net() {
        let slots = [(0x100, 1u32), (0x200, 2), (0x300, 4)];
        assert_eq!(select_second_net_addr(&slots), None);
    }

    #[test]
    fn select_none_when_no_net() {
        let slots = [(0x200, 2u32), (0x300, 4)];
        assert_eq!(select_second_net_addr(&slots), None);
    }

    #[test]
    fn select_ignores_third_and_later_nets() {
        let slots = [(0x100, 1u32), (0x500, 1), (0x900, 1)];
        // Always the second net, never the third.
        assert_eq!(select_second_net_addr(&slots), Some(0x500));
    }

    #[test]
    fn select_empty_list() {
        assert_eq!(select_second_net_addr(&[]), None);
    }

    // ── Mock NIC for the TapNic state machine ──────────────────────────────────

    /// A scripted RawNic. `rx_script` is a queue of receive outcomes; each
    /// `read_frame` that reaches the complete phase consumes one.
    struct MockNic {
        begin_should_fail: bool,
        // Each entry: (poll_ready, complete_result_as_(hdr,pkt)_with_fill_byte)
        // None complete → receive_complete returns Err.
        rx_script: Vec<RxStep>,
        rx_idx: usize,
        sent: Vec<Vec<u8>>,
        send_should_fail: bool,
        begin_calls: usize,
    }

    #[derive(Clone)]
    struct RxStep {
        poll_ready: bool,
        // Some((hdr_len, pkt_len, fill)) → receive_complete fills buf[hdr..hdr+pkt]
        // with `fill` and returns Ok; None → Err.
        complete: Option<(usize, usize, u8)>,
    }

    impl MockNic {
        fn new() -> Self {
            Self {
                begin_should_fail: false,
                rx_script: Vec::new(),
                rx_idx: 0,
                sent: Vec::new(),
                send_should_fail: false,
                begin_calls: 0,
            }
        }
    }

    impl RawNic for MockNic {
        fn receive_begin(&mut self, _buf: &mut [u8]) -> Result<u16, NicError> {
            self.begin_calls += 1;
            if self.begin_should_fail {
                Err(NicError)
            } else {
                Ok(7) // arbitrary token
            }
        }
        fn poll_receive(&mut self) -> bool {
            self.rx_script.get(self.rx_idx).map(|s| s.poll_ready).unwrap_or(false)
        }
        fn receive_complete(&mut self, _token: u16, buf: &mut [u8]) -> Result<(usize, usize), NicError> {
            let step = self.rx_script[self.rx_idx].clone();
            self.rx_idx += 1;
            match step.complete {
                Some((hdr, pkt, fill)) => {
                    // Only fill within real buffer bounds (a malformed pkt_len may
                    // exceed the buffer — the guard under test must reject it
                    // before any copy, so we must not write OOB here either).
                    let buf_len = buf.len();
                    let start = hdr.min(buf_len);
                    let end = hdr.saturating_add(pkt).min(buf_len);
                    for b in &mut buf[start..end] {
                        *b = fill;
                    }
                    Ok((hdr, pkt))
                }
                None => Err(NicError),
            }
        }
        fn send(&mut self, frame: &[u8]) -> Result<(), NicError> {
            if self.send_should_fail {
                return Err(NicError);
            }
            self.sent.push(frame.to_vec());
            Ok(())
        }
    }

    #[test]
    fn read_frame_none_when_no_packet_ready() {
        let mut nic = MockNic::new();
        nic.rx_script = vec![RxStep { poll_ready: false, complete: None }];
        let mut tap = TapNic::new(nic);
        let mut out = [0u8; 64];
        assert_eq!(tap.read_frame(&mut out), None);
        // Buffer was posted exactly once even though no frame arrived.
        assert_eq!(tap.nic().begin_calls, 1);
    }

    #[test]
    fn read_frame_posts_buffer_only_once_across_empty_polls() {
        let mut nic = MockNic::new();
        nic.rx_script = vec![
            RxStep { poll_ready: false, complete: None },
            RxStep { poll_ready: false, complete: None },
        ];
        let mut tap = TapNic::new(nic);
        let mut out = [0u8; 64];
        assert_eq!(tap.read_frame(&mut out), None);
        assert_eq!(tap.read_frame(&mut out), None);
        // The token persists across empty polls — no re-post.
        assert_eq!(tap.nic().begin_calls, 1);
    }

    #[test]
    fn read_frame_returns_packet_past_header() {
        let mut nic = MockNic::new();
        // header 10 bytes, packet 20 bytes filled with 0xAB.
        nic.rx_script = vec![RxStep { poll_ready: true, complete: Some((10, 20, 0xAB)) }];
        let mut tap = TapNic::new(nic);
        let mut out = [0u8; 64];
        assert_eq!(tap.read_frame(&mut out), Some(20));
        assert!(out[..20].iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn read_frame_truncates_to_out_len() {
        let mut nic = MockNic::new();
        nic.rx_script = vec![RxStep { poll_ready: true, complete: Some((10, 100, 0xCD)) }];
        let mut tap = TapNic::new(nic);
        let mut out = [0u8; 32];
        // pkt_len 100 > out 32 → truncated to 32.
        assert_eq!(tap.read_frame(&mut out), Some(32));
        assert!(out.iter().all(|&b| b == 0xCD));
    }

    #[test]
    fn read_frame_rejects_malformed_length_past_buffer() {
        let mut nic = MockNic::new();
        // hdr + pkt > FRAME_BUF (2048) → must be rejected, no OOB, returns None.
        nic.rx_script = vec![RxStep { poll_ready: true, complete: Some((10, FRAME_BUF, 0xEE)) }];
        let mut tap = TapNic::new(nic);
        let mut out = [0u8; 64];
        assert_eq!(tap.read_frame(&mut out), None);
    }

    #[test]
    fn read_frame_none_when_begin_fails() {
        let mut nic = MockNic::new();
        nic.begin_should_fail = true;
        let mut tap = TapNic::new(nic);
        let mut out = [0u8; 64];
        assert_eq!(tap.read_frame(&mut out), None);
    }

    #[test]
    fn read_frame_none_when_complete_errors() {
        let mut nic = MockNic::new();
        nic.rx_script = vec![RxStep { poll_ready: true, complete: None }];
        let mut tap = TapNic::new(nic);
        let mut out = [0u8; 64];
        assert_eq!(tap.read_frame(&mut out), None);
    }

    #[test]
    fn write_frame_sends_and_returns_len() {
        let mut tap = TapNic::new(MockNic::new());
        let frame = [0xffu8; 60];
        assert_eq!(tap.write_frame(&frame), Ok(60));
        assert_eq!(tap.nic().sent.len(), 1);
        assert_eq!(tap.nic().sent[0], frame.to_vec());
    }

    #[test]
    fn write_frame_propagates_error() {
        let mut nic = MockNic::new();
        nic.send_should_fail = true;
        let mut tap = TapNic::new(nic);
        assert_eq!(tap.write_frame(&[1, 2, 3]), Err(NicError));
    }
}
