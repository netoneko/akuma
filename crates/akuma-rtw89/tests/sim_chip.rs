//! The firmware download against a simulated chip: what the packets carry,
//! which no register trace can show — the payload goes by DMA.
//!
//! [`Chip`] answers the bring-up's polls the way the real card did in the W0
//! trace (power handshakes complete, the XTAL serial interface finishes, the
//! DLE reports ready, `H2C_PATH_RDY` once the CPU is enabled in download mode,
//! `FWDL_PATH_RDY` once the header packet has been kicked, status 7 once the
//! last data packet has). How fast it consumes packets is a knob, so the slot
//! reuse path is exercised too.

use std::collections::HashMap;

use akuma_rtw89::bringup::{self, Dma, Error, RING_BYTES, SLOT, Stage};
use akuma_rtw89::fw::{self, Source};
use akuma_rtw89::{Bus, h2c, regs};

struct Chip {
    regs: HashMap<u32, u32>,
    /// Kicks the firmware has to see before it reports ready: header + data.
    expect_kicks: u16,
    host_idx: u16,
    hw_idx: u16,
    /// Packets consumed in total (the ring index wraps; this does not).
    consumed: u16,
    /// Packets consumed per read of the index register (0 = all at once).
    consume_per_read: u16,
    /// The status the firmware ends in.
    final_status: u8,
    max_in_flight: u16,
}

impl Chip {
    fn new(expect_kicks: u16) -> Self {
        let mut regs = HashMap::new();
        regs.insert(regs::SYS_CFG1, 0x0c49_1d39); // cut 1, as ryzen's
        regs.insert(regs::SYS_STATUS1, 0x1c01_7258);
        regs.insert(regs::DMAC_FUNC_EN, 0);
        Self {
            regs,
            expect_kicks,
            host_idx: 0,
            hw_idx: 0,
            consumed: 0,
            consume_per_read: 0,
            final_status: bringup::FWDL_INIT_RDY,
            max_in_flight: 0,
        }
    }

    fn get(&self, off: u32) -> u32 {
        *self.regs.get(&(off & !3)).unwrap_or(&0)
    }

    fn set(&mut self, off: u32, v: u32) {
        self.regs.insert(off & !3, v);
    }

    fn fw_ctrl(&self) -> u32 {
        self.get(regs::WCPU_FW_CTRL)
    }

    fn update_fw_ctrl(&mut self) {
        let mut v = self.fw_ctrl();
        let cpu_on = self.get(regs::PLATFORM_ENABLE) & regs::WCPU_EN != 0;
        if cpu_on && v & u32::from(regs::WCPU_FWDL_EN) != 0 {
            v |= u32::from(regs::H2C_PATH_RDY) | (1 << regs::WCPU_FWDL_STS_SHIFT);
            if self.consumed >= 1 {
                v |= u32::from(regs::FWDL_PATH_RDY);
            }
            if self.consumed >= self.expect_kicks {
                v = (v & !regs::WCPU_FWDL_STS_MASK) | (u32::from(self.final_status) << regs::WCPU_FWDL_STS_SHIFT);
            }
        }
        self.set(regs::WCPU_FW_CTRL, v);
    }

    /// The chip runs on its own: every register read is a moment in which it
    /// fetches up to `consume_per_read` pending packets (all of them when 0).
    fn advance(&mut self) {
        let step = if self.consume_per_read == 0 { u16::MAX } else { self.consume_per_read };
        let pending = (self.host_idx + regs::RING_LEN - self.hw_idx) % regs::RING_LEN;
        let n = pending.min(step);
        self.hw_idx = (self.hw_idx + n) % regs::RING_LEN;
        self.consumed += n;
    }

    fn read_reg(&mut self, off: u32) -> u32 {
        self.advance();
        let word = off & !3;
        let v = match word {
            regs::SYS_PW_CTRL => (self.get(word) | regs::RDY_SYSPWR) & !(regs::APFN_ONMAC | regs::APFM_OFFMAC),
            regs::WLAN_XTAL_SI_CTRL => self.get(word) & !regs::XTAL_SI_CMD_POLL,
            regs::WDE_INI_STATUS | regs::PLE_INI_STATUS => regs::DLE_INI_RDY,
            regs::HAXI_INIT_CFG1 => self.get(word) & !regs::RST_BDRAM,
            regs::CH12_TXBD_IDX => (u32::from(self.hw_idx) << 16) | u32::from(self.host_idx),
            regs::WCPU_FW_CTRL => {
                self.update_fw_ctrl();
                self.fw_ctrl()
            }
            _ => self.get(word),
        };
        v >> ((off % 4) * 8)
    }

