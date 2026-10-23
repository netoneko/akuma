//! Request codes: `dir(2) size(14) type(8) nr(8)`, the encoding every request uses. All the
//! requests seen are type [`TYPE`].

/// The type byte of every request the VMM sent.
pub const TYPE: u8 = 0xAE;

/// Direction bits (the top two bits of the request).
pub const DIR_NONE: u8 = 0;
/// The VMM writes the argument; the kernel reads it.
pub const DIR_WRITE: u8 = 1;
/// The kernel writes the argument back.
pub const DIR_READ: u8 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Req {
    pub dir: u8,
    pub size: u16,
    pub ty: u8,
    pub nr: u8,
}

impl Req {
    #[must_use]
    pub const fn decode(code: u32) -> Self {
        Self { dir: (code >> 30) as u8, size: ((code >> 16) & 0x3fff) as u16, ty: (code >> 8) as u8, nr: code as u8 }
    }

    #[must_use]
    pub const fn encode(self) -> u32 {
        (self.dir as u32) << 30 | (self.size as u32) << 16 | (self.ty as u32) << 8 | self.nr as u32
    }
}

/// Which kind of descriptor a request was issued on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `open("/dev/kvm")`.
    System,
    /// Returned by `CreateVm`.
    Vm,
    /// Returned by `CreateVcpu`.
    Vcpu,
}

/// The requests seen, by inferred name. `code` is the full request word as observed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    GetApiVersion,
    CheckExtension,
    GetSupportedCpuid,
    CreateVm,
    GetVcpuMmapSize,
    GetMsrIndexList,
    SetTssAddr,
    CreateIrqchip,
    CreatePit2,
    CreateVcpu,
    SetUserMemoryRegion,
    Irqfd,
    Ioeventfd,
    SetCpuid2,
    SetMsrs,
    SetRegs,
    SetFpu,
    GetSregs,
    SetSregs,
    GetLapic,
    SetLapic,
    Run,
}

/// (request word, op, descriptor kind it is valid on). `CheckExtension` is valid on the system and VM
/// descriptors alike (the VMM asked both).
const TABLE: [(u32, Op, Kind); 22] = [
    (0x0000_AE00, Op::GetApiVersion, Kind::System),
    (0x0000_AE03, Op::CheckExtension, Kind::System),
    (0xC008_AE05, Op::GetSupportedCpuid, Kind::System),
    (0x0000_AE01, Op::CreateVm, Kind::System),
    (0x0000_AE04, Op::GetVcpuMmapSize, Kind::System),
    (0xC004_AE02, Op::GetMsrIndexList, Kind::System),
    (0x0000_AE47, Op::SetTssAddr, Kind::Vm),
    (0x0000_AE60, Op::CreateIrqchip, Kind::Vm),
    (0x4040_AE77, Op::CreatePit2, Kind::Vm),
    (0x0000_AE41, Op::CreateVcpu, Kind::Vm),
    (0x4020_AE46, Op::SetUserMemoryRegion, Kind::Vm),
    (0x4020_AE76, Op::Irqfd, Kind::Vm),
    (0x4040_AE79, Op::Ioeventfd, Kind::Vm),
    (0x4008_AE90, Op::SetCpuid2, Kind::Vcpu),
    (0x4008_AE89, Op::SetMsrs, Kind::Vcpu),
    (0x4090_AE82, Op::SetRegs, Kind::Vcpu),
    (0x41A0_AE8D, Op::SetFpu, Kind::Vcpu),
    (0x8138_AE83, Op::GetSregs, Kind::Vcpu),
    (0x4138_AE84, Op::SetSregs, Kind::Vcpu),
    (0x8400_AE8E, Op::GetLapic, Kind::Vcpu),
    (0x4400_AE8F, Op::SetLapic, Kind::Vcpu),
    (0x0000_AE80, Op::Run, Kind::Vcpu),
];

/// What a request word means on a descriptor of `kind`. `None` is "not a request this protocol
/// has" (the caller answers `ENOTTY`).
#[must_use]
pub fn decode(code: u32, kind: Kind) -> Option<Op> {
    for &(c, op, k) in &TABLE {
        if c == code && (k == kind || (op == Op::CheckExtension && matches!(kind, Kind::Vm))) {
            return Some(op);
        }
    }
    None
}

/// The request word of `op`.
#[must_use]
pub fn code_of(op: Op) -> u32 {
    TABLE.iter().find(|e| e.1 == op).map_or(0, |e| e.0)
}
