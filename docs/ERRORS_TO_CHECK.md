# Errors to check

## panic in httpd

```
[accept] Waiting on port 8080 (slot 2)
[accept] fd=2966 idx=1
[accept] Waiting on port 8080 (slot 1)
[accept] fd=2967 idx=1


!!! PANIC !!!
Location: /Users/netoneko/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/embassy-net-0.6.0/src/lib.rs:386
Message: RefCell already borrowed
```
