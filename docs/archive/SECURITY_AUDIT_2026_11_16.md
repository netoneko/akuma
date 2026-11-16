# Security audit before an EC2 AMI (2026-11-16)

**Status: PARTLY FIXED (2026-11-16).** The findings below are as written at audit time. What
has landed since, and what is still open, is in the next section.

## Status

Fixed on branch `ouch` (each verified as stated in its commit message):

| Finding | Commit | How it was checked |
|---|---|---|
| C1, C2 sshd key material | `41559adb` | AArch64 QEMU: the rebuilt sshd on a second port, three OpenSSH public-key logins; host key loaded from the existing file. The no-entropy refusal path was not exercised |
| C3 kernel entropy pool, M2 TCP seed and first ephemeral port | `31badd58` | RFC 8439 vector and pool host tests; AArch64 QEMU boot, `/dev/urandom` differs per read |
| H2, M9 host-root gate (`caller_is_host_root`) | `f9c13a54` | AArch64 QEMU: root keeps clock and mount; uid 1000 refused `date -s`, `mount`, `/dev/vda`, `reboot -f` |
| M7 validation bypass sealed before init | `ed65401d` | AArch64 QEMU boot suite unchanged |
| New: path strings must be EL0-readable (AArch64 kernel memory read through path arguments) | `c4a34af6` | boot suite and path syscalls; **the exploit probe itself was not run** |
| New: caller-chosen lengths sized infallible kernel allocations (unprivileged halt) | `c4a34af6` | `dd` from `/dev/zero` and `/dev/urandom`, pipes, `getdents`; the halt was not reproduced first |
| New: amd64 `reboot` had no gate at all | `f9c13a54` | build only; not booted on amd64 |
| `akuma-slot-table::with_active_mut` aliasing (`UNSAFE_SITE_REDUCTION.md` §3) | `7f35af4f` | nine kernel configurations build; AArch64 boot suite unchanged |
| Allocator canary unaligned reads (same §3) | `7f35af4f` | build only; the code is dead while canaries are off |

A deployable image must put `dac=1 net.allow=22` (plus whatever else it serves) on its boot line:
both are off without it.

Not verified anywhere yet: every amd64 change above has been built but not booted, and nothing
has run on the ryzen (the sanity run found it booted into another OS and stopped).

Still open:

- **H1, partly:** `dac=1` on the boot line now enforces mode bits from init (`467af9ff`), but
  it is not the default, sshd still logs every user in as uid 0, and services still run as root.
  Intermediate-directory search permission is still unchecked.
- **H3:** the program base, the dynamic linker, the stack top and the `mmap` area are randomized
  (`8a7fe7b0`, `25bc71ab`). Not randomized: non-PIE programs, the brk heap relative to its image,
  the amd64 stack top, and the kernel.
- **H4:** the AWS image carries no host key (`ee9670d0`), the metadata address is closed to
  everything but host root (`2e906d33`), and `/sbin/instance-key` (a herd oneshot in the AWS
  image) installs each instance's launch key at boot. The script is tested against a stand-in
  metadata service in a container (ten cases) and inside an Akuma guest; it has **not** run
  against real EC2 or Firecracker MMDS, and the image with the service in it has not been built.
  A builder who wants no baked key at all passes a `--pubkey` file that does not exist.
- **H5:** done as an opt-in. `net.allow=` installs the ingress filter (`2e906d33`); without the
  parameter nothing is filtered. There is no egress filter beyond the metadata address.
- **H6:** done (`ee9670d0`). The exact `apk add --keys-dir` command was run in an arm64 Alpine
  container: it installs with the container's keys and refuses with an empty key directory. The
  whole image build was not run.
- **M1** sshd rekeying, per-source limits, key file modes; **M3** quotas; **M4** panic triage;
  **M5** speculative-execution mitigations; **M6** AArch64 PAN; **M8** build the AMI with `server`.
