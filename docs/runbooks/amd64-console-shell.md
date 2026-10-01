# A shell on the trashcan's TV — `/bin/sh` on the console, no ssh

**Stability: B.** The path from a keystroke to a command is verified under QEMU.
The real keyboard and the real screen are verified only by someone looking at the
machine — see [Verify](#verify), which says which claim each check settles.

The HP box's only console is the framebuffer GRUB hands the kernel, and its only
local input is a USB keyboard that firmware presents as an i8042 (`kbd.rs`).
Since 2026-10-01 a login shell runs on that console beside `sshd`.

## What is running

`init=/bin/herd` supervises services, and **herd's services get pipes, not the
console** — so for the first month of this boot path nothing read the keyboard
and the TV showed the boot log and then nothing. The fix is one more service:

```
/etc/herd/enabled/console.conf
    command = /bin/sh
    console = true
    restart = true
    restart_delay = 1000
    env = TERM=linux
```

| piece | where | what it does |
|---|---|---|
| `console = true` | `userspace/herd/src/main.rs` | spawns the service with `SPAWN_FLAG_CONSOLE` and a full environment (`PATH`, `HOME`, `TERM`) |
| `SPAWN_FLAG_CONSOLE` (2) | `amd64/src/usermode.rs` `sys_spawn` | the child gets **no** channel of its own, so `register_exec_process` attaches it to the console channel and the shared `TerminalState` — exactly what `init` gets. Refused (`EPERM`) unless the **caller** is itself console-attached, so an ssh session cannot take the keyboard |
| the pump | `amd64/src/console.rs` | keyboard -> line discipline (`^C` -> `SIGINT`) -> the shell; the shell's echo -> screen |
| Backspace = `0x7f` | `amd64/src/kbd.rs` | was `0x08`, which cooked-mode `read(0)` does not treat as erase |
| ANSI subset | `crates/akuma-fbcon/src/console.rs` | busybox's line editor emits `\b`, `ESC[nD`, `ESC[J`, `ESC[K`; they used to be drawn as glyphs |
| cursor | same, + `multiboot2::cursor_idle` | a block, drawn when output goes quiet, removed by the next byte |

It restarts on exit, so `exit` gives a fresh prompt instead of a dead screen.
The shell's output is on the screen and **not** in `/var/log/herd/console.log`
(herd's drain reads the exit-status handle, which carries nothing).

## Turn it on or off

```sh
ssh akuma 'cp /etc/herd/available/console.conf /etc/herd/enabled/'   # herd reloads within 20 s
ssh akuma 'rm /etc/herd/enabled/console.conf; herd stop console'
```

`mkdisk.sh` stages and enables it on a fresh image. On the live partition it is a
file you write; **install the kernel and `/bin/herd` first** — an old herd ignores
`console =` and would run an ordinary piped `/bin/sh` that reads nothing.

## Test it off the metal

```sh
python3 scripts/utils/amd64_console_probe.py                  # INIT=/bin/herd (default)
INIT=/bin/busybox python3 scripts/utils/amd64_console_probe.py   # control: init is the shell
```

It boots `amd64/run.sh`, types through the 16550, and requires: a command's
output, `$0` naming the shell, a pipeline, **line editing with DEL**, and **`^C`
killing a `sleep 100` with the shell surviving**. Each marker is something the
typed line does not contain, so a reflected echo cannot pass. Run the **control**
once when you change the harness: a probe that has never been seen to pass proves
nothing about a silent boot.

What QEMU does **not** prove: the i8042 path (`microvm` has none — input arrives
on the serial port), the real keyboard, and any pixel on a real screen. The
framebuffer console's parsing is host-tested
(`cargo test -p akuma-fbcon`, which renders into memory and checks pixels).

## Verify

On the **TV**, after a boot (an ssh answer says nothing about this):

1. **A prompt** — `/ #` appears after the `[herd] Started console` line, with a
   solid block cursor after it. *(Settles: the service started and the screen shows it.)*
2. **Keys** — type `echo hello` and Enter: the characters appear as you type and
   `hello` follows. *(Settles: the i8042 emulation delivers keystrokes through the
   pump. If nothing appears, the gap is input, not the shell — ssh in and run
   `herd status`; the shell being up while the keys are dead points at `kbd.rs`.)*
3. **Erase** — type `abc`, press Backspace twice: only `a` remains, no `^H`, no
   `[D` debris.
4. **`^C`** — `sleep 100`, then Ctrl-C: the prompt returns at once.
5. **Restart** — `exit`: a new prompt within ~1–2 s.
6. **ssh still works** — `ssh akuma 'echo ok'`, and an interactive ssh session is
   unaffected by what you typed on the TV.

To tell a *false* pass from a real one: the screen **is** the evidence for 1–5;
`dmesg` only shows what the kernel believes. For 2, a prompt you did not type
into proves nothing — look at your own keystrokes appearing.

If the box does not come back: pick **`Akuma/amd64 (known good)`** at the GRUB
menu (10 s). `/boot/akuma-amd64.prev` holds the kernel that was installed before
this one.

## Known limits

- No arrow keys, Home/End or function keys (`kbd.rs` drops extended scancodes);
  history and cursor-left editing are unavailable. Backspace works.
- busybox sends `ESC[6n` (cursor-position query) at each prompt and the console
  does not answer it; the shell proceeds after its own short timeout.
- Colours are ignored (`ESC[…m` is parsed and dropped).
- One console shell. Two programs reading the console share one input queue.

## Background

- [`../archive/AKUMA_TEAHOUSE_CONSOLE_EXPERIMENT.md`](../archive/AKUMA_TEAHOUSE_CONSOLE_EXPERIMENT.md) — the experiment, the paused swarm attempt, and the 2026-10-01 log of this work.
- [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) — the box, its two personalities and the GRUB entries; the `tail -f ignores ^C` row there is the shared-`TerminalState` bug the console-attachment rule comes from.
