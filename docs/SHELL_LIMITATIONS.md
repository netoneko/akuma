# Shell Limitations

This document describes current limitations of the Akuma shell implementation.

## Not Command-Aware

The shell is currently **not command-aware**. It treats all commands as opaque strings and does not have semantic knowledge about what each command does or expects.

### What This Means

1. **No command-specific completion**: The shell cannot provide intelligent tab-completion based on a command's expected arguments or options.

2. **No argument validation**: The shell passes arguments directly to commands without any validation or type checking.

3. **No built-in help for external commands**: While built-in commands have help text, the shell cannot introspect external binaries in `/bin` for usage information.

4. **No command suggestions**: If you mistype a command, the shell cannot suggest similar commands.

5. **No semantic parsing**: The shell doesn't understand command-specific syntax like flags (`-v`, `--verbose`), subcommands, or positional arguments.

### Current Behavior

The shell currently:

- Splits input on whitespace to separate the command name from arguments
- Handles basic quoting (`"..."` and `'...'`) for arguments with spaces
- Supports pipeline execution via `|`
- Supports command chaining via `;` and `&&`
- Supports output redirection via `>` and `>>`
- Looks up commands in the built-in registry first, then falls back to `/bin`

## Known Bugs

### Output Redirection Ignores Current Directory

Output redirection (`>` and `>>`) does not respect the current working directory. Relative paths are treated as absolute paths from root.

**Example:**
```bash
$ cd /home
$ echo meow > meow.txt
# Creates /meow.txt instead of /home/meow.txt
```

The redirect target path is passed directly to `write_file`/`append_file` without being resolved through `ShellContext::resolve_path()`. This means `meow.txt` is interpreted as `/meow.txt` rather than being relative to the shell's current directory.

**Workaround:** Use absolute paths for redirection targets:
```bash
$ echo meow > /home/meow.txt
```

### Userspace Programs Ignoring CWD

The following userspace programs do not respect the current working directory:

- **quickjs** - JavaScript file paths are resolved from root
- **sqld** - Database file paths are resolved from root

These programs need to be updated to use the CWD passed by the kernel.

### Future Improvements

Potential enhancements to make the shell more command-aware:

- Command metadata/manifest files describing arguments and options
- Tab completion for file paths and command names
- Man-page style documentation for external commands
- Shell aliases and custom command definitions
