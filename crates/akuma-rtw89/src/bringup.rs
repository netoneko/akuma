//! From a powered-off card to running firmware: Linux's `rtw89_mac_init` up to
//! and including `rtw89_fw_download`, the 8852C arms only.
//!
//! [`bring_up`] is the whole of it. In order:
//!
//! 1. [`power_on`] — `rtw89_mac_pwr_on` / `rtw8852c_pwr_on_func`: wake the MAC,
//!    the analog block over the XTAL serial interface, the DMAC and CMAC.
//! 2. [`pre_init`] — `rtw89_mac_partial_init` up to the download: host DMA on,
//!    the DMAC's download-mode quotas (`dle_init(DLFW)`) and H2C flow control,
//!    then `rtw89_pci_ops_mac_pre_init_ax`: PCIe PHY tweaks, every ring
//!    programmed, every TX channel stopped but CH12.
//! 3. [`disable_cpu`] then [`enable_cpu`] — the firmware CPU reset into
//!    download mode.
//! 4. [`download`] — the header packet, then every section packet, on CH12.
//! 5. wait for `WCPU_FW_CTRL`'s status field to read 7, **init ready**.
//!
//! Every register access goes through [`Bus`]; DMA memory is [`Dma`], plain
//! byte slices the glue hands in with their bus addresses. Nothing allocates.

use core::sync::atomic::{Ordering, fence};

use crate::fw::{self, Image, Source};
use crate::h2c;
use crate::regs as r;
use crate::{Bus, clr8, clr16, clr32, mask16, mask32, poll8, poll32, set8, set32};

/// Bytes of DMA memory per in-flight CH12 packet: descriptor, H2C header and
/// the largest payload, rounded up.
pub const SLOT: usize = 2048;
/// Bytes of a ring: [`r::RING_LEN`] entries of [`h2c::BD_LEN`].
pub const RING_BYTES: usize = r::RING_LEN as usize * h2c::BD_LEN;

/// `FWDL_WAIT_CNT`: the budget, in 1 µs polls, for each download handshake.
const FWDL_WAIT_US: u32 = 400_000;
/// How long a full ring of in-flight packets may take to drain one slot.
const RECLAIM_WAIT_US: u32 = 100_000;

/// `RTW89_FWDL_WCPU_FW_INIT_RDY`, and the failure codes before it.
pub const FWDL_INIT_RDY: u8 = 7;
const FWDL_CHECKSUM_FAIL: u8 = 2;
const FWDL_SECURITY_FAIL: u8 = 3;
const FWDL_CV_NOT_MATCH: u8 = 4;

/// Where the bring-up was when it stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    PowerOn,
    PowerOff,
    DmacPreInit,
    DleInit,
    HfcInit,
    PciPreInit,
    Firmware,
    CpuEnable,
    H2cPathReady,
    HeaderSent,
    FwdlPathReady,
    Data,
    FwReady,
    /// A firmware command after the download.
    H2c,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A register never reached the value a step waits for. `last` is what it
    /// read when the budget ran out — the number to report.
    Poll { stage: Stage, reg: u32, last: u32 },
    /// `MAC_FUNC_EN | DMAC_FUNC_EN` were not on when a DMAC step needed them
    /// (`rtw89_mac_check_mac_en`); `DMAC_FUNC_EN` read this.
    MacOff(u32),
    /// The firmware CPU was already running at [`enable_cpu`].
    CpuRunning,
    /// The firmware file was refused.
    Firmware(fw::Error),
    /// The file read failed partway through the download.
    Read { packet: u32 },
    /// The chip refused the image: checksum (2), security (3), wrong cut (4),
    /// or another status. `FwReady`'s poll error, decoded as Linux does.
    Rejected(u8),
    /// The DMA memory handed in is too small.
    DmaTooSmall,
}

/// The DMA memory the download uses, and its bus addresses.
///
/// Every slice must be the memory its address names, zeroed by the caller where noted; there is
/// no IOMMU in the path on ryzen, so a bus address is a physical one.
pub struct Dma<'a> {
    /// CH12's ring: [`RING_BYTES`], 8-byte aligned.
    pub ring: &'a mut [u8],
    pub ring_phys: u64,
    /// Packet slots, [`SLOT`] bytes each; at least one.
    pub slots: &'a mut [u8],
    pub slots_phys: u64,
    /// One zeroed [`RING_BYTES`] ring every stopped TX channel is pointed at.
    pub idle_phys: u64,
    /// The RX and release-report rings, [`RING_BYTES`] each. Every entry must
    /// already hold a buffer ([`h2c::rx_bd`]), as Linux's do.
    pub rx_phys: [u64; 2],
}

impl Dma<'_> {
    fn slot_count(&self) -> usize {
        self.slots.len() / SLOT
    }
}

/// What a successful bring-up found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// The chip cut, `SYS_CFG1[15:12]`.
    pub cv: u8,
    /// The image used.
    pub image: fw::Entry,
    /// Its version, `(major, minor, sub, idx)`.
    pub version: [u8; 4],
    /// Section packets sent after the header packet.
    pub data_packets: u32,
    /// `WCPU_FW_CTRL` at the end: `0xe2`-like, status 7.
    pub fw_ctrl: u8,
}

