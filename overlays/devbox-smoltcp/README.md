# Akuma devbox-smoltcp

The **smoltcp** (native, in-kernel stack) counterpart to [`../devbox`](../devbox)
(rump). It boots the **same `devbox.img`** with a **rump-free** release kernel so you
can A/B the two network stacks with everything else held constant — same disk, same
userspace, same `curl`, same QEMU SLIRP/DNS. The only variable is the stack.

Why it exists: the rump stack pays a large per-syscall tax (a cross-process sysproxy
round-trip through a cooperatively-scheduled NetBSD kernel). This overlay is the clean
control for measuring that tax — see
[`docs/reference/subsystems/rump-stack.md`](../../docs/reference/subsystems/rump-stack.md)
"Rump tax vs native smoltcp" (measured ~8.7× on an HTTP GET, ~6× on HTTPS).

## Rump-free by construction

`run.sh` builds with `--no-default-features` (which drops `rump`, part of the default
feature set) and selects **no** `rump` / `rump-default` / `userspace-sshd` /
`rump-tests` feature. So the binary contains **zero** rump code and the runtime has no
rump_server, no `/dev/net/tap0`, no sysproxy, and no `RUMP_NIC`. Box 0 is native
smoltcp and the SSH server is the **built-in in-kernel** one (not the userspace
`/bin/sshd` the rump devbox uses).

Verify:

```bash
# 0 rump symbols (the rump devbox build has ~76):
nm target/aarch64-unknown-none/release/akuma | grep -c rump
```

## Quick start

```bash
# 1. Reuse the devbox image (build it once via the rump devbox bootstrap if needed).
overlays/devbox/bootstrap.sh          # only if devbox.img doesn't exist yet

# 2. Stage your SSH key once — the built-in SSH is publickey-only.
#    (devbox.img is shared, so this also covers the rump devbox.)
cp ~/.ssh/id_ed25519.pub bootstrap/etc/sshd/authorized_keys
DISK=devbox.img scripts/populate_disk.sh --etc-only     # needs Docker

# 3. Boot the smoltcp build (rump-free release kernel, no RUMP_NIC).
overlays/devbox-smoltcp/run.sh

# 4. SSH in on 2222 (built-in SSH, NIC0/smoltcp) with your key.
ssh -o StrictHostKeyChecking=no -i ~/.ssh/id_ed25519 -p 2222 root@localhost
```

Note the port differs from the rump devbox: **smoltcp SSH is `:2222`** (built-in SSH on
NIC0), whereas the rump devbox is `:2223` (userspace sshd over the rump tap NIC1). The
built-in SSH drops you in a limited command shell — for shell loops, invoke a binary
directly (e.g. `curl ...`) per connection.

## A/B example (the rump-tax measurement)

```bash
# smoltcp:  overlays/devbox-smoltcp/run.sh   → ssh -p 2222 -i ~/.ssh/id_ed25519 ... 'curl -w ... http://example.com/'
# rump:     overlays/devbox/run.sh           → ssh -p 2223 ...                       'curl -w ... http://example.com/'
```
