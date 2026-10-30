# goose-glm as an ACP server on Akuma/ryzen (2026-10-24)

**Goal:** a long-running GLM agent on the ryzen box that other clients (a Kimi
client, for collaborating experiments) can drive over the network.

## What runs

- `goose serve` (goose 1.52.0, ACP over HTTP + WebSocket) as the herd service
  `goose-glm` (`overlays/ryzen/rootfs/etc/herd/available/goose-glm.conf`, symlinked
  into `enabled/`), started through `/bin/sh /usr/local/bin/goose-glm-serve`
  (`overlays/ryzen/goose-glm-serve`).
- Listens on `0.0.0.0:3284` (wifi address, `192.168.1.159` on 2026-10-24).
- Provider: z.ai coding plan, `glm-5.3` (the wrapper default; `goose-glm` the
  one-shot wrapper stays `glm-5.3-flash`), `GOOSE_THINKING_EFFORT=low`.
- `HOME=/root` and `cd /root`, so git, logs (`/root/.local/state/goose/logs`),
  sessions and config are where an ssh session sees them.
- `GOOSE_MODE=auto` (every tool call approved) and `--dangerously-unauthenticated`
  (no secret key). **Anyone on the LAN can run commands as root.** The user chose
  this on 2026-10-24; first the service ran with a key
  (`GOOSE_SERVER__SECRET_KEY`), which is one flag to bring back.

## Talking to it

`/health` and `/status` answer 200. ACP is on `/acp`:

1. `POST /acp` `initialize` -> response carries header `acp-connection-id`.
2. `GET /acp` with `accept: text/event-stream` and that id: replies arrive here.
3. `POST` `session/new` (`cwd` `/root`) -> **202**, result arrives on the stream.
4. Open a second `GET` with `acp-session-id: <sessionId>`; `session/prompt`
   updates and its final `stopReason` arrive on **that** one.

`scripts/goose_acp_probe.py` does all four (`GOOSE_ACP=http://host:3284/acp`).
Verified: prompt "Reply with the single word: pong" -> `pong`, `end_turn`.

## Traps found

- **Replies are not in the POST response.** The first probe waited on the
  connection stream only and timed out, while the llm log showed GLM had answered;
  session updates ride the session-scoped stream.
- **herd cannot exec a script by shebang here**: `command = /bin/sh`,
  `args = <script>`. With the script as `command` there was no log at all.
- **Address in use.** A hand-started instance holds :3284 and herd's restart loops
  on `Address in use (os error 98)` until it is killed (by pid, never by name).
- **Shared `secrets.yaml`.** The wrapper writes the z.ai token into
  `/root/.config/goose/secrets.yaml`, which `goose-kimi` also rewrites per run.
  Do not run `goose-kimi` while the server is up if it must keep the z.ai key.
- **wifi dormancy**: ~5 s of ping before `wifi-ssh.py`, or ssh dies mid-kex.

## Verify

```sh
herd status | grep goose-glm            # running
curl -s -o /dev/null -w '%{http_code}\n' http://<ip>:3284/health   # 200
python3 -I scripts/goose_acp_probe.py   # agent said: pong
```

Background: [`overlays/ryzen/README.md`](../../overlays/ryzen/README.md), [`GOOSE_TOKEN_AUDIT.md`](GOOSE_TOKEN_AUDIT.md).
