//! The VM spec: parse, validate, encode. See the crate header.

/// Longest path (kernel image, disk image) a spec carries.
pub const MAX_PATH: usize = 128;
/// Longest guest command-line tail. The kernel assembles the whole line (device windows,
/// `nosmp`, this) in a 400-byte stack buffer; the devices take under 130 of them.
pub const MAX_CMDLINE: usize = 192;
/// Most port forwards one spec carries.
pub const MAX_FORWARDS: usize = 4;
/// Guest RAM bounds (MiB).
///
/// The ceiling is the nested page table: four directories of 2 MiB
/// leaves cover 4 GiB — three below the device window (`0xC000_0000`..`0x1_0000_0000`, where
/// the virtio-mmio and local-APIC pages sit) and one above it, so guest RAM splits around the
/// window the way a PC's RAM splits at the PCI hole (`akuma_svm_amd64::rammap` owns the map).
/// An Akuma guest also needs 768 (its fixed 512 MiB heap).
pub const MIN_RAM_MIB: u32 = 16;
pub const MAX_RAM_MIB: u32 = 4096;
pub const DEFAULT_RAM_MIB: u32 = 768;
/// Virtual processors: all take turns on one host core, so more is latency, not compute.
pub const MAX_VCPUS: u8 = 4;
/// The wire document the kernel accepts in one `write` (`encode` never exceeds it).
pub const MAX_WIRE: usize = 768;

/// The only raw block device a spec may name: a partition of the machine's NVMe disk,
/// `/dev/nvme0n1p<N>`. The host's own root partition is refused by the kernel, not here.
pub const NVME_PARTITION_PREFIX: &str = "/dev/nvme0n1p";

/// Where a spec's `disk` lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskSource<'a> {
    /// No `disk`: the machine's second virtio-blk device, if it has one.
    None,
    /// A file on the host's filesystem (or the image copied into RAM, with `disk_ram`).
    File(&'a str),
    /// GPT partition `N` of the NVMe disk, used in place: the guest's writes are the partition's.
    NvmePartition(u32),
}

fn disk_source_of(path: &str) -> Result<DiskSource<'_>, Error> {
    if path.is_empty() {
        return Ok(DiskSource::None);
    }
    if let Some(num) = path.strip_prefix(NVME_PARTITION_PREFIX) {
        if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::BadValue);
        }
        return match num.parse::<u32>() {
            Ok(n @ 1..=128) => Ok(DiskSource::NvmePartition(n)),
            _ => Err(Error::BadValue),
        };
    }
    // No other raw device: a guest does not get to name a disk the kernel has not vetted.
    if path.starts_with("/dev/") {
        return Err(Error::BadValue);
    }
    Ok(DiskSource::File(path))
}

/// A `Copy` string of at most `N` bytes: printable ASCII, no newline.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Text<const N: usize> {
    len: u8,
    buf: [u8; N],
}

impl<const N: usize> Text<N> {
    #[must_use]
    pub const fn new() -> Self {
        Self { len: 0, buf: [0; N] }
    }

    /// Replace the contents. `Err(TooLong)` past `N`; `Err(BadChar)` for anything but
    /// printable ASCII (the wire is line-oriented, and the guest's command line is not UTF-8).
    pub fn set(&mut self, s: &str) -> Result<(), Error> {
        if s.len() > N {
            return Err(Error::TooLong);
        }
        if !s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            return Err(Error::BadChar);
        }
        self.buf[..s.len()].copy_from_slice(s.as_bytes());
        self.len = s.len() as u8;
        Ok(())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..usize::from(self.len)]).unwrap_or("")
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> Default for Text<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> core::fmt::Debug for Text<N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

/// Which machine monitor runs the guest.
///
/// `Firecracker` is reserved: it parses, so a config written for it is not a syntax error,
/// and the host that cannot run it says so ([`Backend::is_supported`]) instead of the parser
/// guessing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Kvm,
    Firecracker,
}

impl Backend {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "kvm" => Some(Self::Kvm),
            "firecracker" => Some(Self::Firecracker),
            _ => None,
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Kvm => "kvm",
            Self::Firecracker => "firecracker",
        }
    }

    /// Can the Akuma kernel's own hypervisor run this backend? (AMD SVM only; Intel VMX is out.)
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Kvm)
    }
}

