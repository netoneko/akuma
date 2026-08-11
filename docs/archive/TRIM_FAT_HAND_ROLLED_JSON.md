# Hand-rolled JSON: an inventory

Audit of `src/`, `crates/`, and first-party `userspace/*/src` for code that
parses or emits JSON without a real parser crate. Read as-is at commit
`e94b69c` (branch `box-run`); nothing here was edited. `userspace/box/src/json.rs`
was mid-conversion to `picojson` in a parallel session while this was written
(uncommitted working-tree changes on top of `d8ab0d5`) — it was read for
context but not audited or touched, per instruction.

## Summary

- **`src/` and `crates/`: zero.** `grep -rli json src crates` finds nothing —
  no file, comment, or identifier in the kernel or the host-testable crates
  mentions JSON at all. Nothing to trim there.
- **First-party `userspace/*/src`: 8 files, ~675 lines, two independent
  problem domains.** `userspace/herd` (the OCI-bundle supervisor) hand-parses
  `config.json` with four functions (~180 lines). `userspace/meow` (the chat
  agent) has **seven** separate hand-rolled JSON sites spread across
  `util.rs`, `tools/helpers.rs`, `api/mod.rs`, `api/client.rs`, `app/chat.rs`,
  `app/history.rs`, and `ui/tui/stream.rs` (~495 lines) — none of them share
  code with each other, and three of them (`api/mod.rs`, `api/client.rs`,
  `app/chat.rs`) independently reimplement the same `extract_json_string`
  function with three different bug profiles.
- **`userspace/box`: in progress, excluded from this count.** `json.rs` (184
  lines at HEAD) is being rewritten right now to a path-addressed layer over
  `picojson` (already in `Cargo.toml`); the new file already exists in the
  working tree (480 lines) with `manifest.rs` (272 lines, untracked) as its
  first consumer, but `main.rs`/`run.rs`/`oci.rs`/`images.rs`/`tests.rs` still
  call the old API (`extract_object`, `extract_string`, `extract_array`,
  `extract_string_array`, `iter_array_objects`) that no longer exists in the
  working-tree `json.rs` — the tree does not currently compile with
  `--features akuma` until those callers are migrated too. That migration is
  someone else's in-flight work; not analyzed further here beyond using its
  own doc comment (quoted below) as corroboration for bug classes found
  independently in `herd`.
- **No crate anywhere in the workspace pulls in a real JSON parser except
  `userspace/box`'s pending `picojson`.** `grep -rl picojson\|serde --include
  Cargo.toml .` turns up only `userspace/box/Cargo.toml` (picojson) and
  `userspace/nca/native-cli-ai/**/Cargo.toml` (serde — that's the vendored
  submodule, out of scope, already doing this right).
- Three concrete, reproduced correctness bugs are documented below (§ "What
  actually breaks"): two in `herd`'s OCI config parser that corrupt real
  `sh -c "..."`-style container commands, one in `meow`'s JSONL history writer
  that can emit syntactically invalid JSON on the next turn.

## Sites

### `userspace/herd/src/main.rs` — OCI runtime `config.json` reader

Lines 214–403 (`json_get_str`, `json_get_str_array`, `json_get_object`,
`json_get_mounts`, `parse_oci_config`), ~180 lines. **Parses only.**

Reads the OCI runtime-spec `config.json` that `box` (or any OCI-compliant
image tool) writes into a bundle directory, to find the root filesystem path,
the process's `args`/`env`/`cwd`, and the `mounts` array — this is what `herd`
uses to actually spawn the containerized process (`main.rs:1152–1189`,
`parse_oci_config` result feeds `command`/`args` passed to `spawn_in_box`).
No unit tests exist for any of these four functions (`grep -c '#\[test\]'
userspace/herd/src/main.rs` → 0) — the parsing logic that decides what command
a container boots into has zero coverage.

- `json_get_str` (216–229): `find("\"key\"")`, skip to next `:`, take
  everything up to the next `"`. No escape handling — a bare `find('"')`.
- `json_get_str_array` (232–276): same idea for `["a", "b"]`, splitting on the
  first `]` for the array's extent, then per-element `find('"')` pairs, again
  no escape handling.
- `json_get_object` (280–309): brace-depth counting to find a nested object's
  extent — but the counter is **not string-aware**: it counts every `{`/`}`
  byte in the slice, including ones inside quoted string values.
