//! Intel HD Audio playback — the metal half.
//!
//! `akuma-hda` (host-tested, `forbid(unsafe_code)`) decides *what* to say: the
//! verb words, the codec's widget graph and the route from an output pin to a
//! DAC, the stream format word, PCM conversion and the ring's arithmetic. This
//! module owns what cannot be host-tested: the BAR0 mapping, the CORB/RIRB and
//! stream DMA memory, and the registers.
//!
//! ## DMA contract
//!
//! The CORB, RIRB, buffer descriptor list and PCM ring are `static`s in `.bss`
//! (the device never sees a caller's buffer). A buffer is the device's from the
//! moment its address is published — CORBWP for a verb, `RUN` for the ring —
//! until the matching completion: RIRBWP moving, or `LPIB` having passed a
//! ring byte. x86 DMA is cache-coherent, so ordering is a `fence`, not a flush.
//!
//! ## What the first bring-up got wrong
//!
//! It hand-assembled verbs as hex literals with the node id one nibble too high
//! (so it configured a widget 0x22 that does not exist), wrote the stream
//! format at `SD+0x14` (reserved; `SDnFMT` is `+0x12`) and the last-valid-index
//! at `SD+0x10` (`SDnFIFOS`, read-only; `SDnLVI` is `+0x0C`), converted 24-bit
//! audio that `wavplay` had already converted to 16, and accepted any
//! rate/format/channel ioctl without remembering it. Every readback it used to
//! declare success read a phantom. See `docs/runbooks/add-intel-hda-audio.md`.

use crate::serial;
use akuma_hda::codec::{self, Kind, PinRole, VerbBus, VerbList};
use akuma_hda::stream::{self, PlayRing, SampleFormat};
use akuma_hda::{reg, verb, Regs16, RegsW16};
use core::sync::atomic::{fence, Ordering};
use spinning_top::Spinlock;

/// Intel's vendor id. The NVIDIA HDMI audio function is class 04:03 too, so
/// the walk matches class **and** vendor or it brings up the wrong chip.
const INTEL: u16 = 0x8086;

/// Stream tag used for playback (1..=15; 0 means "unbound").
const TAG: u8 = 1;
/// The DAC amplifier's level as a percentage of its 0 dB step. 64 is the level
/// Linux left this machine's ALC662 at when it was audible.
const DAC_PCT: u8 = 64;

/// Ring geometry: 8 fragments of 8 KiB (64 KiB, ~371 ms of 44.1 kHz stereo).
const NFRAG: usize = 8;
const FRAG: usize = 8192;
const RING_BYTES: usize = NFRAG * FRAG;
/// Bytes kept between the write point and the hardware's read point.
const GUARD: u32 = 1024;

// ---------------------------------------------------------------------------
// DMA memory
// ---------------------------------------------------------------------------

#[repr(C, align(128))]
struct Corb([u32; 256]);
#[repr(C, align(128))]
struct Rirb([u64; 256]);
#[repr(C, align(128))]
struct Bdl([[u32; 4]; NFRAG]);
#[repr(C, align(4096))]
struct Ring([u8; RING_BYTES]);

/// The DMA position buffer: one 8-byte slot per stream descriptor, the first
/// word being the stream's position. Written by the controller, so it is
/// always read after a cache-line flush.
#[repr(C, align(128))]
struct PosBuf([u32; 32]);

static mut POSBUF: PosBuf = PosBuf([0; 32]);
static mut CORB: Corb = Corb([0; 256]);
static mut RIRB: Rirb = Rirb([0; 256]);
static mut BDL: Bdl = Bdl([[0; 4]; NFRAG]);
static mut RING: Ring = Ring([0; RING_BYTES]);

fn phys<T>(p: *mut T) -> u64 {
    akuma_primitives::addr::virt_to_phys(p as usize) as u64
}

/// Write back and evict the cache lines covering `[p, p+len)`, then fence.
///
/// Linux clears the controller's no-snoop enable (see `init`), after which
/// device DMA is coherent with the CPU caches. If the platform ignores that,
/// a ring the CPU just filled is still in cache and the DAC reads stale RAM:
/// a stream that "runs" perfectly and plays silence. Flushing costs a line per
/// 64 bytes and makes the ordering independent of the snoop bit.
fn flush(p: *const u8, len: usize) {
    let mut a = (p as usize) & !63;
    let end = p as usize + len;
    while a < end {
        // SAFETY: `clflush` on an address inside one of this file's statics.
        unsafe { core::arch::x86_64::_mm_clflush(a as *const u8) };
        a += 64;
    }
    // SAFETY: a fence has no memory-safety requirements.
    unsafe { core::arch::x86_64::_mm_mfence() };
}

// ---------------------------------------------------------------------------
// MMIO
// ---------------------------------------------------------------------------

/// BAR0. Every access is naturally aligned and exactly the register's width;
/// the controller answers `0xff` to a halfword read that straddles a dword.
struct Mmio {
    base: *mut u8,
}

// SAFETY: `base` is a device mapping with no thread affinity; all access goes
// through the `HDA` lock or happens single-threaded at boot.
unsafe impl Send for Mmio {}

