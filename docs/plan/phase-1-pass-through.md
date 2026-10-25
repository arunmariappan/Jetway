# Phase 1: Streaming pass-through

**Goal:** any Ollama or OpenAI client works through Jetway exactly as it does against Ollama. Streams are forwarded
chunk by chunk with well under a millisecond added, a client that disconnects stops the generation, and every failure
reaches the client in its own protocol's error format.

**Size:** M · **Depends on:** 0 · **Status:** done (2026-10-25)

## Tasks

### 1.1 Configuration and startup
- [x] `config.rs`: figment reads `jetway.toml`, then `JETWAY__*` environment variables (`__` separates sections);
      `Config::validate` checks ports, URLs (`localhost` in an upstream URL is refused with a hint to use
      `127.0.0.1`, D17), app names (`[a-z0-9-]{1,32}`), shared keys and timeouts. `jetway check-config` prints the
      merged configuration (there are no secret settings yet to mask).
- [x] `jetway serve`: `tracing-subscriber` with an env filter (`JETWAY_LOG`, default `info`), compact console
      output, ANSI colours only on a terminal.
- [x] Graceful shutdown on Ctrl+C: stop accepting, let in-flight streams finish for up to
      `shutdown_grace_seconds`, then close them. In-flight count logged.

### 1.2 Listener
- [x] axum on `127.0.0.1:8300`, HTTP/1.1 with keep-alive, `TCP_NODELAY` on accepted sockets (`tap_io`, D17).
- [x] Request bodies read in full up to `max_request_bytes`. Too large: 413 in the client's error format.

### 1.3 App identity (D12)
- [x] In order: path prefix `/a/{app}/` (stripped before routing), `X-Jetway-App` header, a bearer key listed in
      `[apps.<name>] keys = [...]`, otherwise `default`. An invalid name is a 400.
- [x] `/a/{app}` and `/a/{app}/` with nothing after them mean `/`: OllamaSharp's "is Ollama running" check requests
      its base address itself (spike S4).
- [x] `server.require_known_app = false` by default; when true, an unknown app gets 403 (`default` is always known).
- [x] The app's name is on the exchange and its log line.

### 1.4 Paths (D4)

| Path | Handling |
|---|---|
| `POST /api/chat`, `/api/generate`, `/api/embed`, `/api/embeddings` | Forwarded; generation and embeddings |
| `POST /v1/chat/completions`, `/v1/completions`, `/v1/embeddings` | Forwarded; OpenAI-compatible |
| `GET /api/tags`, `/api/ps`, `/api/version`, `/v1/models`, `/v1/models/{id}`, `POST /api/show`, `GET` and `HEAD /` | Forwarded; model information (phase 8 adds the virtual model to the lists) |
| `/api/pull`, `/api/push`, `/api/create`, `/api/copy`, `/api/delete`, `/api/blobs/*` | 403 unless the app has `admin = true`; then forwarded |
| Anything else | 404 in the client's error format: `{"error":"Jetway doesn't forward <path>"}` |

- [x] An explicit allow-list instead of a blind pass-through (`server::routes`).

### 1.5 Upstream client
- [x] `reqwest` with a keep-alive pool to the upstream, `TCP_NODELAY`, `connect_timeout_ms`, no total timeout, and
      no proxy from the environment.
- [x] Headers: hop-by-hop headers (and any named in `Connection`) removed both ways; `Host` and `Content-Length`
      set by the client; `Authorization`, `Accept-Encoding` (so the recorder sees plain bytes) and `X-Jetway-*` never
      forwarded; `traceparent` and `tracestate` forwarded unchanged; the upstream's `Date` replaced by Jetway's.
- [x] `X-Jetway-Id: <ulid>` on every response.

### 1.6 Stream tee (D15)
- [x] The response body is `Body::from_stream` over a `TeeBody` wrapping the upstream's `bytes_stream()`. Each chunk
      is handed to the client as it arrives; a clone of the `Bytes` goes into a bounded channel to a per-exchange
      recorder task.