- Every host key generated before `41559adb` must be regenerated; the fix does not rotate them.
- From `UNSAFE_SITE_REDUCTION.md` §3: the `akuma_switch` functions that are safe to call but
  whose obligations are doc-only. It is wider than the three named there:
  `build_closure_context`, `write_frame_word`, `setup_fake_irq_frame` and `irq_frame_at` take a
  bare stack address the same way. The fix that fits is a per-slot stack registry
  (`bind` by the code that allocates the stack, every accessor takes the slot and is
  bounds-checked against it), and it touches the context-switch path, so it needs its own
  measured change. `akuma-pagetable`'s amd64 `read_user`/`write_user` can move into `akuma-phys`
  (which already depends on `akuma-pagetable`) for one site fewer; left because only the
  Taoxichuan kernels call them and they could not be boot-tested here.
- Found while testing H4 and left alone: a descriptor is copied by value across `fork`, so
  `{ cmd1; cmd2; } > file` keeps only the last command's bytes (three external `echo`s wrote one
  line). It is `fdtable.md`'s pinned divergence 1, with its fix designed in
  `AKUMA_FDTABLE_EXTRACTION.md`; shell scripts written for Linux lose data on it silently.
- No boot-suite test pins the host-root gate or the bounded allocations yet; both were checked
  by hand over ssh.

**Question asked:** what has to be patched before Akuma can be published as an Amazon machine
image and exposed to the internet?

**Short answer:** not yet deployable. Three findings break SSH confidentiality or host identity
outright (C1 to C3) and must be fixed before any instance is reachable from an untrusted network.
After those, the largest gap is that the system has one privilege level in practice: every
service and every ssh session is uid 0, and file permission enforcement is off by default.

## Method and limits

A source read of the surfaces an attacker reaches, in this order: `userspace/sshd`, the kernel
entropy path, the user-pointer boundary, credential and privilege checks in
`crates/akuma-syscalls-glue`, device nodes, box isolation, network defaults, image build scripts.

Nothing was exploited or booted. Each finding says whether it was **confirmed in code** or
**needs a probe** on a running system.

Not covered: smoltcp internals, ext2 against a hostile disk image, the TLS stacks inside userspace
clients (`meow`, `apk`, `curl`), the SVM hypervisor and its device models, wifi, the rump stack,
the Taoxichuan kernels, and the amd64 native dispatch (`crates/akuma-kernel-amd64/src/usermode/`)
beyond its device nodes.

## Findings at a glance

| ID | Severity | Finding | Evidence |
|---|---|---|---|
| C1 | Critical | sshd's key-exchange secret is predictable from a value it sends in clear | confirmed |
| C2 | Critical | sshd host key has 64 bits of entropy, and a constant key if `getrandom` fails | confirmed |
| C3 | Critical | No kernel CSPRNG; AArch64 has no entropy source on EC2 | confirmed |
| H1 | High | Everything runs as uid 0; DAC enforcement is off by default | confirmed |
| H2 | High | Privileged operations are gated by box, not by uid, or not gated | confirmed, part needs a probe |
| H3 | High | No ASLR and no KASLR | confirmed |
| H4 | High | No per-instance identity: keys are baked at image build; IMDS is open to every process | confirmed |
| H5 | High | No packet filter; boxes on the native stack share the host's ports and routes | confirmed |
| H6 | High | The AWS image build installs packages with `--allow-untrusted` | confirmed |
| M1 | Medium | sshd: no rekeying, 24 pre-auth slots held 120 s each, private key file mode | confirmed |
| M2 | Medium | Predictable TCP seed and ephemeral ports | confirmed |
| M3 | Medium | Global resource tables with no per-box or per-user quota | confirmed |
| M4 | Medium | Kernel `panic = "abort"` with panic sites in syscall-reachable code | needs triage |
| M5 | Medium | No Meltdown/Spectre mitigations found | needs a probe |
| M6 | Medium | AArch64: no PAN found | needs a probe |
| M7 | Medium | `BYPASS_VALIDATION` exists in the user-access crate | needs a probe |
| M8 | Medium | Unauthenticated root shell on the serial console | confirmed |
| M9 | Medium | `clock_settime` has no privilege check | confirmed |

## Critical

### C1. sshd's key-exchange secret is predictable from the KEXINIT cookie

`userspace/sshd/src/protocol.rs`:

- `SshSession::new` seeds `session.rng` with `new_seeded_rng()` (line 136): a
  `SimpleRng`, which is xorshift64 with 64 bits of state
  (`userspace/akuma-ssh-crypto/src/crypto.rs:42`).
- `build_kexinit` (line 1448) fills the 16-byte cookie from that generator and sends it
  unencrypted.
