# Akuma on Firecracker: Container Demo

Demo proposal: running an OCI-compatible Node.js container on Akuma, inside Firecracker, on a tiny AWS Graviton instance.

## Elevator Pitch

Boot Akuma as a microVM on AWS Graviton via Firecracker. Pull an official `node:alpine` OCI image from Docker Hub. Run it inside an Akuma box (container). Serve a single-file HTTP API that returns the Akuma ASCII art as a greeting. Then use an LLM (meow or Gemini) running in the same box to rewrite the greeting live.

The demo proves Akuma can be a real container host on real cloud hardware, not just a QEMU curiosity.

## Target Setup

```
┌─────────────────────────────────────────────────┐
│  EC2 Graviton instance (t4g.micro / a1.medium)  │
│                                                 │
│  ┌───────────────────────────────────────────┐  │
│  │  Firecracker microVM                      │  │
│  │                                           │  │
│  │  ┌─────────────────────────────────────┐  │  │
│  │  │  Akuma kernel                       │  │  │
│  │  │  VirtIO-blk · VirtIO-net · serial   │  │  │
│  │  │                                     │  │  │
│  │  │  ┌─────────────────────────────┐    │  │  │
│  │  │  │  box: node-greeting         │    │  │  │
│  │  │  │  rootfs: node:alpine layers │    │  │  │
│  │  │  │                             │    │  │  │
│  │  │  │  node /app/server.js        │    │  │  │
│  │  │  │  :8080 → greeting API       │    │  │  │
│  │  │  └─────────────────────────────┘    │  │  │
│  │  └─────────────────────────────────────┘  │  │
│  └───────────────────────────────────────────┘  │
└─────────────────────────────────────────────────┘
```

### Why Firecracker

Firecracker provides VirtIO-blk, VirtIO-net, and a serial console — the same device model Akuma already supports on QEMU virt. The main delta is the memory map and device tree layout. No new drivers needed.

### Why Graviton

Akuma targets AArch64. Graviton is native AArch64. No emulation overhead. A `t4g.micro` (2 vCPU, 1 GB RAM) costs ~$0.0084/hr and is more than enough to boot a 256 MB microVM.

## Demo Script

### Step 1 — Boot Akuma on Firecracker

On the Graviton host:

```bash
# Build the kernel (cross-compile or build on Graviton directly)
cargo build --release --target aarch64-unknown-none

# Launch via Firecracker with VirtIO-blk for disk, VirtIO-net for networking
firecracker --config-file akuma-firecracker.json
```

SSH into the Akuma guest from the host:

```bash
ssh -p 2222 akuma@localhost
```

### Step 2 — Pull the Node.js OCI image

From the Akuma shell:

```bash
# Pull official node:22-alpine from Docker Hub
box pull docker.io/library/node:22-alpine

# This fetches the OCI manifest, downloads and extracts each layer
# into /var/lib/box/images/node-22-alpine/
```

### Step 3 — Create and start the container

```bash
# Create a box from the pulled image
box run --name node-greeting --image node:22-alpine --port 8080:8080 -- node /app/server.js
```

### Step 4 — The greeting API

`/app/server.js` is a single file, copied into the box before start:

```javascript
const http = require('http');
const fs = require('fs');

const art = fs.readFileSync('/app/akuma_40.txt', 'utf8');

const server = http.createServer((req, res) => {
  if (req.url === '/greeting') {
    res.writeHead(200, { 'Content-Type': 'text/plain' });
    res.end(art + '\n\nGreetings from Akuma OS!\n');
  } else {
    res.writeHead(404);
    res.end('Not found\n');
  }
});

server.listen(8080, () => {
  console.log('Greeting API running on :8080');
});
```

`/app/akuma_40.txt` is the kernel's ASCII art banner:

```
                      =#=      .-
                      +*#*:.:-**
                      +%%#%##***
                      +%%%#%%#**.
                      +%@@@%%%+*:
          :::::--=+++*%@%@%%%%*-
     :-+##%%%%%%%%@%@#%%@%%%%##%+
  .=##%%%%%%%%%@@@@@%#%%@%%%@@@%%-
.*%%%%%%%@%%%%@%@@@@%%%%@%%%%%@@#-
%@%%%%%%%%%%@%%%%%@@@%%%@@@@@%%%#+
*%%%%%@%@@%@@@%%%%#%@@@@@@@%%@@%@@@*+--
 ::=+*#@@@@@@@@@@@%%%%%%@%%@#----=**@%@#
         .--+**%@@@@@%@@%%@@%*       :-.
                  ::::---#@%%*
```

### Step 5 — Test from the host

```bash
curl http://localhost:8080/greeting
```

## Bonus Challenge: LLM-Rewritten Greeting

