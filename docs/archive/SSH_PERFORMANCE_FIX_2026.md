# SSH Performance Fixes (February 2026)

## Problem
Users reported input lag in SSH sessions. Investigation revealed two primary causes:
1. **Output Buffering**: SSH channel data was not being flushed immediately to the network, causing small packets (like individual keystroke echoes) to be delayed until a buffer filled or a timeout occurred.
2. **Artificial Delays**: The interactive shell execution loop (`execute_external_interactive`) contained a loop that yielded 20 times after every output chunk. This was intended to allow network transmission but effectively throttled the loop, delaying input polling.

## Fixes

### 1. Auto-Flush in `SshChannelStream`
Modified `src/ssh/protocol.rs`:
- Added `self.flush().await` to the end of `SshChannelStream::write`.
- Wrapped this flush in a 10ms timeout (`embassy_time::with_timeout`) to prevent blocking if the network is backed up.
- This ensures that every write operation (e.g., echoing a character) triggers a flush of the TCP stream and a yield to the network runner immediately, without risking a hang.

### 2. Removed Artificial Yield Loop
Modified `src/shell/mod.rs`:
- Removed the `for _ in 0..20 { yield_now() }` loop in `execute_external_interactive`.
- Relied on the explicit `flush()` (and the new auto-flush in `write`) to handle flow control and yielding.
- This significantly reduces the cycle time of the interactive loop, allowing it to poll for input much more frequently, even when the process is producing output.

## Result
- **Lower Input Latency**: Keystrokes are echoed immediately.
- **Smoother Output**: Screen updates in applications like `meow` should be faster and less "jerky".
- **Better Responsiveness**: The shell handles mixed input/output workloads (like typing while a command is running) much better.
