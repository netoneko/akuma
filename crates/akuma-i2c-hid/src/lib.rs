//! The HID-over-I²C transport decisions, pure and host-tested: descriptor
//! validation, command encoding, input-report framing, and the bring-up
//! timing constants.
//!
//! Protocol of record: "HID Over I²C" v1.0 (Microsoft/Intel). Behavioural
//! references — consulted for behaviour, not pasted (see
//! `proposals/AMD64_TOUCHPAD_I2C_HID.md` §3 for the section-by-section map):
//! Linux `drivers/hid/i2c-hid/i2c-hid-core.c` + `i2c-hid-acpi.c`, FreeBSD
//! `sys/dev/iicbus/iichid.c`, OpenBSD `ihidev(4)`.
//!
//! The kernel-side half (`amd64/src/touchpad.rs`) owns the I²C bus
//! transaction itself; everything here is bytes in, decisions out, so a
//! device that is absent costs one failed bus turn and nothing else.
//!
//! Registers of a device, for orientation (the HID descriptor supplies the
//! real ones — never hardcode them past the first read):
//!
//! ```text
//! 0x0001  HID descriptor      (the conventional wHIDDescRegister)
//! 0x0002  report descriptor   (wReportDescRegister, from the descriptor)
//! 0x0003  command register    (wCommandRegister)
//! 0x0004  data register       (wDataRegister)
//! ```

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

/// Where the HID descriptor lives when the ACPI `_DSM` does not say
///
/// (GUID `3cdff6f7-4267-4555-ad05-b30a3d8938de`, rev 1, func 1 → u16).
/// Every device in practice answers here; FreeBSD falls back to the same
/// value (`IICHID_REG_ELAN`).
pub const HID_DESC_REGISTER: u16 = 0x0001;

/// `wReportDescRegister` in every known descriptor — validated, not trusted.
pub const EXPECTED_REPORT_DESC_REGISTER: u16 = 0x0002;

/// The HID descriptor is 30 bytes and says so itself; anything else is not
/// an i2c-hid device (Linux and FreeBSD both gate on this).
pub const HID_DESC_SIZE: usize = 30;

/// Command opcodes (spec §7.2).
pub mod opcode {
    pub const RESET: u8 = 0x01;
    pub const GET_REPORT: u8 = 0x02;
    pub const SET_REPORT: u8 = 0x03;
    pub const GET_IDLE: u8 = 0x04;
    pub const SET_IDLE: u8 = 0x05;
    pub const GET_PROTOCOL: u8 = 0x06;
    pub const SET_PROTOCOL: u8 = 0x07;
    pub const SET_POWER: u8 = 0x08;
}

/// Which report half a command addresses (the `report_type` field, shifted
/// into the command's first byte by [`encode_command`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportType {
    Input = 0x1,
    Output = 0x2,
    Feature = 0x3,
}

/// `SET_POWER` states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerState {
    On = 0x0,
    Sleep = 0x1,
}

/// The bring-up delays, in one place because they are the difference
/// between a pad that works and one that silently never interrupts:
///
/// - 60 ms after power-on: the spec lets the device clock-stretch, but
///   Linux measured Windows sleeping 1 ms here and **60 ms for Goodix**,
///   and "several devices will not work properly without a delay after a
///   PWR_ON request".
/// - 100 ms reset settle: FreeBSD's sampling mode sleeps then consumes the
///   zero-length reset ack; Linux's `NO_IRQ_AFTER_RESET` quirk path sleeps
///   the same 100 ms.
/// - 400 µs address retry: STM-based panels need 400 µs after a rising
///   clock edge to wake from deep sleep; the first read NACKs (Linux
///   `i2c_hid_probe_address`).
pub const POWER_ON_DELAY_MS: u64 = 60;
pub const RESET_SETTLE_MS: u64 = 100;
pub const ADDR_RETRY_DELAY_US: u64 = 400;
/// Linux retries the whole power+reset dance three times before declaring
/// the device dead.
pub const BRINGUP_ATTEMPTS: u32 = 3;

