// SPDX-License-Identifier: BSD-2-Clause
//! Validation and evaluation of Linux seccomp classic BPF programs.
#![no_std]
#![forbid(unsafe_code)]

mod bpf;

pub use bpf::Instruction;
use bpf::*;

pub const MAX_INSNS: usize = 4096;
pub const MAX_INSNS_PER_PATH: usize = 32768;
pub const FILTER_OVERHEAD: usize = 4;

pub const AUDIT_ARCH_AMD64: u32 = 0xc000_003e;
pub const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;

pub const KILL_PROCESS: u32 = 0x8000_0000;
pub const KILL_THREAD: u32 = 0x0000_0000;
pub const TRAP: u32 = 0x0003_0000;
pub const ERRNO: u32 = 0x0005_0000;
pub const TRACE: u32 = 0x7ff0_0000;
pub const LOG: u32 = 0x7ffc_0000;
pub const ALLOW: u32 = 0x7fff_0000;
const ACTION_MASK: u32 = 0xffff_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Data {
    pub nr: i32,
    pub arch: u32,
    pub instruction_pointer: u64,
    pub args: [u64; 6],
}

impl Data {
    pub const SIZE: usize = 64;

    fn word(&self, offset: u32) -> u32 {
        match offset {
            0 => self.nr.cast_unsigned(),
            4 => self.arch,
            8 | 12 => half(self.instruction_pointer, offset),
            _ => half(self.args[((offset - 16) / 8) as usize], offset),
        }
    }
}

fn half(word: u64, offset: u32) -> u32 {
    let upper = (offset & 4 != 0) == cfg!(target_endian = "little");
    (word >> if upper { 32 } else { 0 }) as u32
}

