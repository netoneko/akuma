//! Register offsets (into BAR2) and bits, named as Linux's `reg.h`/`pci.h`
//! name them, so a line here can be grepped for there. Only what the 8852C
//! download path touches.
//!
//! Where a value is a multi-bit constant Linux builds from a dozen named bits
//! (`DMAC_FUNC_EN_ALL`, the quota words), it is given as the number the W0
//! trace shows the driver writing, with the names in its doc comment; the
//! golden replay test is what keeps the two honest.

// ---- system / power (0x0000-0x03ff) --------------------------------------

pub const SYS_ISO_CTRL: u32 = 0x0000;
pub const ISO_EB2CORE: u32 = 1 << 8;
pub const PWC_EV2EF_B14: u32 = 1 << 14;
pub const PWC_EV2EF_B15: u32 = 1 << 15;

pub const SYS_FUNC_EN: u32 = 0x0002;
pub const FEN_BBRSTB: u8 = 1 << 0;
pub const FEN_BB_GLB_RSTN: u8 = 1 << 1;

pub const SYS_PW_CTRL: u32 = 0x0004;
pub const APFN_ONMAC: u32 = 1 << 8;
pub const APFM_OFFMAC: u32 = 1 << 9;
pub const APFM_SWLPS: u32 = 1 << 10;
pub const AFSM_WLSUS_EN: u32 = 1 << 11;
pub const AFSM_PCIE_SUS_EN: u32 = 1 << 12;
pub const APDM_HPDN: u32 = 1 << 15;
pub const EN_WLON: u32 = 1 << 16;
pub const RDY_SYSPWR: u32 = 1 << 17;
pub const DIS_WLBT_PDNSUSEN_SOPC: u32 = 1 << 18;
pub const XTAL_OFF_A_DIE: u32 = 1 << 22;

pub const SYS_CLK_CTRL: u32 = 0x0008;
pub const CPU_CLK_EN: u32 = 1 << 14;

pub const SYS_SWR_CTRL1: u32 = 0x0010;
pub const SYM_CTRL_SPS_PWMFREQ: u32 = 1 << 10;

pub const SYS_ADIE_PAD_PWR_CTRL: u32 = 0x0018;
pub const SYM_PADPDN_WL_RFC_1P3: u32 = 1 << 5;
pub const SYM_PADPDN_WL_PTA_1P3: u32 = 1 << 6;

pub const AFE_CTRL1: u32 = 0x0024;
/// `R_SYM_WLCMAC1_{PC,P1..P4}_PC_EN`: CMAC1's power cuts.
pub const R_SYM_WLCMAC1_PC_EN_ALL: u32 = 0x1f;

pub const SYS_SDIO_CTRL: u32 = 0x0070;
pub const PCIE_CALIB_EN_V1: u32 = 1 << 12;
pub const PCIE_DIS_L2_CTRL_LDO_HCI: u32 = 1 << 15;

pub const HCI_OPT_CTRL: u32 = 0x0074;
pub const BIT_WAKE_CTRL: u32 = 1 << 5;

pub const SYS_ISO_CTRL_EXTEND: u32 = 0x0080;
pub const R_SYM_ISO_CMAC12PP: u32 = 1 << 5;
pub const R_SYM_FEN_WLBBFUN_1: u32 = 1 << 16;
pub const R_SYM_FEN_WLBBGLB_1: u32 = 1 << 17;
pub const CMAC1_FEN: u32 = 1 << 30;

pub const PLATFORM_ENABLE: u32 = 0x0088;
pub const PLATFORM_EN: u32 = 1 << 0;
pub const WCPU_EN: u32 = 1 << 1;
pub const AXIDMA_EN: u32 = 1 << 3;

pub const WLLPS_CTRL: u32 = 0x0090;
pub const DIS_WLBT_LPSEN_LOPC: u32 = 1 << 1;
/// Linux's `SW_LPS_OPTION`, written whole at power-off.
pub const SW_LPS_OPTION: u32 = 0x0001_a0b2;