- `handle_kex_ecdh_init` (line 1470) fills the 32-byte X25519 server secret from the same
  generator.

xorshift64 outputs its own state. Bytes 8..16 of the cookie are the generator state after the
second step, so the next four outputs, which are the server's ephemeral secret, follow by
running the generator forward. Unencrypted packets use zero padding (`build_packet`), so no
other draw sits between the cookie and the secret.

**Impact.** Anyone who can observe the handshake computes the shared secret and all six derived
keys. They can read every session, and an on-path attacker can also forge packets into an
authenticated root session. Recorded past sessions are equally exposed.

The client had the same flaw and was fixed (`userspace/sshd/docs/SECURITY_IMPROVEMENTS.md` §4);
that document states the server's use was only "the KEXINIT cookie, anti-fingerprinting padding".
That statement is wrong for the server.

**Fix.**
1. In `handle_kex_ecdh_init`, read the 32 secret bytes from `libakuma::getrandom` and abort the
   connection on failure, as `client/protocol.rs:272` does.
2. Fill the cookie from `getrandom` as well, or replace `SimpleRng` with a real CSPRNG (ChaCha20
   keyed from 32 bytes of `getrandom`) so no secret-adjacent value ever comes from a 64-bit state.
3. Add a host test that fails if the ECDH secret is a function of the cookie.

### C2. sshd host key: 64 bits of entropy, constant on entropy failure

`userspace/sshd/src/keys.rs:58` `generate_keypair` takes all 32 secret-key bytes from
`new_seeded_rng()`. `userspace/sshd/src/crypto.rs:20` builds that generator as:

```rust
let mut seed = [0u8; 8];
let _ = libakuma::getrandom(&mut seed);
SimpleRng::from_seed(seed)
```

- The Ed25519 host key is a deterministic function of 8 bytes.
- If `getrandom` fails the seed stays zero, `from_seed` substitutes `0xDEAD_BEEF_CAFE_BABE`, and
  the host key is **the same on every such machine**. Anyone with the source can impersonate the
  host. C3 shows this is the expected case on an AArch64 EC2 instance.

**Fix.** Generate from 32 bytes of `getrandom`; refuse to start if it fails. Treat every host key
generated by the current code as compromised and regenerate it. See H4 for the per-instance rule.

### C3. No kernel CSPRNG; AArch64 has no entropy source on EC2

- `getrandom` (`crates/akuma-syscalls-glue/src/proc.rs:1480`) hands out raw device output: the
  registered hook, else virtio-rng. There is no pool, no mixing and no reseeding.
- **AArch64:** the only source is virtio-rng (`crates/akuma-kernel-glue/src/lib.rs:1028`). A Nitro
  instance has no virtio devices. `getrandom` returns `EIO`, `AT_RANDOM` becomes sixteen `0xA5`
  bytes (`crates/akuma-elf/src/stack.rs:320`), so every process has the same stack canary, and
  C2's constant host key follows.
- **amd64:** the source is `RDRAND` alone (`crates/akuma-kernel-amd64/src/net.rs:197`). On a CPU
  without it, `weak_fill` (SplitMix64 of a counter and the TSC) is returned **as success**. EC2
  CPUs have `RDRAND`, so this is a design weakness here rather than a live hole.

**Fix.** One kernel CSPRNG (ChaCha20) in a leaf crate, seeded at boot from every source present:
`RDRAND`/`RDSEED`, `RNDR` on ARMv8.5 (Graviton3 and later), virtio-rng, plus timer jitter. Reseed
periodically. `getrandom` and `AT_RANDOM` read from it and fail closed until it is seeded. Remove
the "weak but complete" success path.

## High

### H1. One privilege level in practice

- sshd parses the username and ignores it for anything but the signature
  (`userspace/sshd/src/auth.rs`); `session_env` hard-codes `USER=root`
  (`protocol.rs:389`). Every session is uid 0 in box 0.
- herd services run as root unless boxed with a user.
- Mode-bit enforcement exists but starts off (`/proc/dac`,
  `docs/reference/subsystems/permissions.md`). With it off, any uid reads and writes any file.
- Known under-deny even when on: search permission on intermediate directories is not checked.

**Impact.** Any bug in any network-facing program is a full compromise. There is no second wall.

