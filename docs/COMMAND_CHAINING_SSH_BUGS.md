# Command Chaining (`;` and `&&`) SSH Bugs

This document describes issues found when testing command chaining via SSH on 2026-01-02.

## Summary

The `;` and `&&` command chaining operators are implemented but have bugs in the SSH exec mode that cause incorrect behavior.

## Test Results

### Working Cases

| Test | Command | Expected | Actual | Status |
|------|---------|----------|--------|--------|
| 1 | `echo first; echo second; echo third` | All three printed | All three printed | ✅ PASS |
| 2 | `echo first && echo second && echo third` | All three printed | All three printed | ✅ PASS |
| 3 | `echo a; echo b && echo c` | All three printed | All three printed | ✅ PASS |
| 4 | `nonexistent_cmd && echo should_not_print` | Only error | Only "Command not found" | ✅ PASS |
| 8 | `cat /etc/passwd; echo done` | Error then "done" | "Error reading file" then "done" | ✅ PASS |
| 11 | `echo hello && ls` | hello + ls output | hello + ls output | ✅ PASS |

### Failing Cases

| Test | Command | Expected | Actual | Status |
|------|---------|----------|--------|--------|
| 5 | `nonexistent_cmd; echo should_print` | Error then "should_print" | Only "Command not found" | ❌ FAIL |
| 6 | `ls && pwd` | ls output + pwd output | ls output + "Command not found" | ❌ FAIL (pwd not found) |
| 7 | `cd /etc && ls` | ls /etc output | "Command not found" | ❌ FAIL (cd not found) |

## Bug #1: `;` Operator Breaks on Command Not Found

**Location**: `src/ssh/protocol.rs` lines 1298-1302

**Problem**: The exec handler unconditionally breaks out of the command chain loop when any command fails:

```rust
// For && operator from previous command, skip if last failed
if !last_success {
    // Check if previous was &&
    break;  // <-- BUG: Always breaks, ignoring ; operator
}
```

**Expected Behavior**: 
- `;` should ALWAYS execute the next command regardless of previous result
- `&&` should ONLY execute next command if previous succeeded

**Root Cause**: The code doesn't track the operator from the PREVIOUS command. It checks `chained_cmd.next_operator` which is the operator AFTER the current command, not before.

**Fix**: Track `prev_operator` across iterations:

```rust
let chain = shell::parse_command_chain(trimmed);
let mut last_success = true;
let mut prev_operator: Option<shell::ChainOperator> = None;

for chained_cmd in &chain {
    // Check if we should skip based on PREVIOUS operator
    if let Some(shell::ChainOperator::And) = prev_operator {
        if !last_success {
            // && and previous failed - skip this and remaining commands
            break;
        }
    }
    // For ; operator or no previous operator, always continue
    
    // ... execute command ...
    
    // Track operator for next iteration
    prev_operator = chained_cmd.next_operator;
}
```

## Bug #2: Missing Builtin Commands (pwd, cd)

**Problem**: Several common shell commands are not implemented:
- `pwd` - "Command not found"
- `cd` - "Command not found"

**Root Cause**: These commands are not registered in `create_default_registry()`.

**Location**: `src/shell/commands/mod.rs` lines 72-105

**Currently Registered Commands**:
- Builtin: `echo`, `akuma`, `stats`, `free`, `help`, `grep`
- Filesystem: `ls`, `cat`, `write`, `append`, `rm`, `mv`, `mkdir`, `df`
- Network: `curl`, `nslookup`, `pkg`
- Scripting: `rhai`
- Process: `exec`

**Missing Common Commands**:
- `pwd` - Print working directory
- `cd` - Change directory
- `cp` - Copy files
- `touch` - Create empty file
- `head` / `tail` - View file portions
- `env` - Environment variables
- `whoami` - Current user

## Bug #3: Command Chaining NOT Supported in Interactive SSH Shell

**Problem**: Command chaining works in exec mode (`ssh user@host "cmd1; cmd2"`) but NOT in interactive shell mode.

**Test**:
```
$ ssh user@localhost -p 2222
akuma> echo a; echo b
a; echo b     <-- Wrong! Treated ; as part of echo arguments
```

**Expected**:
```
a
b
```

**Root Cause**: Interactive shell mode in `run_shell_session()` (line 727) uses `parse_command_line()` directly without calling `parse_command_chain()` first.

**Location**: `src/ssh/protocol.rs` line 818

**Current Flow (Interactive)**:
```
user input → parse_command_line() → execute_pipeline()
```

**Should Be**:
```
user input → parse_command_chain() → for each → parse_command_line() → execute_pipeline()
```

## Bug #4: Console Shell Also Missing Command Chaining

**Problem**: The console shell (non-SSH) also doesn't support command chaining.

**Location**: `src/shell/mod.rs` lines 467-468

```rust
// Parse and execute pipeline
let stages = parse_pipeline(trimmed);  // <-- Should call parse_command_chain() first
```

**Current**: Direct call to `parse_pipeline()` without chain parsing.

## Summary: Where Command Chaining is (Not) Implemented

| Shell Mode | Uses parse_command_chain()? | Chaining Works? |
|------------|---------------------------|-----------------|
| SSH exec (`ssh host "cmd"`) | ✅ Yes (line 1289) | ⚠️ Partial (bug #1) |
| SSH interactive (`ssh host`) | ❌ No | ❌ No |
| Console shell | ❌ No | ❌ No |

The `parse_command_chain()` function exists and is correct, but it's only called in SSH exec mode, and even there it has a bug in the operator handling logic.

## Additional Notes

- The SSH exit code is always 255 regardless of command success (separate issue)
- Test 8 shows `;` works correctly after a command that exists but fails (cat with missing file)
- Test 5 shows `;` fails after "Command not found" - suggesting the issue is specific to command lookup failures

## Recommendations

### Priority 1: Fix Exec Mode Operator Logic
Fix `src/ssh/protocol.rs` lines 1298-1302 to track `prev_operator` and only break for `&&` failures.

### Priority 2: Add Command Chaining to All Shell Modes
1. **SSH interactive**: Modify `run_shell_session()` at line 818 to use `parse_command_chain()`
2. **Console shell**: Modify `ShellSession::run()` in `src/shell/mod.rs` line 468 to use `parse_command_chain()`

### Priority 3: Add Missing Builtin Commands
Add `pwd` and `cd` commands to `create_default_registry()` in `src/shell/commands/mod.rs`.

### Priority 4: Testing
Add automated tests for command chaining in all shell modes.

---

## Architectural Note: Remove the Console Shell

**Recommendation**: Remove the basic console shell entirely and keep only the SSH shell.

**Rationale**:
- Having two shell implementations (console + SSH) causes confusion and duplicated logic
- Features like command chaining need to be implemented in multiple places
- The console shell in `src/shell/mod.rs` duplicates functionality from `src/ssh/protocol.rs`
- SSH is the primary interface for interacting with Akuma
- Maintaining one shell implementation is simpler and less error-prone

**Migration Path**:
1. Ensure SSH shell has all features from console shell
2. Remove `ShellSession::run()` and related console shell code
3. Boot directly into SSH server mode (current behavior)
4. Users connect via `ssh user@localhost -p 2222`