pub const SCOREBOARD: u32 = 0x00ac;
/// `MAC_AX_NOTIFY_TP_MAJOR`, to `SCOREBOARD + 3` after power-on.
pub const NOTIFY_TP_MAJOR: u8 = 0x81;
/// `MAC_AX_NOTIFY_PWR_MAJOR`, to `SCOREBOARD + 3` after power-off.
pub const NOTIFY_PWR_MAJOR: u8 = 0x80;

pub const PMC_DBG_CTRL2: u32 = 0x00cc;
pub const SYSON_DIS_PMCR_AX_WRMSK: u32 = 1 << 2;

pub const SYS_CFG1: u32 = 0x00f0;
/// `B_AX_CHIP_VER_MASK`: the chip cut (CV). ryzen's reads 1 (CBV).
pub const CHIP_VER_MASK: u32 = 0xf << 12;

pub const SYS_STATUS1: u32 = 0x00f4;
pub const PAD_HCI_SEL_V2_MASK: u32 = 0x7 << 3;
/// `MAC_AX_HCI_SEL_PCIE_USB`.
pub const HCI_SEL_PCIE_USB: u32 = 3;

pub const HALT_H2C_CTRL: u32 = 0x0160;
pub const HALT_C2H_CTRL: u32 = 0x0164;
pub const HALT_H2C: u32 = 0x0168;
pub const HALT_C2H: u32 = 0x016c;

pub const WCPU_FW_CTRL: u32 = 0x01e0;
pub const WCPU_FWDL_EN: u8 = 1 << 0;
pub const H2C_PATH_RDY: u8 = 1 << 1;
pub const FWDL_PATH_RDY: u8 = 1 << 2;
pub const WCPU_FWDL_STS_SHIFT: u32 = 5;
pub const WCPU_FWDL_STS_MASK: u32 = 0x7 << 5;

pub const BOOT_REASON: u32 = 0x01e6;
pub const BOOT_REASON_MASK: u16 = 0x7;

pub const UDM1: u32 = 0x01f4;
pub const UDM2: u32 = 0x01f8;

pub const SPS_DIG_ON_CTRL0: u32 = 0x0200;
pub const OCP_L1_MASK: u32 = 0x7 << 13;
pub const REG_ZCDC_H_MASK: u32 = 0x3 << 17;

pub const LDO_AON_CTRL0: u32 = 0x0218;
pub const PD_REGU_L: u32 = 1 << 16;

pub const WLAN_XTAL_SI_CTRL: u32 = 0x0270;
pub const XTAL_SI_CMD_POLL: u32 = 1 << 31;

pub const LED1_FUNC_SEL: u32 = 0x02dc;
pub const PINMUX_EESK_FUNC_SEL_V1_MASK: u32 = 0xf << 24;
/// `PINMUX_EESK_FUNC_SEL_BT_LOG`.
pub const PINMUX_EESK_FUNC_SEL_BT_LOG: u32 = 1;

pub const GPIO0_15_EECS_EESK_LED1_PULL_LOW_EN: u32 = 0x02e4;
/// `EECS_PULL_LOW_EN | EESK_PULL_LOW_EN | LED1_PULL_LOW_EN`.
pub const PULL_LOW_EN_ALL: u32 = 0x7 << 16;

pub const WLRF_CTRL: u32 = 0x02f0;
pub const AFC_AFEDIG: u32 = 1 << 17;

pub const IC_PWR_STATE: u32 = 0x03f0;
pub const WLMAC_PWR_STE_MASK: u32 = 0x3 << 8;
/// `PWR_ACT`: the MAC is already powered on.
pub const PWR_ACT: u32 = 1;

