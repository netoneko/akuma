//! DesignWare `DW_apb_i2c` decisions for the AMD FCH I²C controllers
//! (ACPI `_HID` `AMD0010`/`AMDI0010`/`AMDI0019`), pure and host-tested:
//! the register map, the master `IC_CON` value, SCL timing arithmetic,
//! FIFO-depth and component-type decode, and the `TX_ABRT_SOURCE` → error
//! mapping. The MMIO transaction loop itself lives kernel-side
//! (`amd64/src/i2c.rs`, proposed); everything here is the part worth
//! testing on the host.
//!
//! Behavioural reference: Linux `drivers/i2c/busses/i2c-designware-*.c`
//! and `include/linux/designware_i2c.h` (GPL, consulted not copied); U-Boot's
//! `drivers/i2c/designware_i2c.c` is the same programming model in less
//! code. Platform notes come from `i2c_dw_probe_master`'s AMD comment: BIOS
//! advertises the bus-clear feature, the SMU firmware performs stuck-bus
//! recovery, and the driver must preserve `IC_CON.BUS_CLEAR_CTRL`.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

/// Register offsets (32-bit MMIO registers).
pub mod ic {
    pub const CON: usize = 0x00;
    pub const TAR: usize = 0x04;
    pub const SAR: usize = 0x08;
    pub const DATA_CMD: usize = 0x10;
    pub const SS_SCL_HCNT: usize = 0x14;
    pub const SS_SCL_LCNT: usize = 0x18;
    pub const FS_SCL_HCNT: usize = 0x1C;
    pub const FS_SCL_LCNT: usize = 0x20;
    pub const HS_SCL_HCNT: usize = 0x24;
    pub const HS_SCL_LCNT: usize = 0x28;
    pub const RAW_INTR_STAT: usize = 0x34;
    pub const RX_TL: usize = 0x38;
    pub const TX_TL: usize = 0x3C;
    pub const CLR_TX_ABRT: usize = 0x54;
    pub const ENABLE: usize = 0x6C;
    pub const STATUS: usize = 0x70;
    pub const TXFLR: usize = 0x74;
    pub const RXFLR: usize = 0x78;
    pub const SDA_HOLD: usize = 0x7C;
    pub const TX_ABRT_SOURCE: usize = 0x80;
    pub const ENABLE_STATUS: usize = 0x9C;
    pub const COMP_PARAM_1: usize = 0xF4;
    pub const COMP_VERSION: usize = 0xF8;
    pub const COMP_TYPE: usize = 0xFC;
}

/// `IC_CON` bits.
pub mod con {
    pub const MASTER: u32 = 1 << 0;
    /// Speed field, bits 2:1.
    pub const SPEED_STD: u32 = 1 << 1;
    pub const SPEED_FAST: u32 = 2 << 1;
    pub const SPEED_HIGH: u32 = 3 << 1;
    pub const RESTART_EN: u32 = 1 << 5;
    pub const SLAVE_DISABLE: u32 = 1 << 6;
    /// AMD: set by BIOS, recovery performed by SMU firmware — preserve it.
    pub const BUS_CLEAR_CTRL: u32 = 1 << 11;
}

/// `IC_DATA_CMD` bits above the 8 data bits.
pub mod data_cmd {
    /// Issue a read for this position (data bits must be 0).
    pub const READ: u32 = 1 << 8;
    /// Emit a RESTART before this byte (first byte of the second half of a
    /// write-then-read transaction).
    pub const RESTART: u32 = 1 << 10;
    /// Emit a STOP after this byte (the last byte of the transaction; the
    /// controller holds SCL otherwise when `IC_EMPTYFIFO_HOLD_MASTER_EN`).
    pub const STOP: u32 = 1 << 9;
}

/// `IC_RAW_INTR_STAT` / `IC_INTR_STAT` bits a polled driver needs.
pub mod intr {
    pub const RX_FULL: u32 = 1 << 2;
    pub const TX_EMPTY: u32 = 1 << 4;
    pub const TX_ABRT: u32 = 1 << 6;
    pub const STOP_DET: u32 = 1 << 9;
}

