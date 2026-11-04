#!/usr/bin/env python3
"""Block until the trashcan Qwen answers a real chat completion, then print its URL and exit 0.

    python3 -I scripts/kvm/goose/wait_qwen.py [--url http://192.168.1.123:8081] [--deadline SECONDS]

Readiness is a completed generation, not an open port or a /v1/models answer: llama-server answers
/v1/models (and 503s the completion) while the model is still loading. Exit 0 ready, 1 deadline passed.
An agent that must not start before the endpoint exists runs this first, in the background, and polls its log.
"""
import argparse, json, sys, time, urllib.request

ap = argparse.ArgumentParser()
ap.add_argument("--url", default="http://192.168.1.123:8081")
ap.add_argument("--deadline", type=int, default=6 * 3600)
a = ap.parse_args()
end = time.time() + a.deadline
body = json.dumps({"messages": [{"role": "user", "content": "Say pong."}], "max_tokens": 8}).encode()
while time.time() < end:
    try:
        r = urllib.request.urlopen(urllib.request.Request(
            a.url + "/v1/chat/completions", body, {"content-type": "application/json"}), timeout=120)
        if json.load(r).get("choices"):
            print("QWEN_READY=" + a.url + "/v1", flush=True)
            sys.exit(0)
    except Exception as e:
        print(time.strftime("%H:%M:%S"), "not ready:", type(e).__name__, flush=True)
    time.sleep(30)
print("QWEN_DEADLINE", flush=True)
sys.exit(1)