- [x] The recorder collects up to 8 MiB and marks the exchange `truncated` beyond it, while forwarding goes on.
      Line parsing moves into the recorder in phase 2.
- [x] If the recorder's channel is full, the chunk is skipped and the exchange marked `incomplete`; forwarding never
      waits for the recorder.
- [x] Back-pressure is natural, and the idle timeout counts only upstream silence.
- [x] Found while testing: hyper stops reading a body once it has sent the `Content-Length` it declared, and never
      reads one for `HEAD`, 204 or 304, so the tee ends those exchanges as `ok` at that point instead of reporting a
      cancellation when the body is dropped. Streams have their `Content-Length` removed, since an error line may be
      appended.

### 1.7 Timeouts and cancellation
- [x] First-byte timeout around the upstream request (until its headers arrive; for an unstreamed request that is
      the whole generation) and an idle timeout between chunks.
- [x] When the client disconnects, the body (and with it the upstream response) is dropped, which closes the
      upstream connection. The exchange ends as `client_cancelled`. This also works before the first byte: hyper
      drops the handler while it waits for the upstream, which cancels the upstream request.

### 1.8 Errors in the client's format

| Case | Ollama API | OpenAI API |
|---|---|---|
| Upstream refused or unreachable | 502 `{"error":"Ollama isn't reachable at http://127.0.0.1:11434"}` | 502 `{"error":{"message":...,"type":"upstream_unavailable","param":null,"code":null}}` |
| First-byte timeout | 504 | 504, `upstream_timeout` |
| Upstream error status | Passed through unchanged | Passed through unchanged |
| Failure or idle timeout mid-stream | A final line `{"error":"..."}` (after closing a partial line), then close | A final `data: {"error":{...}}` event, then close |
| Jetway's own refusals (400, 403, 404, 413) | `{"error":"..."}` | `{"error":{"message","type","param","code"}}` |

- [x] One module (`proxy::errors`) writes all of them. Exact-body assertions instead of snapshot files (insta isn't
      needed yet).

