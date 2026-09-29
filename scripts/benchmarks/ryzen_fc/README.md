# ryzen_fc — the self-host `-j4` build gate on the Ryzen's Firecracker

Everything here runs **on ryzen** (`ssh ryzen`, root), in `/home/netoneko/akuma-selfhost/`,
and was the exact set of scripts that produced the 2026-09-29 11/11 result in
`docs/runbooks/selfhost-kernel-build-amd64.md` § "Second rig: ryzen". It never
touches the live guest (`/home/netoneko/akuma/akuma-vm.json`, tap0, kot): its VM
is `selfhost-vm.json`, its tap is `tapsh` (10.0.2.2/24), its disk is `selfhost.img`.

| script | run as | does |
|---|---|---|
| `stage1.sh` | netoneko | native kernel build, musl-host nightly toolchain (+ `x86_64-unknown-none`, `rust-src`), `cargo vendor` |
| `stage2.sh` | root | `amd64/mkdisk.sh` a 4 GiB image, loop-mount, toolchain to `/usr/local/rust`, source to `/src/github.com/netoneko/akuma`, vendor, `kbuild`, `akuma-dev.env` |
| `stage3.sh` | root | the host-linker trap: `libgcc_s` (Alpine apk) + musl `libc.so` into `/usr/lib` and the rustlib dirs, `linker =` ld.lld in the guest's `~/.cargo/config.toml` |
| `fcrun.sh [vcpu] [mem]` | root | (re)boot the guest; waits for the old VM to release the tap |
| `gssh.sh '<cmd>'` | any | run a command in the guest with the env preamble it needs (its shell has no PATH) |
| `jrun.sh N [jobs]` | root | one fresh-boot `kbuild -c -j<jobs>`; PASS / FAIL / WEDGE / TIMEOUT / NOBOOT; writes `runs/N/{summary,build.out,console.log}` |
| `batch.sh FIRST LAST [jobs]` | root | `jrun.sh` in sequence |

Source is `git clone --depth 1 --branch <branch> https://github.com/netoneko/akuma.git`
plus `git submodule update --init --depth 1 --force crates/akuma-fbcon/vendor/spleen`
— **not** rsynced from a laptop (ryzen is on wifi; it dropped for ~4 min during
the first batch). Run detached (`setsid nohup … &`) so a dropped ssh does not
end a batch.