impl Mmio {
    fn r8(&self, off: usize) -> u8 {
        // SAFETY: `off` is inside the 16 KiB BAR0 mapping `init` created.
        unsafe { self.base.add(off).read_volatile() }
    }
    fn w8(&self, off: usize, v: u8) {
        // SAFETY: as `r8`.
        unsafe { self.base.add(off).write_volatile(v) }
    }
    fn r32(&self, off: usize) -> u32 {
        // SAFETY: as `r8`; `off` is 4-aligned at every call site.
        unsafe { (self.base.add(off) as *const u32).read_volatile() }
    }
}

impl Regs16 for Mmio {
    fn r16(&self, off: usize) -> u16 {
        // SAFETY: as `r8`; `off` is 2-aligned at every call site.
        unsafe { (self.base.add(off) as *const u16).read_volatile() }
    }
}

impl RegsW16 for Mmio {
    fn w16(&self, off: usize, v: u16) {
        // SAFETY: as `r16`.
        unsafe { (self.base.add(off) as *mut u16).write_volatile(v) }
    }
    fn w32(&self, off: usize, v: u32) {
        // SAFETY: as `r32`.
        unsafe { (self.base.add(off) as *mut u32).write_volatile(v) }
    }
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

fn now_us() -> Option<u64> {
    crate::lapic::tsc_uptime_us()
}

/// A bounded wait. Uses the TSC when it is calibrated and a spin count when it
/// is not, so no loop in this file can outlive its budget on either path.
struct Wait {
    t0: Option<u64>,
    spins: u64,
    us: u64,
}

impl Wait {
    fn new(us: u64) -> Self {
        Self { t0: now_us(), spins: 0, us }
    }
    /// `true` once the budget is spent.
    fn expired(&mut self) -> bool {
        self.spins += 1;
        match self.t0 {
            Some(t0) => now_us().is_some_and(|n| n.saturating_sub(t0) > self.us),
            None => self.spins > self.us * 200,
        }
    }
}

fn delay_us(us: u64) {
    let mut w = Wait::new(us);
    while !w.expired() {
        core::hint::spin_loop();
    }
}

// ---------------------------------------------------------------------------
// Console helpers (no allocation: `serial` writes straight to the port)
// ---------------------------------------------------------------------------

fn p(s: &str) {
    serial::puts(s);
}
fn hx(v: u64, nibbles: u32) {
    serial::put_hexn(v, nibbles);
}
fn dec(v: u64) {
    serial::put_dec(v);
}

// ---------------------------------------------------------------------------
// The controller
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PosSrc {
    /// `SDnLPIB`, the link position register.
    Lpib,
    /// The DMA position buffer in memory.
    PosBuf,
    /// Nothing reports position: derive it from the clock.
    Clock,
}

struct Hda {
    regs: Mmio,
    /// Codec address (lowest set bit of STATESTS).
    cad: u8,
    /// CORB/RIRB unusable; verbs go through the immediate command registers.
    use_ici: bool,
    /// RIRB entries consumed so far (the index we last saw RIRBWP at).
    rirb_rp: u8,
    /// Offset of output stream descriptor 0.
    sd: usize,
    /// DACs that carry the stream (unique).
    dacs: [u8; 4],
    ndacs: usize,
    // Playback parameters (what `/dev/dsp` last asked for).
    rate: u32,
    channels: u8,
    fmt: SampleFormat,
    // Playback state.
    configured: bool,
    running: bool,
    /// Where playback position comes from. Starts at `Lpib`; a source that has
    /// not moved 60 ms after `RUN` is replaced by the next one.
    pos_src: PosSrc,
    /// Index of output stream 0 among all stream descriptors (its slot in the
    /// DMA position buffer).
    sd_index: usize,
    /// The first DAC's supported PCM sizes/rates (parameter 0xA); 0 = unknown.
    pcm: u32,
    ring: PlayRing,
    t_run: u64,
    t_obs: u64,
    underruns: u32,
    /// May the write loop `yield_now`? False at boot, before the scheduler runs.
    may_yield: bool,
    /// The codec's widget graph, kept for `dump_state`.
    graph: Option<codec::Graph>,
    /// The state dump has run at the first `RUN`.
    dumped_running: bool,
}

/// The controller once `init` succeeded. `/dev/dsp` exists exactly when the
/// backend below is registered, which happens after this is filled.
static HDA: Spinlock<Option<Hda>> = Spinlock::new(None);

impl Hda {
    // ---- command transport ------------------------------------------------

