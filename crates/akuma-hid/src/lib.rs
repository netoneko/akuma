//! HID report-descriptor parsing and report decoding, for the touchpad and
//! any later HID device (USB mice included).
//!
//! # Attribution
//!
//! This crate is a Rust port of `sys/dev/hid/hid.c` from OpenBSD, which is
//! itself derived from the NetBSD original by Lennart Augustsson:
//!
//! ```text
//! $OpenBSD: hid.c,v 1.10 2025/11/03 01:41:22 jmatthew Exp $
//! $NetBSD: hid.c,v 1.23 2002/07/11 21:14:25 augustss Exp $
//! $FreeBSD: src/sys/dev/usb/hid.c,v 1.11 1999/11/17 22:33:39 n_hibma Exp $
//!
//! Copyright (c) 1998 The NetBSD Foundation, Inc.
//! All rights reserved.
//!
//! This code is derived from software contributed to The NetBSD Foundation
//! by Lennart Augustsson (lennart@augustsson.net) at
//! Carlstedt Research & Technology.
//!
//! Redistribution and use in source and binary forms, with or without
//! modification, are permitted provided that the following conditions
//! are met:
//! 1. Redistributions of source code must retain the above copyright
//!    notice, this list of conditions and the following disclaimer.
//! 2. Redistributions in binary form must reproduce the above copyright
//!    notice, this list of conditions and the following disclaimer in the
//!    documentation and/or other materials provided with the distribution.
//!
//! THIS SOFTWARE IS PROVIDED BY THE NETBSD FOUNDATION, INC. AND CONTRIBUTORS
//! ``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED
//! TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
//! PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE FOUNDATION OR CONTRIBUTORS
//! BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
//! CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
//! SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
//! INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
//! CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
//! ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
//! POSSIBILITY OF SUCH DAMAGE.
//! ```
//!
//! The same notice ships in [`NOTICE`](./NOTICE) beside this file. The port
//! follows the C structurally (`hid_get_item` → [`HidParser::next_item`],
//! `hid_locate` → [`locate`], `hid_get_data` → [`get_bits`]) so upstream
//! behaviour stays traceable; where it deliberately differs it says so
//! (long-item data bytes are skipped, which the C does not do, and
//! [`locate`] walks the shared bit counter rather than the C's
//! kind-relative one — see its doc).
//!
//! # What it does
//!
//! A HID report descriptor is the item stream USB HID §6.2.2 defines: global
//! state (usage page, logical bounds, report size/count/id, push/pop) and
//! local state (usages) scoping each `Input`/`Output`/`Feature` main item.
//! Walking it yields, for every field, the *bit position* inside its report
//! and the usage that names it. [`FieldTable`] is the one-shot form the
//! touchpad driver uses: one walk at probe time, then O(n) lookups by usage.
//!
//! Positions deliberately **exclude the report-ID byte**: when a descriptor
//! numbers its reports, callers hand [`get_bits`] the payload *after* the ID
//! byte, and [`report_size`] returns the payload size (add 1 on the wire for
//! numbered reports). This matches the C and keeps one numbering convention.
//!
//! No allocation, no `core`-plus: fixed tables sized like the C (`MAX_USAGE`,
//! `MAX_PUSH`, `MAX_REPORT_IDS`), so the kernel can build a table in a
//! `static`.

#![no_std]
#![forbid(unsafe_code)]

mod field;
mod item;
/// Real report descriptors captured from hardware, for tests in this crate
/// and its consumers (unused consts cost nothing in a release image).
pub mod fixtures;
#[cfg(test)]
mod tests;

pub use field::{FieldTable, HidField, MAX_FIELDS};
pub use item::{
    HIO_BUFFEREDBYTE, HIO_CONST, HIO_NONLINEAR, HIO_NOPREF, HIO_NULLSTATE, HIO_RELATIVE,
    HIO_VARIABLE, HIO_VOLATILE, HIO_WRAP, HidLocation, ItemKind,
};

/// Local-state capacity: usages queued by one main item (`MAXUSAGE` in the C).
pub const MAX_USAGE: usize = 64;
/// Global-state push depth (`MAXPUSH` in the C).
pub const MAX_PUSH: usize = 4;
/// Per-report-ID bit-position bookkeeping slots (`MAXID` in the C).
pub const MAX_REPORT_IDS: usize = 16;
/// Per-usage cap for one VARIABLE main item (`MAXLOCCNT` in the C, which
/// clamps rather than failing — so does this port).
pub const MAX_LOC_COUNT: u32 = 2048;

