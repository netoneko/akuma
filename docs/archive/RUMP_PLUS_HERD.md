# Rump + herd: running the NetBSD stack as a managed box service

**Status: PLANNED (Phase 5).** M1 is already green — a rump-net payload (`rumphttp`)
DHCPs and HTTP-fetches the host inside a `RUMP_NIC=1` box (see
`userspace/rumpkernel/docs/HANDOFF.md`). M1 ran that payload **by hand over SSH**.
This doc is the design for the next step: let **herd** spawn and supervise the
rump-net payload as a first-class box service, with the bundle wiring the VFS/mounts.

Related: `IMPLEMENTATION_PLAN.md` §10.1 (config-driven rump), §10.3 (resource caps),
§10.4 (running unmodified binaries), and `ARCHITECTURE_QUESTIONS.md`.

---

## 1. The runtime data path (what's already proven)

A rump-net process is one Akuma process that contains the whole NetBSD TCP/IP stack.
Its packets ride `/dev/net/tap0` (NIC1), L2-isolated from Akuma's own smoltcp (NIC0):

```
        ┌─────────────────────────── Akuma box (one process) ───────────────────────────┐
        │                                                                                │
        │   app logic                                                                    │
        │   ─ self-contained:  rumphttp  ── rump_sys_socket/connect/read/write ─┐        │
        │   ─ unmodified bin:   curl ─ LD_PRELOAD hijack.so ─ libc→rump_sys_* ───┤        │
        │                                                                        ▼        │
        │                                            NetBSD rump kernel (librump.a)       │
        │                                            TCP/IP · virtif "virt0" · DHCP · bpf │
        │                                                        │ rumpcomp_virt_send/recv│
        │                                                        ▼                        │
        │                          rumpcomp_tap.c  (our virtif backend)                   │
        │                                  │ open/read/write + rumpuser hypercalls        │
        │                                  ▼ (rumpuser: mmap/threads/clock → Akuma)       │
        └──────────────────────────────────┼─────────────────────────────────────────────┘
                                            │  read()/write() one L2 frame  (BLOCKING read)
                                            ▼
                       Akuma kernel:  /dev/net/tap0   (FileDescriptor::Tap)
                                            │  raw L2 frames
                                            ▼
                          NIC1 (virtio-net, virtio-mmio-bus.4)
                                            │
                                            ▼
                       QEMU net1 SLIRP  ── DHCP server + NAT ──▶ host 10.0.2.2 / internet

   (NIC0 / smoltcp / Akuma's native sockets is a separate path — not involved here.)
```

Key fact: a frame seen at `rumpcomp_tap.c` (the `[VIRTIF TX/RX]` counters) provably
went through the NetBSD stack, not smoltcp.

---

## 2. herd lifecycle for a rump-net service

herd is an OCI-bundle supervisor: it reads service bundles from
`/etc/herd/available`, enables them via `/etc/herd/enabled`, and spawns + restarts
their `process.args`. It already sets up each service's filesystem namespace
(`root_path` + `mounts`). The rump-net flow:

```
  boot (kernel built with `rump`, QEMU started with RUMP_NIC=1 → /dev/net/tap0 live)
    │
    ▼
  herd starts, scans /etc/herd/enabled/
    │
    ├─ for service "rumpnet":  read herd's OWN config (stack: rump)
    │     │
    │     ├─ (0) GENERATE the OCI bundle from intent: inject env (LD_PRELOAD=hijack.so,
    │     │           RUMP_DHCP), mounts (/dev/net/tap0 + SDK), args, linux.resources
    │     │           → /etc/herd/available/rumpnet/config.json  (pure standard OCI)
    │     │
    │     ├─ (1) PREP namespace  ── unpack rump SDK from /archives (temporary install),
    │     │                          bind-mount /dev/net/tap0 into the box root
    │     │
    │     ├─ (2) SPAWN  process.args  from the generated bundle (rumphttp, or an
    │     │           unmodified binary with the injected LD_PRELOAD=hijack.so)
    │     │
    │     └─ (3) payload runs:  rump_init() → ifcreate virt0 → dhcp_ipv4_oneshot
    │                            → serve / fetch  (over /dev/net/tap0)
    │
    ▼
  herd SUPERVISES: liveness, restart-on-exit, logs.  `box close` / disable → teardown
                   (VIFHYPER_DYING/DESTROY closes the tap, frees the rump instance).
```

The only thing M1 did manually (SSH in, run `/bin/rumphttp`) becomes steps (2)+(3),
driven by the bundle.

---

## 3. Two layers: herd config (intent) → generated OCI bundle (wiring)

The "which stack" decision is **not** in the OCI config — it lives in **herd's own
service config**. herd then acts as a *compiler*: from that high-level intent it
**generates a 100% standard OCI bundle**, injecting the concrete values (preload
libs, executable/args, mounts, caps) that realize it. The OCI config never mentions
"rump" — it just carries the wiring. No OCI extension.

