//! A polled DesignWare `DW_apb_i2c` master — the AMD FCH `AMDI0010` blocks —
//! for the touchpad (`crate::touchpad`).
//!
//! Every *decision* (register map, `IC_CON`, timing arithmetic, abort decode,
//! the AOAC wake sequence) is `akuma-dwi2c`'s, host-tested. This file is only
//! mechanism: the MMIO mapping, the volatile accesses and the transaction
//! loop. No interrupts, no allocation, every wait bounded by a TSC deadline
//! — a dead bus costs the caller a timeout, never a wedge.
//!
//! # The contract
//!
//! `regs` is a 4 KiB window mapped as **device memory** (`map_mmio`), and every
//! offset passed to `rd`/`wr` is a `akuma_dwi2c::ic` register inside it - the
//! window refuses one that is not.
//!
//! # Power
//!
//! A runtime-suspended block reads `0xFFFF_FFFF` everywhere (2026-10-16,
//! `docs/archive/AKUMA_AMD64_TOUCHPAD_I2C_HID.md` §4a). [`wake`] performs the
//! DSDT's `_PS0` (`DSAD`) before the alive-check, so "all ones" means "powered
//! down" and not "absent".

use akuma_dwi2c::{BusSpeed, XferError, aoac, data_cmd, ic, intr, status};
use akuma_primitives::mmio::MmioWindow;

use crate::paging::{self, MemAttr, PteProt};
use crate::phys::DEVMAP_BASE;
use crate::polltime::{spin_us, tsc, ticks_ms};

const PAGE: u64 = 4096;

/// Map `[pa, pa + len)` as device memory in the device window and return the
/// the `len` bytes at `pa` as a register window.
pub fn map_mmio(pa: u64, len: u64) -> Option<MmioWindow> {
    let first = pa & !(PAGE - 1);
    let last = (pa + len).div_ceil(PAGE) * PAGE;
    let mut p = first;
    while p < last {
        if !paging::map_page((DEVMAP_BASE + p) as usize, p, PteProt::KERNEL_RW, MemAttr::Device) {
            return None;
        }
        p += PAGE;
    }
    // SAFETY: every page of `[pa, pa + len)` was just mapped `MemAttr::Device`
    // for the kernel's lifetime. The accesses made through the window are the
    // widths the callers name: bytes for the AOAC pair, dwords for a
    // DesignWare block, whose base is 4 KiB-aligned.
    Some(unsafe { MmioWindow::new((DEVMAP_BASE + pa) as usize, len as usize) })
}

/// Request D0 for AOAC device `dev` and wait (bounded) for the status field to
/// say the block is on. Returns whether it is. Idempotent: a block already in
/// D0 is not written to.
///
/// The control byte is a read-modify-write of one byte (`ByteAcc` in the
/// DSDT's field); the status byte is the next.
pub fn wake(dev: u32) -> bool {
    let pa = aoac::addr(dev);
    let Some(win) = map_mmio(pa, 2) else { return false };
    // Both are single-byte accesses, the width the DSDT uses.
    let rd_ctl = || win.read::<u8>(0);
    let rd_st = || win.read::<u8>(1);
    if aoac::wants_d0(rd_ctl()) && aoac::is_on(rd_st()) {
        return true;
    }
    win.write::<u8>(0, aoac::d0_control(rd_ctl()));
    let deadline = tsc().wrapping_add(ticks_ms(200));
    while !aoac::is_on(rd_st()) {
        if tsc().wrapping_sub(deadline) < u64::MAX / 2 {
            return false;
        }
        spin_us(50);
    }
    true
}

/// One controller.
pub struct Dw {
    regs: MmioWindow,
    tx_depth: u32,
    rx_depth: u32,
}

/// What `Dw::init` found, for the bring-up log.
#[derive(Clone, Copy, Debug)]
pub struct Probe {
    pub version: u32,
    pub fs_hcnt: u32,
    pub fs_lcnt: u32,
    pub tx_depth: u32,
    pub rx_depth: u32,
    /// The counts were the firmware's/OS's (true) or computed (false).
    pub kept_counts: bool,
}

/// The controller input clock to assume when no divide counts survive. The
/// 16AHP9's live fast-mode counts (`0x84 + 0xEF` for 2.5 µs) put it near
/// 150 MHz.
const FALLBACK_IC_CLK_KHZ: u32 = 150_000;

impl Dw {
    /// Wrap a mapped 4 KiB window of a DesignWare I²C block. No access happens
    /// until `init`. A register offset outside the window panics instead of
    /// reading whatever is there.
    #[must_use]
    pub const fn new(regs: MmioWindow) -> Self {
        Self { regs, tx_depth: 8, rx_depth: 8 }
    }

    fn rd(&self, off: usize) -> u32 {
        self.regs.read::<u32>(off)
    }

    fn wr(&self, off: usize, v: u32) {
        self.regs.write::<u32>(off, v);
    }

    /// Is this a (powered) DesignWare block?
    #[must_use]
    pub fn alive(&self) -> bool {
        self.rd(ic::COMP_TYPE) == akuma_dwi2c::COMP_TYPE_VALUE
    }

    fn wait(&self, ms: u64, mut done: impl FnMut(&Self) -> bool) -> bool {
        let deadline = tsc().wrapping_add(ticks_ms(ms));
        while !done(self) {
            if tsc().wrapping_sub(deadline) < u64::MAX / 2 {
                return false;
            }
            spin_us(10);
        }
        true
    }

    fn set_enabled(&self, on: bool) -> bool {
        self.wr(ic::ENABLE, u32::from(on));
        self.wait(25, |d| (d.rd(ic::ENABLE_STATUS) & 1 == 1) == on)
    }