/// Which main-item kinds [`HidParser::next_item`] yields. Collections and
///
/// end-collections are yielded in **every** mode (the C always returns them
/// and lets the caller filter — that return is load-bearing for the state
/// machine), so a kind-filtered walk still sees them and must skip them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Every main item.
    All,
    /// Only *field* items of this kind (collections pass through as noted).
    Kind(ItemKind),
}

impl Filter {
    fn matches(self, kind: ItemKind) -> bool {
        match self {
            Self::All => true,
            Self::Kind(want) => want == kind,
        }
    }
}

/// One step of the item stream: a main item, with every scoping value that
///
/// was current where it appeared. Array (non-`HIO_VARIABLE`) items come out
/// as a single item with [`HidLocation::count`] > 1 and the first usage of
/// their range; VARIABLE items come out one per usage, positions advancing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedItem {
    pub kind: ItemKind,
    pub report_id: u8,
    /// `usage_page << 16 | code` for this item (for a collection: its usage).
    pub usage: u32,
    pub usage_minimum: u32,
    pub usage_maximum: u32,
    /// `HIO_*` bits.
    pub flags: u32,
    pub loc: HidLocation,
    pub logical_minimum: i32,
    pub logical_maximum: i32,
    /// `Collection` main item's type value (0 = Physical, 1 = Logical, …).
    pub collection: u32,
}

/// State carried across main items, one slot per push level.
#[derive(Clone, Copy)]
struct CurState {
    report_id: u8,
    kind: ItemKind,
    flags: u32,
    usage: u32,
    usage_minimum: u32,
    usage_maximum: u32,
    logical_minimum: i32,
    logical_maximum: i32,
    physical_minimum: i32,
    physical_maximum: i32,
    unit_exponent: u32,
    unit: u32,
    designator_index: u32,
    designator_minimum: u32,
    designator_maximum: u32,
    string_index: u32,
    string_minimum: u32,
    string_maximum: u32,
    set_delimiter: u32,
    loc_pos: u32,
    loc_size: u32,
    loc_count: u32,
    collection: u32,
    collevel: u32,
    usage_page: u32,
}

impl CurState {
    const ZERO: Self = Self {
        report_id: 0,
        kind: ItemKind::Input,
        flags: 0,
        usage: 0,
        usage_minimum: 0,
        usage_maximum: 0,
        logical_minimum: 0,
        logical_maximum: 0,
        physical_minimum: 0,
        physical_maximum: 0,
        unit_exponent: 0,
        unit: 0,
        designator_index: 0,
        designator_minimum: 0,
        designator_maximum: 0,
        string_index: 0,
        string_minimum: 0,
        string_maximum: 0,
        set_delimiter: 0,
        loc_pos: 0,
        loc_size: 0,
        loc_count: 0,
        collection: 0,
        collevel: 0,
        usage_page: 0,
    };

    fn clear_local(&mut self) {
        self.loc_count = 0;
        self.loc_size = 0;
        self.usage = 0;
        self.usage_minimum = 0;
        self.usage_maximum = 0;
        self.designator_index = 0;
        self.designator_minimum = 0;
        self.designator_maximum = 0;
        self.string_index = 0;
        self.string_minimum = 0;
        self.string_maximum = 0;
        self.set_delimiter = 0;
    }
}

/// The streaming parser. Construct with [`HidParser::new`], pull with
/// [`HidParser::next_item`] until `None`.
pub struct HidParser<'d> {
    desc: &'d [u8],
    p: usize,
    filter: Filter,
    cur: [CurState; MAX_PUSH],
    /// `(report_id, bit position)` bookkeeping across report-ID switches.
    last_pos: [(u8, u32); MAX_REPORT_IDS],
    usages_min: [u32; MAX_USAGE],
    usages_max: [u32; MAX_USAGE],
    usage_last: u32,
    loc_size: u32,
    loc_count: u32,
    /// Remaining per-usage iterations of the current main item (`ncount`).
    ncount: u32,
    /// Iterations served of the current main item (`icount`).
    icount: u32,
    pushlevel: usize,
    iusage: usize,
    nusage: usize,
    ousage: u32,
    susage: u8,
}