    /// Bring up the CORB and RIRB (HDA 1.0a §4.4, the sequence Linux's
    /// `azx_init_cmd_io` uses). `false` if the rings will not start.
    fn rings_init(&mut self) -> bool {
        let r = &self.regs;
        r.w8(reg::CORBCTL, 0);
        r.w8(reg::RIRBCTL, 0);
        let mut w = Wait::new(2_000);
        while (r.r8(reg::CORBCTL) & 2 != 0 || r.r8(reg::RIRBCTL) & 2 != 0) && !w.expired() {}

        // 256-entry rings, if the controller supports them (size cap bit 6).
        if r.r8(reg::CORBSIZE) & 0x40 == 0 || r.r8(reg::RIRBSIZE) & 0x40 == 0 {
            p("[HDA] rings: no 256-entry support\n");
            return false;
        }
        r.w8(reg::CORBSIZE, 0x02);
        r.w8(reg::RIRBSIZE, 0x02);

        let (cb, rb) = (phys(&raw mut CORB), phys(&raw mut RIRB));
        r.w32(reg::CORBLBASE, cb as u32);
        r.w32(reg::CORBUBASE, (cb >> 32) as u32);
        r.w32(reg::RIRBLBASE, rb as u32);
        r.w32(reg::RIRBUBASE, (rb >> 32) as u32);

        // CORB read pointer reset: set the strobe, wait for it to read back,
        // clear it, wait for that to read back.
        r.w16(reg::CORBRP, 0x8000);
        let mut w = Wait::new(2_000);
        while r.r16(reg::CORBRP) & 0x8000 == 0 && !w.expired() {}
        r.w16(reg::CORBRP, 0);
        let mut w = Wait::new(2_000);
        while r.r16(reg::CORBRP) & 0x8000 != 0 && !w.expired() {}
        r.w16(reg::CORBWP, 0);

        r.w16(reg::RIRBWP, 0x8000);
        self.rirb_rp = 0;
        r.w16(reg::RINTCNT, 0xFF);
        r.w8(reg::RIRBSTS, 0x05);
        r.w8(reg::RIRBCTL, 0x03); // DMA enable + response IRQ (INTCTL stays 0: no interrupt)
        r.w8(reg::CORBCTL, 0x02); // RUN
        let mut w = Wait::new(2_000);
        while (r.r8(reg::CORBCTL) & 2 == 0 || r.r8(reg::RIRBCTL) & 2 == 0) && !w.expired() {}
        r.r8(reg::CORBCTL) & 2 != 0 && r.r8(reg::RIRBCTL) & 2 != 0
    }

    fn send_corb(&mut self, v: u32) -> Option<u32> {
        let r = &self.regs;
        let wp = (r.r16(reg::CORBWP) & 0xFF) as usize;
        let np = (wp + 1) & 0xFF;
        // SAFETY: `np` < 256; the controller reads this slot only after the
        // CORBWP write below publishes it.
        unsafe {
            let slot = (&raw mut CORB.0).cast::<u32>().add(np);
            slot.write_volatile(v);
            flush(slot.cast::<u8>(), 4);
        }
        fence(Ordering::SeqCst);
        r.w16(reg::CORBWP, np as u16);

        let mut w = Wait::new(50_000);
        loop {
            let rwp = (r.r16(reg::RIRBWP) & 0xFF) as u8;
            if rwp != self.rirb_rp {
                fence(Ordering::SeqCst);
                let mut resp = None;
                let mut k = self.rirb_rp;
                while k != rwp {
                    k = k.wrapping_add(1);
                    // SAFETY: `k` < 256; the controller wrote this entry before
                    // advancing RIRBWP past it.
                    let e = unsafe {
                        let slot = (&raw mut RIRB.0).cast::<u64>().add(usize::from(k));
                        flush(slot.cast::<u8>(), 8);
                        slot.read_volatile()
                    };
                    let ex = (e >> 32) as u32;
                    if ex & 0x10 != 0 {
                        continue; // unsolicited: not the answer to what we sent
                    }
                    if resp.is_none() {
                        resp = Some(e as u32);
                    }
                }
                self.rirb_rp = rwp;
                r.w8(reg::RIRBSTS, 0x05);
                return resp;
            }
            if w.expired() {
                return None;
            }
            core::hint::spin_loop();
        }
    }

    /// Immediate command interface: one verb at a time, no DMA. The fallback
    /// Linux takes when the rings will not answer.
    fn send_ici(&mut self, v: u32) -> Option<u32> {
        let r = &self.regs;
        let mut w = Wait::new(5_000);
        while r.r16(reg::IRS) & 1 != 0 {
            if w.expired() {
                return None;
            }
        }
        r.w16(reg::IRS, 0x02); // clear a stale IRV (write 1 to clear)
        r.w32(reg::ICW, v);
        r.w16(reg::IRS, 0x01); // ICB: go
        let mut w = Wait::new(50_000);
        loop {
            let s = r.r16(reg::IRS);
            if s & 1 == 0 && s & 2 != 0 {
                return Some(r.r32(reg::IRR));
            }
            if w.expired() {
                return None;
            }
        }
    }

    fn run(&mut self, list: &VerbList) -> usize {
        let mut failed = 0;
        for &v in list.as_slice() {
            if self.send(v).is_none() {
                failed += 1;
            }
        }
        failed
    }

    // ---- stream -------------------------------------------------------------

    /// Reset the stream descriptor and program it for the current parameters,
    /// and bind the DACs to it. The stream is left stopped.
    fn stream_setup(&mut self) -> bool {
        let Some(fw) = stream::format_word(self.rate, 16, 2) else { return false };
        let r = &self.regs;
        let sd = self.sd;
        r.w8(sd + reg::SD_CTL, 0);
        let mut w = Wait::new(2_000);
        while r.r8(sd + reg::SD_CTL) & 2 != 0 && !w.expired() {}
        // Stream reset: SRST high (wait for it to stick), then low.
        r.w8(sd + reg::SD_CTL, 1);
        let mut w = Wait::new(2_000);
        while r.r8(sd + reg::SD_CTL) & 1 == 0 && !w.expired() {}
        r.w8(sd + reg::SD_CTL, 0);
        let mut w = Wait::new(2_000);
        while r.r8(sd + reg::SD_CTL) & 1 != 0 && !w.expired() {}

        // SAFETY: the stream is stopped, so the device is not reading the BDL.
        unsafe {
            let ring = phys(&raw mut RING);
            for i in 0..NFRAG {
                BDL.0[i] = stream::bdl_entry(ring + (i * FRAG) as u64, FRAG as u32, true);
            }
        }
        let bdl = phys(&raw mut BDL);
        flush((&raw const BDL).cast::<u8>(), core::mem::size_of::<Bdl>());
        r.w8(sd + reg::SD_STS, 0x1C); // clear BCIS | FIFOE | DESE
        r.w32(sd + reg::SD_BDPL, bdl as u32);
        r.w32(sd + reg::SD_BDPU, (bdl >> 32) as u32);
        r.w32(sd + reg::SD_CBL, RING_BYTES as u32);
        r.w16(sd + reg::SD_LVI, (NFRAG - 1) as u16);
        r.w16(sd + reg::SD_FMT, fw);
        r.w8(sd + reg::SD_CTL + 2, TAG << 4); // stream tag, bits 23:20

        let mut l = VerbList::new();
        for i in 0..self.ndacs {
            codec::stream_verbs(self.dacs[i], TAG, fw, &mut l);
        }
        let failed = self.run(&l);
        self.ring = PlayRing::new(RING_BYTES as u32);
        self.running = false;
        self.pos_src = PosSrc::Lpib;
        self.configured = true;
        failed == 0
    }

