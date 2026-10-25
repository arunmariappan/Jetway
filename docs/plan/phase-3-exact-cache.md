# Phase 3: Exact cache and joined requests

**Goal:** a request that matches an earlier one exactly is answered in about a millisecond, in the client's own
protocol and stream mode, without touching the GPU; and identical requests that arrive while the first is still
running share its one upstream call.

**Size:** M · **Depends on:** 2

## Tasks

### 3.1 Cache key
- [ ] blake3 over a canonical form of the fields that change the answer: protocol, endpoint, effective model (after
      aliases, phase 8), messages or prompt and system, image hashes, tools, `format` or `response_format`, all
      options, `think`, Ollama's `template`, `raw` and `suffix`; OpenAI's `temperature`, `top_p`, `seed`,
      `max_tokens`, `n`, `stop` and the like.
- [ ] Left out because they don't change the answer: `stream`, `keep_alive`, `stream_options`, OpenAI's `user` and
      `metadata`.
- [ ] Canonical form: parsed into `serde_json::Value` (whose maps are sorted), numbers normalised (`1` and `1.0` are
      equal), strings kept byte for byte. Defaults are not filled in: an omitted `temperature` and an explicit one
      give different keys, which is safe.
- [ ] Partitioned by app unless `cache.shared = true`.

### 3.2 What is cacheable
- [ ] Endpoints: `/api/chat`, `/api/generate`, `/v1/chat/completions`, `/v1/completions`, and the embedding
      endpoints (deterministic, so ideal for caching).
- [ ] Only complete, successful answers: status 200, a final chunk with `done: true` (or OpenAI's `finish_reason`),
      a complete recording (not truncated or cancelled).
- [ ] Sampled requests (temperature above 0 and no seed) follow the app's `sampled` policy (R3). Default `allow`.
- [ ] Request controls: `Cache-Control: no-cache` skips the lookup but stores the answer; `no-store` does neither;
      `X-Jetway-Cache: off` does neither.
- [ ] Response headers: `X-Jetway-Cache: hit`, `miss`, `bypass` or `joined`; `X-Jetway-Cache-Age` on hits.

### 3.3 Stored form and serving
- [ ] Each entry stores a protocol-neutral answer (content, thinking, tool calls, done reason, token counts,
      durations) plus the raw response bytes as they were sent.
- [ ] Serving a hit in the stored stream mode replays the raw bytes; in the other mode, Jetway writes the answer
      from the neutral form in the client's protocol: one JSON body, or a stream. Timestamps (`created_at`,
      `created`) are set to now; OpenAI's `id` is new.
- [ ] Streamed hits are `instant` by default (one chunk per original line, sent at once); `replay = "paced"` sends
      them at the original token rate for UIs that look odd when a whole answer appears at once.
- [ ] Token counts and durations in the served body stay as recorded, so clients that compute tokens per second keep
      working; the headers say it was a hit, and Jetway's record counts the saved tokens and GPU time.

### 3.4 Storage
- [ ] In memory: `moka`'s async cache, weighted by entry bytes, bounded by `max_memory_bytes`, with the TTL.
- [ ] On disk: `cache_entries` in SQLite (zstd), written through the store writer. A memory miss falls back to a
      SQLite lookup by key (an indexed read, well under a millisecond), so a restart keeps every entry without
      loading them all.
- [ ] Expiry and eviction in both; an hourly sweep deletes expired rows.
- [ ] `jetway cache stats [--app A]`, `jetway cache list`, `jetway cache clear [--app A]`.

### 3.5 Joined requests (D14)
- [ ] An in-flight map from cache key to a shared stream: the chunks so far plus a notifier. A second identical
      request joins: it gets the chunks so far, then the live tail. If its stream mode differs from the first
      request's, it waits for the end and gets the answer written in its own mode.
- [ ] The upstream call goes on while at least one client still reads it. If the first client disconnects and others
      remain, nothing is cancelled; when the last one leaves, the upstream call is dropped.
- [ ] Joined exchanges are recorded as `joined`, with the id of the exchange that made the upstream call.
- [ ] Follows the same cacheability and `sampled` rules as the cache; an app can turn it off.

### 3.6 Cache metrics
- [ ] Hits, misses, bypasses and joins per app; saved tokens; saved GPU-seconds (the stored `total_duration`);
      entries and bytes. In Prometheus now, on the dashboard in phase 4.

## Tests
- [ ] Key table: field order, left-out fields, number forms, defaults not filled, app partitioning.
- [ ] Cacheability rules: errors, cancelled, truncated, `done_reason`, sampled policy, request controls.
- [ ] Every combination of stored and requested stream mode, in both protocols, against golden files.
- [ ] Joined requests: 10 identical concurrent requests give one upstream call and 10 identical answers; the first
      client disconnects and the others still finish; all disconnect and the fake Ollama sees its connection closed.
- [ ] Persistence: entries survive a restart; TTL expiry with an injected clock; eviction by size.

## Done when
- [ ] A repeated `ollama run` prompt and a repeated `/v1/chat/completions` request are answered from the cache, with
      the first byte in under 2 ms.
- [ ] A restart keeps the cache, and `jetway cache stats` shows the hits.
- [ ] The joined-request tests pass, and the phase 1 overhead targets still hold on misses.