- `json_get_mounts` (313–380): same non-string-aware brace counting, once per
  array element, to carve out each mount object before calling `json_get_str`
  on it for `destination`/`type`.
- `parse_oci_config` (383–403): glues the above into `OciConfig` — `root.path`,
  `process.args`/`process.env`/`process.cwd`, `mounts`.

Adversarial input, reproduced (see "What actually breaks" below): an escaped
quote inside an `args` string (`"-c", "echo \"hi\""` — an entirely ordinary
OCI config for any image whose entrypoint is `sh -c '...'` with quoting)
truncates the arguments array mid-element and silently drops everything after
it. An unescaped `}` inside any string value under `process` (an env var
value, a cwd, an arg) closes the `process` object early, silently discarding
`args` if it comes after the offending field in the object — the container
then either launches the wrong command (falls back to the service `.conf`'s
`command`) or fails to start ("No command in OCI config or service config").

### `userspace/meow/src/util.rs` — `json_escape_to`

Lines 4–16 (13 lines). **Emits only.** Character-by-character escaper: `"`,
`\`, `\n`, `\r`, `\t` to their two-char forms, other control characters (via
`char::is_control()`) to `\u{:04x}`, everything else passed through. This one
is *correct* as far as it goes — it's the one escaper in `meow` that every
other hand-rolled emit site in this list reuses (`app/history.rs`,
`api/client.rs`, `app/chat.rs`) — but see § "What actually breaks" for two
call sites that skip it where it was needed.

### `userspace/meow/src/tools/helpers.rs` — tool-argument extraction

Lines 5–83 (`extract_string_field`, `extract_number_field`), ~77 lines.
**Parses only.** This is the path every tool call actually executes through:
`tools/mod.rs::execute_tool_by_name` calls `extract_string_field`/
`extract_number_field` on the model's `args_json` for every one of the 17
tools (`FileRead`, `FileWrite`, `Shell`, …) — a `filename`, `content`, `cmd`,
etc. that comes back wrong here is a wrong file written or a wrong shell
command run, not just a cosmetic glitch.

`extract_string_field` does handle backslash escapes (`\n`, `\r`, `\t`, `\"`,
`\\`, `\/`, `\uXXXX`) character-by-character, so it survives escaped quotes
inside the value it's extracting — better than `herd`'s equivalent. But like
every site in this list it finds `"field"` as a **flat substring search**
with no depth or structure awareness; for `meow`'s flat, single-level tool-args
objects that happens not to bite today (no nested objects, no key name reused
across two tools' argument sets), but it is not depth-safe by construction
and would misfire the moment any tool grew a nested-object argument.
`extract_number_field` only accepts `is_ascii_digit()` characters — no sign,
no decimal point — so a negative `start`/`end`/`context` argument silently
returns `None` (callers `unwrap_or(1)`/`unwrap_or(2)` mask it as "use the
default" rather than erroring).

### `userspace/meow/src/api/mod.rs` — models-list parser

Lines 40–112 (`parse_openai_models`, `extract_json_string`), ~72 lines.
**Parses only.** Fetches `GET /v1/models` from the configured provider and
extracts each entry's `id`. `parse_openai_models` is actually the
most careful hand-rolled scanner in this whole audit: it walks the `data`
array's characters tracking `depth`/`in_string`/`escape_next` correctly (a
real backslash-escape-aware, string-aware brace matcher), so braces or
brackets inside a model's metadata strings don't break object-boundary
detection — this is the one site in the whole list that gets that part right.
Its `extract_json_string` (81–112), used per matched object to pull `id`, is
weaker than its sibling copies elsewhere in `meow` (see next entry) — it has
no `\u` unicode-escape handling at all, so a model ID containing a
`\uXXXX` escape comes out with the literal two characters `\` and the escape
digits' first character in the wrong place (falls into the `_ => { push('\\');
push(next) }` catch-all, which for `u` pushes `\u` literally and leaves the
four hex digits as plain text afterward) instead of decoding it. Low real-world
impact — model catalog IDs are `[a-z0-9:._-]` ASCII in every provider observed
in this codebase's docs — but it's a correctness gap absent from the `client.rs`
copy of the "same" function.

### `userspace/meow/src/api/client.rs` — streaming response reader + request body writer

Lines 348–437 (emit: `write_chat_body`/`write_chat_body_inner`/
`stream_conversation_messages`) and 736–866 (parse:
`parse_streaming_line`, `accumulate_tool_call_delta`,
`extract_openai_delta_content`, `json_value_start`, `json_field_is`,
`extract_json_string`), ~188 lines total. **Both parses and emits** — the
biggest single hand-rolled JSON site in the tree.

Emit half streams the OpenAI-compatible chat-completions request body
straight to an fd (`{"model":...,"messages":[...],"stream":true,...,"tools":
<const>,"tool_choice":"auto"}`), reading the on-disk JSONL conversation log
one line at a time so the whole conversation never sits in RAM at once
(explicitly the point — see the file's doc comment). This is closer to a
templated writer than a general JSON emitter: everything but `model` and each
already-JSON-encoded conversation line is a literal string constant, and
`model` goes through `json_escape_to`.

Parse half handles Server-Sent-Events streaming from the API: `parse_streaming_line`
strips the `data:` SSE prefix, `extract_openai_delta_content` digs out
`.choices[0].delta.content`, `accumulate_tool_call_delta` digs out a
`tool_calls` delta's `id`/`name`/`arguments` fragments and appends them
(streaming tool-call arguments arrive character-by-character across many SSE
events, so this genuinely needs to be incremental/fragment-tolerant — a real
parser would still need a resumable-tokenizer story here, not a trivial swap).
`json_value_start`/`json_field_is`/`extract_json_string` are the load-bearing
primitives underneath both: `json_value_start` explicitly tolerates optional
whitespace on both sides of the colon (doc comment: "mlx-server's `json.dumps`
defaults insert a space after every colon; ollama's compact serializer does
not"), which is a real, previously-hit compatibility bug, not theoretical —
the fix is why this helper exists and is factored out at all.
`extract_json_string` here is the most complete of the three copies: it does
handle `\uXXXX` (via `u32::from_str_radix` + `char::from_u32`), but only a
single 4-hex-digit escape — no UTF-16 surrogate-pair handling, so any
character outside the Basic Multilingual Plane (astral characters, e.g. most
emoji) JSON-encodes as *two* `\uXXXX` escapes and this parser decodes each
half independently; `char::from_u32` on a lone surrogate half returns `None`,
so the character is silently dropped rather than reconstructed. Given this is
decoding a *tool name*, not free-form model prose, the practical odds of
hitting a surrogate pair here are low, but it is a real gap.

### `userspace/meow/src/app/chat.rs` — summary extraction + tool-call re-serialization

Lines 171–220 (`json_value_start`, `extract_json_string`,
`serialize_tool_calls`), ~48 lines. **Both.**

`json_value_start`/`extract_json_string` (171–204) are, per their own doc
comment, a deliberate copy of `api::client`'s pair — "mirrors
`api::client::json_value_start`" — kept in sync by hand rather than shared,
used here to pull the `summary` argument out of a `CompactContext` tool call.
This copy lacks the `\u` handling `client.rs`'s has (falls through to the
`_ => push('\\'); push(next)` arm same as `api/mod.rs`'s copy) — so of the
three near-identical `extract_json_string`s in this codebase, two behave
differently from the third on the exact same class of input, despite one
being written specifically "to mirror" another.

`serialize_tool_calls` (206–220) emits `[{"id":...,"type":"function",
"function":{"name":...,"arguments":...}}]` to store as the JSONL log's
`tool_calls` field for the next turn. `arguments` goes through
`json_escape_to`; **`tc.id` and `tc.name` are pushed with `s.push_str(&tc.id)`
/ `s.push_str(&tc.name)` directly, unescaped.** Both come from
`accumulate_tool_call_delta`/`extract_json_string` in `client.rs` — i.e. they
are already-unescaped strings sourced from the model provider's response, not
compile-time constants. See "What actually breaks" below.

### `userspace/meow/src/app/history.rs` — `Message::write_json`

Lines 25–46 (22 lines). **Emits only.** Writes one JSONL line per
conversation message: `{"role":...,"content":...}` or (for assistant messages
with tool calls) `{"role":...,"content":null,"tool_calls":<pre-serialized
JSON from serialize_tool_calls>}`, plus an optional `tool_call_id`. `role` is
always one of meow's own string literals (`"user"`/`"assistant"`/`"tool"`/
`"system"`) so pushing it unescaped is safe. `content` correctly goes through
`json_escape_to`. `tool_call_id` (39–43) is pushed unescaped — same
provider-controlled-string gap as `tc.id`/`tc.name` in `app/chat.rs` above,
since `ToolCallData::id` is where `tool_call_id` ultimately comes from.

### `userspace/meow/src/ui/tui/stream.rs` — inline tool-call notification parser

Lines 176–256 (`extract_tool_info`, `find_wrapped`, `extract_field_value`),
~75 lines, plus the streaming brace/string-tracking state machine in
`StreamState::BufferingJson` (11–17, 122–149) that decides *when* to try
parsing. **Parses only, and not the path that executes tools.**

This is cosmetic: while streaming the assistant's markdown response to the
terminal, if the model embeds a JSON tool-call blob inline in prose (as
opposed to using the API's structured `tool_calls` field, which is what
actually drives execution via `client.rs`/`tools/mod.rs`), this code
best-effort recognizes it and prints `ToolCalled: X | Arguments y="z"` instead
of the raw JSON. `find_wrapped`/`extract_field_value` search a fixed list of
15 candidate field names (`filename`, `path`, `cmd`, `url`, …) for the first
occurrence of `"field"`, `'field'`, or bare `field:` anywhere in the buffered
JSON text — no depth tracking at all, the same "matches a key at any depth"
shape `box`'s own pre-conversion doc comment calls out (quoted below). Because
this only decorates a chat notification and never reaches a filesystem or
shell call, a misattributed field here is a wrong word in a status line, not
a wrong file write.

The **buffering state machine** that decides when a `{`/`}` block is "done"
(122–149) *does* track `in_string`/`escape` correctly — braces inside string
values don't prematurely close the block — so the bug in this file is
entirely in the *field extraction* step after the block is captured, not in
finding the block's boundary.

## Consolidation

`box`'s in-flight direction — a thin path-addressed layer over `picojson`
(`no_std`, allocation-free pull parser, `int64` + `float-skip` features) — is
the right target to measure every other site against. Its own doc comment
states precisely the bug class this audit re-derived independently in `herd`:

> This used to be a hand-rolled scanner that searched for `"key"` and
> returned the raw slice after it. That is a substring search wearing a
> parser's name: it matches a key at *any* depth... and a brace inside a
> string could end an object early.

| Site | Move to picojson? | Cost / why |
|---|---|---|
| `herd::json_get_*`/`parse_oci_config` | **Yes, straightforward.** | Same document shape as `box`'s OCI *manifest*/*config* reading (OCI runtime-spec `config.json` vs. `box`'s image-spec `config.json` — different schema, identical parsing shape: nested objects, string/array fields, no floats). `herd` is a `no_std` musl-target binary already linking `libakuma`, same constraints `box` has. A `walk`/`string_at`/`strings_at`-style module ported from `box::json` (or literally shared via a small crate, see below) would directly replace all four functions and *fix* both reproduced bugs for free — this is the highest-value, lowest-risk conversion in the list, and the one with the most severe consequence (wrong argv for a spawned container) if left alone. |
| `meow::tools::helpers` (`extract_string_field`/`extract_number_field`) | **Yes.** | Flat single-level objects, no floats needed today (`start`/`end`/`context` are all integers — picojson's `int64` feature covers them, and `extract_number_field`'s missing negative-number support would be fixed as a side effect). Would also fix the depth-fragility noted above before it ever gets exercised by a future nested-argument tool. |
| `meow::api::mod`/`client`/`app::chat`'s three `extract_json_string` + `json_value_start` copies | **Yes — collapse to one, then move that one to picojson.** | These should not exist as three copies regardless of parser choice; the immediate win is deleting two of them and calling the third. Moving the survivor to picojson would also fix the `\u`-surrogate-pair gap and the ollama-vs-mlx-server whitespace compatibility issue would come for free (a real streaming/pull parser doesn't care about incidental whitespace). The one real complication: this code parses **SSE fragments of a still-growing JSON value** (`tool_calls[].function.arguments` arrives one partial string-chunk at a time across many events) — a document-oriented pull parser like `picojson::SliceParser` wants a complete document per call, so `accumulate_tool_call_delta`'s incremental accumulation would need to stay hand-rolled at the *fragment* level (buffer a full SSE `data:` line, which usually *is* one complete small JSON object even though the aggregate `arguments` string is not) and only the per-line extraction switches to picojson. Doable, not a pure find-and-replace. |
| `meow::api::mod::parse_openai_models` | **Yes, and it's the easiest of the meow sites** — it already does correct string/depth-aware brace matching by hand; swapping that hand-rolled bracket walk for `picojson::walk`-style iteration is a pure simplification, not a bug fix, since this one wasn't buggy. |
| `meow::util::json_escape_to` | **No need to move, but should stay the single shared escaper.** | It's correct, `no_std`-friendly (`core::fmt::Write`), and already the one place every emit site *should* funnel through. The actual fix needed here is not "replace with a library" but "make every call site use it" — see `serialize_tool_calls`/`Message::write_json`'s unescaped `id`/`name`/`tool_call_id` fields below. |
| `meow::app::history::Message::write_json` / `app::chat::serialize_tool_calls` / `api::client`'s request-body writer | **No — these are hand-rolled *by design*, and that's correct.** | All three exist specifically to avoid materializing a JSON value/tree in memory: `write_chat_body_inner` streams the request body straight to an fd from an on-disk JSONL log so peak RAM is bounded by one message, not the whole conversation (explicit design goal per its doc comment, on a memory-constrained target). A `serde`-shaped "build a `Value`, then serialize it" approach is exactly the allocation pattern this was written to avoid. The fix these three need is narrower: route the two missed fields (`id`, `name`/`tool_call_id`) through the `json_escape_to` they already import and mostly use, not a parser swap. |
| `ui::tui::stream::extract_tool_info`/`find_wrapped`/`extract_field_value` | **Maybe, low priority.** | It's the most structurally unsound parser in the list (no depth tracking at all) but the least consequential (a terminal notification string, not a decision that touches the filesystem or a process). picojson could replace it cleanly since the surrounding buffer is a complete captured JSON blob by the time extraction runs (the state machine already found its extent) — but given the impact ceiling, this is a "nice to have, do it if `box`'s conversion pattern is being copied into `meow` anyway" item, not a priority fix on its own. |
| Anything in `src/` or `crates/` | **N/A — nothing exists to move.** | The audit found zero JSON handling in the kernel or host-testable crates. Nothing here interacts with the `safe_print!`/no-allocation-on-console-paths rule in `CLAUDE.md` either, since none of these sites are console/kernel paths — every one of them is a `userspace/*` binary already using `alloc::String`/`Vec` freely. |

**A structural note for whoever picks this up:** `herd` and `box` both parse
OCI JSON documents (runtime-spec `config.json` and image-spec
manifest/config respectively) but are two entirely separate binaries in the
same `userspace/` workspace with no shared crate between them today. If
`herd`'s conversion happens, factoring the picojson-based path-addressed
`walk`/`string_at`/`strings_at`/`number_at`/`exists` API `box::json` is
building out of `box`'s own crate and into something both binaries depend on
(a new `userspace/oci-json` crate, or promoting it out of `boxlib`) avoids
ending up with a *second* picojson wrapper that's a near-copy of the first —
the same "three copies of `extract_json_string`" mistake this audit found in
`meow`, one abstraction level up.

## What actually breaks

Ranked by how likely real input is to trigger them, most likely first. The
first two were reproduced with a standalone host-`rustc` build of the exact
function bodies (not guessed):

1. **`herd::json_get_str_array` truncates an array element at the first
   escaped quote inside it — breaks any `sh -c "..."`-style OCI entrypoint.**
   `{"args": ["/bin/sh", "-c", "echo \"hi\""]}` parses to
   `["/bin/sh", "-c", "echo \\"]` — the third argument silently loses
   everything from the escaped quote onward, and the array's true remainder
   (`hi""`, the closing `]`) is left dangling unconsumed. This is not an edge
   case: `-c "command with quotes"` is an extremely common container
   entrypoint shape. Reproduced directly (see below).
2. **`herd::json_get_object`/`json_get_mounts`'s brace counting is not
   string-aware — an unmatched `{`/`}` inside any string value inside
   `process` truncates the object early and silently drops whatever comes
   after it in the object.** Reproduced: `{"process": {"cwd": "/", "env":
   ["MOTD=Welcome}"], "args": ["/bin/sh"]}}` — `json_get_object(doc,
   "process")` returns only `"cwd": "/", "env": ["MOTD=Welcome` and `args`
   disappears entirely, because `Welcome}`'s `}` is counted as the object's
   closing brace. `parse_oci_config` then has an empty `process_args`, so the
   container either launches the *service file's* `command` instead of the
   image's real entrypoint, or fails to start with "No command in OCI config
   or service config" — a wrong-command bug that only announces itself as a
   generic failure, not a parse error, because nothing here treats "found
   less than expected" as a signal to report anything.
   ```
   $ rustc host build of json_get_object() against
     {"process": {"cwd": "/", "env": ["MOTD=Welcome}"], "args": ["/bin/sh"]}, "root": {"path": "rootfs"}}
   Some("\"cwd\": \"/\", \"env\": [\"MOTD=Welcome")
   ```
3. **`meow`'s `serialize_tool_calls`/`Message::write_json` push a tool call's
   `id`/`name`/`tool_call_id` into the outgoing JSONL/request body
   unescaped**, while every other field in the same functions correctly goes
   through `json_escape_to`. These strings originate from
   `extract_json_string` parsing the *model provider's* SSE response, so they
   are not compile-time-controlled — a provider or proxy that ever returns a
   tool name/id containing `"` or `\` (protocol-violating on the provider's
   part, but nothing here validates that assumption) writes a JSONL line that
   is not valid JSON. The next `write_chat_body_inner` call streams that
   broken line verbatim into the request body sent back to the API, which
   then fails to parse the *entire* subsequent request — a small
   provider-side glitch on one field silently escalates into losing the whole
   conversation's ability to continue, with no error surfaced at write time
   (the corruption is latent until the next request round-trips and the API
   rejects it, or worse, misparses it and does something with a
   spliced-together field).
4. **Three independent copies of `extract_json_string` in `meow`
   (`api/mod.rs`, `api/client.rs`, `app/chat.rs`) disagree on `\uXXXX`
   handling** — `client.rs`'s decodes single-plane escapes correctly (but
   mishandles surrogate pairs, silently dropping astral characters);
   `api/mod.rs`'s and `app/chat.rs`'s don't decode `\u` at all, emitting the
   literal backslash-u-and-digits into the result string. Low likelihood
   (model IDs and tool names are typically plain ASCII in every provider this
   codebase targets) but a real, silent divergence between three copies of
   "the same" function, one of which (`app/chat.rs`) explicitly says in its
   own doc comment that it "mirrors" another — the mirroring wasn't kept
   complete.
5. **`meow::tools::helpers::extract_number_field` rejects negative numbers
   silently** — `is_ascii_digit()`-only scanning means `"start": -5` returns
   `None`, and every caller (`FileReadLines`'s `start`/`end`,
   `CodeSearch`'s `context`) does `.unwrap_or(default)`, so a negative
   argument from the model is indistinguishable from a missing one. Low
   likelihood (these are 1-based line numbers and context radii, rarely
   negative in practice) but a real gap between what the field name implies
   ("a number") and what's accepted.
6. **None of the depth-unaware substring scanners
   (`herd::json_get_str`/`json_get_str_array`'s *key-finding* step;
   `meow::ui::tui::stream::extract_field_value`; `meow::tools::helpers`'s
   `extract_string_field`) validate that the `"key"` substring they matched
   is actually a key at the intended nesting level.** No concrete failing
   input was found for the in-tree call sites today — `herd`'s config.json
   keys don't collide across `root`/`process`/`mounts`, `meow`'s tool-args
   objects are flat, and the notification field list in `ui/tui/stream.rs`
   doesn't currently overlap with any tool's argument names at a different
   depth — but this is a property of *today's* call sites, not a property the
   parsers enforce. The very first cost of adding a nested-object tool
   argument, or an OCI config field with a name that happens to also appear
   nested elsewhere, is a silent wrong answer, not a compile error or a
   parse failure.

## Background

- `userspace/box/src/json.rs` (working tree, in progress) — the reference
  direction for consolidation; its own doc comment names the "substring
  search wearing a parser's name" bug class this audit found independently
  in `herd`.
- `docs/reference/subsystems/console.md` § "Printing rules" — the
  `safe_print!`/no-allocation-on-console-paths rule from `CLAUDE.md`; noted
  in the consolidation table as inapplicable here since none of these sites
  are kernel/console paths.
- `docs/archive/TRIM_FAT_SSHD.md`, `docs/archive/TRIM_FAT_DEAD_CODE.md` —
  prior `TRIM_FAT_*` docs this one follows the format of.
