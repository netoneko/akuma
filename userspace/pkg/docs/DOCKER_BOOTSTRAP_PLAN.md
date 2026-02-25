# Docker Akuma OS Bootstrap Plan

## 1. Goal

To create a `docker.sh` script that, when executed (e.g., via `curl -L ... | bash`), will automatically set up a working Akuma OS environment inside a Docker container. This includes preparing a disk image, configuring SSH access, and providing a QEMU launch script.

## 2. Assumptions & Prerequisites

*   **Host Environment**: Standard Linux-based Docker environment (e.g., Ubuntu, Debian, Alpine base image).
*   **Dependencies**: The script will assume `curl`, `bash`, `qemu-system-aarch64`, `e2fsprogs` (for `mkfs.ext2`), `mtools` (optional, for FAT if needed), `ssh-keygen`, `rsync` are installed or can be installed via `apt` / `apk`.
*   **Akuma OS Components Location**: A web server (e.g., `https://install.akuma.sh`) hosts the following:
    *   Akuma Kernel Image: `kernel.bin`
    *   Base Filesystem Tarball: `base_fs.tar.gz` (contains `/bin/pkg`, `/bin/tar`, `/bin/paws`, `/bin/sshd`, etc., and possibly `/etc/pkg/config` pointing to the public package server).
    *   `qemu-system-aarch64` binary (optional, if not installed from apt/apk directly).

## 3. Implementation Phases

### Phase 3.1: Initial Script Setup (`docker.sh`)

1.  **Shebang & Error Handling**:
    ```bash
    #!/bin/bash
    set -euo pipefail
    ```
2.  **Log File**: Redirect all output to a log file for debugging.
3.  **Dependency Check & Install**:
    *   Check for `qemu-system-aarch64`, `mkfs.ext2`, `ssh-keygen`, `rsync`, `curl`.
    *   If missing, attempt installation based on detected package manager (`apt`, `apk`).
    ```bash
    if ! command -v qemu-system-aarch64 &> /dev/null; then
        echo "QEMU not found, installing..."
        # ... apt install or apk add commands ...
    fi
    # ... repeat for other tools ...
    ```
4.  **Create Working Directory**: Create a temporary directory for all bootstrap files (e.g., `/tmp/akuma_install`).
    ```bash
    INSTALL_DIR="/tmp/akuma_install_$$"
    mkdir -p "$INSTALL_DIR"
    cd "$INSTALL_DIR"
    ```

### Phase 3.2: Download Akuma OS Components

1.  **Define Base URL**:
    ```bash
    AKUMA_INSTALL_BASE="https://install.akuma.sh"
    ```
2.  **Download Kernel**:
    ```bash
    curl -L "${AKUMA_INSTALL_BASE}/kernel.bin" -o kernel.bin
    ```
3.  **Download Base Filesystem Tarball**: This tarball will contain the essential userspace binaries (`pkg`, `paws`, `sshd`, `tar`, etc.) and a default `/etc/pkg/config` pointing to the public package server.
    ```bash
    curl -L "${AKUMA_INSTALL_BASE}/base_fs.tar.gz" -o base_fs.tar.gz
    ```

### Phase 3.3: Prepare Disk Image

This phase involves creating a raw disk image and populating it with the base filesystem. **Directly mounting and modifying a raw disk image within a shell script can be complex and error-prone due to loop devices and root privileges within a container.** A simpler approach for Docker might be to create a small FAT partition just for kernel/initrd and then another partition for the rootfs. Or, just make a single ext2/ext4 image. Given the constraints and typical Akuma setup, a single ext2 image is simplest.

1.  **Create Raw Disk Image**:
    ```bash
    DISK_IMG="akuma_disk.img"
    QEMU_DISK_SIZE="512M" # Or adjust as needed
    qemu-img create -f raw "$DISK_IMG" "$QEMU_DISK_SIZE"
    ```
2.  **Format Disk Image (ext2)**:
    ```bash
    mkfs.ext2 -F "$DISK_IMG"
    ```
