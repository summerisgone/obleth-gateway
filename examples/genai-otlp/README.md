# GenAI export to Langfuse

Exports each completion call (chat, completions, responses, messages) as a root
GenAI span over OTLP/HTTP, with scrubbed prompt and output, grouped by session.
Off until `OBLETH_GENAI_OTLP_ENDPOINT` is set. Nothing is written to disk: events
live in memory until they are sent, dropped or expired.

Files:

- `docker-compose.yml`: obleth service wired to Langfuse.
- `../genai-scrubber/`: optional Presidio adapter for `OBLETH_PII_NER_URL`.

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

All 16 variables. Defaults apply when a variable is unset or empty. An invalid value (unknown scrub mode, bad number, `ner` or `llm` without a URL) turns the export off and logs `GenAI OTLP export is off: invalid configuration`; the gateway still starts. `GET /api/v1/genai-export` returns the reason in `error`, and the tenant's "Export to observability" setting shows it.

| Variable | Default | Meaning |
|---|---|---|
| `OBLETH_GENAI_OTLP_ENDPOINT` | unset (feature off) | Base URL. The exporter POSTs to `{endpoint}/v1/traces`. For Langfuse: `https://<langfuse>/api/public/otel`. |
| `OBLETH_GENAI_OTLP_HEADERS` | none | Comma-separated `key=value` pairs sent with every POST. |
| `OBLETH_GENAI_CONTENT_TTL_HOURS` | `24` | Maximum age of an event inside obleth (queue, scrubbing, retries). Older events are dropped (`reason="ttl"`). |
| `OBLETH_GENAI_QUEUE_MAX_BYTES` | `67108864` (64 MiB) | Limit for the queue, in-flight captures and batches in retry together. Above it, events are sent without content (`queue_full`). |
| `OBLETH_GENAI_FLUSH_MS` | `2000` | Batch flush interval. Batches also flush at 256 spans or 4 MiB. |
| `OBLETH_GENAI_MAX_REQUEST_BYTES` | `4194304` (4 MiB) | Larger request bodies are not captured. The span carries `obleth.content.dropped="request_too_large"`. |
| `OBLETH_GENAI_MAX_CONTENT_BYTES` | `262144` (256 KiB) | Cap per content attribute. Text parts are cut first, then the oldest input messages are dropped. The `*_truncated` flag is set. |
| `OBLETH_GENAI_SCRUB` | `regex` | Default trace profile: `off`, `regex`, `ner` or `llm` (`llm` is deprecated). Used only when `BoonSettings.pii.traces` is unset. |
| `OBLETH_PII_NER_URL` | unset | NER service, called with `POST {url}`. Required for `ner` and `llm`. Without it the export stays off; obleth still starts. Read at start: a change needs a restart. |
| `OBLETH_PII_NER_CONCURRENCY` | `8` | Trace scrub calls in flight at once. When none is free, content is dropped (`scrub_failed`). Minimum 1. Read at start. |
| `OBLETH_GENAI_SCRUB_URL` | unset | Alias of `OBLETH_PII_NER_URL`. When both are set the new name wins and obleth logs a warning. |
| `OBLETH_GENAI_SCRUB_CONCURRENCY` | `8` | Alias of `OBLETH_PII_NER_CONCURRENCY`; same precedence. |
| `OBLETH_GENAI_SCRUB_MODEL` | unset | Deprecated. Model name for `llm`, sent to `POST {SCRUB_URL}/v1/chat/completions`. |
| `OBLETH_GENAI_SCRUB_TIMEOUT_MS` | `5000` | Timeout per scrub call. Used only when `pii.ner.timeout_ms` is unset. A failed or slow scrub drops the content (`scrub_failed`). |
| `OBLETH_GENAI_USER_ID_HMAC_KEY` | unset | Secret. HMAC key for `user.id` and for conversation ids that contain PII. Unset: no `user.id` attribute, and such conversation ids are not exported. |
| `OBLETH_GENAI_CONVERSATION_SOURCES` | `client` | Comma-separated. `client` uses the id the client sends. `derived` also groups by a hash of the conversation start; chats from different users with the same opening messages can then merge into one session. |

Scrub detectors:

- `regex` catches only US SSNs, emails, phone numbers and card numbers. It does not
  find names, addresses, IBANs or national IDs, so it is not GDPR scrubbing. Use
  `ner` for that.
- `ner` runs `regex` first, then sends the text to the NER service.
- `llm` is deprecated (see below).

## PII profiles