    fn stream_run(&mut self) {
        self.regs.w8(self.sd + reg::SD_CTL, 0x02);
        self.running = true;
        self.t_run = now_us().unwrap_or(0);
        self.t_obs = self.t_run;
        if !self.dumped_running {
            self.dumped_running = true;
            // Sample the stream's own registers over the first 150 ms: does the
            // controller fetch the ring (BCIS after the first fragment, a
            // position that moves), and does the DMA position buffer agree?
            for _ in 0..5 {
                delay_us(30_000);
                let sts = self.regs.r8(self.sd + reg::SD_STS);
                p("[HDA] +30ms lpib=");
                dec(u64::from(self.regs.r32(self.sd + reg::SD_LPIB)));
                p(" posbuf=");
                dec(u64::from(self.posbuf()));
                p(" sts=0x");
                hx(u64::from(sts), 2);
                p(" ctl=0x");
                hx(u64::from(self.regs.r32(self.sd + reg::SD_CTL) & 0x00FF_FFFF), 6);
                p(" fifos=");
                dec(u64::from(self.regs.r16(self.sd + reg::SD_FIFOS)));
                p("\n");
                self.regs.w8(self.sd + reg::SD_STS, 0x1C);
            }
            dump_state(self, "while running");
        }
    }

    fn stream_stop(&mut self) {
        self.regs.w8(self.sd + reg::SD_CTL, 0);
        self.running = false;
        self.configured = false;
    }

    /// The position-buffer slot for the output stream.
    fn posbuf(&self) -> u32 {
        // SAFETY: `sd_index` < 16, so the slot is inside the 128-byte static;
        // the controller writes it, hence the flush before the read.
        unsafe {
            let slot = (&raw mut POSBUF.0).cast::<u32>().add(self.sd_index * 2);
            flush(slot.cast::<u8>(), 4);
            slot.read_volatile()
        }
    }

    /// Fold the hardware position into the ring.
    fn observe(&mut self) {
        let now = now_us().unwrap_or(0);
        let pos = match self.pos_src {
            PosSrc::Lpib => self.regs.r32(self.sd + reg::SD_LPIB),
            PosSrc::PosBuf => self.posbuf(),
            PosSrc::Clock => {
                let bps = u64::from(self.rate) * 4;
                ((now.saturating_sub(self.t_run) * bps / 1_000_000) % RING_BYTES as u64) as u32
            }
        };
        self.ring.observe(pos);
        // A source that has not moved 60 ms after RUN does not work on this
        // controller: move to the next.
        if self.running && self.ring.consumed() == 0 && self.pos_src != PosSrc::Clock
            && now.saturating_sub(self.t_run) > 60_000 && self.t_run != 0
        {
            if self.pos_src == PosSrc::Lpib && self.posbuf() != 0 {
                p("[HDA] LPIB not advancing; using the DMA position buffer\n");
                self.pos_src = PosSrc::PosBuf;
            } else {
                p("[HDA] no position source advancing; pacing by clock\n");
                self.pos_src = PosSrc::Clock;
            }
        }
        self.t_obs = now;
    }

    /// One lap of the ring, in µs.
    fn lap_us(&self) -> u64 {
        RING_BYTES as u64 * 1_000_000 / (u64::from(self.rate) * 4)
    }

    fn idle(&self) {
        if self.may_yield {
            crate::sched::yield_now();
        } else {
            core::hint::spin_loop();
        }
    }

