extern crate std;

use core::fmt::Write as _;

use crate::arena::{Arena, MAX_EXTENTS};
use crate::spec::{MAX_CMDLINE, MAX_FORWARDS, MAX_PATH, MAX_WIRE};
use crate::{Backend, DiskSource, Error, Forward, Spec};

const GENTOO: &str = "type=kvm\nkernel=/boot/akuma-guest\ncmdline=init=/usr/bin/bash initargs=/akuma-kvm-init.sh\n\
ram_mib=768\nvcpus=1\ndisk=/boot/gentoo-guest.img\ndisk_ram=1\nforward=2223:4444\n";

fn parse(t: &str) -> Result<Spec, Error> {
    Spec::parse_wire(t).map_err(|(e, _)| e)
}

#[test]
fn a_full_spec_parses() {
    let s = parse(GENTOO).unwrap();
    assert_eq!(s.backend, Backend::Kvm);
    assert_eq!(s.kernel.as_str(), "/boot/akuma-guest");
    assert_eq!(s.cmdline.as_str(), "init=/usr/bin/bash initargs=/akuma-kvm-init.sh");
    assert_eq!((s.ram_mib, s.vcpus, s.disk_ram), (768, 1, true));
    assert_eq!(s.disk.as_str(), "/boot/gentoo-guest.img");
    assert_eq!(s.forwards(), &[Forward { host: 2223, guest: 4444 }]);
}

#[test]
fn defaults_are_the_akuma_guest() {
    let s = parse("kernel=/boot/akuma-guest\n").unwrap();
    assert_eq!((s.ram_mib, s.vcpus, s.disk_ram), (768, 1, false));
    assert!(s.disk.is_empty() && s.forwards().is_empty() && s.cmdline.is_empty());
}

#[test]
fn encode_round_trips() {
    let s = parse(GENTOO).unwrap();
    let mut b = [0u8; MAX_WIRE];
    let n = s.encode(&mut b).unwrap();
    assert_eq!(parse(core::str::from_utf8(&b[..n]).unwrap()).unwrap(), s);
}

#[test]
fn the_largest_spec_still_encodes_within_the_wire_limit() {
    let mut s = Spec::default();
    s.set("kernel", &"k".repeat(MAX_PATH)).unwrap();
    s.set("disk", &"d".repeat(MAX_PATH)).unwrap();
    s.set("cmdline", &"c".repeat(MAX_CMDLINE)).unwrap();
    for p in 0..MAX_FORWARDS {
        s.add_forward(&std::format!("{}:{}", 60000 + p, 65000 + p)).unwrap();
    }
    let mut b = [0u8; MAX_WIRE];
    assert!(s.encode(&mut b).is_some());
}

#[test]
fn firecracker_spellings_are_aliases() {
    let mut s = Spec::default();
    s.set("kernel_image_path", "/k").unwrap();
    s.set("boot_args", "console=ttyS0").unwrap();
    s.set("vcpu_count", "2").unwrap();
    s.set("mem_size_mib", "512").unwrap();
    s.set("path_on_host", "/d.img").unwrap();
    assert_eq!((s.kernel.as_str(), s.cmdline.as_str(), s.vcpus, s.ram_mib, s.disk.as_str()), ("/k", "console=ttyS0", 2, 512, "/d.img"));
}

#[test]
fn backend_firecracker_parses_but_is_not_supported() {
    let s = parse("type=firecracker\nkernel=/k\n").unwrap();
    assert_eq!(s.backend, Backend::Firecracker);
    assert!(!s.backend.is_supported());
    assert!(Backend::Kvm.is_supported());
    assert_eq!(parse("type=qemu\nkernel=/k\n"), Err(Error::BadValue));
}

#[test]
fn comments_blank_lines_and_spaces_in_values() {
    let s = parse("# c\n\n  kernel = /k  \ncmdline = a b  c\n").unwrap();
    assert_eq!((s.kernel.as_str(), s.cmdline.as_str()), ("/k", "a b  c"));
}

#[test]
fn every_rejection() {
    assert_eq!(parse(""), Err(Error::Missing));
    assert_eq!(parse("ram_mib=768\n"), Err(Error::Missing));
    assert_eq!(parse("kernel=/k\ndisk_ram=1\n"), Err(Error::Missing));
    assert_eq!(parse("kernel=/k\nbogus=1\n"), Err(Error::UnknownKey));
    assert_eq!(parse("kernel=/k\nnoequals\n"), Err(Error::BadLine));
    assert_eq!(parse("kernel=/k\nram_mib=abc\n"), Err(Error::BadValue));
    assert_eq!(parse("kernel=/k\nram_mib=15\n"), Err(Error::OutOfRange));
    assert_eq!(parse("kernel=/k\nram_mib=1025\n"), Err(Error::OutOfRange));
    assert_eq!(parse("kernel=/k\nvcpus=0\n"), Err(Error::OutOfRange));
    assert_eq!(parse("kernel=/k\nvcpus=5\n"), Err(Error::OutOfRange));
    assert_eq!(parse("kernel=/k\nvcpus=-1\n"), Err(Error::BadValue));
    assert_eq!(parse("kernel=/k\ndisk_ram=maybe\n"), Err(Error::BadValue));
    assert_eq!(parse("kernel=/k\nforward=22\n"), Err(Error::BadValue));
    assert_eq!(parse("kernel=/k\nforward=0:22\n"), Err(Error::BadValue));
    assert_eq!(parse("kernel=/k\nforward=70000:22\n"), Err(Error::BadValue));
    assert_eq!(parse("kernel=/k\nforward=1:2\nforward=1:3\n"), Err(Error::DuplicateForward));
    assert_eq!(parse(&std::format!("kernel={}\n", "k".repeat(MAX_PATH + 1))), Err(Error::TooLong));
    assert_eq!(parse(&std::format!("kernel=/k\ncmdline={}\n", "c".repeat(MAX_CMDLINE + 1))), Err(Error::TooLong));
    assert_eq!(parse("kernel=/k\ncmdline=caf\u{e9}\n"), Err(Error::BadChar));
    assert_eq!(parse(&"#\n".repeat(MAX_WIRE)), Err(Error::TooBig));
}