/// Progress, for the glue's log: a step name and a value worth printing.
pub trait Log {
    fn step(&mut self, what: &'static str, val: u32);
}

impl<F: FnMut(&'static str, u32)> Log for F {
    fn step(&mut self, what: &'static str, val: u32) {
        self(what, val);
    }
}

fn poll_err(stage: Stage, reg: u32) -> impl FnOnce(u32) -> Error {
    move |last| Error::Poll { stage, reg, last }
}

fn poll8_err(stage: Stage, reg: u32) -> impl FnOnce(u8) -> Error {
    move |last| Error::Poll { stage, reg, last: u32::from(last) }
}

/// The chip cut (`rtw89_read_chip_ver`; the 8852A-only correction left out).
pub fn chip_cut(bus: &mut impl Bus) -> u8 {
    ((bus.read32(r::SYS_CFG1) & r::CHIP_VER_MASK) >> 12) as u8
}

/// `rtw89_mac_write_xtal_si_ax`: one write to the analog block, then wait for
/// the serial interface to finish it.
fn xtal_si(bus: &mut impl Bus, stage: Stage, offset: u8, val: u8, mask: u8) -> Result<(), Error> {
    let cmd = u32::from(offset) | (u32::from(val) << 8) | (u32::from(mask) << 16) | r::XTAL_SI_CMD_POLL;
    bus.write32(r::WLAN_XTAL_SI_CTRL, cmd);
    poll32(bus, r::WLAN_XTAL_SI_CTRL, 50, 50_000, |v| v & r::XTAL_SI_CMD_POLL == 0)
        .map(drop)
        .map_err(poll_err(stage, r::WLAN_XTAL_SI_CTRL))
}

/// `rtw89_mac_pwr_on`: power on; if the MAC says it already is, power off and
/// try once more, as Linux does.
///
/// # Errors
///
/// [`Error::Poll`] from a step that never completed.
pub fn power_on(bus: &mut impl Bus) -> Result<(), Error> {
    if power_switch_on(bus).is_err() {
        power_off(bus)?;
        power_switch_on(bus)?;
    }
    Ok(())
}

/// `rtw89_mac_power_switch(on)`.
fn power_switch_on(bus: &mut impl Bus) -> Result<(), Error> {
    let st = (bus.read32(r::IC_PWR_STATE) & r::WLMAC_PWR_STE_MASK) >> 8;
    if st == r::PWR_ACT {
        return Err(Error::Poll { stage: Stage::PowerOn, reg: r::IC_PWR_STATE, last: st });
    }
    pwr_on_func(bus)?;
    bus.write8(r::SCOREBOARD + 3, r::NOTIFY_TP_MAJOR);
    Ok(())
}

/// `rtw8852c_pwr_on_func`.
fn pwr_on_func(bus: &mut impl Bus) -> Result<(), Error> {
    const S: Stage = Stage::PowerOn;
    let hci = (bus.read32(r::SYS_STATUS1) & r::PAD_HCI_SEL_V2_MASK) >> 3;
    if hci == r::HCI_SEL_PCIE_USB {
        set32(bus, r::LDO_AON_CTRL0, r::PD_REGU_L);
    }
    clr32(bus, r::SYS_PW_CTRL, r::AFSM_WLSUS_EN | r::AFSM_PCIE_SUS_EN);
    set32(bus, r::SYS_PW_CTRL, r::DIS_WLBT_PDNSUSEN_SOPC);
    set32(bus, r::WLLPS_CTRL, r::DIS_WLBT_LPSEN_LOPC);
    clr32(bus, r::SYS_PW_CTRL, r::APDM_HPDN);
    clr32(bus, r::SYS_PW_CTRL, r::APFM_SWLPS);
    mask32(bus, r::SPS_DIG_ON_CTRL0, r::OCP_L1_MASK, 0x7);
    poll32(bus, r::SYS_PW_CTRL, 1000, 20_000, |v| v & r::RDY_SYSPWR != 0)
        .map_err(poll_err(S, r::SYS_PW_CTRL))?;
    set32(bus, r::SYS_PW_CTRL, r::EN_WLON);
    set32(bus, r::SYS_PW_CTRL, r::APFN_ONMAC);
    poll32(bus, r::SYS_PW_CTRL, 1000, 20_000, |v| v & r::APFN_ONMAC == 0)
        .map_err(poll_err(S, r::SYS_PW_CTRL))?;

    let en = r::PLATFORM_EN as u8;
    set8(bus, r::PLATFORM_ENABLE, en);
    clr8(bus, r::PLATFORM_ENABLE, en);
    set8(bus, r::PLATFORM_ENABLE, en);
    clr8(bus, r::PLATFORM_ENABLE, en);
    set8(bus, r::PLATFORM_ENABLE, en);
    clr32(bus, r::SYS_SDIO_CTRL, r::PCIE_CALIB_EN_V1);

    clr32(bus, r::SYS_ISO_CTRL_EXTEND, r::CMAC1_FEN);
    set32(bus, r::SYS_ISO_CTRL_EXTEND, r::R_SYM_ISO_CMAC12PP);
    clr32(bus, r::AFE_CTRL1, r::R_SYM_WLCMAC1_PC_EN_ALL);
    set32(bus, r::SYS_ADIE_PAD_PWR_CTRL, r::SYM_PADPDN_WL_PTA_1P3);
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, r::XTAL_SI_GND_SHDN_WL, r::XTAL_SI_GND_SHDN_WL)?;
    set32(bus, r::SYS_ADIE_PAD_PWR_CTRL, r::SYM_PADPDN_WL_RFC_1P3);
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, r::XTAL_SI_SHDN_WL, r::XTAL_SI_SHDN_WL)?;
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, r::XTAL_SI_OFF_WEI, r::XTAL_SI_OFF_WEI)?;
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, r::XTAL_SI_OFF_EI, r::XTAL_SI_OFF_EI)?;
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, 0, r::XTAL_SI_RFC2RF)?;
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, r::XTAL_SI_PON_WEI, r::XTAL_SI_PON_WEI)?;
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, r::XTAL_SI_PON_EI, r::XTAL_SI_PON_EI)?;
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, 0, r::XTAL_SI_SRAM2RFC)?;
    xtal_si(bus, S, r::XTAL_SI_XTAL_XMD_2, 0x10, r::XTAL_SI_LDO_LPS)?;
    xtal_si(bus, S, r::XTAL_SI_XTAL_XMD_4, 0, r::XTAL_SI_LPS_CAP)?;

    set32(bus, r::PMC_DBG_CTRL2, r::SYSON_DIS_PMCR_AX_WRMSK);
    set32(bus, r::SYS_ISO_CTRL, r::ISO_EB2CORE);
    clr32(bus, r::SYS_ISO_CTRL, r::PWC_EV2EF_B15);
    bus.delay_us(1000);
    clr32(bus, r::SYS_ISO_CTRL, r::PWC_EV2EF_B14);
    clr32(bus, r::PMC_DBG_CTRL2, r::SYSON_DIS_PMCR_AX_WRMSK);
    set32(bus, r::GPIO0_15_EECS_EESK_LED1_PULL_LOW_EN, r::PULL_LOW_EN_ALL);

    set32(bus, r::DMAC_FUNC_EN, r::DMAC_FUNC_EN_ALL);
    set32(bus, r::CMAC_FUNC_EN, r::CMAC_FUNC_EN_ALL);
    mask32(bus, r::LED1_FUNC_SEL, r::PINMUX_EESK_FUNC_SEL_V1_MASK, r::PINMUX_EESK_FUNC_SEL_BT_LOG);
    Ok(())
}