    /// Queue PCM. Blocks (yielding) while the ring is full; returns the number
    /// of bytes consumed from `data` (all of it, less a trailing partial frame).
    fn write(&mut self, data: &[u8]) -> usize {
        let mut src = data;
        let mut tmp = [0u8; 2048];
        while !src.is_empty() {
            if !self.configured && !self.stream_setup() {
                p("[HDA] stream setup failed\n");
                return data.len() - src.len();
            }
            if self.running {
                // A lap missed while nobody was writing loses count of the
                // position (LPIB only says where in the ring the hardware is),
                // and either way the hardware is replaying stale bytes.
                let missed = now_us().unwrap_or(0).saturating_sub(self.t_obs) > self.lap_us();
                self.observe();
                if self.ring.underrun() || missed {
                    self.underruns += 1;
                    self.stream_stop();
                    continue;
                }
            }
            let space = self.ring.space(GUARD) & !3;
            if space < 4 {
                self.idle();
                continue;
            }
            let room = space.min(tmp.len());
            let (ci, co) = stream::to_s16_stereo(self.fmt, self.channels, src, &mut tmp[..room]);
            if co == 0 {
                break; // less than one whole frame left: drop it
            }
            let (off, first, second) = self.ring.reserve(co);
            // SAFETY: `reserve` handed out `[off, off+first)` and `[0, second)`,
            // which `space` proved the hardware has already consumed.
            unsafe {
                let base = (&raw mut RING.0).cast::<u8>();
                core::ptr::copy_nonoverlapping(tmp.as_ptr(), base.add(off), first);
                core::ptr::copy_nonoverlapping(tmp.as_ptr().add(first), base, second);
                flush(base.add(off), first);
                flush(base, second);
            }
            src = &src[ci..];
            if !self.running && self.ring.buffered() >= (RING_BYTES / 2) as u64 {
                fence(Ordering::SeqCst);
                self.stream_run();
            }
        }
        data.len()
    }

    /// Append `bytes` of digital silence (16-bit stereo zeros) to the ring,
    /// waiting for space as `write` does.
    fn push_silence(&mut self, mut bytes: usize) {
        while bytes > 0 && self.configured {
            if self.running {
                self.observe();
            }
            let n = self.ring.space(GUARD).min(bytes) & !3;
            if n < 4 {
                self.idle();
                continue;
            }
            let (off, first, second) = self.ring.reserve(n);
            // SAFETY: as in `write`; the spans are ones `space` proved consumed.
            unsafe {
                let base = (&raw mut RING.0).cast::<u8>();
                core::ptr::write_bytes(base.add(off), 0, first);
                core::ptr::write_bytes(base, 0, second);
                flush(base.add(off), first);
                flush(base, second);
            }
            bytes -= n;
            if !self.running && self.ring.buffered() >= (RING_BYTES / 2) as u64 {
                fence(Ordering::SeqCst);
                self.stream_run();
            }
        }
    }

    /// Play out what is buffered and stop the stream.
    fn drain(&mut self) {
        if !self.configured {
            return;
        }
        // The position register runs ahead of what has left the converter
        // (an emulated controller by a whole buffer, real silicon by its FIFO
        // and the DAC pipeline): stopping the moment it reaches the last real
        // sample truncates the tail. 100 ms of silence behind the audio costs
        // nothing on a path that runs once, at close.
        self.push_silence(self.rate as usize * 4 / 10);
        if !self.running && self.ring.buffered() > 0 {
            fence(Ordering::SeqCst);
            self.stream_run();
        }
        if self.running {
            let mut w = Wait::new(self.lap_us() + 200_000);
            loop {
                self.observe();
                if self.ring.underrun() || w.expired() {
                    break;
                }
                self.idle();
            }
        }
        self.stream_stop();
    }
}

impl VerbBus for Hda {
    fn send(&mut self, v: u32) -> Option<u32> {
        let v = verb::with_cad(v, self.cad);
        if self.use_ici { self.send_ici(v) } else { self.send_corb(v) }
    }
}

// ---------------------------------------------------------------------------
// Bring-up
// ---------------------------------------------------------------------------

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::OutputConverter => "dac",
        Kind::InputConverter => "adc",
        Kind::Mixer => "mixer",
        Kind::Selector => "selector",
        Kind::Pin => "pin",
        Kind::Other(_) => "other",
        Kind::Absent => "-",
    }
}

fn role_name(r: PinRole) -> &'static str {
    match r {
        PinRole::LineOut => "line-out",
        PinRole::Speaker => "speaker",
        PinRole::Headphone => "headphone",
    }
}

/// Print the widget graph once — the runbook's step 4, which is where a wrong
/// turn is silent.
fn print_graph(g: &codec::Graph) {
    p("[HDA] graph: afg=0x");
    hx(u64::from(g.afg), 2);
    p(" widgets=");
    dec(u64::from(g.count));
    p("\n");
    for w in g.iter() {
        if matches!(w.kind, Kind::Other(_)) {
            continue;
        }
        p("[HDA]   ");
        hx(u64::from(w.nid), 2);
        p(" ");
        p(kind_name(w.kind));
        if w.kind == Kind::Pin {
            p(" cfg=");
            hx(u64::from(w.pin_cfg), 8);
            if let Some(role) = w.output_role() {
                p(" ");
                p(role_name(role));
            }
        }
        if w.nconn > 0 {
            p(" <-");
            for c in &w.conn[..usize::from(w.nconn)] {
                p(" ");
                hx(u64::from(*c), 2);
            }
        }
        p("\n");
    }
}

