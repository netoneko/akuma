# Command Trait Crash Investigation

This document describes a subtle crash that occurs when adding new `Command` trait implementations to the shell.

## Symptom

When adding a new struct that implements the `Command` trait and registering it with the `CommandRegistry`, the kernel crashes during SSH host key initialization with:

```
[Exception] Sync from EL1: EC=0x0, ISS=0x0, ELR=0x400fxxxx, FAR=0x0
```

The crash always occurs in `curve25519_dalek` functions like:
- `curve25519_dalek::backend::variable_base_mul`
- `curve25519_dalek::backend::serial::curve_models::ProjectiveNielsPoint::conditional_assign`
- `curve25519_dalek::backend::serial::u64::scalar::Scalar52::pack`

## Key Findings

### What Works

| Pattern | Result |
|---------|--------|
| `Box<dyn TestTrait>` (simple sync trait) | ✓ Works |
| `Box::leak<dyn TestTrait>` | ✓ Works |
| `&dyn TestTrait` (stack reference) | ✓ Works |
| `Box::leak<u32>` (concrete type) | ✓ Works |
| Vec growing to 100 elements | ✓ Works |
| Existing 6 Command impls | ✓ Works |

### What Crashes

| Pattern | Result |
|---------|--------|
| New `impl Command for NewStruct` + register | ✗ Crashes |
| Even a minimal stub Command impl | ✗ Crashes |
| Code exists but NOT registered | ✓ Works |
| Same registration count with duplicated existing cmd | ✓ Works |

## The Command Trait

The `Command` trait has a complex async method signature:

```rust
pub trait Command: Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn aliases(&self) -> &'static [&'static str] { &[] }
    fn usage(&self) -> &'static str { "" }
    
    fn execute<'a>(
        &'a self,
        args: &'a [u8],
        stdin: Option<&'a [u8]>,
        stdout: &'a mut VecWriter,
    ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>>;
}
```

The `execute` method returns `Pin<Box<dyn Future<...>>>`, creating a nested trait object structure.

## Binary Search Debugging

We used binary search with padding to isolate the issue:

1. **Binary size is NOT the issue** - 4.04MB with padding works, 4.29MB with new Command crashes
2. **Command count is NOT the issue** - 20 commands works when duplicating ECHO_CMD
3. **The code must be REGISTERED** - Code existing but not called = works

## Reproduction Steps

1. Add any new struct implementing `Command`:
```rust
struct DummyCommand;
impl Command for DummyCommand {
    fn name(&self) -> &'static str { "dummy" }
    fn description(&self) -> &'static str { "Test" }
    fn execute<'a>(...) -> Pin<Box<dyn Future<...> + 'a>> {
        Box::pin(async move { Ok(()) })
    }
}
pub static DUMMY_CMD: DummyCommand = DummyCommand;
```

2. Register it:
```rust
registry.register(&DUMMY_CMD);
```

3. Run the kernel - crashes during SSH key initialization

## Probable Cause

The issue appears to be related to:

1. **Vtable layout for nested dyn traits** - The `Command` trait contains `Pin<Box<dyn Future>>`, creating complex vtable structures

2. **Linker section ordering** - Adding new trait implementations changes how vtables are laid out in `.rodata`, potentially affecting alignment of curve25519-dalek's lookup tables

3. **LTO interactions** - Link-time optimization may be reordering or optimizing code in ways that affect memory layout

4. **curve25519-dalek lookup tables** - The crypto library uses precomputed tables that may have specific alignment requirements not being honored

## Workarounds

1. **Don't add new Command implementations** - Use existing commands or extend them

2. **Use different trait patterns** - If possible, avoid `Pin<Box<dyn Future>>` return types

3. **Check linker script alignment** - Ensure `.rodata` has sufficient alignment (currently 128 bytes)

## Current Linker Script

```ld
/* Read-only data - 128-byte aligned for crypto lookup tables */
.rodata : ALIGN(128) {
    *(.rodata .rodata.*)
}
```

## Environment

- Target: `aarch64-unknown-none`
- Rust: nightly
- curve25519-dalek: 4.1.3
- QEMU: virt machine with `-cpu max`

## Related Files

- `src/shell/mod.rs` - Command trait definition
- `src/shell/commands/mod.rs` - CommandRegistry
- `src/ssh/protocol.rs` - SSH key initialization (crash site)
- `linker.ld` - Section layout

## Status

**Unresolved** - The root cause is unknown. The baseline works with existing commands, but adding new ones triggers the crash. This appears to be a compiler/linker edge case with complex async trait objects in no_std bare-metal environments.

