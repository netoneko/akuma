//! `vm = true`: a KVM guest as a herd service — the config keys, pure.
//!
//! herd does not talk to the hypervisor. It turns the `vm_*` keys into an
//! [`akuma_vm_spec::Spec`], writes the spec's wire text to a file, and supervises one
//! ordinary process, `herd vm-run <file>`, which creates the VM through `/dev/kvm0`, copies its
//! console to stdout (so `/var/log/herd/<name>.log` is the guest's console) and exits when the
//! guest does. The VM is "running" while that process is; killing the process ends the guest
//! (the kernel ties the VM's lifetime to its creator) and the usual `restart`/`max_retries`
//! policy brings it back. Nothing here is specific to KVM except the `vm_type` default: the
//! same keys will drive a Firecracker backend.
//!
//! | key | spec field |
//! |---|---|
//! | `vm = true` | this service is a VM |
//! | `vm_type` | backend: `kvm` (default) or `firecracker` (reserved) |
//! | `vm_kernel` | kernel image |
//! | `vm_cmdline` | guest command line tail (rest of the line, spaces kept) |
//! | `vm_ram_mib` | guest RAM, 16-1024 |
//! | `vm_vcpus` | virtual processors, 1-4 |
//! | `vm_disk` | disk image path (empty: the machine's second virtio-blk) |
//! | `vm_disk_ram` | copy the disk into RAM at start |
//! | `vm_forward` | `host:guest`, repeatable |
//! | `vm_config` | a Firecracker JSON config (`boot-source`, `machine-config`, `drives`) read first; explicit `vm_*` keys override it |

use akuma_vm_spec::{Error, Spec};
use alloc::string::String;
use alloc::vec::Vec;

/// Where `vm-run` specs are written (one `<service>.vm` each).
pub const SPEC_DIR: &str = "/var/run/herd";
/// The helper a VM service really runs: herd itself, as `herd vm-run <spec file>`.
pub const HELPER: &str = "/bin/herd";

/// Why a service's `vm_*` keys do not make a VM.
#[derive(Debug, PartialEq, Eq)]
pub enum VmError {
    /// A key the spec refused (the key, why).
    Key(String, Error),
    /// `vm_config` named a file that could not be read.
    ConfigUnreadable(String),
    /// `vm_config` is not JSON this importer can read, or has no kernel.
    ConfigInvalid,
    /// The spec is incomplete (no kernel).
    Spec(Error),
    /// A backend this herd cannot run.
    Unsupported(&'static str),
}

impl VmError {
    /// One line for the log.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Key(k, e) => alloc::format!("{k}: {}", e.describe()),
            Self::ConfigUnreadable(p) => alloc::format!("vm_config: cannot read {p}"),
            Self::ConfigInvalid => String::from("vm_config: not a usable Firecracker config"),
            Self::Spec(e) => String::from(e.describe()),
            Self::Unsupported(b) => alloc::format!("vm_type {b} is not supported by this kernel"),
        }
    }
}

/// The `vm_*` keys of one service config, gathered as the file is read.
#[derive(Default)]
pub struct VmConfig {
    enabled: bool,
    config_path: Option<String>,
    pairs: Vec<(String, String)>,
}

impl VmConfig {
    /// Offer one `key = value` line. True if it was a VM key (consumed).
    pub fn apply(&mut self, key: &str, value: &str) -> bool {
        match key {
            "vm" => {
                self.enabled = matches!(value, "true" | "1" | "yes");
                true
            }
            "vm_config" => {
                self.config_path = Some(String::from(value));
                true
            }
            _ => match key.strip_prefix("vm_") {
                Some(rest) => {
                    self.pairs.push((String::from(rest), String::from(value)));
                    true
                }
                None => false,
            },
        }
    }

    /// Does this config describe a VM at all?
    #[must_use]
    pub fn is_vm(&self) -> bool {
        self.enabled
    }

    /// Build the spec: the Firecracker config first (read through `read`), then every explicit
    /// key in file order. `Ok(None)` when `vm` is not set (stray `vm_*` keys without it are
    /// ignored, like any key of a feature that is off).
    pub fn finish(&self, read: impl Fn(&str) -> Option<String>) -> Result<Option<Spec>, VmError> {
        if !self.enabled {
            return Ok(None);
        }
        let mut spec = Spec::default();
        if let Some(path) = &self.config_path {
            let doc = read(path).ok_or_else(|| VmError::ConfigUnreadable(path.clone()))?;
            from_firecracker(&doc, &mut spec)?;
        }
        for (k, v) in &self.pairs {
            spec.set(k, v).map_err(|e| VmError::Key(alloc::format!("vm_{k}"), e))?;
        }
        spec.validate().map_err(VmError::Spec)?;
        if !spec.backend.is_supported() {
            return Err(VmError::Unsupported(spec.backend.name()));
        }
        Ok(Some(spec))
    }
}