/// `rtw89_mac_power_switch(off)` / `rtw8852c_pwr_off_func`: what a reset
/// should find, and what Linux expects to take over from on the next boot.
///
/// # Errors
///
/// [`Error::Poll`] from a step that never completed.
pub fn power_off(bus: &mut impl Bus) -> Result<(), Error> {
    const S: Stage = Stage::PowerOff;
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, r::XTAL_SI_RFC2RF, r::XTAL_SI_RFC2RF)?;
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, 0, r::XTAL_SI_OFF_EI)?;
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, 0, r::XTAL_SI_OFF_WEI)?;
    xtal_si(bus, S, r::XTAL_SI_WL_RFC_S0, 0, r::XTAL_SI_RF00)?;
    xtal_si(bus, S, r::XTAL_SI_WL_RFC_S1, 0, r::XTAL_SI_RF10)?;
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, r::XTAL_SI_SRAM2RFC, r::XTAL_SI_SRAM2RFC)?;
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, 0, r::XTAL_SI_PON_EI)?;
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, 0, r::XTAL_SI_PON_WEI)?;

    set32(bus, r::SYS_PW_CTRL, r::EN_WLON);
    clr32(bus, r::WLRF_CTRL, r::AFC_AFEDIG);
    clr8(bus, r::SYS_FUNC_EN, r::FEN_BB_GLB_RSTN | r::FEN_BBRSTB);
    clr32(bus, r::SYS_ISO_CTRL_EXTEND, r::R_SYM_FEN_WLBBGLB_1 | r::R_SYM_FEN_WLBBFUN_1);
    clr32(bus, r::SYS_ADIE_PAD_PWR_CTRL, r::SYM_PADPDN_WL_RFC_1P3);
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, 0, r::XTAL_SI_SHDN_WL)?;
    clr32(bus, r::SYS_ADIE_PAD_PWR_CTRL, r::SYM_PADPDN_WL_PTA_1P3);
    xtal_si(bus, S, r::XTAL_SI_ANAPAR_WL, 0, r::XTAL_SI_GND_SHDN_WL)?;

    set32(bus, r::SYS_PW_CTRL, r::APFM_OFFMAC);
    poll32(bus, r::SYS_PW_CTRL, 1000, 20_000, |v| v & r::APFM_OFFMAC == 0)
        .map_err(poll_err(S, r::SYS_PW_CTRL))?;
    bus.write32(r::WLLPS_CTRL, r::SW_LPS_OPTION);
    set32(bus, r::SYS_PW_CTRL, r::XTAL_OFF_A_DIE);
    set32(bus, r::SYS_SWR_CTRL1, r::SYM_CTRL_SPS_PWMFREQ);
    mask32(bus, r::SPS_DIG_ON_CTRL0, r::REG_ZCDC_H_MASK, 0x3);
    set32(bus, r::SYS_PW_CTRL, r::APFM_SWLPS);
    bus.write8(r::SCOREBOARD + 3, r::NOTIFY_PWR_MAJOR);
    Ok(())
}

