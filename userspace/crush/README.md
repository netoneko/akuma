# Crush on Akuma OS

This directory contains the port of [Crush](https://github.com/charmbracelet/crush) to the Akuma operating system. Crush is a terminal-based coding assistant that integrates with various LLM providers (Anthropic, OpenAI, Groq, etc.).

## Project Structure

- **`crush/`**: The original Go source code for Crush.
- **`docs/IMPLEMENTATION_DETAILS.md`**: Technical details on the static build process and environment.

## Building for Akuma

Crush must be built as a statically linked AArch64 binary to run correctly on Akuma.

### Requirements

- **Go Toolchain:** `go1.26.1` or newer.
- **Cross-Compiler:** `aarch64-linux-musl-gcc` (required for static linking with musl).

### Build Command

Execute the following command to build the static binary and place it in the system's `bootstrap` directory:

```bash
cd crush
CC=aarch64-linux-musl-gcc \
CGO_ENABLED=1 \
GOOS=linux \
GOARCH=arm64 \
go build -ldflags="-s -w -linkmode external -extldflags '-static'" \
-o ../../../bootstrap/bin/crush .
```

The resulting binary will be an ELF 64-bit LSB executable, ARM aarch64, statically linked, and stripped.

## Deployment

The binary is automatically staged in `bootstrap/bin/crush` and can be used directly on the target machine.

For more information on the build process and implementation notes, please see [docs/IMPLEMENTATION_DETAILS.md](./docs/IMPLEMENTATION_DETAILS.md).