impl<'d> HidParser<'d> {
    /// Walk `desc`, yielding items matching `filter`.
    #[must_use]
    pub fn new(desc: &'d [u8], filter: Filter) -> Self {
        Self {
            desc,
            p: 0,
            filter,
            cur: [CurState::ZERO; MAX_PUSH],
            last_pos: [(0, 0); MAX_REPORT_IDS],
            usages_min: [0; MAX_USAGE],
            usages_max: [0; MAX_USAGE],
            usage_last: 0,
            loc_size: 0,
            loc_count: 0,
            ncount: 0,
            icount: 0,
            pushlevel: 0,
            iusage: 0,
            nusage: 0,
            ousage: 0,
            susage: 0,
        }
    }

    /// `hid_get_byte`: read one byte, advancing by exactly `advance` bytes
    /// (clamped at the end of the descriptor). Past the end reads 0 and
    /// advances nothing.
    fn get_byte(&mut self, advance: usize) -> u8 {
        if self.p >= self.desc.len() {
            return 0;
        }
        let v = self.desc[self.p];
        self.p = (self.p + advance).min(self.desc.len());
        v
    }

    /// `hid_switch_rid`: switch the current report ID, carrying the bit
    /// position across — each report ID's fields restart at bit 0, and
    /// returning to a seen ID resumes where it left off.
    fn switch_rid(&mut self, nextid: u8) {
        let cur_rid = self.cur[self.pushlevel].report_id;
        if cur_rid == nextid {
            return;
        }
        let pos = self.cur[self.pushlevel].loc_pos;
        if cur_rid == 0 {
            self.last_pos[0] = (0, pos);
        } else {
            let i = self.find_slot(cur_rid);
            if i < MAX_REPORT_IDS {
                self.last_pos[i] = (cur_rid, pos);
            }
        }
        let slot = if nextid == 0 {
            0
        } else {
            self.find_slot(nextid)
        };
        self.cur[self.pushlevel].report_id = nextid;
        self.cur[self.pushlevel].loc_pos = if slot < MAX_REPORT_IDS {
            self.last_pos[slot].1
        } else {
            // "Out of RID entries, position is set to zero!" — the C's
            // DPRINTF case; positions restart, which is at worst a decode
            // bug for descriptors with more than 15 numbered reports.
            0
        };
    }

    fn find_slot(&self, rid: u8) -> usize {
        let mut i = 1;
        while i < MAX_REPORT_IDS {
            if self.last_pos[i].0 == rid || self.last_pos[i].0 == 0 {
                break;
            }
            i += 1;
        }
        i
    }