#[test]
fn forwards_stop_at_the_limit() {
    let mut t = std::string::String::from("kernel=/k\n");
    for p in 0..=MAX_FORWARDS {
        let _ = writeln!(t, "forward={}:22", 1000 + p);
    }
    assert_eq!(parse(&t), Err(Error::TooManyForwards));
}

#[test]
fn error_line_numbers_point_at_the_line() {
    assert_eq!(Spec::parse_wire("kernel=/k\n\nvcpus=9\n"), Err((Error::OutOfRange, 3)));
}

#[test]
fn arena_first_fit_aligned_and_reusable() {
    let mut a = Arena::new(8 << 20);
    let x = a.alloc(3 << 20, 2 << 20).unwrap();
    let y = a.alloc(1 << 20, 2 << 20).unwrap();
    assert_eq!((x, y), (0, 4 << 20));
    assert!(a.alloc(5 << 20, 2 << 20).is_none());
    assert!(a.free(x));
    assert_eq!(a.alloc(4 << 20, 2 << 20), Some(0));
    assert!(!a.free(12345));
    assert_eq!(a.used(), 5 << 20);
}

#[test]
fn arena_refuses_bad_requests_and_overflow() {
    let mut a = Arena::new(1 << 20);
    assert!(a.alloc(0, 4096).is_none());
    assert!(a.alloc(4096, 3).is_none());
    assert!(a.alloc(u64::MAX, 4096).is_none());
    // Offset 0 is aligned to anything; the next candidate rounds up past the end (or overflows).
    assert_eq!(a.alloc(4096, u64::MAX / 2 + 1), Some(0));
    assert!(a.alloc(4096, u64::MAX / 2 + 1).is_none());
}

#[test]
fn arena_extent_table_is_bounded() {
    let mut a = Arena::new(1 << 30);
    for _ in 0..MAX_EXTENTS {
        assert!(a.alloc(4096, 4096).is_some());
    }
    assert!(a.alloc(4096, 4096).is_none());
}

#[test]
fn core_lists() {
    use crate::cores::{first, parse};
    assert_eq!(parse("0"), Some(1));
    assert_eq!(parse("4-7"), Some(0xf0));
    assert_eq!(parse("0,2,4-5"), Some(0b11_0101));
    assert_eq!(parse("7-4"), None);
    assert_eq!(parse("64"), None);
    assert_eq!(parse(""), None);
    assert_eq!(parse("a"), None);
    assert_eq!(parse("1,,2"), None);
    assert_eq!(parse("63"), Some(1 << 63));
    assert_eq!(first(0xf0, |c| c != 4), Some(5));
    assert_eq!(first(0xf0, |_| false), None);
}

#[test]
fn a_disk_is_a_file_a_partition_or_nothing() {
    let src = |d: &str| parse(&std::format!("kernel=/k\ndisk={d}\n")).map(|s| s.disk_source().clone_owned());
    assert_eq!(parse("kernel=/k\n").unwrap().disk_source(), DiskSource::None);
    assert_eq!(src("/boot/x.img"), Ok(Owned::File("/boot/x.img".into())));
    assert_eq!(src("/dev/nvme0n1p7"), Ok(Owned::Part(7)));
    assert_eq!(src("/dev/nvme0n1p128"), Ok(Owned::Part(128)));
}

#[test]
fn only_an_nvme_partition_may_be_named_as_a_device() {
    for bad in ["/dev/nvme0n1", "/dev/nvme0n1p", "/dev/nvme0n1p0", "/dev/nvme0n1p129", "/dev/nvme0n1p7a", "/dev/nvme0n1p-1", "/dev/nvme1n1p3", "/dev/sda1", "/dev/vda", "/dev/"] {
        assert_eq!(parse(&std::format!("kernel=/k\ndisk={bad}\n")), Err(Error::BadValue), "{bad}");
    }
}

#[test]
fn a_partition_is_never_copied_into_ram() {
    assert_eq!(parse("kernel=/k\ndisk=/dev/nvme0n1p7\ndisk_ram=1\n"), Err(Error::Conflict));
    assert!(parse("kernel=/k\ndisk=/dev/nvme0n1p7\ndisk_ram=0\n").is_ok());
    assert!(parse("kernel=/k\ndisk=/boot/g.img\ndisk_ram=1\n").is_ok());
}

#[test]
fn a_partition_spec_round_trips() {
    let s = parse("kernel=/k\ndisk=/dev/nvme0n1p7\n").unwrap();
    let mut b = [0u8; MAX_WIRE];
    let n = s.encode(&mut b).unwrap();
    assert_eq!(parse(core::str::from_utf8(&b[..n]).unwrap()).unwrap(), s);
}

#[derive(Debug, PartialEq)]
enum Owned {
    None,
    File(std::string::String),
    Part(u32),
}

trait CloneOwned {
    fn clone_owned(&self) -> Owned;
}

impl CloneOwned for DiskSource<'_> {
    fn clone_owned(&self) -> Owned {
        match self {
            DiskSource::None => Owned::None,
            DiskSource::File(f) => Owned::File((*f).into()),
            DiskSource::NvmePartition(n) => Owned::Part(*n),
        }
    }
}
