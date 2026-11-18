//! Classic BPF encodings and instruction layout from NetBSD sys/net/bpf.h,
//! CVS 1.82, Git 57b869a1b1e93e77e35bfee34f52e94075019aac.
// SPDX-License-Identifier: BSD-3-Clause
//
// Copyright (c) 1990, 1991, 1993
//         The Regents of the University of California.  All rights reserved.
//
// This code is derived from the Stanford/CMU enet packet filter,
// (net/enet.c) distributed as part of 4.3BSD, and code contributed
// to Berkeley by Steven McCanne and Van Jacobson both of Lawrence
// Berkeley Laboratory.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions
// are met:
// 1. Redistributions of source code must retain the above copyright
//    notice, this list of conditions and the following disclaimer.
// 2. Redistributions in binary form must reproduce the above copyright
//    notice, this list of conditions and the following disclaimer in the
//    documentation and/or other materials provided with the distribution.
// 3. Neither the name of the University nor the names of its contributors
//    may be used to endorse or promote products derived from this software
//    without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE REGENTS AND CONTRIBUTORS ``AS IS'' AND
// ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED.  IN NO EVENT SHALL THE REGENTS OR CONTRIBUTORS BE LIABLE
// FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
// DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
// OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
// HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
// LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
// OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
// SUCH DAMAGE.

#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Instruction {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

impl Instruction {
    pub const SIZE: usize = 8;

    #[must_use]
    pub const fn from_ne_bytes([c0, c1, jt, jf, k0, k1, k2, k3]: [u8; Self::SIZE]) -> Self {
        Self {
            code: u16::from_ne_bytes([c0, c1]),
            jt,
            jf,
            k: u32::from_ne_bytes([k0, k1, k2, k3]),
        }
    }
}

const _: () = {
    assert!(core::mem::size_of::<Instruction>() == Instruction::SIZE);
    assert!(core::mem::offset_of!(Instruction, k) == 4);
};

pub const MEMWORDS: usize = 16;

pub const LD: u16 = 0x00;
pub const LDX: u16 = 0x01;
pub const ST: u16 = 0x02;
pub const STX: u16 = 0x03;
pub const ALU: u16 = 0x04;
pub const JMP: u16 = 0x05;
pub const RET: u16 = 0x06;
pub const MISC: u16 = 0x07;

pub const ABS: u16 = 0x20;
pub const MEM: u16 = 0x60;
pub const LEN: u16 = 0x80;
pub const K: u16 = 0x00;
pub const X: u16 = 0x08;
pub const A: u16 = 0x10;

pub const ADD: u16 = 0x00;
pub const SUB: u16 = 0x10;
pub const MUL: u16 = 0x20;
pub const DIV: u16 = 0x30;
pub const OR: u16 = 0x40;
pub const AND: u16 = 0x50;
pub const LSH: u16 = 0x60;
pub const RSH: u16 = 0x70;
pub const NEG: u16 = 0x80;
pub const XOR: u16 = 0xa0;

pub const JA: u16 = 0x00;
pub const JEQ: u16 = 0x10;
pub const JGT: u16 = 0x20;
pub const JGE: u16 = 0x30;
pub const JSET: u16 = 0x40;
pub const TAX: u16 = 0x00;
pub const TXA: u16 = 0x80;
