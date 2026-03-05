use alloc::vec::Vec;

/// Special byte used to signal a terminal resize event.
pub const RESIZE_SIGNAL_BYTE: u8 = 0x00;

/// Translate terminal escape sequences to simpler byte equivalents.
///
/// SSH clients send raw escape sequences for special keys; apps like
/// neatvi only understand simple byte codes for some of these
/// (e.g. 0x7f for delete/backspace).
#[must_use]
pub fn translate_input_keys(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        // Delete key: ESC [ 3 ~ -> 0x7f (DEL)
        if i + 3 < data.len()
            && data[i] == 0x1b
            && data[i + 1] == b'['
            && data[i + 2] == b'3'
            && data[i + 3] == b'~'
        {
            result.push(0x7f);
            i += 4;
        } else {
            result.push(data[i]);
            i += 1;
        }
    }
    result
}