**Fix.** Turn DAC on by default (the prerequisite, fixing x bits on existing disks, is in
`permissions.md`). Give sshd a real user lookup and `setgroups`/`setgid`/`setuid` before exec.
Run network services under their own uid or in a box. Write the host key and `authorized_keys`
with mode 0600/0644 owned by root (`libakuma::open` has no mode argument today).

### H2. Privileged operations gated by box, not uid, or not at all

The uid model has four root checks in the whole syscall layer: `chroot`, `sethostname`, `capget`
reporting, and the `/proc/dac` write. Everything else privileged is decided by box membership:

| Operation | Gate today | File |
|---|---|---|
| `reboot` | caller in box 0, any uid | `crates/akuma-syscalls-glue/src/reboot.rs:11` |
| `mount`/`umount2` | `caller_may_mount`: box 0, any uid | `container.rs:161` |
| raw block device open (`/dev/vdX`) | box 0, any uid; bypasses file permissions | `fs.rs:2333` |
| `clock_settime`, `adjtimex` | **none** (M9) | `crates/akuma-syscalls-time/src/lib.rs:365` |
| amd64 `/dev/kvm0`, `/dev/power`, `/dev/wifi0`, `/dev/fb0`, `/dev/net/tun` | no check at the open site | `crates/akuma-kernel-amd64/src/chardev.rs:31` |
| `/proc/<pid>/*` | same box; no uid check | `crates/akuma-vfs-glue/src/proc.rs:935` |

**Needs a probe:** whether the amd64 character devices are reachable from inside a box.
`fs.rs:4065` says `/dev` is "resolved ahead of the mount table", and `chardev.rs` has no `box_id`
test, so a boxed process can probably open the hypervisor control device and the power device.

**Fix.** Add a `require_host_root()` helper (box 0 **and** uid 0) and call it from each row above.
Add a boot-suite test per row, driven from a dropped uid and from a box, on the model of
`test_box_isolation_syscall_guards`.

### H3. No address-space randomization

User: `PIE_BASE = 0x1000_0000` (`crates/akuma-elf/src/load.rs:38`), `INTERP_BASE = 0x3000_0000`
(`types.rs:123`), fixed stack top and mmap base. Kernel: fixed link address. A memory-corruption
bug in any service is exploitable with fixed addresses. Combined with the constant canary in C3
on AArch64, stack protection is nominal.

**Fix.** Randomize PIE base, interpreter base, mmap base and stack top per exec from the CSPRNG
in C3. KASLR is a larger job and can follow.

### H4. Instance identity and the metadata service

- `authorized_keys` is written at image build time
  (`overlays/devbox-firecracker-aws/build-rootfs-image.sh:328`). There is no first-boot step that
  reads the launch key pair from IMDS. A public AMI would carry the builder's key.
- The host key is generated on first sshd start and persisted. If an image is captured after a
  boot, every instance launched from it shares one host key.
- Every process, including boxed ones on the native stack, can reach `169.254.169.254`. If the
  instance has an IAM role, any code execution yields its credentials.

**Fix.** A first-boot service that fetches the public key over IMDSv2 and writes
`authorized_keys`; an image-build step that deletes `/etc/sshd/id_ed25519*`; a rule that blocks
the link-local metadata address for everything but that service (needs H5).

### H5. No packet filter

The native stack has no ingress or egress filtering. Any process binds any port, privileged ports
included, with no uid check. Boxes that use the host stack are not network-isolated from the host.
The EC2 security group is the only control.

**Fix.** A minimal default-deny ingress table (allow established, allow the sshd port), a
privileged-port check on `bind`, and per-box egress rules.

### H6. Unverified packages in the image build

`overlays/devbox-firecracker-aws/build-rootfs-image.sh:244` runs `apk add ... --allow-untrusted`.
Package signatures are not checked, so a compromised mirror or path yields a compromised AMI.

**Fix.** Install the Alpine signing keys into the target root first and drop the flag. Pin the
repository to HTTPS and a release branch.

## Medium

