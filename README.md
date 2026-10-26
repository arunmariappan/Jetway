# Jetway

Jetway is an LLM gateway: a proxy between your apps and a local Ollama that makes every model call cheaper, faster
and safer. Apps point at Jetway instead of Ollama and change nothing else. It streams responses through as they
arrive, answers repeated questions from a cache, joins identical requests in flight, shares the GPU fairly between
apps, records every exchange, and shows it all on a live dashboard. One Rust binary; it speaks Ollama's API and the
OpenAI-compatible API.

> **Status:** in development. The MVP is done (phases 0–4: streaming pass-through, request log and metrics, exact
> cache with joined requests, dashboard), and so are limits (phase 9: rate limits, budgets, a priority queue, a
> circuit breaker and alerts). Still to come: measurements with real clients (phase 5, paused), the semantic cache,
> guards for personal data and prompt injection, and thinking on/off routing. See the
> [implementation plan](docs/plan/README.md).

![The overview page of Jetway's dashboard: request counts, latency, time to first token, tokens per second, cache hit rate, errors, and charts of latency percentiles, first-token time, requests by outcome and cache results per minute](docs/images/dashboard.png)

## What works today

- **Pass-through for both APIs.** `/api/chat`, `/api/generate`, `/api/embed`, `/v1/chat/completions`,
  `/v1/completions`, `/v1/embeddings` and the model-info endpoints, streamed chunk by chunk. Jetway adds about
  0.1 ms per request (p50; 0.14 ms p99) on this PC. A client that disconnects stops Ollama's generation.
- **Per-app traffic.** Apps are told apart by a path prefix (`http://127.0.0.1:8300/a/my-app/`), an `X-Jetway-App`
  header or a bearer key; model management (`pull`, `delete`, ...) is refused unless an app is marked admin.
- **An exact cache.** A repeated request is answered in about 0.2 ms, in the client's protocol and stream mode,
  without touching the GPU; entries survive restarts. Identical requests that arrive while the first is running
  share its one upstream call.
- **A request log.** Every exchange is one SQLite row with token counts, time to first token and Ollama's own
  durations, with bodies kept for a week. Prometheus metrics on `/metrics`; OpenTelemetry spans if configured.
- **Fair use of one GPU.** Per-app rate limits and day or month budgets (requests, tokens, GPU-seconds) answer 429
  with `Retry-After`; cache hits cost nothing. One request at a time goes to Ollama, the rest wait by priority
  (interactive, normal, batch), and a request that would wait past its class's limit gets a 503 instead of piling
  up. An app can have an output cap, so a runaway answer can't hold the GPU for minutes. If Ollama stops answering,
  requests fail in about a millisecond until it is back, instead of each waiting 2 s for the connect to fail.
- **Alerts** for used-up budgets, error rates, slow first tokens, a down Ollama and a request log that drops records:
  on the dashboard, and optionally in Telegram.
- **A dashboard** on `http://127.0.0.1:8301`: overview, live feed, each exchange in detail, apps, the cache, limits
  and alerts.

![The Limits page: the upstream's state, slots and waiting requests, queue wait per priority class, budget usage bars, each app's limits and the requests refused](docs/images/limits.png)

## Quick start

```bash
cargo run --release -- serve          # gateway on 127.0.0.1:8300 → Ollama on 127.0.0.1:11434; dashboard on :8301
```

Then point a client at Jetway instead of Ollama:

```bash
OLLAMA_HOST=127.0.0.1:8300 ollama run qwen3.5:4b "Name the capital of France."
```

```python
ollama.Client(host="http://127.0.0.1:8300/a/my-app")                         # the Python ollama client
OpenAI(base_url="http://127.0.0.1:8300/a/my-app/v1", api_key="unused")       # any OpenAI client
```

For a .NET app using OllamaSharp, set its base address to `http://127.0.0.1:8300/a/my-app/`.

### Without a GPU

```bash
cargo run --release -- fake-ollama --port 8399                    # answers from recorded Ollama responses
JETWAY__UPSTREAM__URL=http://127.0.0.1:8399 cargo run --release -- serve
cargo run --release -- load --rate 5 --duration 120 --fake        # demo traffic over three apps
```

## Limits and budgets

Per app in `jetway.toml` (unset values fall back to `[apps.default]`, then to no limit):

```toml
[apps.paperpilot]
priority = "interactive"        # interactive | normal | batch
requests_per_minute = 120       # burst defaults to a tenth of it
tokens_per_day = 3_000_000      # also: requests_per_day, tokens_per_month, gpu_seconds_per_day
max_output_tokens = 2048        # added to requests that set no cap of their own

[apps.nightly-eval]
priority = "batch"
gpu_seconds_per_day = 3600
```

`[queue]` sets how long each class may wait for a slot, `[budgets] time_zone` when days start, `[health]` the probe
and the circuit, `[alerts]` the rules. Telegram alerts need `alerts.telegram = true` and `JETWAY_TELEGRAM_TOKEN` and
`JETWAY_TELEGRAM_CHAT_ID` in the environment or a `.env` file; the token never appears in a log.

## Commands

```bash
jetway serve                                  # the gateway and the dashboard
jetway check-config                           # defaults, then jetway.toml, then JETWAY__* variables, validated
jetway requests tail -f [--app A]             # the request log as it grows; `requests show <id> --bodies`
jetway cache stats | cache list | cache clear [--app A]
jetway db migrate | db prune --older-than 7d
jetway fake-ollama | load                     # development and demos
```

Configuration lives in [`jetway.toml`](jetway.toml); every value can be overridden with an environment variable
(`JETWAY__SERVER__PORT=9000` sets `server.port`).

## Building

Rust 1.99 (pinned in `rust-toolchain.toml`; rustup installs it on first use). On Windows, the MSVC build tools from
Visual Studio are needed.

```bash
cargo build
cargo test                                    # never needs Ollama or a GPU
cargo bench --bench overhead                  # the latency Jetway adds, per request and per chunk
```

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