Policy lives in `BoonSettings.pii`, set with `PUT /api/v1/settings/boons`. It
applies without a restart. Precedence: `BoonSettings.pii`, then the env defaults
above, then built-in values. The service URL and concurrency stay in env.

```json
{"pii": {
  "ner": {"timeout_ms": 5000, "model_timeout_ms": 1000},
  "profiles": {"names": {"detector": "ner", "entities": ["PERSON", "LOCATION"], "score_threshold": 0.5}},
  "traces": "names"
}}
```

- Profile fields: `detector` (`off`, `regex`, `ner`), `entities` (empty means all the
  service knows), `language` and `score_threshold` (unset means the service default).
- Built-in profiles `off`, `regex` and `ner` always exist; your own profiles cannot
  reuse those names. Add your own only for an entity list or a language.
- `pii.traces` is the default trace profile. A `ner` profile is refused while
  `OBLETH_PII_NER_URL` is unset, as are a reference to a missing profile and
  deleting a profile in use.
- Per tenant, set the trace profile with `PUT /api/v1/tenants/{id}/genai-export`:

  ```sh
  curl -X PUT "http://127.0.0.1:9180/api/v1/tenants/<tenant-id>/genai-export" \
    -H "authorization: Bearer $OBLETH_ADMIN_TOKEN" \
    -H "content-type: application/json" \
    -d '{"genai_export":true,"pii_trace_profile":"names"}'
  ```

  `pii_trace_profile` absent keeps the stored value, `null` inherits `pii.traces`.
  Order: tenant, `pii.traces`, `OBLETH_GENAI_SCRUB`, `regex`.
- Guardrails use a profile through `pii_profile` in the tenant policy; without it the
  scanner is `regex` as before.
- `POST /api/v1/settings/boons/pii/check` sends a test text to the NER service and
  returns `{ok, latency_ms, error}`.
- Each span carries `obleth.content.scrubbed` (`none`, `regex`, `ner` or `llm`) and
  `obleth.content.scrub_profile` (the profile name, `<trace>+<guard>` when guardrails
  strengthened it).

Union invariant: traces are never weaker than guardrails. When a tenant's policy has
the `pii` scanner and `action` is `redact` or `block` (not `log_only`), the trace
profile is the union of the trace profile and the guardrails profile: the stronger
detector, the union of entities (an empty list means all and absorbs the rest), and
language and threshold from the stronger detector (the trace profile on a tie).

Failures:

| Where | Timeout | Concurrency | Detector fails (error, timeout, missing profile) |
|---|---|---|---|
| Traces | `pii.ner.timeout_ms` (5000) | `OBLETH_PII_NER_CONCURRENCY`, no waiting | content dropped, `obleth.content.dropped="scrub_failed"`; the client request is unaffected |
| Model, `block` / `redact` | `pii.ner.model_timeout_ms` (1000) | not limited by obleth | `fail_open: true`: request passes unchanged, metric `pii_detector_failures_total`; `false`: 503 |
| Model, `log_only` | `pii.ner.timeout_ms` | background task | metric only |

`fail_open` defaults to `true`, so with an unreachable NER service the model sees the
PII. Set `fail_open: false` for strict tenants. Trace scrubbing runs up to
`OBLETH_PII_NER_CONCURRENCY` events in parallel.

## Deprecated: `llm`

`OBLETH_GENAI_SCRUB=llm` works in this release, logs a warning at start, and
`GET /api/v1/genai-export` returns `deprecated: "llm"`. It is removed in the next
release: the export then turns off with a reason in `error`. Move to `ner`. An LLM
service can sit behind the NER contract below.

`GET /api/v1/genai-export` also returns `profiles` (names) and `ner_configured`.

## NER service contract

```json
POST {url}
{"texts": ["..."], "entities": ["PERSON"], "language": "en", "score_threshold": 0.5}
-> {"texts": ["..."]}
```

`entities`, `language` and `score_threshold` are sent only when the profile sets
them, so older adapters keep working. The reply must have the same length. Models,
recognizers and tags are configured in the service, not in obleth.

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

Scrub calls made in `llm` mode (deprecated) are not billed to the tenant in v1. Billing them as
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

To use the `ner` detector, build and run the adapter in `../genai-scrubber/`,
on a network the obleth container can reach, then set:

```sh
OBLETH_PII_NER_URL=http://genai-scrubber:8000/
```

Then set `OBLETH_GENAI_SCRUB=ner`, or `{"pii": {"traces": "ner"}}` through the boons
settings. The adapter uses Presidio's default setup (English); for other
languages see `../genai-scrubber/README.md`.