/// One `host:guest` port forward (docker's `-p`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Forward {
    pub host: u16,
    pub guest: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A key no backend knows.
    UnknownKey,
    /// A value that does not parse for its key (the key is in `Rejected`).
    BadValue,
    TooLong,
    BadChar,
    TooManyForwards,
    DuplicateForward,
    /// A number outside its documented range.
    OutOfRange,
    /// A required field is empty (`kernel`).
    Missing,
    /// A wire line with no `=`.
    BadLine,
    /// The document is longer than [`MAX_WIRE`].
    TooBig,
    /// Two settings that cannot go together (`disk_ram` with a partition).
    Conflict,
}

impl Error {
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::UnknownKey => "unknown key",
            Self::BadValue => "bad value",
            Self::TooLong => "value too long",
            Self::BadChar => "value has a character outside printable ASCII",
            Self::TooManyForwards => "too many forwards",
            Self::DuplicateForward => "host port forwarded twice",
            Self::OutOfRange => "number out of range",
            Self::Missing => "a required field is empty",
            Self::BadLine => "line without '='",
            Self::TooBig => "document too long",
            Self::Conflict => "settings that cannot be combined",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec {
    pub backend: Backend,
    pub kernel: Text<MAX_PATH>,
    /// Appended to the command line the hypervisor builds (its device windows, `nosmp`).
    pub cmdline: Text<MAX_CMDLINE>,
    pub ram_mib: u32,
    pub vcpus: u8,
    /// Host cores the virtual processors may run on (`vm_cores`); 0: no preference. Intersected
    /// with the machine's `kvmcores=` by [`crate::cores::place`].
    pub cores: u64,
    /// Empty: the guest has no disk of its own (it gets the machine's fallback image).
    pub disk: Text<MAX_PATH>,
    /// Copy the disk into RAM at start: the guest never touches the host's storage and its
    /// writes die with it.
    pub disk_ram: bool,
    /// Let the guest's TCP connections and DNS queries out through the host's network
    /// (`vm_outbound`). Off by default: a guest then has no route anywhere but the ports forwarded in.
    pub outbound: bool,
    forwards: [Forward; MAX_FORWARDS],
    n_forwards: u8,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            backend: Backend::Kvm,
            kernel: Text::new(),
            cmdline: Text::new(),
            ram_mib: DEFAULT_RAM_MIB,
            vcpus: 1,
            cores: 0,
            disk: Text::new(),
            disk_ram: false,
            outbound: false,
            forwards: [Forward { host: 0, guest: 0 }; MAX_FORWARDS],
            n_forwards: 0,
        }
    }
}

fn flag(v: &str) -> Result<bool, Error> {
    match v {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(Error::BadValue),
    }
}

fn port(v: &str) -> Result<u16, Error> {
    match v.parse::<u16>() {
        Ok(0) | Err(_) => Err(Error::BadValue),
        Ok(p) => Ok(p),
    }
}