/// `rtw89_mac_check_mac_en_ax(.., RTW89_DMAC_SEL)`.
fn check_dmac_en(bus: &mut impl Bus) -> Result<(), Error> {
    let v = bus.read32(r::DMAC_FUNC_EN);
    if v == 0xffff_ffff || v == 0xdead_beef || v & r::MAC_DMAC_FUNC_EN != r::MAC_DMAC_FUNC_EN {
        return Err(Error::MacOff(v));
    }
    Ok(())
}

/// The DMAC's download-mode memory layout (`dle_init(RTW89_QTA_DLFW, ...)`'s
/// `dle_mix_cfg` and quota words) for the 8852C on PCIe, as the W0 trace
/// shows them: `(WDE_PKTBUF_CFG fields, PLE_PKTBUF_CFG fields)` and the quota
/// registers in Linux's write order. WDE quota 2 is not written.
const DLFW_WDE_PKTBUF: u32 = 0x0000_0000;
const DLFW_PLE_PKTBUF: u32 = 0x09f0_1001;
const DLFW_WDE_QUOTA: [(u32, u32); 4] = [
    (r::WDE_QTA0_CFG, 0x0000_0000),
    (r::WDE_QTA0_CFG + 4, 0x0000_003c),
    (r::WDE_QTA0_CFG + 12, 0x0000_0000),
    (r::WDE_QTA0_CFG + 16, 0x0000_0000),
];
const DLFW_PLE_QUOTA: [u32; 12] = [0, 0, 0x0020_0010, 0x0100_0100, 0, 0, 0, 0, 0, 0, 0, 0];

/// `rtw89_mac_partial_init` up to the firmware download.
///
/// # Errors
///
/// [`Error::MacOff`] if power-on did not leave the DMAC enabled, or
/// [`Error::Poll`] from a step that never completed.
pub fn pre_init(bus: &mut impl Bus, dma: &Dma<'_>) -> Result<(), Error> {
    // rtw89_mac_ctrl_hci_dma_trx(true)
    set32(bus, r::HCI_FUNC_EN_V1, r::HCI_TXDMA_EN | r::HCI_RXDMA_EN);

    // rtw89_mac_dmac_pre_init: hci_func_en, dmac_func_pre_en
    bus.write32(r::DMAC_FUNC_EN, r::DMAC_FUNC_EN_HCI);
    bus.write32(r::DMAC_CLK_EN, r::DISPATCHER_CLK_EN);
    let v = bus.read32(r::HAXI_INIT_CFG1);
    let v = (v & !(r::DMA_MODE_MASK | r::STOP_AXI_MST)) | r::TXHCI_EN_V1 | r::RXHCI_EN_V1;
    bus.write32(r::HAXI_INIT_CFG1, v); // DMA_MODE = DMA_MOD_PCIE_1B (0)
    clr32(bus, r::HAXI_DMA_STOP1, r::DMA_STOP1_TX_ALL);
    clr32(bus, r::HAXI_DMA_STOP2, r::DMA_STOP2_TX_ALL);
    set32(bus, r::PLATFORM_ENABLE, r::AXIDMA_EN);

    dle_init_dlfw(bus)?;
    hfc_init_h2c_only(bus)?;
    pci_mac_pre_init(bus, dma)
}

/// `rtw89_mac_dle_init(RTW89_QTA_DLFW, ...)`.
fn dle_init_dlfw(bus: &mut impl Bus) -> Result<(), Error> {
    check_dmac_en(bus)?;
    clr32(bus, r::DMAC_FUNC_EN, r::DLE_EN); // dle_func_en(false)
    set32(bus, r::DMAC_CLK_EN, r::DLE_CLK_EN); // dle_clk_en(true)
    // dle_mix_cfg
    let v = bus.read32(r::WDE_PKTBUF_CFG);
    bus.write32(r::WDE_PKTBUF_CFG, (v & !r::PKTBUF_CFG_MASK) | DLFW_WDE_PKTBUF);
    let v = bus.read32(r::PLE_PKTBUF_CFG);
    bus.write32(r::PLE_PKTBUF_CFG, (v & !r::PKTBUF_CFG_MASK) | DLFW_PLE_PKTBUF);
    // dle_quota_cfg
    for (reg, val) in DLFW_WDE_QUOTA {
        bus.write32(reg, val);
    }
    for (i, val) in DLFW_PLE_QUOTA.iter().enumerate() {
        bus.write32(r::PLE_QTA0_CFG + 4 * i as u32, *val);
    }
    set32(bus, r::DMAC_FUNC_EN, r::DLE_EN); // dle_func_en(true)
    for reg in [r::WDE_INI_STATUS, r::PLE_INI_STATUS] {
        poll32(bus, reg, 1, 2000, |v| v & r::DLE_INI_RDY == r::DLE_INI_RDY)
            .map_err(poll_err(Stage::DleInit, reg))?;
    }
    Ok(())
}