### 1.9 Fake Ollama (D18)
- [x] `jetway fake-ollama --port 8399 [--script JSON]` and `dev::fake_ollama::spawn_local()` for tests. It serves
      both APIs from the S1 golden files (embeddings are synthetic, since `qwen3.5:4b` can't embed), with a script per
      request in the `x-fake-script` header: `status`, `ttfb_ms`, `gap_ms`, `chunks`, `drop_after`, `hang_after`,
      `split_lines`.
- [x] It logs each request (method, path, headers, body), each chunk sent with its time, and each disconnect.

### 1.10 Overhead bench
- [x] `benches/overhead.rs` (`cargo bench --bench overhead`, a plain harness: criterion reports means, and the
      targets are percentiles). Release build on this PC, against the fake with no delays:

| Measure | Direct | Through Jetway | Added | Target |
|---|---|---|---|---|
| Request p50 (2 000 non-streamed `/api/chat`, concurrency 1) | 0.108 ms | 0.203 ms | **0.095 ms** | < 0.3 ms |
| Request p99 | 0.177 ms | 0.331 ms | **0.153 ms** | < 1 ms |
| Chunk p50 (603 chunks, 5 ms apart) | 0.134 ms | 0.179 ms | 0.045 ms | |
| Chunk p99 | 0.221 ms | 0.317 ms | **0.096 ms** | < 0.5 ms |

## Tests
- [x] Streamed chunk timing is kept: the fake sends 5 chunks 100 ms apart, and the client receives each within 5 ms
      of it being sent.
- [x] Bodies are forwarded byte for byte, streamed and not, in both APIs (11 POST cases and 5 GET paths).
- [x] A line split across two upstream chunks reaches the client unchanged.
- [x] Client disconnect: the fake sees its connection closed within 100 ms; also before the first byte (within
      500 ms of the client giving up).
- [x] Every row of the error table, with exact bodies.
- [x] Path rules: allow-list, 403 for model management, admin apps forwarded, 404 for the rest; prefix stripping;
      app identity order; `require_known_app`.
- [x] Hop-by-hop headers, `Authorization`, `Accept-Encoding` and `X-Jetway-App` aren't forwarded; `traceparent` is.
- [x] Upstream down: 502, fast on Linux. **On Windows a refused loopback connection takes about 2 s** (Windows
      retries it; measured with .NET's `TcpClient` too), so the test allows 2.5 s there. Phase 9's circuit breaker is
      what makes a stopped Ollama fail fast.
- [x] First-byte and idle timeouts, with 1-second settings in real time (the settings are whole seconds).
- [x] Graceful shutdown lets an in-flight stream run to its end, then returns.

## Done when
- [x] `OLLAMA_HOST=127.0.0.1:8300 ollama run qwen3.5:4b "Say hi in five words."` streams through Jetway; so do
      `ollama list` and `ollama ps` (2026-10-25).
- [x] The Python `ollama` client (`host='http://127.0.0.1:8300/a/pytest'`, streamed) and `curl` against
      `/v1/chat/completions` with `stream: true` work through Jetway.
- [x] The overhead targets are met; CI is checked on push.

## What was built (2026-10-25)

- `server` (listener, handler, shutdown, `identity`, `routes`), `proxy` (`Upstream`, `headers`, `errors`, `tee`,
  `record`), `protocol` (`Protocol`, `Framing`), `dev::fake_ollama`, the `serve` and `fake-ollama` commands, 15
  integration tests (`tests/pass_through.rs`, harness in `tests/common/`) and the overhead bench.
- Phase 1's sink (`LogSink`) writes one log line per exchange; phase 2 replaces it with parsing and storage.
- `rustfmt.toml` uses `use_small_heuristics = "Max"`, so short expressions stay on one line up to 120 columns.

### Rust notes
- **Implementing `Stream` by hand.** [`TeeBody`](../../src/proxy/tee.rs) implements `poll_next`: it polls the
  upstream stream and, when that is `Pending`, the idle timer. Returning `Poll::Pending` is only correct because
  both of those registered the task's waker in `cx`, so the runtime polls again when either is ready.
- **`Drop` as a guaranteed cleanup hook.** When a client disconnects, hyper simply drops the response body. The
  tee's `Drop` turns that into a `client_cancelled` record, and dropping its `upstream` field closes the upstream
  connection; `InflightGuard` decrements a counter the same way. No code path can forget either.
- **Cheap sharing of bytes.** `Bytes::clone` bumps a reference count instead of copying, so the same chunk can go
  to the client and to the recorder ([record.rs](../../src/proxy/record.rs)); `BytesMut` grows a buffer and
  `freeze()` turns it into `Bytes` without a copy.
- **Two kinds of channel.** A bounded `mpsc` with `try_send` never waits, which is what keeps recording off the
  forward path; a `oneshot` carries the single "how it ended" message, so it can't be lost to a full queue.
- **Pinning.** A future or stream must not move in memory once polled, so `tokio::time::Sleep` lives in a
  `Pin<Box<_>>` and the upstream in a `BoxStream` (a pinned box). Because every field is then `Unpin`, the tee can
  use `self.get_mut()` safely.
- **Trait objects across tasks.** `Arc<dyn Sink>` shares one sink between all tasks; the trait requires
  `Send + Sync + 'static`, which is what lets a `tokio::spawn`ed recorder hold it. Tests swap in a collecting sink
  ([tests/common/mod.rs](../../tests/common/mod.rs)) without touching the gateway.
- **Racing futures.** [`server::run`](../../src/server/mod.rs) uses `tokio::select!` to race the server's graceful
  shutdown against "shutdown requested, then the grace period"; a `Notify` connects the two.
- **Compile-time files.** The fake embeds the golden files with `include_str!(concat!(env!("CARGO_MANIFEST_DIR"),
  ...))` ([fake_ollama.rs](../../src/dev/fake_ollama.rs)), so `jetway fake-ollama` needs no files at run time.
