# GNU Make for Akuma OS

This directory contains the necessary files to build GNU Make (version 4.4) as a userspace application for Akuma OS. The build process is orchestrated by the `build.rs` script, which downloads the source, configures it for `aarch64-linux-musl` with `clang`, compiles it statically, and places the resulting binary in `bootstrap/bin`.