// The analog block behind `WLAN_XTAL_SI_CTRL` (Linux `enum xtal_si_offset`).
pub const XTAL_SI_XTAL_XMD_2: u8 = 0x24;
pub const XTAL_SI_XTAL_XMD_4: u8 = 0x26;
pub const XTAL_SI_WL_RFC_S0: u8 = 0x80;
pub const XTAL_SI_WL_RFC_S1: u8 = 0x81;
pub const XTAL_SI_ANAPAR_WL: u8 = 0x90;

pub const XTAL_SI_PON_WEI: u8 = 1 << 0;
pub const XTAL_SI_PON_EI: u8 = 1 << 1;
pub const XTAL_SI_OFF_WEI: u8 = 1 << 2;
pub const XTAL_SI_OFF_EI: u8 = 1 << 3;
pub const XTAL_SI_RFC2RF: u8 = 1 << 4;
pub const XTAL_SI_SHDN_WL: u8 = 1 << 5;
pub const XTAL_SI_GND_SHDN_WL: u8 = 1 << 6;
pub const XTAL_SI_SRAM2RFC: u8 = 1 << 7;
pub const XTAL_SI_RF00: u8 = 1 << 0;
pub const XTAL_SI_RF10: u8 = 1 << 0;
pub const XTAL_SI_LDO_LPS: u8 = 0x7 << 4;
pub const XTAL_SI_LPS_CAP: u8 = 0xf;

// ---- the firmware CPU's local memory, through a window --------------------

/// `R_AX_FILTER_MODEL_ADDR`: selects the address the indirect window reaches.
pub const FILTER_MODEL_ADDR: u32 = 0x0c04;
/// `R_AX_INDIR_ACCESS_ENTRY`: the indirect window.
pub const INDIR_ACCESS_ENTRY: u32 = 0x4_0000;
/// `RTW89_MAC_MEM_CPU_LOCAL`'s base, plus `R_AX_WDT_CTRL`/`R_AX_WDT_STATUS`.
pub const CPU_LOCAL_WDT_CTRL: u32 = 0x1800_3040;
pub const CPU_LOCAL_WDT_STATUS: u32 = 0x1800_3044;
/// `WDT_CTRL_ALL_DIS`.
pub const WDT_CTRL_ALL_DIS: u32 = 0;
pub const FS_WDT_INT: u32 = 1 << 8;
pub const FS_WDT_INT_MSK: u32 = 1 << 9;

// ---- PCIe PHY, through BAR2 (`pci.h`) --------------------------------------

/// `R_RAC_DIRECT_OFFSET_G1 + RAC_ANA24 * 2`, and the Gen2 copy.
pub const RAC_ANA24_G1: u32 = 0x3800 + 0x24 * 2;
pub const RAC_ANA24_G2: u32 = 0x3880 + 0x24 * 2;
pub const DEGLITCH: u16 = 0xf << 8;

pub const PCIE_PS_CTRL_V1: u32 = 0x3008;
pub const SEL_REQ_ENTR_L1: u32 = 1 << 2;
pub const DMAC0_EXIT_L1_EN: u32 = 1 << 6;

pub const PCIE_BG_CLR: u32 = 0x303c;
pub const BG_CLR_ASYNC_M3: u32 = 1 << 4;

pub const PCIE_IO_RCY_M1: u32 = 0x3100;
pub const PCIE_WDT_TIMER_M1: u32 = 0x3104;
pub const PCIE_IO_RCY_M2: u32 = 0x310c;
pub const PCIE_WDT_TIMER_M2: u32 = 0x3110;
pub const PCIE_IO_RCY_E0: u32 = 0x3118;
pub const PCIE_WDT_TIMER_E0: u32 = 0x311c;
pub const PCIE_IO_RCY_S1: u32 = 0x3124;
/// `B_AX_PCIE_IO_RCY_WDT_MODE_{M1,M2,E0,S1}`.
pub const PCIE_IO_RCY_WDT_MODE: u32 = 1 << 3;
/// The 8852C's `io_rcy_tmr`, placed in its field.
pub const PCIE_IO_RCY_TIMER: u32 = 0x0001_1940;