### 3a. herd service config (Akuma's domain — the selector lives here)

```jsonc
// herd's own per-service config (NOT the OCI bundle)
{
  "name": "rumpnet",
  "stack": "rump",            // §10.2 selector: rump | smoltcp   ← the one Akuma knob
  "nic": 1,                   // which tap / NIC for the rump stack
  "memoryMiB": 128,           // high-level cap (herd lowers it into the OCI bundle)
  "payload": { "kind": "unmodified", "exec": "/usr/bin/curl", "args": ["http://10.0.2.2/"] }
  //          or { "kind": "self-contained", "exec": "/bin/rumphttp", ... }
}
```

### 3b. herd GENERATES this standard OCI bundle from the above

```jsonc
// /etc/herd/available/rumpnet/config.json — pure OCI, machine-generated by herd
{
  "root": { "path": "rootfs" },
  "process": {
    "args": ["/usr/bin/curl", "http://10.0.2.2/"],     // ← from payload
    "env":  ["LD_PRELOAD=/usr/lib/hijack.so", "RUMP_DHCP=1"], // ← injected for stack=rump
    "cwd": "/"
  },
  "mounts": [                                           // ← injected for stack=rump
    { "destination": "/dev/net/tap0", "source": "/dev/net/tap0", "type": "bind" },
    { "destination": "/usr",          "source": "/archives/rump-sdk", "type": "bind" }
  ],
  "linux": { "resources": { "memory": { "limit": 134217728 } } } // ← from memoryMiB
}
```

What herd injects, driven purely by `stack: rump` in its own config:
- **`env`** — `LD_PRELOAD=hijack.so` (+ `RUMP_DHCP`) for the unmodified-binary flavor;
  omitted for a self-contained payload that calls `rump_sys_*` itself.
- **`mounts`** — bind `/dev/net/tap0` + the rump SDK into the box root.
- **`process.args`** — the payload exec/args (or the self-contained `rumphttp`).
- **`linux.resources.memory.limit`** — lowered from the high-level `memoryMiB`.

herd then applies that limit **twice**: passes it to the rump kernel as `RUMP_MEMLIMIT`
(uvm/pool sizing) and sets the Akuma-side PMM budget (so a runaway box is SIGKILL'd,
not a kernel panic — `akuma_oom_kill_not_panic`). For a `stack: smoltcp` service herd
injects none of the rump wiring — same generator, different output.

Install (temporary bring-up path): the SDK + payload are staged on the host under
`bootstrap/archives/` (e.g. `rump-sdk-aarch64-musl.tar.gz`), which `populate_disk.sh`
lands at the VM's `/archives`; herd unpacks/binds it into the box root before spawn.

---

## 4. Two payload flavors (pick per service)

```
  (A) self-contained rump app            (B) unmodified binary + hijack
  ───────────────────────────            ──────────────────────────────
  args: ["/bin/rumphttp", ...]           args: ["/usr/bin/curl", "http://10.0.2.2/"]
  app calls rump_sys_* directly          env:  LD_PRELOAD=/usr/lib/hijack.so
  (rumphttp.c — the M1 payload)           hijack.so: ctor rump_init+virt0+DHCP,
  no preload, no translation              libc socket→rump_sys_* (curl/nc only;
                                          NOT musl-stdio binaries like busybox wget)
```

Both link the same backend (`rumpcomp_tap.c` over `/dev/net/tap0`). Flavor (B) is the
"run off-the-shelf tools" story until the kernel ABI-personality work (pkgsrc;
`acceptance/12_netbsd_binary_compatibility.md`) lands.

---

## 5. What herd needs added (open items)

- [ ] herd service config gains a `stack: rump|smoltcp` selector (+ nic, memoryMiB)
      — the one Akuma knob, in herd's OWN config, NOT the OCI bundle.
- [ ] herd **generates** the OCI bundle from that intent: inject `env`
      (LD_PRELOAD=hijack.so, RUMP_DHCP), `mounts` (/dev/net/tap0 + SDK), `process.args`,
      and lower `memoryMiB` → `linux.resources.memory.limit`. The bundle stays pure OCI.
- [ ] Bundle prep step: unpack/bind the rump SDK from `/archives`; bind `/dev/net/tap0`.
- [ ] A long-lived rump-net **daemon** payload (M1's `rumphttp` is one-shot) for
      services that stay up (e.g. the SSH-into-box demo, `acceptance/11`).
- [ ] Enforce `linux.resources.memory.limit` → RUMP_MEMLIMIT (into the rump kernel)
      + Akuma-side PMM budget so a runaway box is killed, not panicking the kernel (§10.3).
- [ ] Phase 5 `box open <name> --net` as the user-facing front door that writes/enables
      the bundle and ensures the box root exposes `/dev/net`.
- [ ] Multi-instance: one rump kernel + one `virt0` per box; distinct MACs/leases
      (ties into §10.1 config-driven, keyed by box id).
