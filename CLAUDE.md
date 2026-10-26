# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Jetway is an LLM gateway in Rust between apps and the host Ollama: streaming pass-through for Ollama's API and the
OpenAI-compatible API, exact and semantic caches, joined in-flight requests, personal-data and prompt-injection
guards, thinking on/off routing for `qwen3.5:4b`, budgets, a priority queue and a live dashboard. **Phases 0–4
(the MVP: pass-through, request log, exact cache, dashboard) are done; phase 5 (real clients) is next.** Read [docs/plan/README.md](docs/plan/README.md) and the current phase file before working; each phase
file has tasks, tests and "done when" items to tick, and ends with a "What was built" section (with "Rust notes",
plan D20) recording decisions and what is left. Keep the plan's status line and the README in step when a phase
lands.

## Working rules

- Commit on `main`, one small conventional commit per task (`feat:`, `fix:`, `test:`, `docs:`, `chore:`, `ci:`),
  pushed at least once per phase. No attribution trailer in commit messages. The repo-local identity is
  `Arun Mariappan Karunanithi <2525449+arunmariappan@users.noreply.github.com>`.
- Rust only (plan D1): dev tools are `jetway` subcommands, not scripts. The dashboard is askama + HTMX with no
  hand-written JavaScript.
- Model calls Jetway makes itself use `qwen3.5:4b` in the host Ollama with `think` off, a JSON schema in `format`,
  validation, one retry, then an explicit `invalid` result.
- Ask the user before any GPU run longer than a few minutes: this PC has shut down during long GPU runs. Develop and
  test against `jetway fake-ollama`.
- Changes to PaperPilot or BlackBox go in their own repos under their own CLAUDE.md, after asking.
- When a decision changes, update the plan (decisions D1–D20, risks R1–R12 in docs/plan/README.md) in the same commit.
- Plan D20: each phase's "What was built" ends with "Rust notes": short explanations of the Rust ideas the phase
  used, with links to the code. The user is learning Rust with this project.

## Commands

```bash
cargo build                                   # rustup installs the pinned 1.99 toolchain on first use
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo test                                    # never needs Ollama, a GPU or a downloaded model
cargo test config::tests::file_then_environment   # a single test
cargo test -- --ignored                       # tests that need real models (later phases)
cargo run -- check-config                     # merged configuration (defaults, jetway.toml, JETWAY__* env)
cargo run -- serve                            # gateway :8300 → upstream.url, admin :8301 (/metrics, /healthz)
cargo run -- requests tail -f [--app A]       # the request log (data/jetway.db); `requests show <id> --bodies`
cargo run -- db migrate | db prune --older-than 7d
cargo run -- cache stats | cache list [--app A] | cache clear [--app A]
cargo run -- load --rate 5 --duration 120 --fake   # demo traffic (with fake-ollama upstream) for dashboard work
cargo run -- fake-ollama --port 8399          # fake upstream; JETWAY__UPSTREAM__URL=http://127.0.0.1:8399 to use it
cargo bench --bench overhead                  # added latency per request and per chunk (release build)
OLLAMA_HOST=127.0.0.1:8300 ollama run qwen3.5:4b "hi"   # the ollama CLI through Jetway (app "default")
cargo deny check                              # licences, advisories, duplicates (cargo install cargo-deny)
```

## Layout

- `src/main.rs`: the clap CLI. `src/lib.rs`: modules. `src/config.rs`: figment configuration with validation.
- `src/server/`: the gateway handler (`mod.rs`), app identity (`identity.rs`: `/a/{app}/`, `X-Jetway-App`, bearer
  key, `default`) and the path allow-list (`routes.rs`).
- `src/proxy/`: `Upstream` (reqwest client), `headers` (what crosses), `errors` (bodies per protocol, stream error
  lines), `tee` (`TeeBody`: forwards chunks, timeouts, cancellation), `record` (per-exchange recorder task, `Sink`,
  `Finished`).