/// `rtw89_mac_hfc_init(reset = true, en = false, h2c_en = true)`: flow control
/// for CH12 only, which is all a download needs.
fn hfc_init_h2c_only(bus: &mut impl Bus) -> Result<(), Error> {
    check_dmac_en(bus)?;
    clr32(bus, r::HCI_FC_CTRL_V1, r::HCI_FC_EN | r::HCI_FC_CH12_EN); // hfc_func_en(false, false)
    // hfc_h2c_cfg
    bus.write32(r::CH_PAGE_CTRL_V1, r::H2C_PREC << r::PREC_PAGE_CH12_MASK.trailing_zeros());
    mask32(bus, r::HCI_FC_CTRL_V1, r::HCI_FC_CH12_FULL_COND_MASK, r::H2C_FULL_COND);
    // hfc_func_en(false, true)
    let v = bus.read32(r::HCI_FC_CTRL_V1);
    bus.write32(r::HCI_FC_CTRL_V1, (v & !r::HCI_FC_EN) | r::HCI_FC_CH12_EN);
    Ok(())
}

/// `rtw89_pci_ops_mac_pre_init_ax`, 8852C arms only: the calls that are
/// no-ops on this chip (`ber`, `rxdma_prefth`, `l1off_pwroff`,
/// `l2_rxen_lat`, `aphy_pwrcut`, `dphy_delay`, `autok_x`,
/// `auto_refclk_cal`, `set_sic`, `set_lbc`, `set_dbg`, `set_keep_reg`, and on
/// cut 1 `l12_vmain` and `gen2_force_ib`) are left out.
fn pci_mac_pre_init(bus: &mut impl Bus, dma: &Dma<'_>) -> Result<(), Error> {
    const S: Stage = Stage::PciPreInit;
    // deglitch_setting
    clr16(bus, r::RAC_ANA24_G1, r::DEGLITCH);
    clr16(bus, r::RAC_ANA24_G2, r::DEGLITCH);
    // hci_ldo
    clr32(bus, r::SYS_SDIO_CTRL, r::PCIE_DIS_L2_CTRL_LDO_HCI);
    // power_wake(true)
    set32(bus, r::HCI_OPT_CTRL, r::BIT_WAKE_CTRL);
    // autoload_hang
    set32(bus, r::PCIE_BG_CLR, r::BG_CLR_ASYNC_M3);
    clr32(bus, r::PCIE_BG_CLR, r::BG_CLR_ASYNC_M3);
    // l1_ent_lat, wd_exit_l1
    clr32(bus, r::PCIE_PS_CTRL_V1, r::SEL_REQ_ENTR_L1);
    set32(bus, r::PCIE_PS_CTRL_V1, r::DMAC0_EXIT_L1_EN);
    // set_io_rcy (enabled on the 8852C)
    bus.write32(r::PCIE_WDT_TIMER_M1, r::PCIE_IO_RCY_TIMER);
    bus.write32(r::PCIE_WDT_TIMER_M2, r::PCIE_IO_RCY_TIMER);
    bus.write32(r::PCIE_WDT_TIMER_E0, r::PCIE_IO_RCY_TIMER);
    set32(bus, r::PCIE_IO_RCY_M1, r::PCIE_IO_RCY_WDT_MODE);
    set32(bus, r::PCIE_IO_RCY_M2, r::PCIE_IO_RCY_WDT_MODE);
    set32(bus, r::PCIE_IO_RCY_E0, r::PCIE_IO_RCY_WDT_MODE);
    clr32(bus, r::PCIE_IO_RCY_S1, r::PCIE_IO_RCY_WDT_MODE);

    set32(bus, r::HAXI_DMA_STOP1, r::STOP_WPDMA);
    ctrl_dma_all(bus, false);
    poll32(bus, r::HAXI_DMA_BUSY1, 10, 100, |v| v & r::DMA_BUSY1_CHECK == 0)
        .map_err(poll_err(S, r::HAXI_DMA_BUSY1))?;
    poll32(bus, r::HAXI_DMA_BUSY2, 10, 100, |v| v & r::DMA_BUSY2_CHECK == 0)
        .map_err(poll_err(S, r::HAXI_DMA_BUSY2))?;
    poll32(bus, r::HAXI_DMA_BUSY3, 10, 100, |v| v & r::DMA_BUSY3_CHECK == 0)
        .map_err(poll_err(S, r::HAXI_DMA_BUSY3))?;

    // clr_idx_all
    set32(bus, r::TXBD_RWPTR_CLR1, r::CLR_IDX1_ALL);
    set32(bus, r::TXBD_RWPTR_CLR2, r::CLR_IDX2_ALL);
    set32(bus, r::RXBD_RWPTR_CLR, r::CLR_RX_IDX_ALL);

    // mode_op: RX BDs in packet mode, burst sizes, tags, WD intervals, and
    // truncated BDs (8-byte host address info).
    clr32(bus, r::HAXI_INIT_CFG1, r::RXBD_MODE_V1);
    mask32(bus, r::HAXI_INIT_CFG1, r::HAXI_MAX_TXDMA_MASK, r::TX_BURST);
    mask32(bus, r::HAXI_INIT_CFG1, r::HAXI_MAX_RXDMA_MASK, r::RX_BURST);
    mask32(bus, r::HAXI_EXP_CTRL, r::MAX_TAG_NUM_MASK, r::MULTI_TAG_NUM);
    mask32(bus, r::HAXI_INIT_CFG1, r::WD_ITVL_IDLE_V1_MASK, r::WD_DMA_INTVL);
    mask32(bus, r::HAXI_INIT_CFG1, r::WD_ITVL_ACT_V1_MASK, r::WD_DMA_INTVL);
    set32(bus, r::TX_ADDRESS_INFO_MODE_SETTING, r::HOST_ADDR_INFO_8B_SEL);
    clr32(bus, r::PKTIN_SETTING, r::WD_ADDR_INFO_LENGTH);

    // ops_reset -> reset_trx_rings
    for (i, ring) in r::TX_RINGS.iter().enumerate() {
        let base = if i == r::TX_RINGS.len() - 1 { dma.ring_phys } else { dma.idle_phys };
        bus.write16(ring.num, r::RING_LEN);
        bus.write32(ring.bdram, ring.bdram_val);
        bus.write32(ring.desa_l, base as u32);
        bus.write32(ring.desa_l + 4, (base >> 32) as u32);
    }
    for (&(num, desa_l), base) in r::RX_RINGS.iter().zip(dma.rx_phys) {
        bus.write16(num, r::RING_LEN);
        bus.write32(desa_l, base as u32);
        bus.write32(desa_l + 4, (base >> 32) as u32);
    }
    // init_wp_16sel
    for i in (0..16u32).step_by(4) {
        bus.write32(r::WP_ADDR_H_SEL0_3 + i, i | ((i + 1) << 8) | ((i + 2) << 16) | ((i + 3) << 24));
    }

    // rst_bdram
    set32(bus, r::HAXI_INIT_CFG1, r::RST_BDRAM);
    poll32(bus, r::HAXI_INIT_CFG1, 1, 1000, |v| v & r::RST_BDRAM == 0)
        .map_err(poll_err(S, r::HAXI_INIT_CFG1))?;

    // Every TX channel stopped, then CH12 alone let go; DMA back on.
    set32(bus, r::HAXI_DMA_STOP1, r::DMA_STOP1_TX_ALL);
    set32(bus, r::HAXI_DMA_STOP2, r::DMA_STOP2_TX_ALL);
    clr32(bus, r::HAXI_DMA_STOP1, r::STOP_CH12);
    ctrl_dma_all(bus, true);
    Ok(())
}

