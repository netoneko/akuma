# akuma-syscalls-net

The read-only network-introspection surface `busybox ifconfig` reads, as pure
marshalling. `#![forbid(unsafe_code)]`, `#![no_std]`, one dependency
(`akuma-syscalls-linux`, for the ABI shapes).

## Why

`ifconfig` with no arguments reads `/proc/net/dev` to enumerate interfaces,
then issues `SIOCGIFADDR` / `SIOCGIFFLAGS` / … on each; `ifconfig -a` also uses
`SIOCGIFCONF`. All of it is byte layout — which field of `struct ifreq` a
command fills, the 40-byte `SIOCGIFCONF` record stride (not the 24 bytes it
uses — the trap `docs/reference/subsystems/networking.md` records), the exact
`/proc/net/dev` column format. That layout is identical on every architecture,
and it used to live only inside the aarch64 kernel's `akuma-syscalls-glue`,
which does not build for `x86_64-unknown-none`.

Both kernels now consume this crate — `akuma-syscalls-glue::net` and
`amd64/src/fd.rs` — so they cannot drift. It does not read or write user
memory, look up an interface, or know what a socket is: the caller supplies the
`Interface` list (from `akuma_net::smoltcp_net::interface_snapshot()`) and does
the copies; this decides the bytes.

Read-only: no `SIOCSIF*`, no netlink.