/// Per-device deviations from the spec's timing, keyed off the HID
/// descriptor's vendor/product. Only the quirks that matter to a polled
/// (no-IRQ) bring-up are carried; the full Linux table is IRQ-shaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quirks {
    /// Extra milliseconds after power-on before the device will answer
    /// meaningfully. Goodix 27c6:0d42 needs 1500 before it will *interrupt*;
    /// a polled host mostly pays it as a first-touch dead zone.
    pub post_power_delay_ms: u64,
    /// Device answers with `0xFFFF` lengths when it has nothing (ELAN);
    /// [`decode_input`] drops those either way, the flag only documents it.
    pub bogus_input_size: bool,
}

impl Quirks {
    /// No known deviation.
    pub const NONE: Self = Self {
        post_power_delay_ms: 0,
        bogus_input_size: false,
    };

    /// The carried subset of Linux's `i2c_hid_quirks` table.
    #[must_use]
    pub fn for_vid_pid(vid: u16, pid: u16) -> Self {
        match (vid, pid) {
            // GOODIX 0x0d42: DELAY_WAKEUP_AFTER_RESUME.
            (0x27C6, 0x0D42) => Self {
                post_power_delay_ms: 1500,
                bogus_input_size: false,
            },
            // ELAN: BOGUS_IRQ (0xFFFF lengths when empty).
            (0x04F3, _) => Self {
                post_power_delay_ms: 0,
                bogus_input_size: true,
            },
            _ => Self::NONE,
        }
    }
}

/// The 30-byte HID descriptor (spec §5.1.1), decoded and validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HidDescriptor {
    pub report_desc_len: u16,
    pub report_desc_register: u16,
    pub input_register: u16,
    pub max_input_len: u16,
    pub output_register: u16,
    pub max_output_len: u16,
    pub command_register: u16,
    pub data_register: u16,
    pub vendor_id: u16,
    pub product_id: u16,
    pub version_id: u16,
}

/// Why a descriptor was rejected — the probe's dmesg line picks from these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescError {
    /// Read returned fewer than 30 bytes.
    Short(usize),
    /// `wHIDDescLength` != 30.
    BadLength(u16),
    /// `bcdVersion` != 0x0100.
    BadBcdVersion(u16),
    /// `wMaxInputLength` < 2 (cannot even carry a length word).
    BadMaxInput(u16),
}

impl HidDescriptor {
    /// Decode and validate a 30-byte read. Both other implementations gate
    /// on exactly these three checks (`i2c_hid_fetch_hid_descriptor`,
    /// `iichid_probe`).
    pub fn parse(bytes: &[u8]) -> Result<Self, DescError> {
        if bytes.len() < HID_DESC_SIZE {
            return Err(DescError::Short(bytes.len()));
        }
        let u16le = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let desc_len = u16le(0);
        if desc_len as usize != HID_DESC_SIZE {
            return Err(DescError::BadLength(desc_len));
        }
        let bcd = u16le(2);
        if bcd != 0x0100 {
            return Err(DescError::BadBcdVersion(bcd));
        }
        let d = Self {
            report_desc_len: u16le(4),
            report_desc_register: u16le(6),
            input_register: u16le(8),
            max_input_len: u16le(10),
            output_register: u16le(12),
            max_output_len: u16le(14),
            command_register: u16le(16),
            data_register: u16le(18),
            vendor_id: u16le(20),
            product_id: u16le(22),
            version_id: u16le(24),
        };
        if d.max_input_len < 2 {
            return Err(DescError::BadMaxInput(d.max_input_len));
        }
        Ok(d)
    }
}

/// Encode one command into `buf`: `[wCommandRegister LE][type<<4|id]
///
/// [opcode]([id if >= 0x0F])`. Returns the written length (4 or 5), the
/// exact wire shape of spec §7.2.2.4's "protocol optimized for Report < 15".
///
/// Panics if `buf` is shorter than 5 bytes — the command buffer is a fixed
/// kernel array; that is a programming error, not a runtime condition.
pub fn encode_command(
    buf: &mut [u8],
    command_register: u16,
    opcode: u8,
    rtype: ReportType,
    report_id: u8,
) -> usize {
    let type_id = if report_id >= 0x0F {
        (rtype as u8) << 4 | 0x0F
    } else {
        (rtype as u8) << 4 | report_id
    };
    encode_command_raw(buf, command_register, type_id, opcode, report_id)
}

