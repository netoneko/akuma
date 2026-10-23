# KVM: BKL out of exit handling, the bulk-download wedge, NetBSD as a guest (2026-10-23)

Three things from one session on branch `kvm-part-1`. Follows `KVM_CPU_ACCOUNTING.md` (Part 2: one
task per vCPU core).

## 1. The BKL is out of a service guest's exit handling (`61e8d96b`)

**Change.** `serve` (`amd64/src/kvm/linux/machine.rs`) runs inside one dropped-BKL window (`Unlocked`)
when the guest is a service. Two vCPU groups' exits therefore contend on the device lock alone.
The BKL is taken, always *after* the device lock and never before it, for:

* `HostDisk` read/write/flush/size except a RAM copy (`HostDisk::bkl`, a `DroppedWindowPause`);
* `ctl::should_stop`'s foreign-pid lookup (every 200 ms; `lookup_process_shared` says foreign
  pids belong on the BKL path);
* `yield_now`'s own switch (it takes the lock itself for a caller that holds none).

The relay's `socket_*` calls and the NIC window run without it (the socket table and `NETWORK` have
their own locks; the netpoll daemon drains them BKL-free for the same reason). Rehearsal guests keep
the BKL-held shape. Reference: `docs/reference/subsystems/kvm.md`.

**Verified.**

| where | result |
|---|---|
| TCG, 1 vCPU | `serve_rehearse.py --bulk 1024` PASS; `--stress 3 --bulk 2048` PASS; no `[BKL] stuck` |
| metal (ryzen entry 30, `vm-serve-mc`: 2 vCPUs on cores 4,6), 32 MiB through the relay | 11.9 MB/s down, 11.5 MB/s up |
| metal, `relay_stress.py --rounds 3` | PASS, ssh healthy after every phase |

**Not a win on bulk.** Before: 11.2-11.5 MB/s on two dedicated cores. After: 11.5-11.9. Exit
serialisation was not what limits one bulk stream. The 12 `[BKL] stuck` lines in dmesg are the wifi
driver's join at boot (`[rtw]`, tag 501/511), before the VM exists. Multi-core TCG is still not
verified (the old SYN-ACK hang is unchanged).

## 2. A large transfer through the relay wedges the guest: pre-existing, not this change

`ssh -p 2223 root@127.0.0.1 'head -c N /dev/zero' | wc -c` run **on the host**:

| kernel | N | outcome |
|---|---|---|
| `61e8d96b` (BKL-free exits) | 32 MiB | fine, 3 s |
| `61e8d96b` | 96 MiB, 192 MiB | stops at `to_host` = 94 MiB (192 MiB run) |
| `3feba1b3` (BKL held; the control) | 96 MiB | stops at `to_host` = 60 MiB |

So the control wedges too: the open "guest -> host bulk download hang" from phase 13
(`project_kvm_ssh_guest_phase13`), not a regression. After the stall the guest does not answer new
connections either (the sshd's connection is stuck), and a `herd stop`/`start` of the VM brings a
fresh guest.

**What the stalled relay says** (`relay: slot 0 ...` and `KVM] guest:` lines in
`/var/log/herd/vm-serve-mc.log`; a copy of the first wedge is `vm-serve-mc.log.wedge-61e8d96b` on p3):
`up_pending=2048` (bytes read from the guest that the host socket will not take), `host_send_blocked`
rising by ~4000/s, `peer_wnd=0`, `gw_queued=0`, `in_flight=0`, `rx_starved` large, cpu0 `halted`
with `next_event_in_us=0`. And the **host's ssh client process burns a core** (8 min of CPU in one
run; host core 0 reads 89% busy) while not draining its socket.

**Where it points.** The relay cannot hand bytes to the host-side loopback socket of `/bin/ssh`
(`userspace/sshd/src/client/protocol.rs`, `pump`), which is busy but not reading. Candidates, not
yet separated:

1. the client is CPU-bound in software crypto + `try_take_packet`'s front-drain of a growing
   `input_buffer`, reading only 4 KiB per loop pass (that would also be the real ~11 MB/s ceiling,
   not the hypervisor);