/// Read back and print what the codec actually holds for every DAC, mixer and
/// output pin — the evidence that separates "the verbs were wrong" from "the
/// verbs were right and the analog side is somewhere we have not looked".
fn dump_state(h: &mut Hda, when: &str) {
    let Some(g) = h.graph.take() else { return };
    p("[HDA] state ");
    p(when);
    p(":\n");
    let gpio = h.send(verb::get_param(g.afg, verb::param::GPIO_COUNT)).unwrap_or(0);
    p("[HDA]   afg power=0x");
    hx(u64::from(h.send(verb::get_power(g.afg)).unwrap_or(0xFFFF)), 2);
    p(" gpios=");
    dec(u64::from(gpio & 0xFF));
    p(" data=0x");
    hx(u64::from(h.send(verb::get_gpio_data(g.afg)).unwrap_or(0xFFFF)), 2);
    p(" enable=0x");
    hx(u64::from(h.send(verb::get_gpio_enable(g.afg)).unwrap_or(0xFFFF)), 2);
    p(" dir=0x");
    hx(u64::from(h.send(verb::get_gpio_dir(g.afg)).unwrap_or(0xFFFF)), 2);
    p("\n");
    for w in g.iter() {
        let interesting = match w.kind {
            Kind::OutputConverter | Kind::Mixer => true,
            Kind::Pin => w.output_role().is_some(),
            _ => false,
        };
        if !interesting {
            continue;
        }
        p("[HDA]   ");
        hx(u64::from(w.nid), 2);
        p(" ");
        p(kind_name(w.kind));
        p(" pwr=0x");
        hx(u64::from(h.send(verb::get_power(w.nid)).unwrap_or(0xFFFF)), 2);
        if w.kind == Kind::OutputConverter {
            p(" stream=0x");
            hx(u64::from(h.send(verb::get_stream(w.nid)).unwrap_or(0xFFFF)), 2);
            p(" fmt=0x");
            hx(u64::from(h.send(verb::get_conv_fmt(w.nid)).unwrap_or(0xFFFF)), 4);
        }
        if w.kind == Kind::Pin {
            p(" ctl=0x");
            hx(u64::from(h.send(verb::get_pin_ctrl(w.nid)).unwrap_or(0xFFFF)), 2);
            p(" eapd=0x");
            hx(u64::from(h.send(verb::get_eapd(w.nid)).unwrap_or(0xFFFF)), 2);
            p(" sense=0x");
            hx(u64::from(h.send(verb::get_pin_sense(w.nid)).unwrap_or(0xFFFF)), 8);
        }
        if w.nconn > 1 && w.kind != Kind::Mixer {
            p(" sel=");
            dec(u64::from(h.send(verb::get_conn_sel(w.nid)).unwrap_or(0xFF)));
        }
        if w.has_out_amp() {
            p(" out=");
            hx(u64::from(h.send(verb::get_amp(w.nid, true, true, 0)).unwrap_or(0xFFFF)), 2);
            p("/");
            hx(u64::from(h.send(verb::get_amp(w.nid, true, false, 0)).unwrap_or(0xFFFF)), 2);
        }
        if w.has_in_amp() {
            p(" in[");
            for i in 0..w.nconn.max(1) {
                if i > 0 {
                    p(" ");
                }
                hx(u64::from(h.send(verb::get_amp(w.nid, false, true, i)).unwrap_or(0xFFFF)), 2);
            }
            p("]");
        }
        p("\n");
    }
    h.graph = Some(g);
}

fn print_verify(h: &mut Hda, pin: u8, dac: u8) {
    let stream = h.send(verb::get_stream(dac)).unwrap_or(0xFFFF);
    let fmt = h.send(verb::get_conv_fmt(dac)).unwrap_or(0xFFFF);
    let amp = h.send(verb::get_amp(dac, true, true, 0)).unwrap_or(0xFFFF);
    let pctl = h.send(verb::get_pin_ctrl(pin)).unwrap_or(0xFFFF);
    let pow = h.send(verb::get_power(dac)).unwrap_or(0xFFFF);
    p("[HDA] verify dac 0x");
    hx(u64::from(dac), 2);
    p(": stream=0x");
    hx(u64::from(stream), 2);
    p(" fmt=0x");
    hx(u64::from(fmt), 4);
    p(" amp=0x");
    hx(u64::from(amp), 2);
    p(" power=0x");
    hx(u64::from(pow), 2);
    p("  pin 0x");
    hx(u64::from(pin), 2);
    p(" ctl=0x");
    hx(u64::from(pctl), 2);
    p("\n");
}