    fn write_reg(&mut self, off: u32, v: u32, width: u32) {
        let word = off & !3;
        let shift = (off % 4) * 8;
        let mask = if width == 32 { u32::MAX } else { ((1u32 << width) - 1) << shift };
        let old = self.get(word);
        self.set(word, (old & !mask) | ((v << shift) & mask));
        if word == regs::CH12_TXBD_IDX && width == 16 && shift == 0 {
            self.host_idx = v as u16;
            let in_flight = (self.host_idx + regs::RING_LEN - self.hw_idx) % regs::RING_LEN;
            self.max_in_flight = self.max_in_flight.max(in_flight);
        }
    }
}

impl Bus for Chip {
    fn read8(&mut self, off: u32) -> u8 {
        self.read_reg(off) as u8
    }
    fn read16(&mut self, off: u32) -> u16 {
        self.read_reg(off) as u16
    }
    fn read32(&mut self, off: u32) -> u32 {
        self.read_reg(off)
    }
    fn write8(&mut self, off: u32, v: u8) {
        self.write_reg(off, u32::from(v), 8);
    }
    fn write16(&mut self, off: u32, v: u16) {
        self.write_reg(off, u32::from(v), 16);
    }
    fn write32(&mut self, off: u32, v: u32) {
        self.write_reg(off, v, 32);
    }
    fn delay_us(&mut self, _: u32) {}
}

/// A container with one cut-1 normal image of three sections, every byte a
/// function of its offset so a misplaced packet shows.
fn firmware(sections: &[u32]) -> Vec<u8> {
    let hdr = 32 + 16 * sections.len();
    let image = hdr + sections.iter().sum::<u32>() as usize;
    let shift = 32;
    let mut f: Vec<u8> = (0..shift + image).map(|i| (i * 7 + i / 251) as u8).collect();
    f[..32].fill(0);
    f[0] = 0xff;
    f[1] = 1;
    f[16] = 1; // cv
    f[17] = fw::TYPE_NORMAL;
    f[18] = 0;
    f[20..24].copy_from_slice(&(shift as u32).to_le_bytes());
    f[24..28].copy_from_slice(&(image as u32).to_le_bytes());
    let h = &mut f[shift..shift + hdr];
    h.fill(0);
    h[4..8].copy_from_slice(&[0, 27, 122, 0]);
    h[25] = sections.len() as u8;
    for (j, &len) in sections.iter().enumerate() {
        let s = 32 + 16 * j;
        h[s + 4..s + 8].copy_from_slice(&(len | (2 << 24)).to_le_bytes());
    }
    f
}

struct Buffers {
    ring: Vec<u8>,
    slots: Vec<u8>,
}

const RING_PHYS: u64 = 0x10_0000;
const SLOTS_PHYS: u64 = 0x20_0000;

impl Buffers {
    fn new(slots: usize) -> Self {
        Self { ring: vec![0; RING_BYTES], slots: vec![0; SLOT * slots] }
    }

    fn dma(&mut self) -> Dma<'_> {
        Dma {
            ring: &mut self.ring,
            ring_phys: RING_PHYS,
            slots: &mut self.slots,
            slots_phys: SLOTS_PHYS,
            idle_phys: 0x30_0000,
            rx_phys: [0x40_0000, 0x50_0000],
            tx_phys: [0; 3],
        }
    }

    /// The packet ring entry `i` points at: (descriptor dword 0, payload).
    fn packet(&self, i: usize) -> (u32, &[u8]) {
        let (addr, len) = h2c::parse_bd(&self.ring[i * h2c::BD_LEN..(i + 1) * h2c::BD_LEN]);
        let at = (addr - SLOTS_PHYS) as usize;
        let mem = &self.slots[at..at + len];
        let d0 = u32::from_le_bytes([mem[0], mem[1], mem[2], mem[3]]);
        (d0, &mem[h2c::DESC_LEN..])
    }
}