/// `rtw89_pci_ctrl_dma_all`: the AXI master stop bit, then the TX/RX engines.
fn ctrl_dma_all(bus: &mut impl Bus, enable: bool) {
    if enable {
        clr32(bus, r::HAXI_INIT_CFG1, r::STOP_AXI_MST);
        set32(bus, r::HAXI_INIT_CFG1, r::TXHCI_EN_V1 | r::RXHCI_EN_V1);
    } else {
        set32(bus, r::HAXI_INIT_CFG1, r::STOP_AXI_MST);
        clr32(bus, r::HAXI_INIT_CFG1, r::TXHCI_EN_V1 | r::RXHCI_EN_V1);
    }
}

/// Stop host DMA the way Linux's teardown does before a power-off.
pub fn stop_dma(bus: &mut impl Bus) {
    set32(bus, r::HAXI_DMA_STOP1, r::STOP_WPDMA | r::DMA_STOP1_TX_ALL);
    set32(bus, r::HAXI_DMA_STOP2, r::DMA_STOP2_TX_ALL);
    ctrl_dma_all(bus, false);
}

/// `rtw89_mac_disable_cpu_ax`, with `rtw89_disable_fw_watchdog`'s two writes
/// into the firmware CPU's local memory through the indirect window.
pub fn disable_cpu(bus: &mut impl Bus) {
    clr32(bus, r::PLATFORM_ENABLE, r::WCPU_EN);
    let fw = u32::from(r::WCPU_FWDL_EN | r::H2C_PATH_RDY | r::FWDL_PATH_RDY);
    clr32(bus, r::WCPU_FW_CTRL, fw);
    clr32(bus, r::SYS_CLK_CTRL, r::CPU_CLK_EN);

    bus.write32(r::FILTER_MODEL_ADDR, r::CPU_LOCAL_WDT_CTRL);
    bus.write32(r::INDIR_ACCESS_ENTRY, r::WDT_CTRL_ALL_DIS);
    bus.write32(r::FILTER_MODEL_ADDR, r::CPU_LOCAL_WDT_STATUS);
    let st = bus.read32(r::INDIR_ACCESS_ENTRY);
    bus.write32(r::FILTER_MODEL_ADDR, r::CPU_LOCAL_WDT_STATUS);
    bus.write32(r::INDIR_ACCESS_ENTRY, (st | r::FS_WDT_INT) & !r::FS_WDT_INT_MSK);

    clr32(bus, r::PLATFORM_ENABLE, r::PLATFORM_EN);
    set32(bus, r::PLATFORM_ENABLE, r::PLATFORM_EN);
}

/// `rtw89_mac_enable_cpu_ax(boot_reason = 0, dlfw = true)`: start the firmware
/// CPU in download mode.
///
/// # Errors
///
/// [`Error::CpuRunning`] if it was never stopped.
pub fn enable_cpu(bus: &mut impl Bus) -> Result<(), Error> {
    if bus.read32(r::PLATFORM_ENABLE) & r::WCPU_EN != 0 {
        return Err(Error::CpuRunning);
    }
    for reg in [r::UDM1, r::UDM2, r::HALT_H2C_CTRL, r::HALT_C2H_CTRL, r::HALT_H2C, r::HALT_C2H] {
        bus.write32(reg, 0);
    }
    set32(bus, r::SYS_CLK_CTRL, r::CPU_CLK_EN);
    let fw = u32::from(r::WCPU_FWDL_EN | r::H2C_PATH_RDY | r::FWDL_PATH_RDY);
    let v = bus.read32(r::WCPU_FW_CTRL) & !(fw | r::WCPU_FWDL_STS_MASK);
    bus.write32(r::WCPU_FW_CTRL, v | u32::from(r::WCPU_FWDL_EN));
    mask16(bus, r::BOOT_REASON, r::BOOT_REASON_MASK, 0);
    set32(bus, r::PLATFORM_ENABLE, r::WCPU_EN);
    Ok(())
}