- `src/protocol/`: `Protocol` (by path) and `Framing` (by content type). `src/dev/fake_ollama.rs`: the fake.
- `src/protocol/decode.rs` (NDJSON/SSE decoders), `summary.rs` (`RequestSummary`, `ResponseSummary`).
  `src/proxy/usage.rs`: OpenAI usage asked for and hidden (plan D21).
- `src/store/`: `Store` (writer thread, read pool), `schema.rs` (migrations; add a new script to `MIGRATIONS`, never
  edit an applied one), `row.rs` (`ExchangeRow`, `RollupRow`, SQL), `sink.rs` (`RequestLog`).
- `src/metrics/`: rollups and Prometheus text. `src/telemetry/`: span ids, spans, OTLP export.
- `src/cache/`: `mod.rs` (decision per request, lookup memory then SQLite, the cache sink that stores answers),
  `key.rs` (canonical blake3 key, join key), `answer.rs` (`Entry`, `Answer`, serving a hit in either stream mode),
  `flight.rs` (joined requests). `src/proxy/served.rs`: bodies for hits and flight readers.
- `src/web/`: the admin listener's routes (`mod.rs`), page models and askama templates (`pages.rs`, `templates/`),
  data from the rollups (`views.rs`), SVG charts (`charts.rs`), the SSE live feed (`feed.rs`), embedded assets
  (`assets.rs`, `assets/`), the JSON API (`api.rs`). `src/server/admin.rs`: the admin state.
- `src/app.rs`: starts and stops everything `serve` runs; tests use it through `tests/common::Stack`.
- `tests/common/`: `Harness` (fake + gateway only) and `Stack` (the whole app with a temporary database).
  `tests/pass_through.rs`: phase 1; `tests/request_log.rs`: phase 2.
- `jetway.toml`: committed defaults; a test checks it equals `Config::default()`, so change both together.
- `docs/plan/`: the plan. Later phases add the modules listed in plan §3.

## Gotchas

- Shells and VS Code windows opened before Rust was installed don't have `%USERPROFILE%\.cargo\bin` on PATH; in Git
  Bash prepend `$HOME/.cargo/bin`.
- `rust-toolchain.toml` pins channel `1.99`, which rustup installs as its own toolchain next to `stable`.
- reqwest has no TLS feature until phase 9 (Telegram): the upstream is plain HTTP on 127.0.0.1, and TLS pulls in a
  heavy C build (`aws-lc-sys`).
- Git here has `core.autocrlf=true`; `.gitattributes` keeps files LF. Python's `Path.write_text` writes CRLF on
  Windows; pass `newline="\n"`.
- Clippy denies `unwrap`/`expect` outside tests (`clippy.toml` allows them in tests). `figment::Jail` tests need
  `#[allow(clippy::result_large_err)]`, because Jail fixes the closure's error type.
