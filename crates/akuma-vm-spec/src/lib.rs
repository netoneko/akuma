//! What a VM is, before anything runs it.
//!
//! One vocabulary for the three parties that must agree on it: **herd** builds a [`Spec`]
//! from a service config (`vm_kernel = …`), **the kernel** parses the same text off
//! `/dev/kvm0` (`amd64/src/kvm/ctl.rs`) and runs it, and a future **Firecracker** backend
//! reads the same fields from a Firecracker JSON config. The keys are Firecracker's own
//! words where it has them ([`Spec::set`] accepts `kernel_image_path`, `boot_args`,
//! `vcpu_count`, `mem_size_mib`, `path_on_host` as aliases), so a config written for one
//! backend needs no translation for the other. Port forwards are ours: Firecracker
//! attaches a tap, this hypervisor's gateway forwards ports.
//!
//! | module | |
//! |---|---|
//! | [`spec`] | [`Spec`]: fixed-size, `Copy`, no allocation; `set` one key, `parse_wire` a document, `encode` it back |
//! | [`arena`] | [`arena::Arena`]: first-fit extents over the reserved guest-RAM block (`kvmmem=`) |
//! | [`cores`] | the `kvmcores=` list as a bit mask |
//!
//! `no_std`, `forbid(unsafe_code)`, no dependencies: the kernel parses on a path that runs
//! under memory pressure and must not allocate.

#![no_std]
#![forbid(unsafe_code)]

pub mod arena;
pub mod cores;
pub mod spec;

pub use spec::{Backend, Error, Forward, Spec, Text};

#[cfg(test)]
mod tests;