    /// Program the block as a master talking to `addr7`, at `speed`.
    ///
    /// Keeps the divide counts already in the registers when they are usable
    /// (the DSDT has no `FMCN`/`SSCN` here, and Linux does the same with no
    /// clock source), else computes them. `IC_CON.BUS_CLEAR_CTRL` is preserved
    /// — the SMU owns stuck-bus recovery.
    pub fn init(&mut self, addr7: u8, speed: BusSpeed) -> Option<Probe> {
        if !self.alive() {
            return None;
        }
        let params = self.rd(ic::COMP_PARAM_1);
        let (tx, rx) = akuma_dwi2c::fifo_depths(params);
        self.tx_depth = tx;
        self.rx_depth = rx;
        if !self.set_enabled(false) {
            return None;
        }
        let bios_con = self.rd(ic::CON);
        self.wr(ic::CON, akuma_dwi2c::master_con(speed, bios_con));

        let (h_reg, l_reg) = match speed {
            BusSpeed::Standard => (ic::SS_SCL_HCNT, ic::SS_SCL_LCNT),
            BusSpeed::Fast => (ic::FS_SCL_HCNT, ic::FS_SCL_LCNT),
        };
        let (h, l) = (self.rd(h_reg), self.rd(l_reg));
        let kept = akuma_dwi2c::counts_usable(h, l);
        if !kept {
            let t = akuma_dwi2c::timings_from_clock(FALLBACK_IC_CLK_KHZ);
            let c = match speed {
                BusSpeed::Standard => t.ss,
                BusSpeed::Fast => t.fs,
            };
            self.wr(h_reg, u32::from(c.hcnt));
            self.wr(l_reg, u32::from(c.lcnt));
        }
        self.wr(ic::TAR, u32::from(addr7));
        self.wr(ic::INTR_MASK, 0);
        self.wr(ic::RX_TL, 0);
        self.wr(ic::TX_TL, 0);
        let _ = self.rd(ic::CLR_INTR);
        if !self.set_enabled(true) {
            return None;
        }
        Some(Probe {
            version: self.rd(ic::COMP_VERSION),
            fs_hcnt: self.rd(ic::FS_SCL_HCNT),
            fs_lcnt: self.rd(ic::FS_SCL_LCNT),
            tx_depth: tx,
            rx_depth: rx,
            kept_counts: kept,
        })
    }

    /// Read `IC_TX_ABRT_SOURCE`, clear it, and name the error.
    fn abort_error(&self) -> XferError {
        let src = self.rd(ic::TX_ABRT_SOURCE);
        let _ = self.rd(ic::CLR_TX_ABRT);
        match akuma_dwi2c::TxAbort::from_source(src) {
            akuma_dwi2c::TxAbort::AddrNoAck | akuma_dwi2c::TxAbort::DataNoAck => XferError::NoDevice,
            _ => XferError::Bus,
        }
    }

    /// Re-initialise after a timeout or bus error (Linux's no-GPIO
    /// recovery): drop the enable so the FIFOs flush, bring it back.
    pub fn recover(&mut self) {
        let _ = self.set_enabled(false);
        let _ = self.rd(ic::CLR_INTR);
        let _ = self.set_enabled(true);
    }

    /// One bus transaction: write `wr` (may be empty), then — with a repeated
    /// start — read `rd.len()` bytes (may be zero). Both empty is a no-op.
    ///
    /// # Errors
    /// [`XferError::NoDevice`] on a NACK, [`XferError::Timeout`] if the
    /// transfer outlives 40 ms (the caller `recover`s), [`XferError::Bus`]
    /// for any other abort.
    pub fn xfer(&mut self, wr: &[u8], rd: &mut [u8]) -> Result<(), XferError> {
        let total = wr.len() + rd.len();
        if total == 0 {
            return Ok(());
        }
        let _ = self.rd(ic::CLR_INTR);
        let deadline = tsc().wrapping_add(ticks_ms(40));
        let (mut sent, mut got) = (0usize, 0usize);

        // Commands go out as FIFO space allows, never more reads in flight
        // than the RX FIFO can hold, and results are drained as they land.
        while got < rd.len() || sent < total {
            if self.rd(ic::RAW_INTR_STAT) & intr::TX_ABRT != 0 {
                return Err(self.abort_error());
            }
            while sent < total && self.rd(ic::TXFLR) < self.tx_depth {
                let is_read = sent >= wr.len();
                if is_read && (sent - wr.len()) - got >= self.rx_depth as usize {
                    break;
                }
                let mut cmd = if is_read { data_cmd::READ } else { u32::from(wr[sent]) };
                if is_read && sent == wr.len() && !wr.is_empty() {
                    cmd |= data_cmd::RESTART;
                }
                if sent + 1 == total {
                    cmd |= data_cmd::STOP;
                }
                self.wr(ic::DATA_CMD, cmd);
                sent += 1;
            }
            while got < rd.len() && self.rd(ic::RXFLR) > 0 {
                rd[got] = (self.rd(ic::DATA_CMD) & 0xFF) as u8;
                got += 1;
            }
            if tsc().wrapping_sub(deadline) < u64::MAX / 2 {
                return Err(XferError::Timeout);
            }
        }
        // Writes only: wait for the FIFO to drain and the master to finish.
        let finished = self.wait(40, |d| {
            let s = d.rd(ic::STATUS);
            s & status::TFE != 0 && s & status::MST_ACTIVITY == 0
        });
        if self.rd(ic::RAW_INTR_STAT) & intr::TX_ABRT != 0 {
            return Err(self.abort_error());
        }
        let _ = self.rd(ic::CLR_STOP_DET);
        if finished { Ok(()) } else { Err(XferError::Timeout) }
    }
}