2. a lost window update on the host's loopback TCP (zero window, the app drained, no update sent
   until the next stack poll);
3. the guest itself: cpu0 halted with a timer already due (`next_event_in_us=0`) means a wake is
   being missed in `Platform::offer`/the idle sleep. The guest's sshd would then be the stuck side
   and the host client would be spinning on its keepalive/`sleep_ms(1)` loop.

**Next experiments** (cheap, not done): (a) the same transfer as `> /dev/null` on a *fresh* guest
(my attempt reused a wedged one); (b) host ssh client against the host's own sshd (needs a key
trusted for root on 2222), which removes the relay and the hypervisor; (c) read the relay's
`[KVM] cpu:` lines during the transfer for the owner core's busy split; (d) a nonblocking `nc`-like
reader on the host instead of `/bin/ssh`. **Do not use `-o` with the host's `ssh`**: it treats
`-o` as the host name and retries ten times.

## 3. NetBSD amd64 as a guest

* `NetBSD-11.0-amd64.iso` (SHA512 verified against `cdn.netbsd.org/.../11.0/SHA512`) carries
  `amd64/binary/kernel/netbsd-{GENERIC,MICROVM,INSTALL}.gz`.
* **Both `netbsd-MICROVM` (9.3 MB) and `netbsd-GENERIC` (30 MB) have the Xen ELF note type 18
  (`PHYS32_ENTRY`), the PVH entry point** (MICROVM: `0x20b460`). Segments are linked at
  `0xffffffff80200000` with `p_paddr = 0x200000`; our loader loads by physical address.
* `MICROVM` is the one to try: it is NetBSD's kernel for QEMU `microvm` / Firecracker, with virtio-mmio
  and no PCI.
* **What is missing in the hypervisor** (a `Guest::NetBsd`, not a spec change): a spec VM is always
  `Guest::AkumaSpec`. NetBSD's microvm port expects an IOAPIC and its own device placement
  (virtio-mmio windows, ISA `com0`); our platform has PIT, PIC, per-vCPU LAPIC, 16550 and virtio-mmio
  at `0xD0000000`, no IOAPIC, no ACPI for non-Linux guests unless asked. First step: boot MICROVM with
  no disk and read its console to see what it asks for.
* **Root filesystem.** The plan was a ramdisk first: the `netbsd-INSTALL` kernel embeds one, but it
  is a GENERIC-class kernel (PCI), not microvm. For MICROVM, build an FFS root image from the ISO's
  `*.tar.xz` sets into a RAM-copied disk (`vm_disk_ram`); a few GB squeezed from the Gentoo
  partition (p7) is the later step.

### 2a. After the wedge: the relay's slot is never freed (found at the end of the session)

`kill -9` of the stuck host `ssh` client (and of its `sh -c` wrapper) removes the client's socket, but
the relay's end stays `ESTABLISHED`: `/proc/net/tcp` on the host lists a single loopback-peer socket
(`0, 192.168.1.159:49182, ESTABLISHED`) and the relay keeps printing `slot 0 open ... to_host=63174661
up_pending=2048 host_send_blocked=...` with `host_send_blocked` climbing ~6000/s, i.e. it keeps
retrying a `socket_send` that returns EAGAIN on a socket whose peer is gone. Two consequences:

* a guest whose one forward slot is held by a dead connection stays unreachable until the VM is
  restarted, even though nothing else is wrong; `herd stop vm-serve-mc` killed `vm-run` but the new
  `vm-run` (pid 56) printed the *same* slot counters, so the old guest/relay state outlived the
  stop and `herd restart` is not a reliable reset here (**to verify**: does the VM task survive a
  `vm-run` kill?);
* the host kernel does not send a FIN/RST to a loopback peer when a process holding the other end
  is killed with data in flight (or the relay does not see it). Either is a bug worth its own
  test: kill a client mid-transfer and watch the relay's slot.

The relay should bound this regardless: a slot whose host socket has made no progress for N seconds
(`host_send_blocked` without `up_off` moving) should be reset like an idle one.