    /// Yield the next matching main item, or `None` at end of descriptor
    /// (or after a malformed end-collection).
    pub fn next_item(&mut self) -> Option<ParsedItem> {
        if self.pushlevel >= MAX_PUSH {
            return None;
        }
        loop {
            // top: serve the current main item's usage iteration, if any.
            if self.icount < self.ncount {
                let dval: u32;
                if self.iusage < self.nusage {
                    let d = self.usages_min[self.iusage].wrapping_add(self.ousage);
                    dval = d;
                    self.usage_last = d;
                    if d == self.usages_max[self.iusage] {
                        self.iusage += 1;
                        self.ousage = 0;
                    } else {
                        self.ousage += 1;
                    }
                } else {
                    dval = self.usage_last;
                }
                self.icount += 1;
                let kind = self.cur[self.pushlevel].kind;
                if self.filter.matches(kind) {
                    let c = &mut self.cur[self.pushlevel];
                    c.usage = dval;
                    let item = ParsedItem {
                        kind: c.kind,
                        report_id: c.report_id,
                        usage: c.usage,
                        usage_minimum: c.usage_minimum,
                        usage_maximum: c.usage_maximum,
                        flags: c.flags,
                        loc: HidLocation {
                            pos: c.loc_pos,
                            size: c.loc_size,
                            count: c.loc_count,
                        },
                        logical_minimum: c.logical_minimum,
                        logical_maximum: c.logical_maximum,
                        collection: c.collection,
                    };
                    c.loc_pos = c.loc_pos.wrapping_add(c.loc_size.wrapping_mul(c.loc_count));
                    return Some(item);
                }
                continue;
            }

            // Reset per-main-item state, then parse until the next main item.
            self.icount = 0;
            self.ncount = 0;
            self.iusage = 0;
            self.nusage = 0;
            self.ousage = 0;
            self.susage = 0;
            self.cur[self.pushlevel].clear_local();

            while self.p < self.desc.len() {
                let ItemWord::Short {
                    tag,
                    ty,
                    size,
                    uval,
                    dval,
                } = self.next_item_word()
                else {
                    continue;
                };
                match ty {
                    0 => {
                        // Main.
                        let kind = match tag {
                            8 => Some(ItemKind::Input),
                            9 => Some(ItemKind::Output),
                            11 => Some(ItemKind::Feature),
                            _ => None,
                        };
                        if let Some(kind) = kind {
                            // ret:
                            let loc_count = self.loc_count;
                            let loc_size = self.loc_size;
                            let c = &mut self.cur[self.pushlevel];
                            c.kind = kind;
                            c.flags = dval as u32;
                            c.loc_count = loc_count;
                            c.loc_size = loc_size;
                            let ncount = if loc_count > MAX_LOC_COUNT {
                                MAX_LOC_COUNT
                            } else {
                                loc_count
                            };
                            if c.flags & HIO_VARIABLE != 0 {
                                self.ncount = ncount;
                                c.loc_count = 1;
                            } else {
                                self.ncount = 1;
                            }
                            break; // goto top
                        }
                        match tag {
                            10 => {
                                // Collection. The C returns it from
                                // hid_get_item unconditionally — the *caller*
                                // filters by kind — and that return is what
                                // advances the machine past it so the next
                                // call's reset block clears the usage queue.
                                // Filtering here would leak the collection's
                                // usages into the next field (found by the
                                // boot-mouse test).
                                let usage_last = self.usage_last;
                                let c = &mut self.cur[self.pushlevel];
                                c.kind = ItemKind::Collection;
                                c.collection = uval;
                                c.collevel += 1;
                                c.usage = usage_last;
                                let c = self.cur[self.pushlevel];
                                return Some(ParsedItem {
                                    kind: c.kind,
                                    report_id: c.report_id,
                                    usage: c.usage,
                                    usage_minimum: c.usage_minimum,
                                    usage_maximum: c.usage_maximum,
                                    flags: c.flags,
                                    loc: HidLocation {
                                        pos: c.loc_pos,
                                        size: c.loc_size,
                                        count: c.loc_count,
                                    },
                                    logical_minimum: c.logical_minimum,
                                    logical_maximum: c.logical_maximum,
                                    collection: c.collection,
                                });
                            }
                            12 => {
                                // End collection: always returned, like the C.
                                if self.cur[self.pushlevel].collevel == 0 {
                                    return None; // invalid: unbalanced
                                }
                                let c = &mut self.cur[self.pushlevel];
                                c.kind = ItemKind::EndCollection;
                                c.collevel -= 1;
                                let c = self.cur[self.pushlevel];
                                return Some(ParsedItem {
                                    kind: c.kind,
                                    report_id: c.report_id,
                                    usage: c.usage,
                                    usage_minimum: c.usage_minimum,
                                    usage_maximum: c.usage_maximum,
                                    flags: c.flags,
                                    loc: HidLocation {
                                        pos: c.loc_pos,
                                        size: c.loc_size,
                                        count: c.loc_count,
                                    },
                                    logical_minimum: c.logical_minimum,
                                    logical_maximum: c.logical_maximum,
                                    collection: c.collection,
                                });
                            }
                            _ => {}
                        }
                    }
                    1 => {
                        // Global.
                        let c = &mut self.cur[self.pushlevel];
                        match tag {
                            0 => c.usage_page = uval << 16,
                            1 => c.logical_minimum = dval,
                            2 => c.logical_maximum = dval,
                            3 => c.physical_minimum = dval,
                            4 => c.physical_maximum = dval,
                            5 => c.unit_exponent = uval,
                            6 => c.unit = uval,
                            7 => self.loc_size = uval,
                            8 => self.switch_rid(dval as u8),
                            9 => self.loc_count = uval,
                            10 => {
                                // Push: the C parks the *globals* in the old
                                // level's loc fields and Pop takes them back
                                // from there, so keep that exact shape.
                                if self.pushlevel < MAX_PUSH - 1 {
                                    let old = self.pushlevel;
                                    let (size, count) = (self.loc_size, self.loc_count);
                                    self.cur[old].loc_size = size;
                                    self.cur[old].loc_count = count;
                                    self.pushlevel = old + 1;
                                    self.cur[self.pushlevel] = self.cur[old];
                                }
                            }
                            11
                                // Pop: restore the pushed state, keeping the
                                // position (which belongs to the report, not
                                // the push level).
                                if self.pushlevel > 0 => {
                                    let oldpos = self.cur[self.pushlevel].loc_pos;
                                    self.pushlevel -= 1;
                                    self.loc_size = self.cur[self.pushlevel].loc_size;
                                    self.loc_count = self.cur[self.pushlevel].loc_count;
                                    self.cur[self.pushlevel].loc_pos = oldpos;
                                    self.cur[self.pushlevel].loc_size = 0;
                                    self.cur[self.pushlevel].loc_count = 0;
                                }
                            _ => {}
                        }
                    }
                    2 => {
                        // Local. A 4-byte usage is absolute (page included);
                        // shorter ones are codes in the current page — the
                        // C's `bSize != 4` rule, which is the correct
                        // disambiguator (a 4-byte value with a zero page
                        // prefix still means absolute).
                        let page = self.cur[self.pushlevel].usage_page;
                        match tag {
                            0 => {
                                let u = if size == 4 { uval } else { page | uval };
                                self.usage_last = u;
                                if self.nusage < MAX_USAGE {
                                    self.usages_min[self.nusage] = u;
                                    self.usages_max[self.nusage] = u;
                                    self.nusage += 1;
                                }
                                self.susage = 0;
                            }
                            1 | 2 => {
                                let u = if size == 4 { uval } else { page | uval };
                                if tag == 1 {
                                    self.susage |= 1;
                                    self.cur[self.pushlevel].usage_minimum = u;
                                } else {
                                    self.susage |= 2;
                                    self.cur[self.pushlevel].usage_maximum = u;
                                }
                                if self.susage == 3 {
                                    let (lo, hi) = (
                                        self.cur[self.pushlevel].usage_minimum,
                                        self.cur[self.pushlevel].usage_maximum,
                                    );
                                    if self.nusage < MAX_USAGE && lo <= hi {
                                        self.usages_min[self.nusage] = lo;
                                        self.usages_max[self.nusage] = hi;
                                        self.nusage += 1;
                                    }
                                    // "Usage set dropped" otherwise, as in C.
                                    self.susage = 0;
                                }
                            }
                            3 => self.cur[self.pushlevel].designator_index = uval,
                            4 => self.cur[self.pushlevel].designator_minimum = uval,
                            5 => self.cur[self.pushlevel].designator_maximum = uval,
                            7 => self.cur[self.pushlevel].string_index = uval,
                            8 => self.cur[self.pushlevel].string_minimum = uval,
                            9 => self.cur[self.pushlevel].string_maximum = uval,
                            10 => self.cur[self.pushlevel].set_delimiter = uval,
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            if self.p >= self.desc.len() && self.icount >= self.ncount {
                // Descriptor exhausted and nothing pending: the C falls out
                // of its while loop and returns 0. If the last main item was
                // just queued (icount < ncount) the top loop above would have
                // served it first.
                return None;
            }
        }
    }

    /// Parse one item header. Long items (`0xFE` prefix) are consumed
    /// whole — header plus data bytes — and reported as [`ItemWord::Long`];
    /// the C reads their header and then walks past the data without
    /// consuming it, which desynchronises the stream. Truncated multi-byte
    /// reads fill with zeros and stop advancing, exactly like `hid_get_byte`,
    /// and the caller's `p < len` loop check ends the parse.
    fn next_item_word(&mut self) -> ItemWord {
        let b = self.get_byte(1);
        if b == 0xFE {
            // Long item: 1-byte size low, 1-byte size high, 1-byte tag, data.
            let lo = self.get_byte(1) as usize;
            let hi = self.get_byte(1) as usize;
            let _tag = self.get_byte(1);
            let size = lo | (hi << 8);
            for _ in 0..size {
                if self.p >= self.desc.len() {
                    break;
                }
                self.p += 1;
            }
            return ItemWord::Long;
        }
        let tag = b >> 4;
        let ty = (b >> 2) & 3;
        let mut size = b & 3;
        if size == 3 {
            size = 4;
        }
        let mut uval: u32 = 0;
        for shift in 0..size {
            uval |= u32::from(self.get_byte(1)) << (8 * u32::from(shift));
        }
        let dval: i32 = match size {
            // Sign reinterpretation is the point: HID item values are
            // two's-complement in the declared width.
            1 => i32::from((uval as u8).cast_signed()),
            2 => i32::from((uval as u16).cast_signed()),
            _ => uval.cast_signed(),
        };
        ItemWord::Short {
            tag,
            ty,
            size,
            uval,
            dval,
        }
    }
}

enum ItemWord {
    Long,
    Short {
        tag: u8,
        ty: u8,
        size: u8,
        uval: u32,
        dval: i32,
    },
}

/// Total payload size in bytes of `kind` reports with report ID `id` —
///
/// *excluding* the report-ID byte itself (add 1 on the wire for numbered
/// reports). `None` when the descriptor has no such report (the C returns 0).
#[must_use]
pub fn report_size(desc: &[u8], kind: ItemKind, id: u8) -> Option<usize> {
    let mut lo: i64 = -1;
    let mut hi: i64 = -1;
    let mut p = HidParser::new(desc, Filter::Kind(kind));
    while let Some(h) = p.next_item() {
        if h.report_id == id {
            if lo < 0 {
                lo = i64::from(h.loc.pos);
            }
            hi = i64::from(h.loc.pos + h.loc.size * h.loc.count);
        }
    }
    if lo < 0 {
        None
    } else {
        Some(((hi - lo + 7) / 8) as usize)
    }
}

/// `hid_locate`: the first non-const field named `usage` in `kind` reports
/// with report ID `id`, with its `HIO_*` flags.
///
/// Deliberate divergence from the C: this walks with `Filter::All` and
/// filters per item, so returned positions always reflect the descriptor's
/// shared bit counter (what is actually on the wire). The C's kind-filtered
/// walk only advances positions for *matching* items, which makes a
/// `hid_feature` locate on a descriptor whose inputs come first return
/// position 0 — wrong for exactly the field (Contact Count Maximum after
/// all inputs) this port exists to read.
#[must_use]
pub fn locate(desc: &[u8], usage: u32, id: u8, kind: ItemKind) -> Option<(HidLocation, u32)> {
    let mut p = HidParser::new(desc, Filter::All);
    while let Some(h) = p.next_item() {
        if h.kind == kind && h.flags & HIO_CONST == 0 && h.usage == usage && h.report_id == id {
            return Some((h.loc, h.flags));
        }
    }
    None
}

/// `hid_get_data`: a signed field out of a report payload (report-ID byte
/// already stripped by the caller when the reports are numbered).
#[must_use]
pub fn get_bits(buf: &[u8], loc: &HidLocation) -> i32 {
    get_bits_sub(buf, loc, true)
}

/// `hid_get_udata`: the unsigned form.
#[must_use]
pub fn get_ubits(buf: &[u8], loc: &HidLocation) -> u32 {
    get_bits_sub(buf, loc, false) as u32
}

fn get_bits_sub(buf: &[u8], loc: &HidLocation, signed: bool) -> i32 {
    let hsize = if loc.size > 32 { 32 } else { loc.size };
    if hsize == 0 {
        return 0;
    }
    let hsize: u32 = hsize;
    let mut data: u32 = 0;
    let n = hsize.div_ceil(8) as usize;
    let start = (loc.pos as usize) / 8;
    // The C's `while (n--) { rpos--; data |= buf[rpos] << (8 * n); }` reads
    // the *highest* byte first with the largest shift — i.e. plain
    // little-endian assembly of the field's bytes.
    for i in 0..n {
        let rpos = start + n - 1 - i;
        let shift = 8 * (n - 1 - i);
        if rpos < buf.len() {
            data |= u32::from(buf[rpos]) << shift;
        }
    }
    data >>= loc.pos % 8;
    if hsize == 32 {
        // No mask room; the shift below would overflow. The reinterpret is
        // the value itself.
        return data.cast_signed();
    }
    let sh = 32 - hsize;
    if signed {
        (data << sh).cast_signed() >> sh
    } else {
        ((data << sh) >> sh).cast_signed()
    }
}