- Microsoft Defender excludes `D:\ai_workspace\Jetway\target` (added 2026-10-25 at the user's request) for faster
  builds. Throwaway spike crates build with `CARGO_TARGET_DIR=D:/ai_workspace/Jetway/target/spikes` to benefit.
- Every exchange must end in exactly one `Finished` record. hyper drops a response body without reading to the end
  when it has sent the declared `Content-Length`, and never reads one for HEAD/204/304; the tee ends those as `ok`
  itself (`expected_len`). A dropped body otherwise means the client left (`client_cancelled`).
- Sinks run in order (`Sinks`): metrics, the store, spans, then extras (log, test collector). A test that has seen
  an exchange in its `Collect` sink must still `store.flush().await` before reading the database (`Stack::settle`).
- Exchange ids must come from `server::next_id()` (monotonic): the log pages by id.
- A body that hyper reads with a `Content-Length` (or never reads: HEAD) is dropped without a final poll, so every
  body type ends its exchange as `ok` when its last byte is handed over (`TeeBody::expected_len`, `ServedBody`,
  `FlightBody`). Forget that and finished exchanges show as `client_cancelled`.
- Dashboard colours come from the validated data-viz palette (CSS variables in `assets/jetway.css`, light and dark);
  new chart colours must be checked with the dataviz skill's validator, not picked by eye. No hand-written
  JavaScript: interactivity is htmx attributes, CSS hover and native SVG `<title>` tooltips.
- The dashboard reads in-memory rollups; `app::start` restores the last day from SQLite. Assets are `no-cache` with
  an ETag, so a rebuilt binary's CSS shows up at once. Playwright screenshots go to the scratchpad, never Downloads.
- Test harnesses (`Harness`, `Stack`) start with `cache.exact = false`; tests that want the cache turn it on.
- `serde_json` has `preserve_order`; the cache key must keep sorting keys itself (`cache::key`).
- Streamed and unstreamed versions of the same request share a cache key (`stream` isn't part of it).
- `jetway.toml` must keep matching `Config::default()` (a test checks); new settings go in both.
- Fake-Ollama scripts go in the `x-fake-script` request header (JSON); Jetway forwards it like any other header.
- On Windows a refused loopback connection takes ~2 s (the OS retries), so "Ollama is down" is slow until phase 9's
  circuit breaker; tests allow for it with `cfg!(windows)`.
- Integration test files start with `#![allow(clippy::unwrap_used, clippy::expect_used)]`: `clippy.toml`'s test
  allowance covers `#[test]` functions, not helpers.
- Real clients (phase 5): PaperPilot goes through Jetway with `ConnectionStrings__ollama=Endpoint=http://127.0.0.1:8300/a/paperpilot/`
  (and `Langfuse__Enabled=false`) in the environment of `aspire start`; BlackBox in front with a copy of
  `blackbox.toml` whose `ollama` target is `http://127.0.0.1:8300/a/<app>` (`BLACKBOX_CONFIG=...`, plus
  `PYTHONUTF8=1` when its output is redirected). Stop PaperPilot's docling container for such sessions (2.4 GB).
- The PC has shut down during GPU work again (2026-10-26, phase 5). No GPU run without the user's go-ahead.
- Pushing needs the user's GitHub login for this repo's URL (`credential.usehttppath` is on globally, so BlackBox's
  saved login doesn't apply). Don't copy credentials between entries; ask the user to sign in once.

## Spike findings (phase 0, 2026-10-25)

- **Ollama 0.40.11 (S1):** `OLLAMA_NUM_PARALLEL:1`, `OLLAMA_MAX_QUEUE:512`: requests run one at a time, so
  `upstream.slots = 1`. A client disconnect stops generation at once (log: `srv stop: cancel task`). Native streams
  are NDJSON; the final line carries `done_reason`, durations, `prompt_eval_count`, `prompt_eval_cached_count` and
  `eval_count` (which includes thinking tokens). Thinking: `message.thinking` (native), `delta.reasoning` (OpenAI;
  `reasoning_effort: "none"` turns it off). OpenAI streams carry `usage` only with `stream_options.include_usage`,
  as a `choices: []` event with llama-server `timings`. `qwen3.5:4b` can't embed (501). Golden files:
  `tests/fixtures/ollama/` (byte-exact, `-text`).
- **Embeddings (S2):** fastembed 7.5 / ort 2.0.0-rc.15 build fine with MSVC. fp32 beats int8 on this Zen 2 CPU.
  bge-small, 8 threads: 9.6 ms p95 at 16 tokens, 21.5 ms at 64. MiniLM-L6, 4 threads: 6.4 and 13.2 ms. Phase 6's
  eval picks the model.
- **Injection classifier (S3):** `Horizon-Labs/prompt-injection-guard-small` (int8 ONNX, Apache-2.0): 44 ms at 64
  tokens, 288 ms at 512, ~400 MB; 0/30 benign research questions flagged, 10/10 research-style attacks caught at 0.9.
  ProtectAI's DeBERTa v2 flagged 3/30 benign and is twice as slow; wolf-defender's ONNX alone is uncalibrated.
- **Base paths (S4):** OllamaSharp 5.5.0 uses relative paths against `BaseAddress` (PaperPilot adds the trailing
  slash) and checks "is running" with a GET of the base address itself, so `/a/{app}/` must mean `/`. ollama-python
  keeps a path in `host` too.