fn no_log(_: &'static str, _: u32) {}

#[test]
fn packets_carry_the_header_then_every_section_byte_in_order() {
    let sections = [fw::PKT_LEN * 3 + 17, 15_504, 960];
    let file = firmware(&sections);
    let mut src: &[u8] = &file;
    let container = fw::Container::read(&mut src).unwrap();
    let img = fw::Image::read(&mut src, container.select(fw::TYPE_NORMAL, 1, file.len() as u32).unwrap()).unwrap();
    let packets = img.packets().count();
    // Enough slots that none is reused: every packet is still in memory after.
    let mut b = Buffers::new(packets + 1);
    let mut chip = Chip::new(packets as u16 + 1);
    let report = bringup::bring_up(&mut chip, &mut b.dma(), &mut bringup::Ch12::new(), &mut src, &mut no_log).unwrap();
    assert_eq!(report.data_packets as usize, packets);
    assert_eq!(bringup::fwdl_status(report.fw_ctrl), 7);

    // Packet 0: an H2C FWHDR_DL whose body is the base header, PART_SIZE = 2020.
    let (d0, payload) = b.packet(0);
    let body = img.header_payload();
    assert_eq!(d0, (h2c::HDR_LEN + body.len()) as u32 | (h2c::TYPE_H2C << 24));
    assert_eq!(&payload[..h2c::HDR_LEN], &h2c::h2c_header(1, 3, 0, 0, body.len()));
    assert_eq!(&payload[h2c::HDR_LEN..], body);
    assert_eq!(u16::from_le_bytes([body[28], body[29]]), fw::PKT_LEN as u16);

    // Packets 1..: FWDL type, and together exactly the sections' bytes.
    let mut data = Vec::new();
    for i in 1..=packets {
        let (d0, payload) = b.packet(i);
        assert_eq!(d0 >> 24, h2c::TYPE_FWDL, "packet {i}");
        assert_eq!((d0 & 0x3fff) as usize, payload.len(), "packet {i}");
        assert!(payload.len() <= fw::PKT_LEN as usize);
        data.extend_from_slice(payload);
    }
    let first = img.sections()[0].file_off as usize;
    assert_eq!(data, &file[first..first + sections.iter().sum::<u32>() as usize]);
}

#[test]
fn a_slow_chip_makes_the_driver_wait_for_a_free_slot() {
    let file = firmware(&[fw::PKT_LEN * 40, 100]);
    let mut src: &[u8] = &file;
    let mut b = Buffers::new(4);
    let mut chip = Chip::new(1 + 41);
    chip.consume_per_read = 1;
    let report = bringup::bring_up(&mut chip, &mut b.dma(), &mut bringup::Ch12::new(), &mut src, &mut no_log).unwrap();
    assert_eq!(report.data_packets, 41);
    assert!(chip.max_in_flight <= 4, "{} packets were in flight on 4 slots", chip.max_in_flight);
}

#[test]
fn a_chip_that_never_consumes_times_out_naming_the_ring() {
    let file = firmware(&[fw::PKT_LEN * 8]);
    let mut src: &[u8] = &file;
    let mut b = Buffers::new(2);
    let mut chip = Chip::new(9);
    chip.consume_per_read = 0;
    // Consume the header only, then stall.
    struct Stall(Chip);
    impl Bus for Stall {
        fn read8(&mut self, o: u32) -> u8 {
            self.0.read8(o)
        }
        fn read16(&mut self, o: u32) -> u16 {
            self.0.read16(o)
        }
        fn read32(&mut self, o: u32) -> u32 {
            if o == regs::CH12_TXBD_IDX {
                let hw = self.0.host_idx.min(1);
                return (u32::from(hw) << 16) | u32::from(self.0.host_idx);
            }
            self.0.read32(o)
        }
        fn write8(&mut self, o: u32, v: u8) {
            self.0.write8(o, v);
        }
        fn write16(&mut self, o: u32, v: u16) {
            self.0.write16(o, v);
        }
        fn write32(&mut self, o: u32, v: u32) {
            self.0.write32(o, v);
        }
        fn delay_us(&mut self, _: u32) {}
    }
    chip.consume_per_read = 1;
    let mut stall = Stall(chip);
    let err = bringup::bring_up(&mut stall, &mut b.dma(), &mut bringup::Ch12::new(), &mut src, &mut no_log).unwrap_err();
    assert!(matches!(err, Error::Poll { stage: Stage::Data, reg: regs::CH12_TXBD_IDX, .. }), "{err:?}");
}

#[test]
fn an_index_the_chip_was_never_given_reads_as_a_full_ring() {
    // A chip claiming to have fetched entry 5 before anything was queued.
    struct Ahead(Chip);
    impl Bus for Ahead {
        fn read8(&mut self, o: u32) -> u8 {
            self.0.read8(o)
        }
        fn read16(&mut self, o: u32) -> u16 {
            self.0.read16(o)
        }
        fn read32(&mut self, o: u32) -> u32 {
            if o == regs::CH12_TXBD_IDX {
                return 5 << 16;
            }
            self.0.read32(o)
        }
        fn write8(&mut self, o: u32, v: u8) {
            self.0.write8(o, v);
        }
        fn write16(&mut self, o: u32, v: u16) {
            self.0.write16(o, v);
        }
        fn write32(&mut self, o: u32, v: u32) {
            self.0.write32(o, v);
        }
        fn delay_us(&mut self, _: u32) {}
    }
    let file = firmware(&[100]);
    let mut src: &[u8] = &file;
    let mut b = Buffers::new(4);
    let err = bringup::bring_up(&mut Ahead(Chip::new(2)), &mut b.dma(), &mut bringup::Ch12::new(), &mut src, &mut no_log).unwrap_err();
    assert_eq!(err, Error::Poll { stage: Stage::HeaderSent, reg: regs::CH12_TXBD_IDX, last: 5 << 16 });
}

#[test]
fn a_rejected_image_reports_linux_reason() {
    let file = firmware(&[100]);
    let mut src: &[u8] = &file;
    let mut b = Buffers::new(4);
    let mut chip = Chip::new(2);
    chip.final_status = 2;
    let err = bringup::bring_up(&mut chip, &mut b.dma(), &mut bringup::Ch12::new(), &mut src, &mut no_log).unwrap_err();
    assert_eq!(err, Error::Rejected(2));
    assert_eq!(bringup::rejected_reason(2), "fw checksum fail");
}

#[test]
fn a_short_read_mid_download_is_reported_with_the_packet() {
    struct Truncated<'a>(&'a [u8], u32);
    impl Source for Truncated<'_> {
        fn len(&self) -> u32 {
            self.0.len() as u32
        }
        fn read_at(&mut self, off: u32, buf: &mut [u8]) -> bool {
            off + (buf.len() as u32) <= self.1 && { self.0 }.read_at(off, buf)
        }
    }
    let file = firmware(&[fw::PKT_LEN * 3]);
    let cut = 32 + 48 + fw::PKT_LEN + 5; // packet 1 is short
    let mut src = Truncated(&file, cut);
    let mut b = Buffers::new(4);
    let mut chip = Chip::new(4);
    assert_eq!(bringup::bring_up(&mut chip, &mut b.dma(), &mut bringup::Ch12::new(), &mut src, &mut no_log).unwrap_err(), Error::Read {
        packet: 1
    });
}

#[test]
fn shutdown_stops_the_cpu_and_dma_and_powers_off() {
    let mut chip = Chip::new(0);
    chip.set(regs::PLATFORM_ENABLE, regs::PLATFORM_EN | regs::WCPU_EN);
    chip.set(regs::HAXI_INIT_CFG1, regs::TXHCI_EN_V1 | regs::RXHCI_EN_V1);
    bringup::shutdown(&mut chip).unwrap();
    assert_eq!(chip.get(regs::PLATFORM_ENABLE) & regs::WCPU_EN, 0);
    let cfg = chip.get(regs::HAXI_INIT_CFG1);
    assert_eq!(cfg & (regs::TXHCI_EN_V1 | regs::RXHCI_EN_V1), 0);
    assert_ne!(cfg & regs::STOP_AXI_MST, 0);
    assert_ne!(chip.get(regs::SYS_PW_CTRL) & regs::APFM_SWLPS, 0);
    assert_eq!(chip.get(regs::SCOREBOARD) >> 24, u32::from(regs::NOTIFY_PWR_MAJOR));
}