3.  **Mount and Populate**:
    *   **Loop Device Setup**: Requires `sudo` and `losetup`. This can be problematic in some Docker setups.
    *   **Alternative for Docker: Use `mcopy` (if FAT) or `debugfs`/`mke2fs` (more complex for ext2/ext4).**
    *   **Simpler Alternative (Requires FUSE or specific Docker capabilities):**
        ```bash
        # Create a temporary mount point
        MOUNT_POINT="$INSTALL_DIR/mnt"
        mkdir -p "$MOUNT_POINT"

        # Mount the image (requires specific kernel modules or capabilities)
        # This part is highly dependent on Docker's capabilities.
        # It might require --privileged or hostfstab.
        # Fallback: manually extract to a temporary directory, then use `genisoimage`
        # for a CD-ROM based filesystem, or a more direct copy method.

        # For the purpose of this plan, let's assume a direct loopback mount is possible
        # in a sufficiently privileged Docker container for a full setup.
        sudo losetup -f "$DISK_IMG"
        LOOP_DEV=$(sudo losetup -j "$DISK_IMG" | cut -d: -f0)
        sudo mount "$LOOP_DEV" "$MOUNT_POINT"

        # Extract base filesystem
        sudo tar -xzf base_fs.tar.gz -C "$MOUNT_POINT"

        # Unmount
        sudo umount "$MOUNT_POINT"
        sudo losedup -d "$LOOP_DEV"
        ```
    *   **More Robust Docker-friendly alternative for disk population (if loop devices are hard):**
        *   Extract `base_fs.tar.gz` to a temporary directory on the host filesystem.
        *   Use `rsync -a --delete temp_fs/ "$MOUNT_POINT"` to copy files after loop-mounting.

### Phase 3.4: SSH Key Generation

1.  **Generate SSH Key Pair**:
    ```bash
    ssh-keygen -t rsa -N "" -f id_rsa # Private key
    cp id_rsa.pub authorized_keys    # Public key for the guest
    ```
2.  **Add Public Key to Disk Image**:
    *   Remount the `DISK_IMG`.
    *   Create `/root/.ssh` directory on the image.
    *   Copy `authorized_keys` to `/root/.ssh/authorized_keys` on the image.
    *   Set correct permissions (`chmod 600`).
    *   Unmount.

### Phase 3.5: QEMU Launch Script (`run_akuma.sh`)

1.  **Create QEMU Script**: Write a `run_akuma.sh` script to launch QEMU.
    ```bash
    cat <<EOF > run_akuma.sh
    #!/bin/bash
    qemu-system-aarch64 
        -M virt 
        -cpu cortex-a57 
        -kernel kernel.bin 
        -drive file=${DISK_IMG},if=virtio,format=raw 
        -nographic 
        -netdev user,id=net0,hostfwd=tcp::2222-:22 
        -append "console=ttyAMA0 earlyprintk=pl011"
    EOF
    chmod +x run_akuma.sh
    ```

### Phase 3.6: Final Instructions and Cleanup

1.  **Print Instructions**: Guide the user on how to start Akuma OS and connect via SSH.
    ```bash
    echo "Akuma OS setup complete!"
    echo "To start Akuma OS in QEMU: cd $INSTALL_DIR && ./run_akuma.sh"
    echo "To connect via SSH: ssh -p 2222 -i id_rsa root@localhost"
    ```
2.  **Cleanup Option**: Provide an option to delete the `$INSTALL_DIR`.
    ```bash
    echo "Temporary files are in $INSTALL_DIR. You can delete them manually."
    ```
3.  **Pre-configure `pkg`**: The `base_fs.tar.gz` should ideally contain a pre-configured `/etc/pkg/config` file that points to the `AKUMA_INSTALL_BASE` so that `pkg install` works out of the box in the running Akuma OS. This avoids needing `pkg config set` immediately.

## 4. Testing Considerations

*   **Docker Container Setup**: Test with different base images (Ubuntu, Alpine) to ensure dependency installation works.
*   **QEMU Versions**: Ensure QEMU command works across common versions.
*   **Network Forwarding**: Verify SSH forwarding works correctly.
*   **Disk Image Integrity**: Check if the base filesystem is correctly populated and SSH keys are in place.
*   **`pkg install` inside Akuma**: Once Akuma is running, use `pkg install <test_package>` to verify the entire package management pipeline from within the guest.
