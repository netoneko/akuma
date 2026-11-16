# Direct EC2 bootstrap

This overlay targets a future direct-boot EC2 image, separate from the
Firecracker-on-AWS host overlay. The kernel must start `/bin/herd` as init;
herd runs `sshd` and the `instance-key` service. The service fetches the
Ed25519 launch key from instance metadata and installs it for sshd. sshd reads
the key file on login, so the services need no ordering. This is a metadata
bootstrap, not an implementation of cloud-init user-data.

Run `python3 -m unittest discover -s overlays/aws/tests -v` for local IMDSv2
tests. For a different test server, set `METADATA_URL` to `http://HOST:PORT`,
`AUTHORIZED_KEYS` to a disposable path, and optionally `INSTANCE_KEY_TRIES`.

For a VM test, build the amd64 kernel and provide an existing Akuma amd64 ext2
disk containing herd, sshd and busybox (for example one made with
`amd64/mkdisk.sh`):

```sh
cargo build -p akuma --target x86_64-unknown-none --release --no-default-features --features headless,no-tests
python3 overlays/aws/tests/e2e_vm.py --disk /path/to/akuma-amd64.img
```

The test copies the disk, installs this overlay, injects a metadata URL into
the herd service, starts a host metadata server and boots QEMU. It checks SSH
login with the metadata key and rejection of an unrelated key. The test server
supports IMDSv1; the separate host tests cover IMDSv2. It exercises virtio-net,
not ENA or EC2 firmware. It requires QEMU, debugfs, ssh and ssh-keygen.

An AMI builder must install the overlay and curl, provide herd and sshd, select
herd as init, and omit baked host and authorized keys. The overlay is not yet
wired into an AMI builder; ENA and direct EC2 boot remain under development.
