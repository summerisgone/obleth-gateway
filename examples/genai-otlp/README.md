# GenAI export to Langfuse

Exports each completion call (chat, completions, responses, messages) as a root
GenAI span over OTLP/HTTP, with scrubbed prompt and output, grouped by session.
Off until `OBLETH_GENAI_OTLP_ENDPOINT` is set. Nothing is written to disk: events
live in memory until they are sent, dropped or expired.

Files:

- `docker-compose.yml`: obleth service wired to Langfuse.
- `../genai-scrubber/`: optional Presidio adapter for `OBLETH_GENAI_SCRUB=ner`.

## Start

```sh
export LANGFUSE_BASIC_AUTH=$(printf '%s' 'pk-lf-...:sk-lf-...' | base64 | tr -d '\n')
export LANGFUSE_OTLP_ENDPOINT=https://<langfuse-host>/api/public/otel
docker compose -f examples/genai-otlp/docker-compose.yml up -d
```

The compose file sets the obleth datastore and upstream variables to placeholders
(`change-me`, `postgres`, `redis`, `clickhouse`, `upstream`). Replace them with
the stack you run, or export real values before `up`.

## Langfuse keys and headers

1. In Langfuse, create an API key pair for the project.
2. Base64-encode `public_key:secret_key` (the command above does this).
3. Set the exporter headers. The version header is always required:

   ```
   OBLETH_GENAI_OTLP_HEADERS=Authorization=Basic <base64>,x-langfuse-ingestion-version=4
   ```

Each header pair splits at its first `=`. Header values are never logged.

## Environment variables

All 14 variables. Defaults apply when a variable is unset or empty.

| Variable | Default | Meaning |
|---|---|---|
| `OBLETH_GENAI_OTLP_ENDPOINT` | unset (feature off) | Base URL. The exporter POSTs to `{endpoint}/v1/traces`. For Langfuse: `https://<langfuse>/api/public/otel`. |
| `OBLETH_GENAI_OTLP_HEADERS` | none | Comma-separated `key=value` pairs sent with every POST. |
| `OBLETH_GENAI_CONTENT_TTL_HOURS` | `24` | Maximum age of an event inside obleth (queue, scrubbing, retries). Older events are dropped (`reason="ttl"`). |
| `OBLETH_GENAI_QUEUE_MAX_BYTES` | `67108864` (64 MiB) | Limit for the queue, in-flight captures and batches in retry together. Above it, events are sent without content (`queue_full`). |
| `OBLETH_GENAI_FLUSH_MS` | `2000` | Batch flush interval. Batches also flush at 256 spans or 4 MiB. |
| `OBLETH_GENAI_MAX_REQUEST_BYTES` | `4194304` (4 MiB) | Larger request bodies are not captured. The span carries `obleth.content.dropped="request_too_large"`. |
| `OBLETH_GENAI_MAX_CONTENT_BYTES` | `262144` (256 KiB) | Cap per content attribute. Text parts are cut first, then the oldest input messages are dropped. The `*_truncated` flag is set. |
| `OBLETH_GENAI_SCRUB` | `regex` | `off`, `regex`, `ner` or `llm` (`llm` is experimental). |
| `OBLETH_GENAI_SCRUB_URL` | unset | Required for `ner` and `llm`. The scrub service is called with `POST {url}`. |
| `OBLETH_GENAI_SCRUB_MODEL` | unset | Model name for `llm`, sent to `POST {SCRUB_URL}/v1/chat/completions`. |
| `OBLETH_GENAI_SCRUB_TIMEOUT_MS` | `5000` | Timeout per scrub call. A failed or slow scrub drops the content (`scrub_failed`). |
| `OBLETH_GENAI_SCRUB_CONCURRENCY` | `8` | Scrub calls in flight at once. When none is free, content is dropped (`scrub_failed`). Minimum 1. |
| `OBLETH_GENAI_USER_ID_HMAC_KEY` | unset | Secret. HMAC key for `user.id` and for conversation ids that contain PII. Unset: no `user.id` attribute, and such conversation ids are not exported. |
| `OBLETH_GENAI_CONVERSATION_SOURCES` | `client` | Comma-separated. `client` uses the id the client sends. `derived` also groups by a hash of the conversation start; chats from different users with the same opening messages can then merge into one session. |

Scrub modes:

- `regex` catches only US SSNs, emails, phone numbers and card numbers. It does not
  find names, addresses, IBANs or national IDs, so it is not GDPR scrubbing. Use
  `ner` or `llm` for that.
- If a tenant's guardrail policy redacts or blocks PII, export runs at least
  `regex` even with `off`.

## Enable per tenant and per key

Export is off for every tenant and key by default. The admin API listens on
`OBLETH_ADMIN_LISTEN` (`9180` in the compose file) and takes the admin token as a
bearer token.

Tenant:

```sh
curl -X PUT "http://127.0.0.1:9180/api/v1/tenants/<tenant-id>/genai-export" \
  -H "authorization: Bearer $OBLETH_ADMIN_TOKEN" \
  -H "content-type: application/json" \
  -d '{"genai_export":true}'
```

Key:

```sh
curl -X PUT "http://127.0.0.1:9180/api/v1/keys/<key-id>/genai-export" \
  -H "authorization: Bearer $OBLETH_ADMIN_TOKEN" \
  -H "content-type: application/json" \
  -d '{"genai_export":false}'
```

Key values:

- `false`: opt this key out.
- `null`: inherit the tenant setting (the default).
- `true`: cannot enable a key of a tenant that has export off.

A key exports when its tenant is on and the key is not `false`.

## Sessions and users

- Session: send `x-obleth-session-id` or `x-session-id`. It becomes
  `gen_ai.conversation.id`, which Langfuse shows as the session.
- User: the body's `user` (OpenAI) or `metadata.user_id` (Anthropic). It is exported as
  `user.id` (the Langfuse userId) only when `OBLETH_GENAI_USER_ID_HMAC_KEY` is set, and
  then as an HMAC, never in plain text.
- Langfuse v4 serves no legacy `GET /api/public/traces`. Check exported spans in the UI.

## Billing gap

Scrub calls made in `llm` mode are not billed to the tenant in v1. Billing them as
`payload_scrub` is deferred, so their cost is not recovered from the tenant.

## Host hardening

Prompts and outputs pass through process memory. On Linux hosts that run obleth
with export enabled:

- Core dumps: run obleth with `ulimit -c 0`, set `LimitCORE=0` in the systemd unit,
  or set `ulimits: {core: 0}` on the Docker service (or `--ulimit core=0`).
- Swap: run `sudo swapoff -a` and remove swap entries from `/etc/fstab`.

## Metrics

- `obleth_genai_exported_total`
- `obleth_genai_dropped_total{reason}`
- `obleth_genai_queue_bytes`

## Optional: Presidio scrubber

To use `OBLETH_GENAI_SCRUB=ner`, build and run the adapter in `../genai-scrubber/`,
on a network the obleth container can reach, then set:

```sh
OBLETH_GENAI_SCRUB=ner
OBLETH_GENAI_SCRUB_URL=http://genai-scrubber:8000/
```

The adapter is English-only.