Use meow (Akuma's built-in Ollama LLM client) or the Gemini API to rewrite the greeting dynamically, from inside the same Akuma instance.

```bash
# Option A: meow (requires Ollama running on the host/network)
box use node-greeting -- meow "rewrite /app/akuma_40.txt as a haiku about a demon cat OS"

# Option B: Gemini API (requires TLS + outbound HTTPS)
box use node-greeting -- curl -X POST \
  "https://generativelanguage.googleapis.com/v1/models/gemini-pro:generateContent?key=$API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"contents":[{"parts":[{"text":"Rewrite this ASCII art greeting as a haiku about a demon cat operating system"}]}]}' \
  | jq -r '.candidates[0].content.parts[0].text' > /app/akuma_40.txt
```

After rewriting, `curl http://localhost:8080/greeting` returns the LLM-generated greeting instead.

The challenge is that Node.js serves the file and the LLM rewrites it — both running inside the same Akuma box, on the same Firecracker microVM, on a $6/month Graviton instance.

## What Exists Today vs. What Needs Building

| Component | Status | Notes |
|-----------|--------|-------|
| AArch64 kernel | Done | Boots on QEMU virt, targets same ISA as Graviton |
| VirtIO-blk driver | Done | Same interface on Firecracker |
| VirtIO-net driver | Done | Same interface on Firecracker |
| ext2 filesystem | Done | Read/write, used for root disk |
| TCP/IP stack (smoltcp) | Done | Socket syscalls implemented |
| SSH server | Done | Port 2222, Ed25519 auth |
| Process spawning | Done | ELF loader, static + dynamic linking |
| `box` container manager | Done | Chroot-style isolation, process + FS isolation |
| `httpd` (HTTP server) | Done | Port 8080, CGI support |
| meow (LLM client) | Done | Ollama backend, tool calling |
| Firecracker boot config | **Not started** | Need FDT/memory map adaptation |
| OCI image pull | **Not started** | HTTP registry client, layer extraction, overlay FS |
| libuv / epoll | **Not started** | Required for Node.js event loop (see below) |
| Signal delivery | **Partial** | Stubs exist, real sigaction needed for Node.js |
| Futex | **Partial** | Basic impl exists, needs robustness for libuv |
| `box pull` (registry client) | **Not started** | OCI distribution spec, gzip/tar extraction |

### Critical Path

```
Firecracker boot
  └── OCI image pull (registry HTTP + layer extraction)
       └── libuv infrastructure (epoll, signals, futex)
            └── Node.js runs in box
                 └── greeting API serves akuma_40.txt
```

### 1. Firecracker Boot

**Delta from QEMU virt:**
- Different FDT (device tree) address and layout
- Different memory base address
- No `-semihosting` — need UART-only console fallback
- Firecracker uses `virtio-mmio` transport (same as QEMU with `force-legacy=true`)

**Estimated effort:** Small. The kernel already handles FDT parsing and VirtIO discovery. A Firecracker-specific config file and minor FDT address adjustment should suffice.

### 2. OCI Image Pull

Implement `box pull <image>` that speaks the [OCI Distribution Spec](https://github.com/opencontainers/distribution-spec):

1. **Resolve tag** — `GET /v2/<name>/manifests/<tag>` with `Accept: application/vnd.oci.image.manifest.v1+json`
2. **Download layers** — `GET /v2/<name>/blobs/<digest>` for each layer (gzipped tar)
3. **Extract layers** — gunzip + untar into `/var/lib/box/images/<name>/`, applied in order (base layer first)
4. **Parse config** — extract `Cmd`, `Entrypoint`, `Env`, `WorkingDir` from the image config JSON

Docker Hub requires token auth (`GET /v2/` → 401 → fetch token from `auth.docker.io` → retry with `Bearer` token). This needs the kernel's existing TLS/HTTPS support.

**Dependencies:** TLS (done), HTTP client (kernel `curl` exists), gzip decompression (need to add), tar extraction (need to add).

### 3. libuv Infrastructure

Node.js requires libuv, which requires real implementations of:

- **epoll** — event loop backbone (currently stubbed)
- **Signals** — `sigaction`, signal delivery, self-pipe trick
- **Futex** — thread synchronization (basic impl exists)
- **eventfd** — inter-thread wakeup (currently stubbed)
- **timerfd** — timer-based events (currently stubbed)

See [LIBUV_INFRASTRUCTURE.md](LIBUV_INFRASTRUCTURE.md) for the full analysis and implementation plan. Epoll is the critical path — libuv's main loop is literally `epoll_pwait()` in a while loop.

## Stretch Goals

### Run without Node.js

If libuv proves too heavy, a fallback demo uses QuickJS (already working on Akuma) with a minimal HTTP server written in ES2020 JavaScript — no libuv dependency, no OCI pull needed. Less impressive but proves the container + API story with existing infrastructure.

### Multiple containers

Run two boxes simultaneously — one for the greeting API, one for meow — demonstrating Akuma's multi-process isolation and shared network stack.

### Firecracker API integration

Use Firecracker's REST API to hot-plug a second VirtIO-blk device containing a pre-built container rootfs, avoiding the OCI pull entirely for faster cold starts.

## Success Criteria

1. Akuma boots under Firecracker on a Graviton EC2 instance
2. An OCI-compatible Node.js Alpine container is pulled from Docker Hub and extracted
3. `node /app/server.js` runs inside an Akuma box, serving HTTP on port 8080
4. `curl http://<instance-ip>:8080/greeting` returns the Akuma ASCII art banner
5. (Bonus) meow or Gemini rewrites the greeting file and subsequent curls return the new greeting

## Cost Estimate

- `t4g.micro` (2 vCPU, 1 GB): ~$6.05/month on-demand, ~$2.50/month reserved
- `a1.medium` (1 vCPU, 2 GB): ~$18.40/month on-demand
- EBS gp3 8 GB: ~$0.64/month
- Data transfer: negligible for a demo

Total: under $10/month for a persistent demo instance.