/// The status field of `WCPU_FW_CTRL` (`fwdl_get_status`).
#[must_use]
pub const fn fwdl_status(fw_ctrl: u8) -> u8 {
    fw_ctrl >> r::WCPU_FWDL_STS_SHIFT
}

/// CH12's host-side state: Linux's `bd_ring.wp`, and a count of packets sent
/// (which picks the slot).
///
/// How many packets the chip still owes is `wp - hw` modulo the ring, from the
/// two indices, as Linux's `rtw89_pci_dma_recalc` computes it — never a
/// running count of what was reclaimed, which a chip reporting an index it
/// has not been given would push past what was sent. An index like that reads
/// as a full ring, and the wait for a free slot ends in [`Error::Poll`] with
/// the register's value.
pub struct Ch12 {
    wp: u16,
    submitted: u32,
}

impl Default for Ch12 {
    fn default() -> Self {
        Self::new()
    }
}

impl Ch12 {
    #[must_use]
    pub const fn new() -> Self {
        Self { wp: 0, submitted: 0 }
    }

    /// Packets sent so far, download included.
    #[must_use]
    pub const fn sent(&self) -> u32 {
        self.submitted
    }

    /// Send one firmware command after the download: `cmd` is the H2C
    /// header and body, as `rtw89_h2c_tx` hands it to the PCI layer.
    ///
    /// # Errors
    ///
    /// [`Error::Poll`] if no slot came free, [`Error::DmaTooSmall`] if `cmd`
    /// does not fit one.
    pub fn send_h2c(&mut self, bus: &mut impl Bus, dma: &mut Dma<'_>, cmd: &[u8]) -> Result<(), Error> {
        if cmd.len() + h2c::DESC_LEN > SLOT {
            return Err(Error::DmaTooSmall);
        }
        self.send(bus, dma, Stage::H2c, h2c::TYPE_H2C, |m| {
            m[..cmd.len()].copy_from_slice(cmd);
            Some(cmd.len())
        })
        .map(drop)
    }

    /// Read the index register: `(raw value, packets the chip has not fetched)`.
    fn in_flight(&self, bus: &mut impl Bus) -> (u32, u16) {
        let reg = bus.read32(r::CH12_TXBD_IDX);
        let hw = h2c::hw_idx(reg) % r::RING_LEN;
        (reg, (self.wp + r::RING_LEN - hw) % r::RING_LEN)
    }

    /// Queue one packet built by `fill` (which gets the slot's bytes after
    /// the descriptor and returns the payload length) and kick the chip.
    fn send(
        &mut self,
        bus: &mut impl Bus,
        dma: &mut Dma<'_>,
        stage: Stage,
        ty: u32,
        fill: impl FnOnce(&mut [u8]) -> Option<usize>,
    ) -> Result<bool, Error> {
        // Fewer slots than ring entries, so the slot count is the bound.
        let slots = dma.slot_count().min(usize::from(r::RING_LEN) - 1);
        let (mut reg, mut owed) = self.in_flight(bus);
        let mut waited = 0;
        while usize::from(owed) >= slots {
            if waited >= RECLAIM_WAIT_US {
                return Err(Error::Poll { stage, reg: r::CH12_TXBD_IDX, last: reg });
            }
            bus.delay_us(1);
            waited += 1;
            (reg, owed) = self.in_flight(bus);
        }
        let slot = (self.submitted as usize) % slots;
        let mem = &mut dma.slots[slot * SLOT..(slot + 1) * SLOT];
        let Some(payload) = fill(&mut mem[h2c::DESC_LEN..]) else {
            return Ok(false);
        };
        mem[..h2c::DESC_LEN].copy_from_slice(&h2c::desc(payload, ty));
        let addr = dma.slots_phys + (slot * SLOT) as u64;
        let at = usize::from(self.wp) * h2c::BD_LEN;
        dma.ring[at..at + h2c::BD_LEN].copy_from_slice(&h2c::bd(addr, h2c::DESC_LEN + payload));
        // The descriptor, payload and ring entry must all be in memory before
        // the index write tells the chip to fetch them.
        fence(Ordering::Release);
        self.wp = (self.wp + 1) % r::RING_LEN;
        bus.write16(r::CH12_TXBD_IDX, self.wp);
        self.submitted += 1;
        Ok(true)
    }
}

