#!/usr/bin/env python3
"""Cost of a goose session, from its sessions.db: wall time, tokens, LLM calls, tool calls.

    goose_stats.py SESSIONS.DB NAME [NAME ...]        # a copy of /root/.local/share/goose/sessions/sessions.db (+ -wal)

The numbers the KVM dev-cycle experiment reports (docs/archive/AKUMA_KVM_DEV_CYCLE.md § 8). Tool calls are counted once per
tool-call id and usage records once per (in, out, total, elapsed) tuple (goose stores a streamed assistant message more than
once). "LLM time" is the provider's own `elapsedMs`; "tool and other" is wall time minus that (message timestamps cannot split it:
the gap between a request and its response includes the model's generation time).
"""
import collections
import datetime as dt
import json
import sqlite3
import sys


def stats(c, name):
    row = c.execute("select id, created_at, updated_at, model_config_json from sessions where name=? order by created_at desc limit 1", (name,)).fetchone()
    if not row:
        return None
    sid, created, updated = row[0], row[1], row[2]
    fmt = "%Y-%m-%d %H:%M:%S"
    wall = (dt.datetime.strptime(updated, fmt) - dt.datetime.strptime(created, fmt)).total_seconds()
    led = c.execute("select count(*), sum(input_tokens), sum(output_tokens), sum(total_tokens), sum(cache_read_tokens), min(model) from usage_ledger where session_id=?", (sid,)).fetchone()
    calls = collections.Counter()
    shell_cmds = []
    asked_at = {}
    waits = []
    llm_ms = 0
    ttft = []
    think_chars = 0
    text_chars = 0
    seen_usage = set()
    for mid, role, cts, cj, mj in c.execute("select message_id, role, created_timestamp, content_json, metadata_json from messages where session_id=? order by id", (sid,)):
        items = json.loads(cj)
        meta = json.loads(mj or "{}")
        if role == "assistant":
            u = meta.get("usage")
            key = (u.get("inputTokens"), u.get("outputTokens"), u.get("totalTokens"), u.get("elapsedMs")) if u else None
            if u and u.get("elapsedMs") and key not in seen_usage:
                seen_usage.add(key)
                llm_ms += u["elapsedMs"]
                if u.get("timeToFirstTokenMs"):
                    ttft.append(u["timeToFirstTokenMs"])
            for it in items:
                if it["type"] == "toolRequest":
                    tid = it["id"]
                    if tid in asked_at:
                        continue
                    asked_at[tid] = cts
                    tc = it["toolCall"].get("value", {})
                    calls[tc.get("name", "?")] += 1
                    if tc.get("name") == "shell":
                        shell_cmds.append(tc.get("arguments", {}).get("command", ""))
                elif it["type"] == "thinking":
                    think_chars += len(it.get("thinking", ""))
                elif it["type"] == "text":
                    text_chars += len(it.get("text", ""))
        else:
            for it in items:
                if it["type"] == "toolResponse" and it["id"] in asked_at:
                    waits.append((cts - asked_at.pop(it["id"]), it["id"]))
    waits.sort(reverse=True)
    n_calls = sum(calls.values())
    return {
        "name": name, "id": sid, "model": led[5], "start": created, "end": updated, "wall_s": wall,
        "llm_calls": led[0], "input_tokens": led[1] or 0, "output_tokens": led[2] or 0, "total_tokens": led[3] or 0, "cache_read_tokens": led[4] or 0,
        "llm_s": llm_ms / 1000, "ttft_avg_s": (sum(ttft) / len(ttft) / 1000) if ttft else 0,
        "tool_calls": n_calls, "tools": dict(calls), "tool_and_other_s": max(0.0, wall - llm_ms / 1000),
        "thinking_chars": think_chars, "text_chars": text_chars, "shell_commands": len(shell_cmds),
    }


def main():
    c = sqlite3.connect(sys.argv[1])
    for n in sys.argv[2:]:
        s = stats(c, n)
        if not s:
            print(n, "not found")
            continue
        print(json.dumps(s, indent=1))


if __name__ == "__main__":
    main()