// ---- host DMA engine (HAXI, `pci.h`) ---------------------------------------

pub const HAXI_INIT_CFG1: u32 = 0x1000;
pub const HAXI_MAX_TXDMA_MASK: u32 = 0x3;
pub const HAXI_MAX_RXDMA_MASK: u32 = 0x3 << 8;
pub const RST_BDRAM: u32 = 1 << 3;
pub const TXHCI_EN_V1: u32 = 1 << 7;
pub const RXBD_MODE_V1: u32 = 1 << 14;
pub const RXHCI_EN_V1: u32 = 1 << 15;
pub const STOP_AXI_MST: u32 = 1 << 17;
pub const DMA_MODE_MASK: u32 = 0x3 << 18;
pub const WD_ITVL_ACT_V1_MASK: u32 = 0xf << 24;
pub const WD_ITVL_IDLE_V1_MASK: u32 = 0xf << 28;
/// The 8852C's `tx_burst` (`MAC_AX_TX_BURST_V1_256B`), `rx_burst`
/// (`MAC_AX_RX_BURST_V1_128B`) and both `wd_dma_*_intvl`
/// (`MAC_AX_WD_DMA_INTVL_256NS`).
pub const TX_BURST: u32 = 2;
pub const RX_BURST: u32 = 1;
pub const WD_DMA_INTVL: u32 = 1;

pub const HAXI_DMA_STOP1: u32 = 0x1010;
/// `STOP_ACH0..ACH7 | STOP_CH8 | STOP_CH9 | STOP_CH12`.
pub const DMA_STOP1_TX_ALL: u32 = 0x000f_ff00 & !STOP_WPDMA;
pub const STOP_CH12: u32 = 1 << 18;
pub const STOP_WPDMA: u32 = 1 << 19;

pub const TXBD_RWPTR_CLR1: u32 = 0x1014;
/// `CLR_ACH0_IDX..CLR_ACH7_IDX | CLR_CH8_IDX | CLR_CH9_IDX | CLR_CH12_IDX`.
pub const CLR_IDX1_ALL: u32 = 0x7ff;

pub const HAXI_DMA_BUSY1: u32 = 0x101c;
/// `DMA_BUSY1_CHECK`: `ACH0_BUSY..ACH7_BUSY | CH8_BUSY | CH9_BUSY | CH12_BUSY`.
pub const DMA_BUSY1_CHECK: u32 = 0x7_ff00;

pub const CH12_TXBD_IDX: u32 = 0x1080;
/// `TXBD_HW_IDX_MASK`: where the chip's read pointer is.
pub const TXBD_HW_IDX_SHIFT: u32 = 16;
pub const TXBD_HW_IDX_MASK: u32 = 0xfff << 16;

/// `R_AX_HAXI_DMA_STOP2`: `STOP_CH10 | STOP_CH11`.
pub const HAXI_DMA_STOP2: u32 = 0x11c0;
pub const DMA_STOP2_TX_ALL: u32 = 0x3;
pub const TXBD_RWPTR_CLR2: u32 = 0x11c4;
pub const CLR_IDX2_ALL: u32 = 0x3;
pub const HAXI_DMA_BUSY2: u32 = 0x11c8;
pub const DMA_BUSY2_CHECK: u32 = 0x3;

pub const RXBD_RWPTR_CLR: u32 = 0x1200;
pub const CLR_RX_IDX_ALL: u32 = 0x3;
pub const HAXI_EXP_CTRL: u32 = 0x1204;
pub const MAX_TAG_NUM_MASK: u32 = 0x7;
/// The 8852C's `multi_tag_num` (`MAC_AX_TAG_NUM_8`, which encodes as 7).
pub const MULTI_TAG_NUM: u32 = 7;
pub const HAXI_DMA_BUSY3: u32 = 0x1208;
pub const DMA_BUSY3_CHECK: u32 = 0x3;