/// Apply the subset of a Firecracker config this hypervisor has: the kernel image and boot
/// args, the machine's vCPU count and memory, and the first drive's path. Network interfaces
/// are ignored (a guest reaches the host through the built-in gateway; forwards are
/// `vm_forward`), as is everything else.
pub fn from_firecracker(doc: &str, spec: &mut Spec) -> Result<(), VmError> {
    use boxlib::json::{number_at, string_at, strings_at};
    let kernel = string_at(doc, &["boot-source", "kernel_image_path"]).ok_or(VmError::ConfigInvalid)?;
    let key = |k: &str, e| VmError::Key(String::from(k), e);
    spec.set("kernel", &kernel).map_err(|e| key("kernel_image_path", e))?;
    if let Some(args) = string_at(doc, &["boot-source", "boot_args"]) {
        spec.set("cmdline", &args).map_err(|e| key("boot_args", e))?;
    }
    if let Some(n) = number_at(doc, &["machine-config", "vcpu_count"]) {
        spec.set("vcpus", &alloc::format!("{n}")).map_err(|e| key("vcpu_count", e))?;
    }
    if let Some(n) = number_at(doc, &["machine-config", "mem_size_mib"]) {
        spec.set("ram_mib", &alloc::format!("{n}")).map_err(|e| key("mem_size_mib", e))?;
    }
    if let Some(path) = strings_at(doc, &["drives", "*", "path_on_host"]).first() {
        spec.set("disk", path).map_err(|e| key("path_on_host", e))?;
    }
    Ok(())
}

/// The file `herd vm-run` reads for service `name`.
#[must_use]
pub fn spec_path(name: &str) -> String {
    alloc::format!("{SPEC_DIR}/{name}.vm")
}

#[cfg(test)]
mod tests {
    use super::*;
    use akuma_vm_spec::{Backend, Forward};

    fn conf(lines: &[(&str, &str)]) -> VmConfig {
        let mut c = VmConfig::default();
        for (k, v) in lines {
            assert!(c.apply(k, v), "{k} should be a VM key");
        }
        c
    }

    fn no_files(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn the_gentoo_conf() {
        let c = conf(&[
            ("vm", "true"),
            ("vm_kernel", "/boot/akuma-guest"),
            ("vm_disk", "/boot/gentoo-guest.img"),
            ("vm_disk_ram", "true"),
            ("vm_ram_mib", "768"),
            ("vm_vcpus", "1"),
            ("vm_forward", "2223:4444"),
            ("vm_cmdline", "init=/usr/bin/bash initargs=/akuma-kvm-init.sh"),
        ]);
        let s = c.finish(no_files).unwrap().unwrap();
        assert_eq!(s.kernel.as_str(), "/boot/akuma-guest");
        assert_eq!(s.disk.as_str(), "/boot/gentoo-guest.img");
        assert!(s.disk_ram);
        assert_eq!(s.cmdline.as_str(), "init=/usr/bin/bash initargs=/akuma-kvm-init.sh");
        assert_eq!(s.forwards(), &[Forward { host: 2223, guest: 4444 }]);
        assert_eq!(s.backend, Backend::Kvm);
    }

    #[test]
    fn vm_type_defaults_to_kvm_and_firecracker_is_unsupported() {
        let c = conf(&[("vm", "true"), ("vm_type", "kvm"), ("vm_kernel", "/k")]);
        assert_eq!(c.finish(no_files).unwrap().unwrap().backend, Backend::Kvm);
        let c = conf(&[("vm", "true"), ("vm_type", "firecracker"), ("vm_kernel", "/k")]);
        assert_eq!(c.finish(no_files), Err(VmError::Unsupported("firecracker")));
        let c = conf(&[("vm", "true"), ("vm_type", "qemu"), ("vm_kernel", "/k")]);
        assert!(matches!(c.finish(no_files), Err(VmError::Key(k, Error::BadValue)) if k == "vm_type"));
    }

    #[test]
    fn forward_is_repeatable_and_limited() {
        let c = conf(&[("vm", "1"), ("vm_kernel", "/k"), ("vm_forward", "2223:22"), ("vm_forward", "8080:80")]);
        assert_eq!(c.finish(no_files).unwrap().unwrap().forwards().len(), 2);
        let c = conf(&[("vm", "1"), ("vm_kernel", "/k"), ("vm_forward", "1:1"), ("vm_forward", "2:2"), ("vm_forward", "3:3"), ("vm_forward", "4:4"), ("vm_forward", "5:5")]);
        assert!(matches!(c.finish(no_files), Err(VmError::Key(_, Error::TooManyForwards))));
    }

    #[test]
    fn rejections_name_the_key() {
        for (k, v, e) in [
            ("vm_vcpus", "9", Error::OutOfRange),
            ("vm_ram_mib", "4096", Error::OutOfRange),
            ("vm_ram_mib", "lots", Error::BadValue),
            ("vm_disk_ram", "maybe", Error::BadValue),
            ("vm_forward", "22", Error::BadValue),
            ("vm_bogus", "1", Error::UnknownKey),
        ] {
            let c = conf(&[("vm", "true"), ("vm_kernel", "/k"), (k, v)]);
            assert_eq!(c.finish(no_files), Err(VmError::Key(String::from(k), e)), "{k}={v}");
        }
    }

    #[test]
    fn a_vm_needs_a_kernel_and_a_ram_disk_needs_a_disk() {
        assert_eq!(conf(&[("vm", "true")]).finish(no_files), Err(VmError::Spec(Error::Missing)));
        let c = conf(&[("vm", "true"), ("vm_kernel", "/k"), ("vm_disk_ram", "true")]);
        assert_eq!(c.finish(no_files), Err(VmError::Spec(Error::Missing)));
    }

    #[test]
    fn without_vm_true_there_is_no_vm() {
        assert_eq!(conf(&[("vm_kernel", "/k")]).finish(no_files), Ok(None));
        assert_eq!(conf(&[("vm", "false"), ("vm_kernel", "/k")]).finish(no_files), Ok(None));
        let mut c = VmConfig::default();
        assert!(!c.apply("command", "/bin/x"));
        assert!(!c.apply("restart", "true"));
    }

    const FC: &str = r#"{
      "boot-source": {"kernel_image_path": "/fc/vmlinux", "boot_args": "console=ttyS0 root=/dev/vda"},
      "drives": [{"drive_id": "rootfs", "path_on_host": "/fc/root.ext4", "is_root_device": true, "is_read_only": false}],
      "machine-config": {"vcpu_count": 2, "mem_size_mib": 512},
      "network-interfaces": [{"iface_id": "eth0", "host_dev_name": "tap0"}]
    }"#;

