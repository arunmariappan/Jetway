# Phase 3: Exact cache and joined requests

**Goal:** a request that matches an earlier one exactly is answered in about a millisecond, in the client's own
protocol and stream mode, without touching the GPU; and identical requests that arrive while the first is still
running share its one upstream call.

**Size:** M · **Depends on:** 2 · **Status:** done (2026-10-25)

## Tasks

### 3.1 Cache key
- [x] blake3 over a canonical form of the request body (`cache::key`): every field except the ones below, with
      object keys sorted at every level and numbers normalised (`1` and `1.0` are equal), strings byte for byte. A
      version string is hashed in first, so a change to the canonical form can't match old entries.
- [x] Left out because they don't change the answer: `stream`, `keep_alive`, `stream_options`, `user`, `metadata`
      (top level).
- [x] Defaults are not filled in: an omitted `temperature` and an explicit one give different keys.
- [x] Scoped by app and path unless `cache.shared = true`.

### 3.2 What is cacheable
- [x] Generation and embedding endpoints; model info and management are never looked at.
- [x] Only complete answers: `ok`, status 200, the summary's `done`, no error object, not truncated or incomplete,
      and a response framing that matches what the request asked for.
- [x] Sampled requests (no seed, and a temperature that isn't 0, including the model's default) follow the app's
      `sampled` policy; default `allow`.
- [x] Request controls: `Cache-Control: no-cache` (fresh answer, stored), `no-store` and `X-Jetway-Cache: off`
      (neither).
- [x] `X-Jetway-Cache: hit | miss | bypass | joined | off` on every generation and embedding response;
      `X-Jetway-Cache-Age` and `X-Jetway-Source` (the exchange that produced the answer) on hits.

