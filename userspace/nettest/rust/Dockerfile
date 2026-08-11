# nettest builder — Alpine arm64. The probe is Linux/musl, not no_std kernel,
# so we cannot use the workspace's aarch64-unknown-none target. We also do NOT
# build on the macOS host: curl-sys builds libcurl + OpenSSL + nghttp2 from
# source when `static-curl`+`static-ssl` are on (matches nightly cargo's
# vendored libcurl), and that autotools build needs Linux.
#
# The resulting binary is statically linked — no .so dependencies — exactly
# like cargo's vendored libcurl setup. This is the configuration that
# reproduces the kernel bug; the dynamically-linked apk-libcurl build does not.
#
# Usage from this directory:
#   docker build --platform linux/arm64 -t nettest-builder -f Dockerfile .
#   docker run --rm --platform linux/arm64 -v "$PWD/../../..:/repo" nettest-builder \
#       /repo/userspace/nettest/rust/build.sh
# or just run ./build.sh, which does both.

FROM --platform=linux/arm64 alpine:latest

# build-base: gcc + musl-dev + make + autotools. curl-sys needs autoconf /
# automake / libtool to build vendored libcurl, plus perl for OpenSSL's
# configure. pkgconf ties it together.
RUN apk add --no-cache \
        build-base \
        autoconf \
        automake \
        libtool \
        perl \
        pkgconf \
        rust \
        cargo \
        bash

WORKDIR /repo