/// `IC_STATUS` bits.
pub mod status {
    pub const ACTIVITY: u32 = 1 << 0;
    pub const TFE: u32 = 1 << 2;
    pub const RFNE: u32 = 1 << 3;
    pub const MST_ACTIVITY: u32 = 1 << 5;
}

/// `IC_ENABLE` bits.
pub mod enable {
    pub const ENABLE: u32 = 1 << 0;
    pub const ABORT: u32 = 1 << 1;
}

/// `IC_COMP_TYPE` value: `"DW" + 0x0140`. Reading anything else through a
///
/// supposedly-mapped controller window means the MMIO base is wrong — this
/// is the probe's first gate and the one QEMU can never exercise.
pub const COMP_TYPE_VALUE: u32 = 0x4457_0140;

/// `IC_COMP_VERSION` at/above which `IC_SDA_HOLD` is implemented
/// (Linux: `0x3131312A`, ASCII `"111*"` == v1.11*).
pub const SDA_HOLD_MIN_VERSION: u32 = 0x3131_312A;

/// The bus speeds the IP supports, and the only ones worth naming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusSpeed {
    Standard,
    Fast,
}

/// SCL high/low counter pairs for one speed, in controller counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SclTimings {
    pub hcnt: u16,
    pub lcnt: u16,
}

/// The full per-speed timing set written to `IC_SS/FS_SCL_*` at init.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timings {
    pub ss: SclTimings,
    pub fs: SclTimings,
    /// `IC_SDA_HOLD` value (0 = leave the reset value; the register only
    /// exists from `SDA_HOLD_MIN_VERSION`).
    pub sda_hold: u32,
}

/// Compute SCL counts from the controller input clock — Linux
/// `i2c_dw_scl_hcnt`/`i2c_dw_scl_lcnt` verbatim:
///
/// ```text
/// hcnt = round(ic_clk * (tHIGH + tf)) - 3
/// lcnt = round(ic_clk * (tLOW  + tf)) - 1
/// ```
///
/// with `ic_clk_khz` in kHz and the period arguments in ns, so the product
/// is kHz·ns = counts·10⁶, divided by 10⁶. The `- 3` / `- 1` offsets are the
/// databook's internal-latency corrections, not style.
#[must_use]
pub fn scl_hcnt(ic_clk_khz: u32, t_high_ns: u32, tf_ns: u32) -> u32 {
    let ticks = div_round_closest(
        u64::from(ic_clk_khz) * u64::from(t_high_ns + tf_ns),
        1_000_000,
    );
    (ticks - 3) as u32
}

#[must_use]
pub fn scl_lcnt(ic_clk_khz: u32, t_low_ns: u32, tf_ns: u32) -> u32 {
    let ticks = div_round_closest(
        u64::from(ic_clk_khz) * u64::from(t_low_ns + tf_ns),
        1_000_000,
    );
    (ticks - 1) as u32
}

fn div_round_closest(x: u64, d: u64) -> u64 {
    (x + d / 2) / d
}

/// Timings when the DSDT offers no `SSCN`/`FMCN` methods: compute from the
///
/// input clock with the I²C spec's minimum periods and a 300 ns fall time
/// (Linux's defaults). AMD ACPI controllers almost always ship the methods,
/// which are the accurate source — this is the fallback, and callers should
/// say so in dmesg.
#[must_use]
pub fn timings_from_clock(ic_clk_khz: u32) -> Timings {
    let tf = 300;
    Timings {
        ss: SclTimings {
            hcnt: scl_hcnt(ic_clk_khz, 4_000, tf) as u16,
            lcnt: scl_lcnt(ic_clk_khz, 4_700, tf) as u16,
        },
        fs: SclTimings {
            hcnt: scl_hcnt(ic_clk_khz, 600, tf) as u16,
            lcnt: scl_lcnt(ic_clk_khz, 1_300, tf) as u16,
        },
        sda_hold: 0,
    }
}

