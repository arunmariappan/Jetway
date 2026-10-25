# Phase 1: Streaming pass-through

**Goal:** any Ollama or OpenAI client works through Jetway exactly as it does against Ollama. Streams are forwarded
chunk by chunk with well under a millisecond added, a client that disconnects stops the generation, and every failure
reaches the client in its own protocol's error format.

**Size:** M · **Depends on:** 0

## Tasks

### 1.1 Configuration and startup
- [ ] `config.rs`: figment reads `jetway.toml`, then `JETWAY__*` environment variables (`__` separates sections);
      `Config::validate` checks ports, URLs (`localhost` in an upstream URL is refused with a hint to use
      `127.0.0.1`, D17), app names (`[a-z0-9-]{1,32}`) and timeouts. `jetway check-config` prints the merged
      configuration with secrets masked.
- [ ] `jetway serve`: `tracing-subscriber` with an env filter (`JETWAY_LOG`, default `info`), compact console
      output.
- [ ] Graceful shutdown on Ctrl+C: stop accepting, let in-flight streams finish for up to
      `shutdown_grace_seconds`, then close them. In-flight count logged.

### 1.2 Listener
- [ ] axum on `127.0.0.1:8300`, HTTP/1.1 with keep-alive, `TCP_NODELAY` on accepted sockets (D17).
- [ ] Request bodies read in full up to `max_request_bytes` (Jetway has to parse them; they are small next to
      generation time). Too large: 413 in the client's error format.

### 1.3 App identity (D12)
- [ ] In order: path prefix `/a/{app}/` (stripped before routing), `X-Jetway-App` header, a bearer key listed in
      `[apps.<name>] keys = [...]`, otherwise `default`.
- [ ] `/a/{app}` and `/a/{app}/` with nothing after them mean `/`: OllamaSharp's "is Ollama running" check requests
      its base address itself (spike S4).
- [ ] `server.require_known_app = false` by default; when true, an unknown app gets 403.
- [ ] The app's name goes on the exchange and on log lines (a `tracing` span field).

### 1.4 Paths (D4)

| Path | Handling |
|---|---|
| `POST /api/chat`, `/api/generate`, `/api/embed`, `/api/embeddings` | Forwarded; generation and embeddings |
| `POST /v1/chat/completions`, `/v1/completions`, `/v1/embeddings` | Forwarded; OpenAI-compatible |
| `GET /api/tags`, `/api/ps`, `/api/version`, `/v1/models`, `POST /api/show`, `GET` and `HEAD /` | Forwarded; model information (phase 8 adds the virtual model to the lists) |
| `/api/pull`, `/api/push`, `/api/create`, `/api/copy`, `/api/delete`, `/api/blobs/*` | 403 unless the app has `admin = true`; then forwarded |
| Anything else | 404 in Ollama's error format: `{"error":"Jetway doesn't forward <path>"}` |

- [ ] An explicit allow-list instead of a blind pass-through: a client can't reach anything Jetway doesn't know.