/// `rtw89_fw_download_suit` for one image, after [`enable_cpu`]: the header
/// packet, the path-ready handshakes, every section packet. Returns the number
/// of section packets sent.
///
/// # Errors
///
/// [`Error::Poll`] from a handshake that never came, [`Error::Read`] if the
/// file failed partway, [`Error::DmaTooSmall`].
pub fn download(
    bus: &mut impl Bus,
    dma: &mut Dma<'_>,
    ring: &mut Ch12,
    src: &mut impl Source,
    img: &Image,
    log: &mut impl Log,
) -> Result<u32, Error> {
    if dma.slot_count() == 0 || dma.ring.len() < RING_BYTES {
        return Err(Error::DmaTooSmall);
    }

    // fwdl_check_path_ready(h2c), then the header (__rtw89_fw_download_hdr).
    let v = poll8(bus, r::WCPU_FW_CTRL, 1, FWDL_WAIT_US, |v| v & r::H2C_PATH_RDY != 0)
        .map_err(poll8_err(Stage::H2cPathReady, r::WCPU_FW_CTRL))?;
    log.step("h2c path ready, FW_CTRL", u32::from(v));
    let body = img.header_payload();
    let hdr = h2c::h2c_header(h2c::CAT_MAC, h2c::CL_MAC_FWDL, h2c::FUNC_MAC_FWHDR_DL, 0, body.len());
    ring.send(bus, dma, Stage::HeaderSent, h2c::TYPE_H2C, |m| {
        m[..h2c::HDR_LEN].copy_from_slice(&hdr);
        m[h2c::HDR_LEN..h2c::HDR_LEN + body.len()].copy_from_slice(body);
        Some(h2c::HDR_LEN + body.len())
    })?;

    // fwdl_check_path_ready(fwdl): the firmware accepted the header.
    let v = poll8(bus, r::WCPU_FW_CTRL, 1, FWDL_WAIT_US, |v| v & r::FWDL_PATH_RDY != 0)
        .map_err(poll8_err(Stage::FwdlPathReady, r::WCPU_FW_CTRL))?;
    log.step("header accepted, FW_CTRL", u32::from(v));
    bus.write32(r::HALT_H2C_CTRL, 0);
    bus.write32(r::HALT_C2H_CTRL, 0);

    // __rtw89_fw_download_main, every section.
    let mut sent = 0;
    for (off, len) in img.packets() {
        let ok = ring.send(bus, dma, Stage::Data, h2c::TYPE_FWDL, |m| {
            src.read_at(off, &mut m[..len as usize]).then_some(len as usize)
        })?;
        if !ok {
            return Err(Error::Read { packet: sent });
        }
        sent += 1;
    }
    log.step("section packets sent", sent);
    Ok(sent)
}

/// `rtw89_fw_check_rdy(RTW89_FWDL_CHECK_FREERTOS_DONE)`, after the 5 ms
/// Linux waits first.
///
/// # Errors
///
/// [`Error::Rejected`] with the status the chip settled on.
pub fn wait_fw_ready(bus: &mut impl Bus) -> Result<u8, Error> {
    bus.delay_us(5000);
    poll8(bus, r::WCPU_FW_CTRL, 1, FWDL_WAIT_US, |v| fwdl_status(v) == FWDL_INIT_RDY)
        .map_err(|v| Error::Rejected(fwdl_status(v)))
}

/// Human words for [`Error::Rejected`]'s status, as Linux prints them.
#[must_use]
pub const fn rejected_reason(status: u8) -> &'static str {
    match status {
        FWDL_CHECKSUM_FAIL => "fw checksum fail",
        FWDL_SECURITY_FAIL => "fw security fail",
        FWDL_CV_NOT_MATCH => "fw cv not match",
        _ => "fw unexpected status",
    }
}

/// The whole of stage W1: power on, pre-init, pick the image for this cut,
/// reset the firmware CPU into download mode, download, wait for ready.
///
/// # Errors
///
/// Any [`Error`]; the card is left as it was when the step failed, and the
/// caller should [`shutdown`] it.
pub fn bring_up(
    bus: &mut impl Bus,
    dma: &mut Dma<'_>,
    ring: &mut Ch12,
    src: &mut impl Source,
    log: &mut impl Log,
) -> Result<Report, Error> {
    let cv = chip_cut(bus);
    log.step("chip cut", u32::from(cv));
    let container = fw::Container::read(src).map_err(Error::Firmware)?;
    let entry = container.select(fw::TYPE_NORMAL, cv, src.len()).map_err(Error::Firmware)?;
    let img = Image::read(src, entry).map_err(Error::Firmware)?;
    log.step("image offset", entry.shift);
    log.step("image bytes", entry.size);

    // No reads beyond Linux's own here: the golden replay holds the bring-up
    // to exactly the trace's accesses.
    power_on(bus)?;
    log.step("powered on", 0);
    pre_init(bus, dma)?;
    log.step("download-mode DMA ready", 0);
    disable_cpu(bus);
    enable_cpu(bus)?;
    let data_packets = download(bus, dma, ring, src, &img, log)?;
    let fw_ctrl = wait_fw_ready(bus)?;
    Ok(Report { cv, image: entry, version: img.version, data_packets, fw_ctrl })
}

/// Leave the card quiet: firmware CPU stopped, host DMA stopped, MAC powered
/// off.
///
/// Run before a reset — a bus master left pointing at this kernel's
/// memory is what crash-looped the xHCI box (`AKUMA_AMD64_XHCI_WEDGED_BOX`).
///
/// # Errors
///
/// [`Error::Poll`] if the power-off handshake did not complete; DMA is
/// stopped regardless.
pub fn shutdown(bus: &mut impl Bus) -> Result<(), Error> {
    disable_cpu(bus);
    stop_dma(bus);
    power_off(bus)
}