### 3.3 Stored form and serving
- [x] An entry keeps the bytes the first client received and a protocol-neutral answer (content, thinking, tool
      calls, done reason, token counts, Ollama's durations). Streamed OpenAI tool calls are assembled from their
      pieces for this (`ResponseSummary::tool_call_values`).
- [x] **Changed from the plan:** a hit in the same stream mode replays the stored bytes exactly, original
      timestamps included, instead of rewriting each line's `created_at`; that keeps hits byte-identical to the
      first answer and costs nothing. A hit in the other mode is written from the answer in the request's protocol,
      with fresh timestamps and ids (`cache::answer::serve`, tested against every golden file in both directions).
- [x] OpenAI streams are stored without a usage event; a client that asks for usage gets one written from the
      recorded counts (D21). `usage` is left out of written bodies when the counts are unknown.
- [x] `replay = "instant"` sends a hit at once, with a `Content-Length`; `"paced"` sends its lines spread over the
      original generation time (`eval_duration`).
- [x] Token counts and durations in served bodies stay as recorded; the exchange's cache note carries the saved
      tokens and GPU time.

### 3.4 Storage
- [x] In memory: `moka`'s sync cache (a lookup never waits), weighted by entry bytes, bounded by
      `max_memory_bytes`, with the TTL.
- [x] On disk: `cache_entries` (migration 2; zstd raw bytes, the answer as JSON), written by the store's writer; a
      memory miss falls back to an indexed read, so a restart keeps every entry. Expired entries are never served
      and are deleted by the hourly prune.
- [x] `jetway cache stats`, `jetway cache list [--app A]`, `jetway cache clear [--app A]` (a running Jetway keeps
      what it holds in memory until it expires; the admin API in phase 4 clears both).

### 3.5 Joined requests (D14)
- [x] Flights by join key (the exact key plus the stream mode and `include_usage`, which shape the bytes): the
      first request leads; later identical requests read the same flight from its first chunk.
- [x] A task (`cache::flight::fly`) reads the upstream through the leader's tee for every reader, so the leader's
      client leaving doesn't cancel it while others read; when the last reader leaves, the task drops the upstream,
      which stops Ollama. Joined requests waiting for headers get the leader's error if its call fails first.
- [x] The leader's exchange is the upstream call (tokens, GPU time); each joined request has its own record with
      cache note `joined` and the leader's id.
- [x] Follows the cache's rules; `join = false` per app turns it off.

### 3.6 Cache metrics
- [x] Prometheus: `jetway_cache_total{app,result}`, `jetway_cache_saved_tokens_total`,
      `jetway_cache_saved_gpu_seconds_total`, `jetway_cache_entries`, `jetway_cache_memory_bytes`, `jetway_flights`.
      Rollups gain cache hits, joins, saved tokens and saved GPU time per minute (for phase 4's charts).

## Tests
- [x] Key table: field order, ignored fields, number forms, defaults not filled, scope and path, join keys.
- [x] Cacheability: an upstream 500, a mid-stream failure and a cancelled stream aren't stored; request controls;
      the `sampled = "skip"` policy; another app or temperature is a miss.
- [x] Stored and requested stream mode in every combination, both protocols, thinking and tool calls (unit tests
      over the golden files); hits in the other mode end to end; OpenAI usage added on request; embeddings.
- [x] Joined requests: 10 identical concurrent requests give one upstream call and 10 identical bodies (9 joined);
      the first client leaving doesn't cancel the others; everyone leaving closes the upstream connection within
      150 ms; a leader's 504 reaches the joined request; joined unstreamed requests end `ok`.
- [x] Persistence across a restart (a new Jetway on the same database answers from SQLite); expiry and the sweep;
      `cache clear`; paced replay takes about the original generation time; Prometheus counts.

## Done when
- [x] A repeated `ollama run` prompt is answered from the cache: 6 079 ms the first time (cold model), 125 ms the
      second (most of it the CLI), 123 ms after a restart (from SQLite), checked live on 2026-10-25. The bench: a
      hit's whole round trip is **0.171 ms** p50 and 0.317 ms p99.
- [x] A restart keeps the cache, and `jetway cache stats` shows the hits.
- [x] The joined-request tests pass, and the overhead targets still hold on misses (0.091 ms p50, 0.137 ms p99
      added per request with the cache bypassed).

## What was built (2026-10-25)

- `cache` (`mod`: decision, lookup, the cache sink; `key`; `answer`: entries and serving; `flight`: joined
  requests), `proxy::served` (bodies for hits and flight readers), migration 2 and `store::cache_rows`, cache
  metrics, the `cache` commands, `tests/cache.rs` (16 tests).
- The gateway now owns the cache and puts it first in its sink chain; `Gateway::new` takes an optional store.
- Found live: hits sent with a `Content-Length` were recorded as cancelled, because hyper stops polling a body once
  it has sent the declared length; served and joined bodies now count as delivered when their last byte is handed
  over (the same lesson as phase 1's `expected_len`).
- `serde_json` now has `preserve_order`, so written bodies keep Ollama's field order (model first) and Jetway's own
  OpenAI error bodies match Ollama's (`message`, `type`, `param`, `code`); the cache key sorts keys itself and
  doesn't depend on it.
- Test harnesses start with the cache off, since pass-through and log tests send identical requests on purpose;
  cache tests turn it on.

### Rust notes
- **Hand-written waker lists.** A flight [`Reader`](../../src/cache/flight.rs) implements `Stream` by checking the
  shared chunk list under a mutex; when there's nothing new it stores `cx.waker().clone()` and returns `Pending`.
  `Flight::push` takes the stored wakers out *before* waking them, so no lock is held while other tasks run.
- **Reference counting decides lifetimes.** Each `Reader` increments an `AtomicUsize`; its `Drop` decrements it and,
  at zero, cancels a `CancellationToken` that the flight task is `select!`ing on. Dropping the task's tee then
  closes the upstream connection: the whole "stop when everyone leaves" rule is three `Drop`s.
- **`Arc<Self>` receivers.** `fn reader(self: &Arc<Self>)` lets a method hand out new `Arc` clones of the flight it
  is called on, which a plain `&self` can't.
- **`watch` channels for "has it started yet".** `Flight::wait_start` uses `watch::Receiver::wait_for`, which
  returns at once if the value is already set; a `oneshot` couldn't serve several waiting joiners.
- **Closures that borrow, then a move.** In the handler, `fail_flight` borrows `leader`; the borrow ends at its last
  use, so `match leader` can move it afterwards (non-lexical lifetimes) ([server/mod.rs](../../src/server/mod.rs)).
- **Trait objects in a `Vec`.** The gateway builds `Sinks(vec![cache as Arc<dyn Sink>, sink])`: the `as` cast turns
  an `Arc<Cache>` into an `Arc<dyn Sink>`, so different sink types share one list.
- **Feature flags change behaviour crate-wide.** Turning on `serde_json/preserve_order` changes `serde_json::Map`
  for every crate in the build that uses it, which is why the cache key sorts keys explicitly instead of relying on
  the map's order.
