# Phase 2: Request log, metrics and telemetry

**Goal:** every exchange, including failures and cancellations, becomes exactly one record with correct token counts
and timings, written without ever slowing the request. Live rollups feed Prometheus now and the dashboard in phase 4;
OpenTelemetry spans join the app's trace when configured.

**Size:** M · **Depends on:** 1 · **Status:** done (2026-10-25)

## Tasks

### 2.1 Protocol models
- [x] `protocol::summary`: `RequestSummary` reads model, `stream` (after each API's default: Ollama streams when it
      is omitted, OpenAI doesn't), `think` or `reasoning_effort`, temperature, `top_p`, seed, `num_predict` or
      `max_tokens`/`max_completion_tokens`, `num_ctx`, message count, images, tools, `format`/`response_format` and
      `stream_options.include_usage`. Lenient `serde_json::Value` lookups; unknown fields are simply not read.
- [x] `protocol::decode`: incremental NDJSON and SSE decoders (lines split anywhere, several per chunk, `\r\n`,
      SSE comments and multi-line `data:`).
- [x] `ResponseSummary` per protocol: content, thinking (`message.thinking`, `delta.reasoning`), tool calls, done
      and `done_reason`, token counts, Ollama's durations, error objects, response model.
- [x] **Decision D21** (added to the plan): Jetway asks for usage in OpenAI streams whose client didn't
      (`stream_options.include_usage`) and removes the usage-only event (`choices: []`) from what the client receives;
      the recorder still reads it. Token counts are exact for every stream. `proxy::usage` holds the filter: whole
      events pass untouched and uncopied; an event split across chunks waits for its blank line.
- [x] `prompt_eval_cached_count` is kept as `cached_prompt_tokens`.

### 2.2 Exchange record

| Field group | Fields |
|---|---|
| Identity | id (monotonic ULID), app, protocol, method, path, class, trace id, parent span id |
| Request | model, model sent (= requested until phase 8), stream, think, temperature, seed, max tokens, message count, images, tools, format, request bytes |
| Outcome | `ok`, `upstream_error`, `client_cancelled`, `timeout`, `rejected` (phases 3+ add cache and guard outcomes); HTTP status; error (Jetway's, or the upstream's own error message) |
| Timings (µs from receipt) | upstream request sent, headers, first chunk, first answer token, first thinking token, done; queue wait (phase 9) |
| Tokens | prompt, completion (Ollama's `eval_count`, thinking included), cached prompt; Ollama's `load`, `prompt_eval`, `eval`, `total` durations |
| Jetway | truncated and incomplete recording flags; JSON columns for cache, guards and route, filled by later phases |

- [x] Time to first token is measured by the recorder as the client sees it: the arrival time of the chunk that
      completed the first line with answer content (or a tool call).

### 2.3 Store (D8)
- [x] `rusqlite` with the bundled SQLite, WAL, `synchronous = NORMAL`, `foreign_keys = ON`. Migrations are an
      ordered list of SQL scripts tracked by `PRAGMA user_version` (`rusqlite_migration` pins an older `rusqlite`,
      whose bundled SQLite clashes); `jetway db migrate`, and `serve` runs them at startup.
- [x] Tables `exchanges`, `bodies` (zstd level 3, by `store.bodies`: `none`, `errors`, `all`), `rollups_minute`,
      `meta`.
- [x] One writer thread owns the write connection; work arrives on a bounded channel (`try_send`; full means the
      record is dropped and `jetway_log_dropped_total` counts it) and is written in batches of up to 200 per
      transaction. Bodies are compressed on the writer thread.
- [x] Reads use a pool of read-only connections through `spawn_blocking`.
- [x] Retention: `jetway db prune --older-than 7d`, and an hourly task per `retention_days` (exchanges, bodies by
      cascade) and `rollup_retention_days` (rollups).
- [x] A body-policy hook in the store sink (`RequestLog`); phase 7 adds redaction there.

### 2.4 Metrics
- [x] In-memory rollups per minute × app × model: counts by outcome, tokens, and HDR histograms (2 significant
      figures) for total latency, time to first token, Jetway's own time before the upstream call, and output tokens
      per second (Ollama's `eval_duration` when present). A day is kept in memory; closed minutes are written to
      `rollups_minute` every 15 s (a late exchange makes its minute due again).
- [x] Prometheus `/metrics` on the admin port, rendered by Jetway itself instead of the `metrics` crate, whose single
      global recorder doesn't fit tests that run several gateways in one process: request and token counters,
      histograms for duration, time to first token and added time, in-flight, memory, uptime, log drops and write
      errors, incomplete and truncated recordings. Also `/healthz`.
- [x] Jetway's resident memory comes from `memory-stats` when `/metrics` is read.

### 2.5 Telemetry (D16)
- [x] With `otel.endpoint` set: span ids are chosen when the request arrives (continuing a valid incoming
      `traceparent`), the upstream gets `traceparent` with Jetway's client span, and when the exchange ends a server
      span (`POST /api/chat`) and a client span (`chat qwen3.5:4b`) are built from the recorded timings, with GenAI
      attributes (`gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.response.model`,
      `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.request.temperature`, ...,
      `gen_ai.response.finish_reasons`). No message content.
- [x] Export: OTLP/HTTP protobuf, built with `opentelemetry-proto` and `prost` and posted by a tokio task with
      Jetway's own reqwest client, in batches of up to 512 or every 2 s; failures are logged once a minute. The
      OpenTelemetry SDK isn't used: its batch processor exports from its own thread with a blocking client, and
      spans built after the fact don't need it.
- [x] Off by default; when off, `traceparent` is forwarded unchanged.

### 2.6 CLI
- [x] `jetway requests tail [--app A] [--outcome O] [--limit N] [--follow]` and `jetway requests show <id>
      [--bodies]`.

## Tests
- [x] Decoders: property tests over random splits of recorded streams; several lines per chunk; `\r\n`; a stream
      that ends without its last terminator.
- [x] Response summaries of every golden file, streamed and whole, both APIs, thinking, tool calls and errors.
- [x] One row per exchange for `ok`, upstream error status, mid-stream failure, rejected and client-cancelled, with
      Ollama's token counts (`tests/request_log.rs`).
- [x] Bodies stored compressed and read back byte for byte; the `errors` policy.
- [x] Writer: a database locked by another connection makes records drop instead of blocking, and kept + dropped =
      sent; migrations are idempotent; prune cascades to bodies.
- [x] Rollups and percentiles against hand-computed values; closed minutes taken once, or again after a late
      exchange; Prometheus text and label escaping.
- [x] Spans: an in-process OTLP collector receives a server and a client span in the caller's trace, with the
      right parents and attributes, and the upstream saw the client span as its parent.

## Done when
- [x] Every request through Jetway shows up in `jetway requests tail` with correct tokens and timings: checked live
      on 2026-10-25 with an Ollama `/api/chat` stream (18/30 tokens) and an OpenAI stream that didn't ask for usage
      (15/10 tokens recorded, no usage event sent to the client).
- [x] `curl http://127.0.0.1:8301/metrics` returns the metrics (live, same day).
- [~] Spans in the Aspire dashboard or BlackBox: checked against an in-process OTLP collector; the live check moves
      to phase 5, where PaperPilot (with its Aspire dashboard) and BlackBox run.
- [x] Overhead targets from phase 1 still hold with recording on (the bench now runs the whole stack with a
      temporary database): p50 **0.099 ms** and p99 **0.220 ms** added per request; per chunk p99 0.426 ms through
      Jetway against 0.243 ms direct; 2 203 exchanges recorded, none dropped.

## What was built (2026-10-25)

- Modules `protocol::{decode, summary}`, `proxy::usage`, `store` (`mod`, `schema`, `row`, `sink`), `metrics`,
  `telemetry`, `server::admin` and `app` (start and stop everything; `jetway serve` and the tests use it), plus the
  `requests` and `db` commands.
- The recorder now takes timed pieces, decodes them, times the first answer and thinking tokens, and reads hidden
  usage events; `Sinks` fans a finished exchange out to metrics, the store, spans and the log.
- Exchange ids come from a monotonic ULID generator: plain ULIDs from the same millisecond sort randomly, which
  broke "list after id" (found by a test).
- Crates: `rusqlite` (bundled), `zstd`, `hdrhistogram`, `memory-stats`, `opentelemetry-proto`, `prost`, `rand`;
  dev: `proptest`, `tempfile`. Not used, against plan §4: `rusqlite_migration`, `metrics`,
  `metrics-exporter-prometheus`, the OpenTelemetry SDK and `tracing-opentelemetry` (reasons above).

### Rust notes
- **A thread for blocking work.** SQLite calls block, so the writer runs on its own `std::thread` and receives from
  a tokio `mpsc` channel with `blocking_recv()` ([store/mod.rs](../../src/store/mod.rs)); async code only ever calls
  `try_send`, which never waits. Reads go through `tokio::task::spawn_blocking`, which runs a closure on the
  runtime's blocking pool and returns a future of its result.
- **Closures that are `Send + 'static`.** `Store::read` takes `F: FnOnce(&Connection) -> rusqlite::Result<T> + Send
  + 'static` because the closure moves to another thread and may outlive the caller's stack; that is why callers
  write `move |c| ...` and clone what they capture.
- **`Drop` for shutdown.** Dropping the `Store` replaces its sender, which closes the channel, then joins the writer
  thread, so everything queued is written before the program exits.
- **Interior mutability.** `Metrics` is shared as `Arc<Metrics>` but updated through `&self`, using a
  `Mutex<State>` inside ([metrics/mod.rs](../../src/metrics/mod.rs)); `lock()` recovers from a poisoned mutex
  (`unwrap_or_else(PoisonError::into_inner)`) instead of panicking, since the data stays usable.
- **`let` chains.** `if cond && let Some(x) = expr { ... }` (stable since Rust 1.88, edition 2024) replaces nested
  `if let` blocks; see the usage injection in [server/mod.rs](../../src/server/mod.rs).
- **Static state.** `next_id` keeps one `ulid::Generator` in a `static Mutex`; `Mutex::new` and `Generator::new`
  are `const fn`, so no lazy initialisation is needed.
- **Generated protocol types.** `opentelemetry-proto` provides structs generated from the OTLP `.proto` files;
  `prost::Message::encode_to_vec` turns them into bytes and `decode` back ([telemetry/mod.rs](../../src/telemetry/mod.rs)).
