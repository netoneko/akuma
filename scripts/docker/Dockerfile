FROM alpine:latest

RUN apk add --no-cache qemu-system-aarch64

# Kernel binary and disk image are mounted as volumes at runtime
RUN mkdir -p /kernel /data

EXPOSE 22 8080

CMD ["qemu-system-aarch64", \
     "-machine", "virt", \
     "-cpu", "max", \
     "-m", "128M", \
     "-nographic", \
     "-serial", "mon:stdio", \
     "-netdev", "user,id=net0,hostfwd=tcp::22-:22,hostfwd=tcp::8080-:8080", \
     "-global", "virtio-mmio.force-legacy=true", \
     "-device", "virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0", \
     "-drive", "file=/data/disk.img,if=none,format=raw,id=hd0", \
     "-device", "virtio-blk-device,drive=hd0,bus=virtio-mmio-bus.1", \
     "-device", "virtio-rng-device,bus=virtio-mmio-bus.2", \
     "-kernel", "/kernel/akuma"]