/// Find the Intel HDA controller, bring it and its codec up, and register
/// `/dev/dsp`. Best-effort: on any failure it prints one line and returns, and
/// the box boots without sound. `selftest` plays a one-second tone through the
/// same write path `wavplay` uses.
pub fn init(selftest: bool) {
    let Some(dev) = crate::pci::find_class(akuma_pci::class::MULTIMEDIA, akuma_pci::subclass::AUDIO) else {
        p("[HDA] no audio-class function on the bus\n");
        return;
    };
    // The census: on the trashcan the NVIDIA HDMI function is class 04:03 too.
    crate::pci::for_each(|d| {
        if d.header.is_audio() {
            p("[HDA] audio function ");
            hx(u64::from(d.header.vendor_id), 4);
            p(":");
            hx(u64::from(d.header.device_id), 4);
            p("\n");
        }
    });
    let addr = dev.addr;
    if dev.header.vendor_id != INTEL {
        p("[HDA] first 04:03 function is not Intel; leaving audio off\n");
        return;
    }
    crate::pci::enable(addr, true);

    // What Linux's `azx_init_pci` does for every Intel controller: route all
    // controller traffic through traffic class 0 (config 0x44 TCSEL[2:0]) and
    // turn off no-snoop (config 0x78 DEVC bit 11), so that device DMA is
    // coherent with the CPU caches. Firmware may leave either set.
    let tcsel = crate::pci::read_u16_config(addr, 0x44);
    let devc = crate::pci::read_u16_config(addr, 0x78);
    crate::pci::write_u16_config(addr, 0x44, tcsel & !0x7);
    crate::pci::write_u16_config(addr, 0x78, devc & !(1 << 11));
    p("[HDA] pci tcsel 0x");
    hx(u64::from(tcsel), 4);
    p("->0x");
    hx(u64::from(crate::pci::read_u16_config(addr, 0x44)), 4);
    p(" devc 0x");
    hx(u64::from(devc), 4);
    p("->0x");
    hx(u64::from(crate::pci::read_u16_config(addr, 0x78)), 4);
    p("\n");

    // Power: a healthy controller is already D0. Cycling D3->D0 on one that is
    // wedges it (every BAR0 byte then reads 0xff), so only ever write D0 back
    // when PMCSR says it is something else.
    let cfg = crate::pci::config_space(addr);
    for cap in akuma_pci::capabilities(&cfg, dev.header.capabilities_pointer) {
        if cap.id == akuma_pci::capability_id::POWER_MANAGEMENT {
            let pmcsr = crate::pci::read_u16_config(addr, cap.offset + 4);
            if akuma_pci::pm::power_state(pmcsr) != akuma_pci::pm::D0 {
                crate::pci::write_u16_config(addr, cap.offset + 4, pmcsr & !akuma_pci::pm::POWER_STATE_MASK);
                delay_us(10_000);
                p("[HDA] PCI power state was not D0; wrote D0\n");
            }
            break;
        }
    }

    let Some(bar) = dev.bars.into_iter().next().flatten() else {
        p("[HDA] BAR0 absent\n");
        return;
    };
    let (size, _) = crate::pci::probe_bar_size(addr, 0);
    let Some(base) = crate::pci::map_bar(bar, size.max(0x4000)) else {
        p("[HDA] BAR0 map failed\n");
        return;
    };
    let regs = Mmio { base };
    let Some(info) = akuma_hda::discover(&regs) else {
        p("[HDA] BAR0 reads 0xffff: not decoded\n");
        return;
    };
    p("[HDA] ");
    hx(u64::from(dev.header.vendor_id), 4);
    p(":");
    hx(u64::from(dev.header.device_id), 4);
    p(" version=");
    dec(u64::from(info.vmaj));
    p(".");
    dec(u64::from(info.vmin));
    p(" oss=");
    dec(u64::from(info.gcap.output_streams()));
    p(" iss=");
    dec(u64::from(info.gcap.input_streams()));
    p(" 64bit=");
    dec(u64::from(info.gcap.supports_64bit()));
    p("\n");
    if info.gcap.output_streams() == 0 {
        p("[HDA] no output streams\n");
        return;
    }

    // Reset, then wait for the codecs to announce themselves (they need
    // ~521 us after the link comes up, and STATESTS is only valid after that).
    if !akuma_hda::reset(&regs, 1 << 16, || delay_us(1)) {
        p("[HDA] CRST handshake failed\n");
        return;
    }
    regs.w32(reg::INTCTL, 0);
    // DMA position buffer (an independent view of stream position).
    let pb = phys(&raw mut POSBUF);
    regs.w32(reg::DPUBASE, (pb >> 32) as u32);
    regs.w32(reg::DPLBASE, (pb as u32) | 1);
    delay_us(1_000);
    let mut w = Wait::new(100_000);
    let mut states = regs.r16(reg::STATESTS) & 0x7FFF;
    while states == 0 && !w.expired() {
        states = regs.r16(reg::STATESTS) & 0x7FFF;
    }
    if states == 0 {
        p("[HDA] no codec answered the link wake-up\n");
        return;
    }
    regs.w16(reg::STATESTS, states);
    let cad = states.trailing_zeros() as u8;
    p("[HDA] codec address ");
    dec(u64::from(cad));
    p(" (statests 0x");
    hx(u64::from(states), 4);
    p(")\n");

    let mut h = Hda {
        regs,
        cad,
        use_ici: false,
        rirb_rp: 0,
        sd: reg::output_sd(info.gcap.input_streams(), 0),
        dacs: [0; 4],
        ndacs: 0,
        rate: 44_100,
        channels: 2,
        fmt: SampleFormat::S16,
        configured: false,
        running: false,
        pos_src: PosSrc::Lpib,
        sd_index: usize::from(info.gcap.input_streams()),
        pcm: 0,
        ring: PlayRing::new(RING_BYTES as u32),
        t_run: 0,
        t_obs: 0,
        underruns: 0,
        may_yield: false,
        graph: None,
        dumped_running: false,
    };

    // Transport: the rings, else the immediate command registers.
    let ring_ok = h.rings_init();
    let vendor = if ring_ok { h.send(verb::get_param(0, verb::param::VENDOR_ID)) } else { None };
    let vendor = match vendor {
        Some(v) if v != 0 => Some(v),
        _ => {
            p("[HDA] CORB/RIRB silent; trying the immediate command interface\n");
            h.use_ici = true;
            h.send(verb::get_param(0, verb::param::VENDOR_ID)).filter(|v| *v != 0)
        }
    };
    let Some(vendor) = vendor else {
        p("[HDA] codec does not answer verbs on either path\n");
        return;
    };
    p("[HDA] codec vendor/device 0x");
    hx(u64::from(vendor), 8);
    p(if h.use_ici { " via ICI\n" } else { " via CORB/RIRB\n" });

    let Some(g) = codec::discover(&mut h) else {
        p("[HDA] no audio function group\n");
        return;
    };
    print_graph(&g);

    // Route every connected output pin to a DAC, headphones first.
    let mut list = VerbList::new();
    codec::afg_verbs(&g, &mut list);
    let mut first_pin = 0u8;
    for want in [PinRole::Headphone, PinRole::LineOut, PinRole::Speaker] {
        for w in g.iter().filter(|w| w.output_role() == Some(want)) {
            let Some(path) = codec::find_path(&g, w.nid, &h.dacs[..h.ndacs]) else {
                p("[HDA] pin 0x");
                hx(u64::from(w.nid), 2);
                p(": no route to a DAC\n");
                continue;
            };
            codec::path_verbs(&g, &path, DAC_PCT, &mut list);
            p("[HDA] route ");
            p(role_name(want));
            p(":");
            for i in 0..usize::from(path.len) {
                p(" ");
                hx(u64::from(path.nid[i]), 2);
            }
            p("\n");
            if first_pin == 0 {
                first_pin = w.nid;
            }
            let dac = path.converter();
            if !h.dacs[..h.ndacs].contains(&dac) && h.ndacs < h.dacs.len() {
                h.dacs[h.ndacs] = dac;
                h.ndacs += 1;
            }
        }
    }
    if h.ndacs == 0 {
        p("[HDA] no output route found\n");
        return;
    }
    let failed = h.run(&list);
    if failed != 0 {
        p("[HDA] verbs without a response: ");
        dec(failed as u64);
        p("\n");
    }
    // Read back what the codec says it holds, using the same encoders.
    let dac0 = h.dacs[0];
    h.pcm = h.send(verb::get_param(dac0, verb::param::PCM)).unwrap_or(0);
    p("[HDA] dac pcm caps 0x");
    hx(u64::from(h.pcm), 8);
    p("\n");
    if h.stream_setup() {
        print_verify(&mut h, first_pin, dac0);
    } else {
        p("[HDA] stream setup failed\n");
        return;
    }
    h.stream_stop();
    h.graph = Some(g);
    dump_state(&mut h, "after init");

    *HDA.lock() = Some(h);
    akuma_virtio::audio::hda_backend::register(akuma_virtio::audio::hda_backend::Ops {
        write: dsp_write,
        stop: dsp_stop,
        set_rate: dsp_set_rate,
        set_format: dsp_set_format,
        set_channels: dsp_set_channels,
    });
    p("[HDA] ready (/dev/dsp)\n");

    if selftest {
        tone(1_000);
    }
}

