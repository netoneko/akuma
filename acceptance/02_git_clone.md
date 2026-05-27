# Acceptance: Git Clone via apk

Verify that `git` can be installed from Alpine apk and used to clone a repository over HTTPS.

## Preparation (host)

### 1. Set up SSH authorized keys

```bash
mkdir -p bootstrap/etc/sshd/
cp ~/.ssh/id_ed25519.pub bootstrap/etc/sshd/authorized_keys
```

### 2. Build apk-tools and stage bootstrap assets

```bash
cd userspace && ./build.sh --apk-only && cd ..
```

### 3. Create and populate the disk image

```bash
./scripts/create_disk.sh
./scripts/populate_disk.sh
```

### 4. Start the VM

```bash
cargo run --release
# or: ./scripts/run.sh
```

Wait until SSH is accepting connections on `localhost:2222`.

## Steps (in VM)

### 5. Install git

```bash
ssh -p 2222 root@localhost "apk add git"
```

### 6. Clone the repository

```bash
ssh -p 2222 root@localhost "git clone https://github.com/netoneko/akuma-playground.git"
```

### 7. Verify the clone

```bash
ssh -p 2222 root@localhost "ls akuma-playground/"
```

## Expected Result

Step 5 installs git and its dependencies:

```
(1/x) Installing ...
OK: ... KiB in ... packages
```

Step 6 clones successfully:

```
Cloning into 'akuma-playground'...
remote: Enumerating objects: ...
...
Resolving deltas: 100% (...), done.
```

Step 7 lists the repository contents without error.
