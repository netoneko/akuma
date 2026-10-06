//! The parser against the real `rtw8852c_fw-1.bin`, when one is to hand.
//!
//! The file is Realtek's (redistributable, not in this repo). Point
//! `AKUMA_RTW89_FW` at a copy — Pop!_OS has it at
//! `/lib/firmware/rtw89/rtw8852c_fw-1.bin`, and `overlays/ryzen/fetch-firmware.sh`
//! puts Alpine's beside it on p3 — and this checks what the W0 trace pinned:
//! for ryzen's cut (1), the normal image is 0.27.122.0, and it goes over in
//! exactly 166 section packets. Without the variable the test passes having
//! checked nothing, and says so.

use akuma_rtw89::fw;

#[test]
fn ryzen_firmware_plans_the_166_packets_linux_sent() {
    let Ok(path) = std::env::var("AKUMA_RTW89_FW") else {
        eprintln!("AKUMA_RTW89_FW unset: real-firmware check skipped");
        return;
    };
    let file = std::fs::read(&path).unwrap();
    let mut src: &[u8] = &file;
    let c = fw::Container::read(&mut src).unwrap();
    let e = c.select(fw::TYPE_NORMAL, 1, file.len() as u32).unwrap();
    let img = fw::Image::read(&mut src, e).unwrap();
    assert_eq!(img.version, [0, 27, 122, 0]);
    assert_eq!(img.sections().len(), 3);
    assert_eq!(img.packets().count(), 166);
    let last = img.packets().last().unwrap();
    assert_eq!(last.0 + last.1, e.shift + e.size, "the packets end where the image does");
}
