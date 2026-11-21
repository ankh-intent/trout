# trout

An OpenAI-compatible relay that cuts what a client on a metered link has to upload.

Live relay: [trout.fly.dev](https://trout.fly.dev/). Its home page shows this document and how to connect;
relaying through it needs its access key.

An agent session re-sends its whole conversation on every model turn. trout runs on a server with an
unmetered connection and lets the client talk to it cheaply:

- **gzip both ways**, including to providers that reject compressed request bodies themselves;
- **stored conversations**: `/v1/responses` served by the relay, so a follow-up turn carries only what is
  new, against any provider that has an ordinary `/chat/completions`.

The provider still receives, and bills for, the full conversation each turn. trout reduces bytes on the
client's connection, not tokens.

## Using it

Point the client at trout and add two headers:

| Header | Value |
|--------|-------|
| `X-Trout-Key` | The relay's access key. |
| `X-Trout-Target` | The provider's base URL including its version prefix, e.g. `https://api.example.com/v1`. |

`Authorization` is passed through to the provider untouched and is never stored or logged.

```bash
export TROUT=https://trout.example.com
export TROUT_KEY=...                           # the relay's access key
export TARGET=https://api.example.com/v1       # your provider
export API_KEY=...                             # your provider key
export MODEL=...
```

### Plain relay

Any `/v1/*` path is forwarded to `<target>/*`:

```bash
curl -s $TROUT/v1/chat/completions \
  -H "X-Trout-Key: $TROUT_KEY" -H "X-Trout-Target: $TARGET" \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}"
```

### Compressed

Send `Content-Encoding: gzip` and ask for a gzip answer:

```bash
printf '{"model":"%s","messages":[{"role":"user","content":"hi"}]}' "$MODEL" | gzip | \
curl -s --compressed $TROUT/v1/chat/completions \
  -H "X-Trout-Key: $TROUT_KEY" -H "X-Trout-Target: $TARGET" \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -H 'Content-Encoding: gzip' --data-binary @-
```

Streamed answers are flushed after every event, so compression does not delay them.

### Stored conversations

First turn:

```bash
R1=$(curl -s $TROUT/v1/responses \
  -H "X-Trout-Key: $TROUT_KEY" -H "X-Trout-Target: $TARGET" \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -d "{\"model\":\"$MODEL\",\"instructions\":\"Answer in one word.\",\"input\":\"My name is Ada. Say ok.\"}")
```

Follow-up, carrying only the new input:

```bash
curl -s $TROUT/v1/responses \
  -H "X-Trout-Key: $TROUT_KEY" -H "X-Trout-Target: $TARGET" \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -d "{\"previous_response_id\":$(echo "$R1" | jq .id),\"input\":\"What is my name?\"}"
```

Things to know:

- A follow-up **inherits** `model`, `instructions`, `tools`, `tool_choice`, sampling settings and
  `extra_body` from the answer it continues. Send a field to replace it from then on; send `null` to
  clear it. (OpenAI's own API does not inherit `instructions` or `tools`.)
- `extra_body` is merged into the top level of the provider request, for provider-specific switches.
- Only **completed** answers can be continued. A failed, cut-off or abandoned turn is not stored.
- Two follow-ups from the same answer form two separate branches.
- If a conversation is gone (deleted, expired, evicted), a follow-up returns `404 response_not_found`.
  Recover by sending the full conversation as a new first turn.
- `"store": false` serves the turn and keeps nothing.
- `"stream": true` returns Responses-style events (`response.created` ... `response.completed`).
- Supported: text, function tools, tool calls and results, reasoning text (returned and stored, never
  sent back to the provider). Not supported: images, audio, files, hosted tools, background runs. Request
  fields outside the supported set are refused with `400 unsupported_feature`.

Other routes:

| Route | Effect |
|-------|--------|
| `GET /v1/responses/{id}` | The stored answer. |
| `DELETE /v1/responses/{id}` | Removes the answer and everything continued from it. |
| `DELETE /v1/conversations/{id}` | Removes the whole conversation (`conversation.id` is on every answer). |
| `GET /healthz` | `{"status":"ok"}`, no key needed. |
| `GET /` | Home page with the sign-in form (see below). Needs no key. |
| `GET /dashboard` | Statistics page (see below). The page itself needs no key. |
| `GET /providers` | LLM provider comparison page (see below). Needs no key. |
| `GET /stats` | The figures behind the page, as JSON. Needs the key. |

### Dashboard

Open `https://<your-relay>/` in a browser and sign in with the access key; the home page checks the key
against `/stats` and opens `/dashboard`. Opening `/dashboard` without signing in sends you back to the
home page. Every page carries the same top navigation bar. The dashboard shows:

- **Upload saved** and **download saved**: what the provider received against what the client actually
  sent, and the answer size before compression against what the client actually received.
- **Conversations served**, turns, proxied requests and failures.
- **Stored now**: conversations and bytes in the store, against its size cap.
- A per-day upload chart and table for the last 30 days that saw traffic (UTC days).

The page is one self-contained file: plain HTML, CSS and JavaScript, nothing loaded from anywhere else.
It holds no data itself; it reads `/stats` with the key you signed in with, which is kept only in that
browser tab's session storage. Figures are counts and sizes only, never content.

Statistics are kept in the store file, so they survive restarts. Byte figures count only requests that
reached a provider; requests the relay rejected are counted as failures and nothing else.

The page polls `/stats` while its tab is visible: every 5 seconds at first, doubling the wait each time
the figures have not changed, up to 5 minutes. A change in the figures, the Refresh button, or the
window or tab regaining focus starts it again at 5 seconds. A hidden tab does not poll, so on Fly.io a
forgotten background tab does not keep the machine awake; a visible one does wake it on each check.

### LLM providers page

`/providers` is a comparison of LLM API providers: prices, independent benchmark scores, measured speed
and an interactive ranking weighted between price, intelligence and speed. It is public and static, like the
other pages, and loads nothing from anywhere else.

All of its figures and text come from [`src/pages/providers.json`](src/pages/providers.json), which is
compiled into the binary. To publish new data, edit that file, commit and deploy; the page itself does
not change. `cargo test --lib pages` checks the file parses and every provider has the fields the page
needs. Fields a source did not give (`price_out`, `cost_per_task`, `speed`, `rank`) are `null`.

To add a page to the navigation bar, add it to `NAV_ITEMS` in [`src/pages.rs`](src/pages.rs).

### Errors

Errors produced by the relay carry `X-Trout-Source: relay` and a stable code:

```json
{ "error": { "type": "trout_error", "code": "target_missing", "message": "X-Trout-Target header is required" } }
```

Errors from the provider are returned unchanged with `X-Trout-Source: upstream`.

| Code | Status |
|------|--------|
| `unauthorized` | 401 |
| `target_missing`, `target_invalid`, `body_corrupt`, `invalid_request`, `unsupported_feature` | 400 |
| `target_forbidden` | 403 |
| `route_not_found`, `response_not_found` | 404 |
| `response_in_progress`, `response_not_continuable`, `target_mismatch` | 409 |
| `body_too_large` | 413 |
| `encoding_unsupported` | 415 |
| `upstream_unreachable`, `upstream_encoding_unsupported` | 502 |
| `upstream_timeout` | 504 |
| `internal` | 500 |

## Configuration

Defaults, then the TOML file named by `TROUT_CONFIG` (default `/etc/trout/trout.toml`), then environment
variables. Every key is documented in [`deploy/trout.example.toml`](deploy/trout.example.toml).

| Variable | Overrides |
|----------|-----------|
| `TROUT_ACCESS_KEY` | `access_key` (required, at least 32 characters) |
| `TROUT_LISTEN` | `listen` |
| `TROUT_STORE_PATH` | `store.path` |
| `TROUT_STORE_TTL_SECS` | `store.ttl_secs` |
| `TROUT_LOG_LEVEL` | `log.level` |

Targets: by default any public `https` provider is allowed once the access key is valid. Hosts that
resolve to loopback, private or link-local addresses are refused unless listed with
`allow_private = true`. Set `targets.mode = "listed"` to allow only listed providers.

The log has one line per request (target host, status, duration, bytes in and out). Headers and bodies
are never logged, and there is no setting that changes that.

Stored conversations are kept unencrypted in one SQLite file. They can contain your code and prompts;
the file is readable only by the service user.

## Running locally

```bash
TROUT_CONFIG=/nonexistent TROUT_STORE_PATH=./trout.db TROUT_ACCESS_KEY=$(openssl rand -hex 32) cargo run --release
```

```bash
cargo test
```

## Deploying to a VPS

(For Fly.io, see the next section.)

Needs a Linux server with a public address and a DNS name pointing at it.

1. **Build** a static binary. Either on the server:

   ```bash
   cargo build --release
   ```

   or cross-compile from another machine (needs `cargo-zigbuild` and `zig`):

   ```bash
   cargo zigbuild --release --target x86_64-unknown-linux-musl
   ```

2. **Install** the binary, the service user and the configuration:

   ```bash
   sudo install -m 755 target/release/trout /usr/local/bin/trout
   sudo useradd --system --no-create-home --shell /usr/sbin/nologin trout
   sudo install -d -m 755 /etc/trout
   sudo install -m 644 deploy/trout.example.toml /etc/trout/trout.toml
   printf 'TROUT_ACCESS_KEY=%s\n' "$(openssl rand -hex 32)" | sudo install -m 600 /dev/stdin /etc/trout/trout.env
   sudo install -m 644 deploy/trout.service /etc/systemd/system/trout.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now trout
   ```

   The access key is in `/etc/trout/trout.env`; clients send it as `X-Trout-Key`.

3. **TLS front**: install Caddy, copy [`deploy/Caddyfile`](deploy/Caddyfile) to `/etc/caddy/Caddyfile`
   with your host name, and reload Caddy. It obtains the certificate itself. Open ports 80 and 443;
   8787 stays on loopback.

4. **Check** from the client machine:

   ```bash
   curl -s https://trout.example.com/healthz
   ```

   then the three examples above.

Logs: `journalctl -u trout`. The conversation store is `/var/lib/trout/trout.db`.

## Deploying to Fly.io

An alternative to a VPS: Fly builds the image, terminates TLS and gives the relay an address
(`https://<app>.fly.dev`), so there is no Caddy and no domain to set up. The files are
[`fly.toml`](fly.toml), [`Dockerfile`](Dockerfile) and [`deploy/fly/trout.toml`](deploy/fly/trout.toml),
sized for the smallest machine (shared CPU, 256 MB) with a 1 GB volume.

1. Install `flyctl` and sign in (`fly auth login`).
2. Choose an app name: edit `app = "..."` in `fly.toml`, and `primary_region` if Frankfurt is not the
   region you want.
3. Create the app, the volume and the access key, then deploy:

   ```bash
   fly apps create <app>
   fly volumes create trout_data --region fra --size 1
   fly secrets set TROUT_ACCESS_KEY=$(openssl rand -hex 32)
   fly deploy
   ```

   `fly deploy` uploads the source and builds on Fly's builders; nothing large is downloaded locally.
   Keep the key you generated: clients send it as `X-Trout-Key`. It cannot be read back from Fly.

4. Check it:

   ```bash
   curl -s https://<app>.fly.dev/healthz
   ```

Things to know:

- **Idle stop**: the machine stops when nobody is connected and starts on the next request, which adds
  about a second to that request. Stored conversations stay on the volume. For an always-on relay set
  `auto_stop_machines = "off"` and `min_machines_running = 1` in `fly.toml`.
- **Limits**: `deploy/fly/trout.toml` caps a request at 8 MiB on the wire and 16 MiB decoded, and the
  store at 512 MiB, to fit the small machine and volume. Raise them together with the machine size.
- **Outbound data is billed** by Fly. The relay-to-provider leg carries each full conversation
  uncompressed, so it grows with usage.
- **Long silent requests**: a client that does not stream gets no bytes until the provider finishes.
  Fly's proxy may close a connection that stays silent too long. Test a slow completion before relying
  on this; if it is cut off, use streaming on the client.
- **One machine, one volume**: there is no redundancy. If Fly's host has trouble the relay is down
  until it recovers; clients recover from lost conversations by re-sending the full history.
- Logs: `fly logs`. Shell: `fly ssh console`.

This repository is deployed this way. With Fly's GitHub integration connected to the repository, every
push to `master` is built and deployed without running `fly deploy` by hand.

## Measured

From the test suite, against a scripted provider on loopback (debug build):

| What | Result |
|------|--------|
| gzip of a synthetic 2 MB agent conversation | 186 KB (11.6x smaller) |
| Client upload over a 50-turn tool-call session, full re-submission | 5.9 MB |
| Same session through `/v1/responses` | 214 KB (96.4% less) |
| Added delay to the first streamed byte | about 1 ms (proxy), about 3 ms (`/v1/responses`) |

### In use against a real provider

From the dashboard of a relay on Fly.io carrying one client's agent traffic to MiniMax, 11 to 21 November
2026 (read on the 21st, so that day is partial). Every request went through the plain relay with gzip;
stored conversations were not used.

| What | Result |
|------|--------|
| Requests proxied | 40,048 (66 failed, 0.16%) |
| Upload the provider received | 19.1 GB |
| Upload the client actually sent | 4.9 GB |
| Upload saved | 14.2 GB (74.2%, 3.9x smaller) |
| Download before compression | 187 MB |
| Download the client actually received | 73.4 MB |
| Download saved | 114 MB (60.8%, 2.5x smaller) |

That is roughly 500 KB per request without the relay against roughly 130 KB with it. The saving held
close to three quarters on every day:

| Day (UTC) | Proxied | Failed | Uploaded | Without relay | Saved |
|-----------|---------|--------|----------|---------------|-------|
| 2026-11-21 | 3 | 3 | 726 KB | 3.2 MB | 2.5 MB |
| 2026-11-20 | 2,657 | 1 | 386 MB | 1.5 GB | 1.1 GB |
| 2026-11-19 | 5,016 | 6 | 455 MB | 1.6 GB | 1.2 GB |
| 2026-11-18 | 6,872 | 23 | 784 MB | 2.9 GB | 2.2 GB |
| 2026-11-17 | 4,839 | 4 | 514 MB | 1.9 GB | 1.4 GB |
| 2026-11-16 | 3,049 | 7 | 453 MB | 1.7 GB | 1.3 GB |
| 2026-11-15 | 4,846 | 7 | 616 MB | 2.3 GB | 1.7 GB |
| 2026-11-14 | 3,823 | 1 | 572 MB | 2.1 GB | 1.5 GB |
| 2026-11-13 | 4,985 | 6 | 601 MB | 2.4 GB | 1.8 GB |
| 2026-11-12 | 2,219 | 1 | 360 MB | 1.3 GB | 1001 MB |
| 2026-11-11 | 1,739 | 7 | 324 MB | 1.3 GB | 1.0 GB |

Real traffic compresses less than the synthetic conversation above (3.9x against 11.6x). Most of what is
uploaded is still the conversation re-sent each turn, which `/v1/responses` would avoid.

Still not measured against a real provider: `/v1/responses` (stored conversations), and the delay the
relay adds over a real network.

## Known gaps

- The `/v1/responses` shapes follow OpenAI's for the fields listed above but have not been checked
  field by field against the current OpenAI reference.
- Translation is tested against a scripted provider only. A real provider's chat dialect may differ, for
  example in how it reports reasoning or usage in a stream.
- Assistant messages that carry only tool calls are sent upstream with `content: null`.
- "Preload" (preparing the next request before the client asks) is not implemented.
