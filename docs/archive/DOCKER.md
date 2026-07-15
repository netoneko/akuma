# Running in Docker

## Build the kernel first (if not already built)

```
cargo build --release
```

## Build the Docker image

```
./scripts/build_docker.sh
```

## Run with docker-compose (recommended)

Assuming `disk.img` is formatted to ext2 and exists in the project root:

```
cd scripts/docker
docker compose up
```

This exposes:
- SSH on port 2222
- HTTP on port 8080

The compose setup includes:
- **Healthcheck**: Polls `http://localhost:8080/index.html` every 10s
- **30s startup grace period** before health checks begin
- **Auto-restart**: If health check fails 5 times, the autoheal service restarts akuma

## Healthcheck Architecture

The healthcheck uses `wget` running inside the Alpine container to verify akuma is responsive:

```
wget (in container) → localhost:8080 → QEMU port forward → akuma HTTP server (guest VM)
```

**How it works:**

1. The container runs Alpine Linux with QEMU installed
2. QEMU starts akuma with user-mode networking and port forwarding (`hostfwd=tcp::8080-:8080`)
3. The healthcheck's `wget` command runs inside the container (not inside the VM)
4. `localhost:8080` inside the container reaches QEMU's port forwarder
5. QEMU forwards the request to akuma's HTTP server running in the guest VM

**Timing:**

| Parameter | Value | Description |
|-----------|-------|-------------|
| `start_period` | 30s | Grace period for akuma to boot before checks begin |
| `interval` | 10s | Time between health checks |
| `timeout` | 5s | Max time to wait for a response |
| `retries` | 5 | Consecutive failures before marking unhealthy |

**Auto-restart:**

Docker's native healthcheck only reports status—it doesn't restart containers. The `autoheal` service monitors for unhealthy containers and restarts them automatically.

## Run the container manually

Alternatively, run directly with docker:

```
docker run -it --platform linux/arm64 \
  -p 2222:22 \
  -p 8080:8080 \
  -v $(pwd)/disk.img:/data/disk.img \
  akuma-qemu
```
