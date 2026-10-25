# Phase 2: Request log, metrics and telemetry

**Goal:** every exchange, including failures and cancellations, becomes exactly one record with correct token counts
and timings, written without ever slowing the request. Live rollups feed Prometheus now and the dashboard in phase 4;
OpenTelemetry spans join the app's trace when configured.

**Size:** M · **Depends on:** 1

## Tasks

### 2.1 Protocol models
- [ ] `protocol::ollama` and `protocol::openai`: lenient serde structs that read only what Jetway needs and keep the
      rest in a flattened map: model, messages (role, content, number of images, tool calls), prompt and system for
      `/api/generate`, options (`temperature`, `top_p`, `seed`, `num_ctx`, `num_predict`), `stream`, `think`,
      `format`, `tools`, `keep_alive`; for OpenAI also `max_tokens`, `response_format`, `n`, `stream_options`.
- [ ] Note the defaults that matter: Ollama's `stream` is true when omitted; OpenAI's is false.
- [ ] Incremental decoders: NDJSON (lines may be split across chunks, several lines may share a chunk, `\r\n`
      tolerated) and SSE (`data:` lines, events split anywhere, `[DONE]`).
- [ ] A response accumulator per protocol: content, thinking, tool calls, `done_reason`, token counts and Ollama's
      durations, or OpenAI's `usage` when present. When OpenAI's stream has no usage, token counts are marked
      unknown, never guessed.

### 2.2 Exchange record

| Field group | Fields |
|---|---|
| Identity | id (ULID), app, protocol, endpoint, trace id, parent span id |
| Request | requested model, model sent upstream, stream, think, temperature, seed, message count, has images, has tools, has format, request bytes |
| Outcome | `ok`, `upstream_error`, `client_cancelled`, `timeout`, `blocked`, `rejected`, `cache_hit`, `joined`; HTTP status; error message |
| Timings (µs from receipt) | upstream request sent, first byte, first content token, done; queue wait (phase 9) |
| Tokens | prompt, completion, thinking where known; Ollama's `load_duration`, `prompt_eval_duration`, `eval_duration`, `total_duration` |
| Jetway | cache result, guard verdicts, route (filled by later phases), response bytes, truncated or incomplete record flags |

- [ ] Time to first token is measured by Jetway as the client sees it, separately from Ollama's own durations.

### 2.3 Store (D8)
- [ ] `rusqlite` with the bundled SQLite, WAL, `synchronous = NORMAL`, migrations with `rusqlite_migration`
      (`jetway db migrate`; `serve` runs them at startup).
- [ ] Tables: `exchanges`, `bodies` (zstd level 3, request and response, per the `store.bodies` policy),
      `rollups_minute`, `meta`.
- [ ] The writer thread owns the only write connection. Records arrive on a bounded channel (`try_send`; when full,
      the record is dropped and `jetway_log_dropped_total` goes up) and are written in batches of up to 200 rows or
      100 ms per transaction.
- [ ] Reads use a small pool of read-only connections through `spawn_blocking`.
- [ ] Retention: `jetway db prune --older-than 7d`, and an hourly prune per `retention_days`. Cache tables are
      separate and have their own expiry (phase 3).
- [ ] A hook for phase 7: bodies pass through a redaction function before they are stored.

### 2.4 Metrics
- [ ] In-memory rollups per app × model × minute: counts by outcome, tokens, and `hdrhistogram`s for total latency,
      time to first token, Jetway's added time and output tokens per second. The last 24 hours are kept in memory;
      each closed minute is also written to `rollups_minute`.
- [ ] Prometheus `/metrics` on the admin port (`metrics-exporter-prometheus`): request counters, latency and TTFT
      histograms, tokens, in-flight gauge, recorder and log drops, upstream status.
- [ ] Jetway's own resident memory and CPU sampled every 5 s, as a gauge.

### 2.5 Telemetry (D16)
- [ ] When `otel.endpoint` is set: a server span per exchange (`POST /api/chat`) continuing the incoming
      `traceparent`, and a client span for the upstream call named `chat qwen3.5:4b`, with GenAI attributes
      (`gen_ai.operation.name`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens`,
      `gen_ai.usage.output_tokens`, `gen_ai.request.temperature`, ...). Message content is not recorded.
- [ ] The upstream request carries the client span's `traceparent`. Export over OTLP/HTTP protobuf with a batch
      processor; a failed export is logged once per minute, never per span.
- [ ] Off by default; when off, `traceparent` is still forwarded unchanged.

### 2.6 CLI
- [ ] `jetway requests tail [--app A] [--outcome O]` (polls the store), `jetway requests show <id> [--bodies]`.

## Tests
- [ ] Decoders: property tests that any split of a recorded stream into chunks decodes to the same lines and
      events; several lines in one chunk; `\r\n`; a stream that ends without its final line.
- [ ] Token counts and durations of every golden file match Ollama's final chunk exactly.
- [ ] One record per exchange for every outcome: success, upstream error, refused connection, timeout, client
      cancel, mid-stream failure, truncated recording (table-driven against the fake Ollama).
- [ ] Writer: batching, drop counting when the channel is full, migrations from an empty file, prune.
- [ ] Rollups and percentiles against hand-computed values.
- [ ] Telemetry: spans exported to an in-memory exporter, with the right parent and attributes.

## Done when
- [ ] Every request through Jetway shows up in `jetway requests tail` with correct tokens and timings.
- [ ] `curl http://127.0.0.1:8301/metrics` returns the metrics.
- [ ] With `otel.endpoint` set to the Aspire dashboard or BlackBox, Jetway's spans appear in the client's trace.
- [ ] Overhead targets from phase 1 still hold with recording on.