- **M1. sshd.** No rekeying: one AES-128-CTR key and one 32-bit sequence counter for the whole
  session. `max_sessions = 24` counts pre-auth connections, each held up to 120 s, and the kernel
  listener backlog is 8, so one remote host can keep legitimate users out with a few idle
  connections. No per-source limit. Key files are created with the default mode. The
  `emergency-paws` and `builtin-paws` fallbacks give a shell other than the configured one; build
  the AMI's sshd without them. Strict-KEX is absent; with `aes128-ctr` and a non-EtM MAC the
  published Terrapin attack does not apply, but add it when the suite grows.
- **M2. Predictable network values.** `config.random_seed = uptime_us`
  (`crates/akuma-net/src/smoltcp_net/init.rs:97`), so TCP initial sequence numbers derive from
  boot time. Ephemeral ports count up from 49152 (`consts.rs:57`), which removes source-port
  entropy from DNS queries. Seed both from the CSPRNG. The DNS query id source was not traced.
- **M3. No quotas.** `MAX_PROCESSES = 64` and the socket table are global. One box or user can
  exhaust them. There are no rlimits that bind and no memory limit per box.
- **M4. Kernel panics.** The kernel is `panic = "abort"`, so a reachable panic stops the machine.
  A grep for `unwrap`/`expect`/`panic!`/`unreachable!` outside tests finds about 39 sites in
  `akuma-exec`, 21 in `akuma-ext2`, 9 in `akuma-syscalls-glue`. Each needs a reachability verdict.
  Release builds also have overflow checks off, so arithmetic on user-supplied lengths wraps
  silently; two such bugs were already found this way (`AKUMA_EXTRACT_MMAP.md` §10.1).
- **M5. Speculative execution.** No KPTI, retpoline or IBRS/IBPB handling was found. This matters
  once untrusted local code runs (boxes, guests) on affected CPUs.
- **M6. AArch64 PAN.** amd64 enables SMEP and SMAP (`uaccess.rs:186`). No `PSTATE.PAN` use was
  found for AArch64, so a kernel path that dereferences a user pointer raw would not fault there.
- **M7. `BYPASS_VALIDATION`.** The per-thread switch that lets a kernel address pass as a user
  pointer lives in `akuma-user-access` unconditionally. Confirm it cannot be set in a `no-tests`
  image, then put the static and every `load` behind `cfg(kernel_tests)`. Ship only `no-tests`.
- **M8. Serial console.** A `console = true` herd service is `/bin/sh` as root with no login
  (`overlays/ryzen/rootfs/etc/herd/enabled/console.conf`). The EC2 serial console would expose it
  to anyone with that IAM permission. Build the AMI with the `server` feature, which has no
  interactive console.
- **M9. `clock_settime`.** No privilege check, including from a box. A boxed process can move the
  host clock, which affects certificate validation and timers everywhere. Covered by H2's helper.

## What is already sound

- Authentication is public key only, Ed25519 only, with low-order point rejection; the
  auth-bypass config flag is inert unless a Cargo feature is compiled in.
- No in-kernel ssh, shell or crypto. No `ptrace`, no `process_vm_readv`, no set-uid on exec.
- User copies validate the range and recover from faults; the check is folded into the copy
  helpers rather than optional (`crates/akuma-user-access`).
- amd64 has NX, SMEP and SMAP on.
- Box escapes found in August are fixed and pinned by a boot-suite test
  (`BOX_ISOLATION_SECURITY_FIXES.md`).
- 79 crates forbid `unsafe`, with a ratchet on growth.
- A signed-exec policy exists as middleware (`crates/akuma-mw-signed-exec`); it is not wired into
  the main kernels.

## Patch order

1. C1, C2: sshd reads key material from `getrandom` and fails closed. Small, userspace only.
   Regenerate every existing host key afterwards.
2. C3: kernel CSPRNG with `RNDR`/`RDSEED`/virtio/jitter seeding; fixes `AT_RANDOM` and M2.
3. H2 + M9: `require_host_root()` and the tests.
4. H4, H6, M8: image build rules (first-boot key fetch, no baked host key, verified packages,
   `server` feature, sshd without the paws fallbacks).
5. H1: DAC on by default, sshd user switching, services off uid 0.
6. H3: user ASLR.
7. H5, M1, M3: packet filter, sshd limits, quotas.
8. M4 to M7: triage and probes.

Items 1 to 4 are the minimum for a private AMI behind a restrictive security group. A public AMI
needs all of 1 to 7.