#[must_use]
pub const fn action_available(action: u32) -> bool {
    matches!(
        action,
        ALLOW | ERRNO | KILL_THREAD | KILL_PROCESS | TRAP | LOG | TRACE
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Allow,
    Errno(u16),
    KillThread,
    KillProcess,
    Trap(u16),
    Log,
    Trace,
}

/// Keep the result with the lowest signed action word. Programs are visited
/// newest first, so an equal action word retains the newest program's data.
#[derive(Clone, Copy, Debug)]
pub struct Verdict(u32);

impl Verdict {
    pub const ALLOW: Self = Self(ALLOW);

    pub fn consider(&mut self, result: u32) {
        if (result & ACTION_MASK).cast_signed() < (self.0 & ACTION_MASK).cast_signed() {
            self.0 = result;
        }
    }

    #[must_use]
    pub fn action(self) -> Action {
        let data = self.0 as u16;
        match self.0 & ACTION_MASK {
            ALLOW => Action::Allow,
            ERRNO => Action::Errno(data.min(4095)),
            KILL_THREAD => Action::KillThread,
            TRAP => Action::Trap(data),
            LOG => Action::Log,
            TRACE => Action::Trace,
            // Unknown actions (including USER_NOTIF on Akuma) fail closed.
            _ => Action::KillProcess,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidProgram {
    Length,
    Opcode,
    Operand,
    Jump,
    Return,
    Scratch,
}

// Names for the exact compound opcodes. Matching the full word rejects
// reserved bits and variants outside the Linux seccomp subset.
const LD_ABS: u16 = LD | ABS;
const LD_MEM: u16 = LD | MEM;
const LDX_MEM: u16 = LDX | MEM;
const LD_LEN: u16 = LD | LEN;
const LDX_LEN: u16 = LDX | LEN;
const RET_K: u16 = RET | K;
const RET_A: u16 = RET | A;
const JMP_JA: u16 = JMP | JA;
const ALU_NEG: u16 = ALU | NEG;
const MISC_TAX: u16 = MISC | TAX;
const MISC_TXA: u16 = MISC | TXA;

/// An immutable program checked once at installation. The storage type is
/// private to the carrier so callers cannot mutate the instructions afterward.
pub struct Program {
    instructions: alloc::vec::Vec<Instruction>,
}

extern crate alloc;

impl Program {
    pub fn new(instructions: alloc::vec::Vec<Instruction>) -> Result<Self, InvalidProgram> {
        let n = instructions.len();
        if !(1..=MAX_INSNS).contains(&n) {
            return Err(InvalidProgram::Length);
        }

        let mut reads_scratch = false;
        for (pc, ins) in instructions.iter().enumerate() {
            let remaining = n - pc - 1;
            match ins.code {
                LD | LDX | LD_LEN | LDX_LEN | RET_K | RET_A | MISC_TAX | MISC_TXA | ALU_NEG => {}
                LD_ABS if ins.k < Data::SIZE as u32 && ins.k.is_multiple_of(4) => {}
                LD_ABS => return Err(InvalidProgram::Operand),
                LD_MEM | LDX_MEM | ST | STX if ins.k < MEMWORDS as u32 => {
                    reads_scratch |= matches!(ins.code, LD_MEM | LDX_MEM);
                }
                LD_MEM | LDX_MEM | ST | STX => return Err(InvalidProgram::Operand),
                JMP_JA if (ins.k as usize) < remaining => {}
                JMP_JA => return Err(InvalidProgram::Jump),
                op if conditional(op) => {
                    if usize::from(ins.jt.max(ins.jf)) >= remaining {
                        return Err(InvalidProgram::Jump);
                    }
                }
                op if arithmetic(op) => {
                    if op & X == 0 {
                        match (op & 0xf0, ins.k) {
                            (DIV, 0) | (LSH | RSH, 32..) => return Err(InvalidProgram::Operand),
                            _ => {}
                        }
                    }
                }
                _ => return Err(InvalidProgram::Opcode),
            }
        }
        if !matches!(instructions[n - 1].code, RET_K | RET_A) {
            return Err(InvalidProgram::Return);
        }
        if reads_scratch {
            check_scratch(&instructions)?;
        }
        Ok(Self { instructions })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.instructions.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    #[must_use]
    pub fn evaluate(&self, data: &Data) -> u32 {
        let (mut a, mut x, mut pc) = (0u32, 0u32, 0usize);
        let mut mem = [0u32; MEMWORDS];
        loop {
            let Instruction { code, jt, jf, k } = self.instructions[pc];
            pc += 1;
            match code {
                LD => a = k,
                LDX => x = k,
                LD_ABS => a = data.word(k),
                LD_LEN => a = Data::SIZE as u32,
                LDX_LEN => x = Data::SIZE as u32,
                LD_MEM => a = mem[k as usize],
                LDX_MEM => x = mem[k as usize],
                ST => mem[k as usize] = a,
                STX => mem[k as usize] = x,
                RET_K => return k,
                RET_A => return a,
                MISC_TAX => x = a,
                MISC_TXA => a = x,
                JMP_JA => pc += k as usize,
                ALU_NEG => a = a.wrapping_neg(),
                _ => {
                    let rhs = if code & X == 0 { k } else { x };
                    let op = code & 0xf0;
                    if code & 7 == JMP {
                        let take = match op {
                            JEQ => a == rhs,
                            JGT => a > rhs,
                            JGE => a >= rhs,
                            JSET => a & rhs != 0,
                            _ => unreachable!(),
                        };
                        pc += usize::from(if take { jt } else { jf });
                    } else {
                        a = match op {
                            ADD => a.wrapping_add(rhs),
                            SUB => a.wrapping_sub(rhs),
                            MUL => a.wrapping_mul(rhs),
                            // Classic BPF ends execution with raw return 0.
                            DIV if rhs == 0 => return 0,
                            DIV => a / rhs,
                            OR => a | rhs,
                            AND => a & rhs,
                            LSH => a.wrapping_shl(rhs),
                            RSH => a.wrapping_shr(rhs),
                            XOR => a ^ rhs,
                            _ => unreachable!(),
                        };
                    }
                }
            }
        }
    }
}

fn conditional(code: u16) -> bool {
    code & !0xf8 == JMP && matches!(code & 0xf0, JEQ | JGT | JGE | JSET)
}

fn arithmetic(code: u16) -> bool {
    code & !0xf8 == ALU
        && matches!(
            code & 0xf0,
            ADD | SUB | MUL | DIV | OR | AND | LSH | RSH | XOR
        )
}

/// Intersect the set of written words at each forward join. The linear path
/// starts with no writes and resets to the neutral mask after a jump, when
/// its successors have already been recorded. Keep the linear check across
/// RET, matching Linux's treatment of unreachable code.
#[inline(never)]
fn check_scratch(instructions: &[Instruction]) -> Result<(), InvalidProgram> {
    let mut incoming = [u16::MAX; MAX_INSNS];
    let mut written = 0;
    for (pc, ins) in instructions.iter().enumerate() {
        written &= incoming[pc];
        match ins.code {
            ST | STX => written |= 1 << ins.k,
            LD_MEM | LDX_MEM if written & (1 << ins.k) == 0 => return Err(InvalidProgram::Scratch),
            JMP_JA => {
                incoming[pc + 1 + ins.k as usize] &= written;
                written = u16::MAX;
            }
            op if conditional(op) => {
                incoming[pc + 1 + usize::from(ins.jt)] &= written;
                incoming[pc + 1 + usize::from(ins.jf)] &= written;
                written = u16::MAX;
            }
            _ => {}
        }
    }
    Ok(())
}