/// Base of the 16 write-pointer select bytes (`wp_sel_addr`).
pub const WP_ADDR_H_SEL0_3: u32 = 0x1334;

// ---- DMAC: packet buffer, quotas, flow control -----------------------------

pub const HCI_FC_CTRL_V1: u32 = 0x1700;
pub const HCI_FC_EN: u32 = 1 << 0;
pub const HCI_FC_CH12_EN: u32 = 1 << 3;
pub const HCI_FC_CH12_FULL_COND_MASK: u32 = 0x3 << 10;
pub const CH_PAGE_CTRL_V1: u32 = 0x1704;
pub const PREC_PAGE_CH12_MASK: u32 = 0x1ff << 16;
/// The 8852C PCIe `prec_cfg` in download mode: `h2c_prec` and `h2c_full_cond`.
pub const H2C_PREC: u32 = 0x28;
pub const H2C_FULL_COND: u32 = 0;

pub const HCI_FUNC_EN_V1: u32 = 0x7880;
pub const HCI_TXDMA_EN: u32 = 1 << 0;
pub const HCI_RXDMA_EN: u32 = 1 << 1;

pub const DMAC_FUNC_EN: u32 = 0x8400;
/// What `rtw8852c_pwr_on_func` sets in `DMAC_FUNC_EN`.
///
/// `MAC_FUNC_EN | DMAC_FUNC_EN |
/// MPDU_PROC_EN | WD_RLS_EN | DLE_WDE_EN | TXPKT_CTRL_EN | STA_SCH_EN |
/// DLE_PLE_EN | PKT_BUF_EN | DMAC_TBL_EN | PKT_IN_EN | DLE_CPUIO_EN |
/// DISPATCHER_EN | BBRPT_EN | MAC_SEC_EN | MAC_UN_EN | H_AXIDMA_EN`.
pub const DMAC_FUNC_EN_ALL: u32 = 0x7fff_c000;
/// What `rtw89_mac_hci_func_en_ax` writes for the 8852C: `MAC_FUNC_EN |
/// DMAC_FUNC_EN | DISPATCHER_EN | PKT_BUF_EN | H_AXIDMA_EN`.
pub const DMAC_FUNC_EN_HCI: u32 = 0x6044_4000;
/// `DLE_WDE_EN | DLE_PLE_EN`.
pub const DLE_EN: u32 = 0x0480_0000;
/// The two bits `rtw89_mac_check_mac_en` wants for the DMAC: `MAC_FUNC_EN |
/// DMAC_FUNC_EN`.
pub const MAC_DMAC_FUNC_EN: u32 = (1 << 30) | (1 << 29);

pub const DMAC_CLK_EN: u32 = 0x8404;
pub const DISPATCHER_CLK_EN: u32 = 1 << 18;
/// `DLE_WDE_CLK_EN | DLE_PLE_CLK_EN`.
pub const DLE_CLK_EN: u32 = 0x0480_0000;

pub const TX_ADDRESS_INFO_MODE_SETTING: u32 = 0x8810;
pub const HOST_ADDR_INFO_8B_SEL: u32 = 1 << 0;

pub const WDE_PKTBUF_CFG: u32 = 0x8c08;
pub const PLE_PKTBUF_CFG: u32 = 0x9008;
/// `B_AX_{WDE,PLE}_PAGE_SEL_MASK | ..._START_BOUND_MASK | ..._FREE_PAGE_NUM_MASK`.
pub const PKTBUF_CFG_MASK: u32 = 0x1fff_3f03;

pub const WDE_QTA0_CFG: u32 = 0x8c40;
pub const PLE_QTA0_CFG: u32 = 0x9040;

pub const WDE_INI_STATUS: u32 = 0x8d00;
pub const PLE_INI_STATUS: u32 = 0x9100;
/// `{WDE,PLE}_Q_MGN_INI_RDY | {WDE,PLE}_BUF_MGN_INI_RDY`.
pub const DLE_INI_RDY: u32 = 0x3;