Separately from security: no ENA network driver exists in the tree, so a Nitro instance has no
network today. The existing AWS path is Firecracker on a metal instance.

## Formal verification for the critical paths

The tree is already shaped for this: the memory-management decisions sit in small, pure crates
with no dependencies and no `unsafe`. Three tools fit, at different costs. None has been run
against the tree yet.

### Kani (bounded model checking) — recommended first

Kani checks ordinary Rust. A proof is a function marked `#[kani::proof]` under `#[cfg(kani)]`
that takes symbolic inputs (`kani::any()`) and asserts a property; Kani checks it for every input
and also reports any panic or arithmetic overflow on the way. The kernel build never sees it.

It suits code whose inputs are a few integers or small fixed structures. It handles loops only up
to a stated bound, does not model concurrency, cannot see through inline assembly, and gets slow
on `Vec`-heavy code.

| Crate | Property to prove | Why here |
|---|---|---|
| `akuma-syscalls-mem` | no overflow for any `(addr, len, prot, flags)`; a `MAP_FIXED` range never overlaps kernel VA | both known overflows in this crate are exactly what Kani reports unprompted |
| `akuma-user-access` (range check only) | an accepted `(ptr, len)` lies wholly below `USER_VA_LIMIT` and does not wrap | the single gate between user pointers and kernel memory |
| `akuma-cow` | the decision table is total and ordered as documented | three inputs; exhaustive |
| `akuma-mmap` (`span`, `holes`, `place`) | clip-and-split conserves bytes; results never overlap; a placed mapping is inside `[mmap_base, mmap_limit)` | silent corruption class |
| `akuma-isolation` (`creds`, `dac`, `subdir_fs::confine`) | no transition raises privilege; a confined path never leaves the root | the privilege boundary H1 and H2 will lean on |
| `akuma-fdtable` | lowest-free allocation; no fd is live twice after dup/close/fork | bounded table size |
| `akuma-pmm` (`cow_table`, `ledger`), `akuma-user-space` | a refcount never underflows; a frame is not freed while counted | the premature-free bug class |

### Verus — for one or two unbounded proofs

Verus proves properties for all sizes, not up to a bound, and has been used on page tables in
research kernels. The cost is real: the code must be written in Verus's dialect with
specifications and proof annotations, it pins its own compiler toolchain, and proof upkeep on
every change is ongoing work. It would not go through the self-hosted build.

Use it only where a bounded check is not convincing: the page-table map/unmap walker
(`akuma-mmu`, `akuma-pagetable`), with the property that mapping a user page never alters a
kernel entry and that unmap removes exactly the requested range. Start it after the Kani proofs
exist, since they will show whether the remaining risk justifies the cost.

### Miri and Loom — for the `unsafe` and concurrent leaves

- **Miri** runs the existing host tests under an interpreter that detects undefined behaviour.
  It is the right tool for the crates that cannot forbid `unsafe` but have host tests:
  `akuma-locks-rw-cell`, `akuma-dma`, `akuma-frame-raw`. No code changes are needed.
- **Loom** explores thread interleavings. `akuma-bkl` and `akuma-locks-rw` already carry
  hand-written model checkers; Loom would cover lock-free tables that have none.

### Suggested start

One CI job running Kani on `akuma-syscalls-mem` and the `akuma-user-access` range check, plus a
Miri run of the three crates above. Both are additive and need no kernel change. Extend the Kani
table row by row; decide on Verus after that.

## Background

- [`BOX_ISOLATION_SECURITY_FIXES.md`](BOX_ISOLATION_SECURITY_FIXES.md) — the August box audit.
- [`UNIX_PERMISSIONS.md`](UNIX_PERMISSIONS.md) — the credential model and why enforcement starts off.
- [`SYSCALL_HARDENING.md`](SYSCALL_HARDENING.md), [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) — the user-pointer boundary.
- [`../../userspace/sshd/docs/SECURITY_IMPROVEMENTS.md`](../../userspace/sshd/docs/SECURITY_IMPROVEMENTS.md) — the client audit whose §4 fix was not applied to the server.
- [`../reference/subsystems/permissions.md`](../reference/subsystems/permissions.md),
  [`../reference/subsystems/rng.md`](../reference/subsystems/rng.md),
  [`../reference/subsystems/ssh.md`](../reference/subsystems/ssh.md) — current state.