### 1.5 Upstream client
- [ ] `reqwest` with a keep-alive pool to the upstream, `TCP_NODELAY`, `connect_timeout_ms`, no total timeout.
- [ ] Headers: hop-by-hop headers (`Connection`, `Keep-Alive`, `Transfer-Encoding`, `TE`, `Trailer`, `Upgrade`,
      `Proxy-*`) removed both ways; `Host` set to the upstream; `Authorization` never forwarded (Jetway's keys
      aren't Ollama's, and Ollama has no authentication); `traceparent` and `tracestate` forwarded unchanged in this
      phase (phase 2 adds Jetway's own spans).
- [ ] `X-Jetway-Id: <ulid>` on every response.

### 1.6 Stream tee (D15)
- [ ] The response body is `Body::from_stream` over the upstream's `bytes_stream()`. Each chunk is handed to the
      client as it arrives; a clone of the `Bytes` (a reference count, no copy) goes into a bounded channel to the
      recorder.
- [ ] The recorder reassembles NDJSON lines or SSE events on its own task, so a slow parse never delays the client.
      It stops recording above 8 MiB and marks the exchange `truncated`, while forwarding goes on.
- [ ] If the recorder's channel is full, the chunk is not recorded (counted, exchange marked `incomplete_record`);
      forwarding never waits for the recorder.
- [ ] Back-pressure is natural: when the client reads slowly, hyper stops polling the upstream body. The idle
      timeout counts only upstream silence, so a slow reader isn't cut off.

### 1.7 Timeouts and cancellation
- [ ] First-byte timeout (`first_byte_timeout_seconds`) and idle timeout between chunks
      (`idle_timeout_seconds`), implemented around the upstream stream.
- [ ] When the client disconnects, hyper drops the response body; that drops the upstream response, which closes
      the connection, and Ollama stops generating (spike S1). The exchange ends as `client_cancelled`.

### 1.8 Errors in the client's format

| Case | Ollama API | OpenAI API |
|---|---|---|
| Upstream refused or unreachable | 502 `{"error":"Ollama isn't reachable at http://127.0.0.1:11434"}` | 502 `{"error":{"message":...,"type":"upstream_unavailable"}}` |
| First-byte timeout | 504 | 504 |
| Upstream error status | Passed through unchanged | Passed through unchanged |
| Failure mid-stream | A final line `{"error":"..."}`, then close | A final `data: {"error":{...}}` event, then close |
| Jetway's own refusals (403, 404, 413) | `{"error":"..."}` | `{"error":{"message","type","code"}}` |

- [ ] One module (`proxy::errors`) writes all of them, with snapshot tests per protocol.

### 1.9 Fake Ollama (D18)
- [ ] `jetway fake-ollama --port 8399` and an in-process version for tests (`dev::fake_ollama::spawn()` on a free
      port). It serves both APIs from the S1 golden files, with a script per request: time to first byte, gap
      between chunks, number of tokens, error status, drop after N chunks, never finish.
- [ ] It records what it received (headers, body) and when each client disconnected, so tests can assert on them.

### 1.10 Overhead bench
- [ ] `benches/overhead.rs` (criterion) or `jetway load` v0: client → fake Ollama directly against client → Jetway →
      fake Ollama, both with zero delays. Added latency per request (p50, p99) and per chunk.
- [ ] Targets: p50 under 0.3 ms and p99 under 1 ms added per request at concurrency 1; p99 under 0.5 ms added per
      chunk. Numbers go into `CLAUDE.md`.

## Tests
- [ ] Streamed chunk timing is kept: the fake sends 5 chunks 100 ms apart, and the client receives each within 5 ms
      of it being sent.
- [ ] Bodies are forwarded byte for byte, streamed and not, in both APIs.
- [ ] A line split across two upstream chunks reaches the client unchanged (the forward path never reframes).
- [ ] Client disconnect: the fake sees its connection closed within 100 ms.
- [ ] Every row of the error table, by snapshot.
- [ ] Path rules: allow-list, 403 for model management, 404 for the rest; prefix stripping; app identity order.
- [ ] Hop-by-hop headers and `Authorization` aren't forwarded; `traceparent` is.
- [ ] Upstream down: 502 within 50 ms (a refused connection to 127.0.0.1 is immediate).
- [ ] First-byte and idle timeouts, with paused tokio time.
- [ ] Graceful shutdown waits for an in-flight stream, then ends.

## Done when
- [ ] `$env:OLLAMA_HOST = "127.0.0.1:8300"; ollama run qwen3.5:4b "Say hi in five words"` streams through Jetway
      (a few seconds of GPU).
- [ ] The Python `ollama` client and `curl` against `/v1/chat/completions` with `stream: true` work through Jetway.
- [ ] The overhead targets are met, and CI is green.
