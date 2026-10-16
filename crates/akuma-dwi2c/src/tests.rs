//! Golden values for the timing math and byte-level decisions.

use crate::*;

#[test]
fn comp_type_identifies_designware() {
    assert_eq!(COMP_TYPE_VALUE, 0x4457_0140);
    // The check the kernel probe makes: read IC_COMP_TYPE, compare.
    let fake_mmio_readback: u32 = 0x4457_0140;
    assert_eq!(fake_mmio_readback, COMP_TYPE_VALUE);
}

#[test]
fn scl_counts_at_100_mhz() {
    // Linux's own debug lines on a 100 MHz controller:
    //   Standard Mode HCNT:LCNT = 427:499
    //   Fast Mode     HCNT:LCNT = 87:159
    let t = timings_from_clock(100_000);
    assert_eq!(
        t.ss,
        SclTimings {
            hcnt: 427,
            lcnt: 499
        },
        "SS: 100000*(4000+300)/1e6-3=427, 100000*(4700+300)/1e6-1=499"
    );
    assert_eq!(
        t.fs,
        SclTimings {
            hcnt: 87,
            lcnt: 159
        },
        "FM: 100000*(600+300)/1e6-3=87, 100000*(1300+300)/1e6-1=159"
    );
    assert_eq!(t.sda_hold, 0);
}

#[test]
fn scl_counts_at_133_mhz_round_to_closest() {
    // 133.33 MHz: check the round-to-closest, not truncation.
    // hcnt = round(133333*(600+300)/1e6) - 3 = round(120.0) - 3 ≈ 117
    let h = scl_hcnt(133_333, 600, 300);
    assert_eq!(h, 117);
    // lcnt = round(133333*1600/1e6) - 1 = round(213.33) - 1 = 213 - 1 = 212
    let l = scl_lcnt(133_333, 1_300, 300);
    assert_eq!(l, 212);
}

#[test]
fn acpi_timing_packages_apply() {
    // Shape from the DSDT: package [hcnt, lcnt, sda_hold].
    let mut t = timings_from_clock(100_000);
    let fmcn = [u32::from(0x27Fu16), 0x0323, 0x0]; // plausible real-world FMCN
    apply_acpi_package(&mut t, BusSpeed::Fast, &fmcn);
    assert_eq!(
        t.fs,
        SclTimings {
            hcnt: 0x027F,
            lcnt: 0x0323
        }
    );
    // SS untouched by an FMCN apply.
    assert_eq!(
        t.ss,
        SclTimings {
            hcnt: 427,
            lcnt: 499
        }
    );

    // Wrong shape → ignored, computed values survive.
    let mut t = timings_from_clock(100_000);
    apply_acpi_package(&mut t, BusSpeed::Fast, &[1, 2, 3, 4]);
    assert_eq!(
        t.fs,
        SclTimings {
            hcnt: 87,
            lcnt: 159
        }
    );

    assert!(parse_acpi_timing_package(&[]).is_none());
    assert_eq!(parse_acpi_timing_package(&[10, 20, 30]), Some((10, 20, 30)));
}

#[test]
fn master_con_preserves_bus_clear() {
    // BIOS left the bus-clear feature advertised: keep it.
    let bios = con::BUS_CLEAR_CTRL | 0xFFFF_F000; // other BIOS junk must not leak
    let v = master_con(BusSpeed::Fast, bios);
    assert_eq!(
        v,
        con::MASTER | con::SLAVE_DISABLE | con::RESTART_EN | con::SPEED_FAST | con::BUS_CLEAR_CTRL
    );
    // No bus-clear bit: plain master config.
    let v = master_con(BusSpeed::Standard, 0);
    assert_eq!(
        v,
        con::MASTER | con::SLAVE_DISABLE | con::RESTART_EN | con::SPEED_STD
    );
}

#[test]
fn fifo_depth_decode() {
    // COMP_PARAM_1 with TX depth 64 (63) and RX depth 32 (31).
    let p = (63 << 16) | (31 << 8);
    assert_eq!(fifo_depths(p), (64, 32));
    // Minimum depth 2 encodes as 1|1.
    assert_eq!(fifo_depths((1 << 16) | (1 << 8)), (2, 2));
}

#[test]
fn tx_abort_decode() {
    assert_eq!(TxAbort::from_source(1 << 0), TxAbort::AddrNoAck);
    assert_eq!(TxAbort::from_source(1 << 3), TxAbort::DataNoAck);
    assert_eq!(TxAbort::from_source(1 << 12), TxAbort::ArbLost);
    assert_eq!(TxAbort::from_source(1 << 11), TxAbort::MasterDis);
    assert_eq!(TxAbort::from_source(1 << 7), TxAbort::Other(1 << 7));
    // A NACK bit outranks everything else (it is the actionable one).
    assert_eq!(
        TxAbort::from_source((1 << 12) | (1 << 0)),
        TxAbort::AddrNoAck
    );
}

#[test]
fn error_mapping_is_the_probe_contract() {
    // The kernel glue maps TxAbort -> XferError; pin the vocabulary here so
    // both sides cannot drift.
    let map = |a: TxAbort| match a {
        TxAbort::AddrNoAck => XferError::NoDevice,
        TxAbort::ArbLost | TxAbort::DataNoAck | TxAbort::MasterDis | TxAbort::Other(_) => {
            XferError::Bus
        }
    };
    assert_eq!(map(TxAbort::from_source(1)), XferError::NoDevice);
    assert_eq!(map(TxAbort::from_source(1 << 12)), XferError::Bus);
}

#[test]
fn data_cmd_bit_shapes() {
    // A read command for the last byte of a write-then-read: READ | STOP.
    let last_read = data_cmd::READ | data_cmd::STOP;
    assert_eq!(last_read, 0x300);
    // First byte of the read half carries RESTART.
    let first_read = data_cmd::READ | data_cmd::RESTART;
    assert_eq!(first_read, 0x500);
    // A written data byte is the byte itself; STOP rides on top.
    let last_write: u32 = 0xAB | data_cmd::STOP;
    assert_eq!(last_write, 0x2AB);
}

#[test]
fn aoac_matches_the_measured_i2ca_bytes() {
    // /dev/mem on the 16AHP9: suspended 0x77/0x38, awake 0x7C/0x27.
    assert_eq!(aoac::addr(aoac::DEV_I2C0), 0xFED8_1E4A);
    assert_eq!(aoac::d0_control(0x77), 0x7C);
    assert!(!aoac::wants_d0(0x77));
    assert!(aoac::wants_d0(0x7C));
    assert!(!aoac::is_on(0x38));
    assert!(aoac::is_on(0x27));
    // The upper status bits are not part of the field.
    assert!(aoac::is_on(0xFF));
    assert!(!aoac::is_on(0x06));
}

#[test]
fn aoac_d0_is_idempotent() {
    for b in 0..=255u8 {
        assert_eq!(aoac::d0_control(aoac::d0_control(b)), aoac::d0_control(b));
        assert!(aoac::wants_d0(aoac::d0_control(b)));
    }
}

#[test]
fn firmware_counts() {
    assert!(counts_usable(0x84, 0xEF));
    assert!(!counts_usable(0, 0xEF));
    assert!(!counts_usable(0x84, 0));
    assert!(!counts_usable(0xFFFF_FFFF, 0xFFFF_FFFF));
}
