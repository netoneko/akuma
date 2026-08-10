# Two VM agent workflow

Prove that `meow` running in a 64MB VM can compile `hello.c` with `tcc`, driven
by a Qwen3.5-0.8B model served from a second QEMU VM.

## Network topology (fixed IP, no discovery needed)

QEMU user networking (SLIRP) assigns a predictable gateway at `10.0.2.2` inside
every VM. Port-forwarding rules in `cargo_runner.sh` map:

```
llama VM (INSTANCE=1): host:8180 → llama VM:8080
meow  VM (INSTANCE=0): host:2222 → meow  VM:22 (SSH)
                        host:2322 → llama VM:22 (SSH)
```

From inside the meow VM, `http://10.0.2.2:8180` always reaches the llama VM's
port 8080, regardless of the VM's internal DHCP address. This is set in
`bootstrap/etc/meow/config` as `[provider:llamacpp]` and the two-vm disk is
pre-configured to use it.

## Quick start

```bash
# Build kernel + create disks + launch both VMs (all-in-one):
scripts/run_two_vms.sh

# Skip rebuild if kernel is already built:
scripts/run_two_vms.sh --skip-build

# Skip disk recreation if disks already exist:
scripts/run_two_vms.sh --skip-build --skip-disks
```

The script:
1. Builds the kernel (`cargo build --release`)
2. Creates `tmp/two_vms/llama.img` (1800MiB, includes the model)
3. Creates `tmp/two_vms/meow.img` (256MiB, no model, meow config → llamacpp)
4. Launches llama VM at INSTANCE=1 (ssh port 2322, http port 8180)
5. Launches meow VM at INSTANCE=0 (ssh port 2222)
6. Prints next steps and waits

## Step-by-step manual run

### 1 — Set up the llama VM

```bash
ssh -o StrictHostKeyChecking=no -p 2322 root@localhost

# llama-server is bundled in /bin (built from userspace/llama.cpp).
# --no-mmap: Akuma's VFS doesn't support file-backed mmap.
# --chat-template chatml: Qwen3's Jinja2 template isn't supported by
#   llama-server's built-in parser; chatml is a compatible fallback.
llama-server --model /qwen3.5-0.8b-q4.gguf --host 0.0.0.0 --port 8080 \
  --no-mmap --chat-template chatml &

# Wait ~60s for model to load (health returns 503 while loading):
until curl -s http://localhost:8080/health | grep -q ok; do sleep 5; done
echo "llama-server ready"
```

### 2 — Run the meow agent on the meow VM

```bash
ssh -o StrictHostKeyChecking=no -p 2222 root@localhost

# Verify meow can reach the llama VM (should list models):
curl http://10.0.2.2:8180/v1/models

# Create workspace and run the task:
mkdir -p /akuma-playground
meow -c "compile /akuma-playground/hello.c with tcc and verify that it runs and returns a greeting, write a report to /tmp/tcc_hello_c.md"

# Check the result:
cat /tmp/tcc_hello_c.md
```

### 3 — Expected outcome

- meow iterates through tool calls (FileWrite, Shell) to:
  - Write a `hello.c` source file
  - Compile it with `tcc`
  - Run the resulting binary and capture its output
  - Write a markdown report to `/tmp/tcc_hello_c.md`
- Total memory on meow VM stays within 64MB
- All LLM inference happens on the llama VM (4096MB)

## Configuration

### meow provider (bootstrap/etc/meow/config)

The two-vm disk keeps the `ollama` provider but redirects it to the llama VM
by replacing the port in the base URL:

```ini
current_provider=ollama
current_model=qwen3:4b

[provider:ollama]
base_url=http://10.0.2.2:8180   # was :11434 (Ollama); now :8180 (llama VM)
```

No provider switch needed — `run_two_vms.sh` patches only the URL's port.
llama-server's OpenAI-compatible API is at `/v1/chat/completions` just like
Ollama's, so the rest of meow's config is unchanged.

### Environment overrides for run_two_vms.sh

| Variable | Default | Description |
|---|---|---|
| `MEOW_MEMORY` | `64M` | RAM for meow VM |
| `LLAMA_MEMORY` | `4096M` | RAM for llama VM |
| `LLAMA_PORT` | `8080` | llama-server port inside llama VM |
| `LLAMA_MODEL` | `/qwen3.5-0.8b-q4.gguf` | Model path on the llama VM disk |
| `LLAMA_DISK_MB` | `1800` | Llama disk size (needs > model size ~508MB) |
| `MEOW_DISK_MB` | `2048` | Meow disk size (space for apk installs and agent workspace) |

### Adding the Alpine edge/testing repo permanently to the llama disk

Instead of passing `--repository` each time, add it to the disk:

```bash
# In the llama VM:
echo "https://dl-cdn.alpinelinux.org/alpine/edge/testing/" >> /etc/apk/repositories
apk update
apk add llama.cpp-server
```

## Troubleshooting

**meow can't reach llama VM:**
- Check llama-server is running: `curl http://localhost:8080/health` from llama VM
- Check port from host: `curl http://localhost:8180/health`
- Check from meow VM: `curl http://10.0.2.2:8180/health`
- Ensure llama-server binds `0.0.0.0`, not `127.0.0.1`

**llama-server OOM on load:**
- Increase `LLAMA_MEMORY` (e.g. `LLAMA_MEMORY=6144M`)

**meow VM OOM:**
- The 64MB limit is tight. If it crashes, try `MEOW_MEMORY=128M`

**Disk too small for model:**
- The model is ~508MB; the default 1800MiB disk should fit
- Increase `LLAMA_DISK_MB=2048` if needed
