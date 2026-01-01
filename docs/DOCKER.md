# Running in Docker

## Build the kernel first (if not already built)

```
cargo build --release
```

## Build the Docker image

```
./scripts/build_docker.sh
```

## Run the container with disk.img mounted

Assuming `disk.img` is formatted to ext2:

```
docker run -it --platform linux/arm64 \
  -p 2222:22 \
  -p 8080:8080 \
  -v $(pwd)/disk.img:/data/disk.img \
  akuma-qemu
```
