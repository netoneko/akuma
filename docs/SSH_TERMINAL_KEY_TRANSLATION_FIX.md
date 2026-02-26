# SSH Terminal Key Translation Fix

## Problem

In the built-in SSH shell, pressing the **Delete key** (fn+backspace on Mac, sends `\x1b[3~`) would
insert a literal `~` character instead of deleting the character at the cursor.

The shell's escape sequence state machine only handled single-character CSI sequences like arrow
keys (`\x1b[A`, `\x1b[B`, `\x1b[C`, `\x1b[D`). The sequence `\x1b[3~` requires tracking two
characters after `[` — the digit `3` and the terminating `~`.

## Root Cause

In `src/ssh/protocol.rs`, the `EscapeState` enum had three variants:

```
Normal → Escape (on \x1b) → Bracket (on [) → Normal (on next byte)
```

The `Bracket` handler reset state to `Normal` unconditionally at the top, then matched the
incoming byte. The `b'3'` arm existed but was empty (just a comment). So:

1. `\x1b` → state = `Escape`
2. `[` → state = `Bracket`
3. `3` → state reset to `Normal`, `b'3'` arm matched but did nothing
4. `~` → back in `Normal`, `~` (0x7E) is in the printable range `0x20..0x7F`, so it was
   **inserted as a literal character**

## Fix

Added a `BracketNum(u8)` variant to `EscapeState` to hold the accumulated digit and wait for the
`~` terminator:

```
Normal → Escape → Bracket → BracketNum(digit) → Normal (on ~, with action)
```

The `Bracket` handler now transitions to `BracketNum(byte - b'0')` for digits `1..=8` instead of
resetting to `Normal`. The new `BracketNum(n)` handler fires on `~` and performs:

| Sequence  | Key    | Action                          |
|-----------|--------|---------------------------------|
| `\x1b[3~` | Delete | Remove char at cursor, redraw   |
| `\x1b[1~` | Home   | Move cursor to beginning of line |
| `\x1b[4~` | End    | Move cursor to end of line      |

## Files Changed

- `src/ssh/protocol.rs`: Added `BracketNum(u8)` to `EscapeState`, replaced empty `b'3'` arm with
  `b'1'..=b'8'` range transitioning to `BracketNum`, added `BracketNum` match arm with delete/home/end logic.