/// The shared byte-pusher: `type_id` is the complete first command byte,
/// `escaped_id` is the real report id written as the third byte (present
/// only when the id nibble was the 0x0F escape).
fn encode_command_raw(
    buf: &mut [u8],
    command_register: u16,
    type_id: u8,
    opcode: u8,
    escaped_id: u8,
) -> usize {
    buf[0] = command_register as u8;
    buf[1] = (command_register >> 8) as u8;
    buf[2] = type_id;
    buf[3] = opcode;
    if type_id & 0x0F == 0x0F && escaped_id >= 0x0F {
        buf[4] = escaped_id;
        5
    } else {
        4
    }
}

/// `SET_POWER` — the power state rides in the id nibble with the *type
/// nibble zeroed* (Linux passes report_type 0; FreeBSD writes `param`
/// directly: `{ cmdreg, param, SET_POWER }`).
pub fn encode_set_power(buf: &mut [u8], command_register: u16, state: PowerState) -> usize {
    encode_command_raw(buf, command_register, state as u8, opcode::SET_POWER, 0)
}

/// `RESET` — type 0, id 0.
pub fn encode_reset(buf: &mut [u8], command_register: u16) -> usize {
    encode_command_raw(buf, command_register, 0, opcode::RESET, 0)
}

/// Frame an output report for the plain "send report" path (spec §6.2.3):
///
/// `[wOutputRegister LE][total length LE][report id?][data...]`, used when
/// `wMaxOutputLength != 0` and no `SET_REPORT` round-trip is needed.
/// Returns the written length; `None` when the device takes no output
/// reports at all.
pub fn encode_output(
    buf: &mut [u8],
    output_register: u16,
    report_id: u8,
    data: &[u8],
) -> Option<usize> {
    let len = 2 + usize::from(report_id != 0) + data.len();
    if len + 2 > buf.len() {
        return None;
    }
    buf[0] = output_register as u8;
    buf[1] = (output_register >> 8) as u8;
    buf[2] = len as u8;
    buf[3] = (len >> 8) as u8;
    let mut at = 4;
    if report_id != 0 {
        buf[at] = report_id;
        at += 1;
    }
    buf[at..at + data.len()].copy_from_slice(data);
    Some(at + data.len())
}

/// Decode one polled input read (spec §6.1.3): the device returns
///
/// `[length LE][report...]`; length 0 means "nothing pending" (and is the
/// RESET acknowledgement), `0xFFFF` or an overrun length is garbage.
/// Returns the report payload, without the length word.
#[must_use]
pub fn decode_input(buf: &[u8]) -> Option<&[u8]> {
    if buf.len() < 2 {
        return None;
    }
    let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    if len <= 2 || len > buf.len() {
        return None;
    }
    Some(&buf[2..len])
}

/// Is this read the RESET acknowledgement (a zero length word)? FreeBSD's
/// `reset_acked`: the first polled read whose length word is 0 completes
/// the reset handshake.
#[must_use]
pub fn is_reset_ack(buf: &[u8]) -> bool {
    buf.len() >= 2 && buf[0] == 0 && buf[1] == 0
}

/// Is this read one of the known bogus "nothing" answers (`0xFFFF`)?
#[must_use]
pub fn is_bogus_input(buf: &[u8]) -> bool {
    buf.len() >= 2 && buf[0] == 0xFF && buf[1] == 0xFF
}

/// Should this input report be dropped as a duplicate of `last`? The polled
///
/// loop reads at a fixed cadence whether or not the device has news; FreeBSD
/// compares the whole frame (length word included) for exactly this. First
/// call with `last == None` is never a duplicate.
#[must_use]
pub fn is_duplicate(last: Option<&[u8]>, buf: &[u8]) -> bool {
    match last {
        Some(prev) => prev == buf,
        None => false,
    }
}
