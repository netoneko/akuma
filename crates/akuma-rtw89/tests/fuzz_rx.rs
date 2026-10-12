//! Fuzz the RX buffer parser with what the chip could write: random bytes,
//! and valid buffers with their length fields, descriptor flags and segment
//! bits rewritten. The parser runs in the kernel on every received buffer,
//! so its only acceptable outcomes are `Ok` with a body inside the buffer,
//! or `Err`. Deterministic; no dependencies.

use akuma_rtw89::rx;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

fn valid(body_len: usize, long: bool, drv_info: u8) -> Vec<u8> {
    let desc_len = if long { 32 } else { 16 };
    let start = 4 + desc_len + usize::from(drv_info) * 8;
    let mut b = vec![0u8; start + body_len];
    let written = (start + body_len) as u32;
    b[..4].copy_from_slice(&(written | (1 << 15) | (1 << 14)).to_le_bytes());
    let d0 = body_len as u32 | (u32::from(drv_info) << 28) | if long { 1 << 31 } else { 0 };
    b[4..8].copy_from_slice(&d0.to_le_bytes());
    for (i, x) in b[start..].iter_mut().enumerate() {
        *x = i as u8;
    }
    b
}

#[test]
fn rx_parse_survives_mutated_and_random_buffers() {
    let corpus = [valid(60, false, 0), valid(1500, false, 1), valid(12, true, 0), valid(0, true, 3), valid(300, false, 7)];
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..200_000 {
        let mut b = corpus[rng.below(corpus.len())].clone();
        match rng.below(6) {
            0 => {
                let i = rng.below(b.len());
                b[i] = rng.byte();
            }
            1 => {
                // The info word: written length, segment bits.
                let v = rng.next() as u32;
                b[..4].copy_from_slice(&v.to_le_bytes());
            }
            2 => {
                // Descriptor word 0: pkt_size, type, drv_info_size, long.
                let v = rng.next() as u32;
                b[4..8].copy_from_slice(&v.to_le_bytes());
            }
            3 => b.truncate(rng.below(b.len() + 1)),
            4 => b = (0..rng.below(64)).map(|_| rng.byte()).collect(),
            _ => {
                let i = rng.below(b.len());
                b[i] ^= 1 << rng.below(8);
            }
        }
        if let Ok(p) = rx::parse(&b) {
            assert!(p.body.len() <= b.len());
            assert_eq!(p.body.len(), usize::from(p.desc.pkt_size));
        }
    }
}

#[test]
fn ring_arithmetic_survives_any_index_register_value() {
    struct Idx(u32);
    impl akuma_rtw89::Bus for Idx {
        fn read8(&mut self, _: u32) -> u8 {
            0
        }
        fn read16(&mut self, _: u32) -> u16 {
            0
        }
        fn read32(&mut self, _: u32) -> u32 {
            self.0
        }
        fn write8(&mut self, _: u32, _: u8) {}
        fn write16(&mut self, _: u32, _: u16) {}
        fn write32(&mut self, _: u32, _: u32) {}
        fn delay_us(&mut self, _: u32) {}
    }
    let mut rng = Rng(99);
    for len in [1u16, 2, 16, 256, 4095] {
        let mut r = rx::Ring::new(rx::RXQ_IDX, len);
        for _ in 0..10_000 {
            let mut bus = Idx(rng.next() as u32);
            let n = r.ready(&mut bus);
            assert!(n < len, "ready entries are fewer than the ring");
            r.advance((rng.next() as u16) % len);
            assert!(r.wp < len);
        }
    }
}