/// Play `ms` of a 440 Hz triangle wave through the `/dev/dsp` write path.
fn tone(ms: u32) {
    p("[HDA] selftest tone\n");
    let mut buf = [0u8; 4096];
    let frames_total = 44_100 * ms / 1000;
    let mut done = 0u32;
    let mut phase = 0u32;
    while done < frames_total {
        let n = ((frames_total - done) as usize).min(buf.len() / 4);
        for f in 0..n {
            // Triangle, period 100 frames (441 Hz at 44.1 kHz), +-8000.
            let x = (phase % 100) as i32;
            let s = (if x < 50 { x * 320 - 8000 } else { (100 - x) * 320 - 8000 }) as i16;
            buf[f * 4..f * 4 + 2].copy_from_slice(&s.to_le_bytes());
            buf[f * 4 + 2..f * 4 + 4].copy_from_slice(&s.to_le_bytes());
            phase += 1;
        }
        dsp_write(&buf[..n * 4]);
        done += n as u32;
    }
    dsp_stop();
    p("[HDA] selftest tone done\n");
}

// ---------------------------------------------------------------------------
// /dev/dsp backend (registered into `akuma_virtio::audio::hda_backend`)
// ---------------------------------------------------------------------------

fn dsp_write(data: &[u8]) -> usize {
    match HDA.lock().as_mut() {
        Some(h) => {
            h.may_yield = crate::usermode::current_process().is_some();
            h.write(data)
        }
        None => 0,
    }
}

fn dsp_stop() {
    if let Some(h) = HDA.lock().as_mut() {
        h.may_yield = crate::usermode::current_process().is_some();
        h.drain();
        if h.underruns != 0 {
            p("[HDA] playback underruns: ");
            dec(u64::from(h.underruns));
            p("\n");
            h.underruns = 0;
        }
    }
}

/// Apply a parameter change: anything already buffered plays out first, and the
/// stream is re-set-up on the next write.
fn with_params(f: impl FnOnce(&mut Hda) -> bool) -> bool {
    match HDA.lock().as_mut() {
        Some(h) => {
            h.may_yield = crate::usermode::current_process().is_some();
            h.drain();
            f(h)
        }
        None => false,
    }
}

fn dsp_set_rate(rate: i32) -> bool {
    with_params(|h| {
        let r = rate as u32;
        if stream::format_word(r, 16, 2).is_none() || (h.pcm != 0 && !stream::pcm_supports(h.pcm, r, 16)) {
            return false;
        }
        h.rate = r;
        true
    })
}

fn dsp_set_format(fmt: i32) -> bool {
    with_params(|h| match SampleFormat::from_oss(fmt) {
        Some(f) => {
            h.fmt = f;
            true
        }
        None => false,
    })
}

fn dsp_set_channels(ch: i32) -> bool {
    with_params(|h| {
        if (1..=2).contains(&ch) {
            h.channels = ch as u8;
            true
        } else {
            false
        }
    })
}