    #[test]
    fn a_firecracker_config_is_read_and_explicit_keys_win() {
        let read = |p: &str| (p == "/etc/fc.json").then(|| String::from(FC));
        let c = conf(&[("vm", "true"), ("vm_config", "/etc/fc.json"), ("vm_ram_mib", "768"), ("vm_forward", "2223:22")]);
        let s = c.finish(read).unwrap().unwrap();
        assert_eq!(s.kernel.as_str(), "/fc/vmlinux");
        assert_eq!(s.cmdline.as_str(), "console=ttyS0 root=/dev/vda");
        assert_eq!(s.vcpus, 2);
        assert_eq!(s.ram_mib, 768, "the explicit key overrides mem_size_mib");
        assert_eq!(s.disk.as_str(), "/fc/root.ext4");
        assert_eq!(s.forwards().len(), 1);
    }

    #[test]
    fn firecracker_config_failures() {
        let c = conf(&[("vm", "true"), ("vm_config", "/nope")]);
        assert_eq!(c.finish(no_files), Err(VmError::ConfigUnreadable(String::from("/nope"))));
        let read = |_: &str| Some(String::from("{\"machine-config\": {\"vcpu_count\": 1}}"));
        assert_eq!(c.finish(read), Err(VmError::ConfigInvalid));
        let read = |_: &str| Some(String::from("not json"));
        assert_eq!(c.finish(read), Err(VmError::ConfigInvalid));
        let read = |_: &str| Some(String::from(r#"{"boot-source":{"kernel_image_path":"/k"},"machine-config":{"vcpu_count":8}}"#));
        assert!(matches!(c.finish(read), Err(VmError::Key(k, Error::OutOfRange)) if k == "vcpu_count"));
    }

    #[test]
    fn the_spec_file_path_and_the_wire_round_trip() {
        assert_eq!(spec_path("vm-gentoo"), "/var/run/herd/vm-gentoo.vm");
        let c = conf(&[("vm", "true"), ("vm_kernel", "/k"), ("vm_cmdline", "a b c"), ("vm_forward", "1:2")]);
        let s = c.finish(no_files).unwrap().unwrap();
        let mut buf = [0u8; akuma_vm_spec::spec::MAX_WIRE];
        let n = s.encode(&mut buf).unwrap();
        assert_eq!(Spec::parse_wire(core::str::from_utf8(&buf[..n]).unwrap()), Ok(s));
    }
}