pub const PKTIN_SETTING: u32 = 0x9a00;
pub const WD_ADDR_INFO_LENGTH: u32 = 1 << 1;

pub const CMAC_FUNC_EN: u32 = 0xc000;
/// What `rtw8852c_pwr_on_func` sets: `CMAC_EN | CMAC_TXEN | CMAC_RXEN |
/// FORCE_CMACREG_GCKEN | PHYINTF_EN | CMAC_DMA_EN | PTCLTOP_EN |
/// SCHEDULER_EN | TMAC_EN | RMAC_EN`.
pub const CMAC_FUNC_EN_ALL: u32 = 0x7000_803f;

/// Diagnostics read when a download fails (`rtw89_fw_dl_fail_dump`).
pub const BOOT_DBG: u32 = 0x83f0;

// ---- the TX/RX rings (`rtw89_pci_ch_dma_addr_set_v1`) ------------------------

/// One host TX channel's ring registers.
#[derive(Clone, Copy, Debug)]
pub struct TxRing {
    pub num: u32,
    pub bdram: u32,
    pub desa_l: u32,
    /// `bd_ram_table`'s `start_idx | max_num << 8 | min_num << 16`.
    pub bdram_val: u32,
}

/// Every host TX channel the 8852C has, in Linux's order (ACH0..7, CH8..12).
///
/// The last is CH12, the firmware-command channel — the only one the download
/// uses; the others are programmed (as Linux does) and left stopped.
pub const TX_RINGS: [TxRing; 13] = [
    TxRing { num: 0x1024, bdram: 0x1300, desa_l: 0x1230, bdram_val: 0x0002_0500 },
    TxRing { num: 0x1026, bdram: 0x1304, desa_l: 0x1238, bdram_val: 0x0002_0505 },
    TxRing { num: 0x1028, bdram: 0x1308, desa_l: 0x1240, bdram_val: 0x0002_050a },
    TxRing { num: 0x102a, bdram: 0x130c, desa_l: 0x1248, bdram_val: 0x0002_050f },
    TxRing { num: 0x102c, bdram: 0x1310, desa_l: 0x1250, bdram_val: 0x0002_0514 },
    TxRing { num: 0x102e, bdram: 0x1314, desa_l: 0x1258, bdram_val: 0x0002_0519 },
    TxRing { num: 0x1030, bdram: 0x1318, desa_l: 0x1260, bdram_val: 0x0002_051e },
    TxRing { num: 0x1032, bdram: 0x131c, desa_l: 0x1268, bdram_val: 0x0002_0523 },
    TxRing { num: 0x1034, bdram: 0x1320, desa_l: 0x1270, bdram_val: 0x0001_0528 },
    TxRing { num: 0x1036, bdram: 0x1324, desa_l: 0x1278, bdram_val: 0x0001_052d },
    TxRing { num: 0x1438, bdram: 0x1420, desa_l: 0x1458, bdram_val: 0x0001_0532 },
    TxRing { num: 0x143a, bdram: 0x1424, desa_l: 0x1460, bdram_val: 0x0001_0537 },
    TxRing { num: 0x1038, bdram: 0x1328, desa_l: 0x1280, bdram_val: 0x0001_043c },
];

/// The RX queue (`RXQ`) and release-report queue (`RPQ`): `(num, desa_l)`.
pub const RX_RINGS: [(u32, u32); 2] = [(0x1210, 0x1220), (0x1212, 0x1228)];

/// Entries per ring, as Linux sizes every one of them.
pub const RING_LEN: u16 = 256;

/// The ring base registers — the only writes whose values are DMA addresses,
/// and so the only ones the golden replay does not compare.
#[must_use]
pub fn is_ring_base(off: u32) -> bool {
    TX_RINGS.iter().any(|r| off == r.desa_l || off == r.desa_l + 4)
        || RX_RINGS.iter().any(|&(_, d)| off == d || off == d + 4)
}