impl Spec {
    /// Where the disk lives (validated when it was set).
    #[must_use]
    pub fn disk_source(&self) -> DiskSource<'_> {
        disk_source_of(self.disk.as_str()).unwrap_or(DiskSource::None)
    }

    #[must_use]
    pub fn forwards(&self) -> &[Forward] {
        &self.forwards[..usize::from(self.n_forwards)]
    }

    /// Add `host:guest`.
    pub fn add_forward(&mut self, v: &str) -> Result<(), Error> {
        let (h, g) = v.split_once(':').ok_or(Error::BadValue)?;
        let f = Forward { host: port(h.trim())?, guest: port(g.trim())? };
        if self.forwards().iter().any(|x| x.host == f.host) {
            return Err(Error::DuplicateForward);
        }
        if usize::from(self.n_forwards) == MAX_FORWARDS {
            return Err(Error::TooManyForwards);
        }
        self.forwards[usize::from(self.n_forwards)] = f;
        self.n_forwards += 1;
        Ok(())
    }

    /// Apply one key. The names are herd's `vm_*` keys without the prefix, with
    /// Firecracker's spellings accepted as aliases.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), Error> {
        let value = value.trim();
        match key.trim() {
            "type" | "backend" => self.backend = Backend::parse(value).ok_or(Error::BadValue)?,
            "kernel" | "kernel_image_path" => self.kernel.set(value)?,
            "cmdline" | "boot_args" => self.cmdline.set(value)?,
            "ram_mib" | "mem_size_mib" => {
                let n = value.parse::<u32>().map_err(|_| Error::BadValue)?;
                if !(MIN_RAM_MIB..=MAX_RAM_MIB).contains(&n) {
                    return Err(Error::OutOfRange);
                }
                self.ram_mib = n;
            }
            "vcpus" | "vcpu_count" => {
                let n = value.parse::<u8>().map_err(|_| Error::BadValue)?;
                if !(1..=MAX_VCPUS).contains(&n) {
                    return Err(Error::OutOfRange);
                }
                self.vcpus = n;
            }
            "cores" => self.cores = crate::cores::parse(value).ok_or(Error::BadValue)?,
            "disk" | "path_on_host" => {
                disk_source_of(value)?;
                self.disk.set(value)?;
            }
            "disk_ram" => self.disk_ram = flag(value)?,
            "outbound" => self.outbound = flag(value)?,
            "forward" => self.add_forward(value)?,
            _ => return Err(Error::UnknownKey),
        }
        Ok(())
    }

    /// Cross-field checks, run once the whole document is in.
    pub fn validate(&self) -> Result<(), Error> {
        if self.kernel.is_empty() {
            return Err(Error::Missing);
        }
        if self.disk_ram && self.disk.is_empty() {
            return Err(Error::Missing);
        }
        // A partition is used in place: there is nothing to copy into RAM.
        if self.disk_ram && matches!(self.disk_source(), DiskSource::NvmePartition(_)) {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    /// Parse a wire document: `key=value` lines (blank lines and `#` comments skipped; the
    /// value is the rest of the line, so a command line keeps its spaces), `forward`
    /// repeatable. Applies nothing on error: the spec is built whole or not at all.
    pub fn parse_wire(text: &str) -> Result<Self, (Error, usize)> {
        if text.len() > MAX_WIRE {
            return Err((Error::TooBig, 0));
        }
        let mut s = Self::default();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line.split_once('=').ok_or((Error::BadLine, i + 1))?;
            s.set(k, v).map_err(|e| (e, i + 1))?;
        }
        s.validate().map_err(|e| (e, 0))?;
        Ok(s)
    }

    /// The wire document for this spec, into `out`. `None` if it does not fit.
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        let mut w = Writer { out, n: 0 };
        w.line("type", self.backend.name())?;
        w.line("kernel", self.kernel.as_str())?;
        if !self.cmdline.is_empty() {
            w.line("cmdline", self.cmdline.as_str())?;
        }
        w.num("ram_mib", u64::from(self.ram_mib))?;
        w.num("vcpus", u64::from(self.vcpus))?;
        if self.cores != 0 {
            w.put(b"cores=")?;
            let mut list = [0u8; 40];
            let n = crate::cores::format(self.cores, &mut list)?;
            w.put(&list[..n])?;
            w.put(b"\n")?;
        }
        if !self.disk.is_empty() {
            w.line("disk", self.disk.as_str())?;
            w.line("disk_ram", if self.disk_ram { "1" } else { "0" })?;
        }
        if self.outbound {
            w.line("outbound", "1")?;
        }
        for f in self.forwards() {
            w.put(b"forward=")?;
            w.dec(u64::from(f.host))?;
            w.put(b":")?;
            w.dec(u64::from(f.guest))?;
            w.put(b"\n")?;
        }
        Some(w.n)
    }
}

struct Writer<'a> {
    out: &'a mut [u8],
    n: usize,
}

impl Writer<'_> {
    fn put(&mut self, b: &[u8]) -> Option<()> {
        self.out.get_mut(self.n..self.n + b.len())?.copy_from_slice(b);
        self.n += b.len();
        Some(())
    }

    fn line(&mut self, k: &str, v: &str) -> Option<()> {
        self.put(k.as_bytes())?;
        self.put(b"=")?;
        self.put(v.as_bytes())?;
        self.put(b"\n")
    }

    fn dec(&mut self, mut v: u64) -> Option<()> {
        let mut d = [0u8; 20];
        let mut i = d.len();
        loop {
            i -= 1;
            d[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.put(&d[i..])
    }

    fn num(&mut self, k: &str, v: u64) -> Option<()> {
        self.put(k.as_bytes())?;
        self.put(b"=")?;
        self.dec(v)?;
        self.put(b"\n")
    }
}