/// Build `IC_CON` for master operation at `speed`, preserving BIOS's
///
/// `BUS_CLEAR_CTRL` if firmware set it (AMD SMU owns stuck-bus recovery;
/// clearing the bit forfeits it — Linux `i2c_dw_probe_master` reads `IC_CON`
/// for exactly this bit and nothing else).
#[must_use]
pub fn master_con(speed: BusSpeed, bios_con: u32) -> u32 {
    let speed_bits = match speed {
        BusSpeed::Standard => con::SPEED_STD,
        BusSpeed::Fast => con::SPEED_FAST,
    };
    con::MASTER
        | con::SLAVE_DISABLE
        | con::RESTART_EN
        | speed_bits
        | (bios_con & con::BUS_CLEAR_CTRL)
}

/// The `SSCN`/`FMCN` ACPI method payload: a 3-element package of
///
/// `[hcnt, lcnt, sda_hold]` (Linux `i2c_dw_acpi_params`). `None` when the
/// package is the wrong shape — the caller then falls back to
/// [`timings_from_clock`].
#[must_use]
pub fn parse_acpi_timing_package(pkg: &[u32]) -> Option<(u16, u16, u32)> {
    if pkg.len() != 3 {
        return None;
    }
    Some((pkg[0] as u16, pkg[1] as u16, pkg[2]))
}

/// Apply an ACPI package to the timing set for `speed`, leaving the other
///
/// speed's values (already computed or applied) alone — the same order
/// Linux uses: ACPI counts first, computed counts only where ACPI was
/// silent.
pub fn apply_acpi_package(t: &mut Timings, speed: BusSpeed, pkg: &[u32]) {
    let Some((h, l, hold)) = parse_acpi_timing_package(pkg) else {
        return;
    };
    let slot = match speed {
        BusSpeed::Standard => &mut t.ss,
        BusSpeed::Fast => &mut t.fs,
    };
    slot.hcnt = h;
    slot.lcnt = l;
    t.sda_hold = hold;
}

/// TX and RX FIFO depths from `IC_COMP_PARAM_1` (`[23:16]` and `[15:8]`,
/// encoded as depth−1).
#[must_use]
pub fn fifo_depths(comp_param_1: u32) -> (u32, u32) {
    let tx = ((comp_param_1 >> 16) & 0xFF) + 1;
    let rx = ((comp_param_1 >> 8) & 0xFF) + 1;
    (tx, rx)
}

/// A decoded `TX_ABRT_SOURCE`, with the cause the driver should name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxAbort {
    /// The target address did not ACK — "nothing at this address", the
    /// probe's most useful negative answer.
    AddrNoAck,
    /// Data byte NACKed mid-transfer.
    DataNoAck,
    /// Lost arbitration (multi-master or SCL glitch).
    ArbLost,
    /// Abort requested by software (`IC_ENABLE.ABORT`).
    MasterDis,
    /// Anything else — treat as a bus error and re-init.
    Other(u32),
}

impl TxAbort {
    /// Decode `IC_TX_ABRT_SOURCE`. Read the register *before* clearing it
    /// (reading `IC_CLR_TX_ABRT` is what clears it).
    #[must_use]
    pub fn from_source(src: u32) -> Self {
        const NOACK: u32 = 0b1_0111; // 7b/10b addr, txdata, gcall noack
        if src & NOACK != 0 {
            Self::AddrNoAck
        } else if src & (1 << 12) != 0 {
            Self::ArbLost
        } else if src & (1 << 11) != 0 {
            Self::MasterDis
        } else if src & (1 << 3) != 0 {
            Self::DataNoAck
        } else {
            Self::Other(src)
        }
    }
}

/// The outcome a transaction loop maps errors onto — the vocabulary the
/// kernel glue and the i2c-hid layer share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XferError {
    /// Address NACK: no device answers at this address.
    NoDevice,
    /// The transfer did not finish in its time budget. The controller
    /// should be disabled and re-initialised before the next attempt
    /// (Linux's no-GPIO recovery).
    Timeout,
    /// Bus-level failure (arbitration, abort of another cause). Recover
    /// exactly like [`XferError::Timeout`].
    Bus,
}
