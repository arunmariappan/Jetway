# Phase 9: Limits, budgets and scheduling

**Goal:** each app gets its fair share of one GPU. Rate limits and budgets stop runaway clients, a priority queue
keeps interactive requests fast while batch work waits, requests that would wait too long are refused instead of
piling up, and a dead Ollama is reported at once instead of timing out request by request.

**Size:** M · **Depends on:** 3

> **Status (2026-10-26): done**, ahead of phases 5–8: it needs no GPU, and GPU work is paused after the PC shut down
> during phase 5. Measurements are in [reports/phase-9-limits.md](../../reports/phase-9-limits.md).

## Tasks

### 9.1 Rate limits
- [x] Per app `requests_per_minute` (and an optional burst) with `governor`'s GCRA limiter, checked first, before
      any parsing beyond the app name.
- [x] Refused: 429 with `Retry-After`, in the client's error format.

### 9.2 Budgets (D13)
- [x] Per app, over a day and a month: tokens (prompt, completion and thinking, upstream calls only), GPU-seconds
      (Ollama's `total_duration`) and requests. Cache hits and joined requests use none and count as saved.
- [x] Counters persisted in SQLite (one row per app and period, updated every 15 seconds and at shutdown) and
      restored at startup.
- [x] Checked before the upstream call: an app with nothing left is refused. A request that starts with budget left
      may overshoot it; this is documented rather than prevented, since an answer's length isn't known in advance.
- [x] Refused: 429, for example `budget exceeded: paperpilot used 3,000,000 of 3,000,000 tokens today; resets at
      00:00`, with `Retry-After`. Periods reset in `budgets.time_zone` (default: the system's).

### 9.3 Scheduler (D14)
- [x] `upstream.slots` requests at a time (the value spike S1 found). Others wait in a priority queue: `interactive`
      before `normal` before `batch`, first come first served within a class, with aging so batch work isn't starved
      forever.
- [x] `max_queue_wait_seconds` per class (`queue.max_wait_seconds`); a request that would exceed it is refused with
      503 and `Retry-After` (load shedding). A client that disconnects while queued leaves the queue without an
      upstream call.
- [x] Joined requests (phase 3) and cache hits never take a slot.
- [x] Queue wait is on the exchange, in `X-Jetway-Queue-Wait`, and on the dashboard.

### 9.4 Upstream health
- [x] `GET /api/version` every 5 seconds. After 3 failed connects in a row the circuit opens: requests fail at once
      with 503 "Ollama is down" in the client's format; a probe every 5 seconds closes it again.
- [x] Why it matters on this PC (found in phase 1): Windows reports a refused loopback connection only after about
      2 seconds of retries, so without the circuit every request to a stopped Ollama waits 2 s. ~~Also try
      `TCP_MAXRTMS` on the upstream socket~~: not tried. reqwest doesn't let a caller set socket options before the
      connect, and the circuit already removes the wait; a lower `upstream.connect_timeout_ms` is the knob for
      the first few failures.
- [x] Cold starts: a `load_duration` over 1 second marks the exchange as a model load, shown on the dashboard.

### 9.5 Alerts
- [x] Rules: budget at 80% and 100%; error rate over 5% for 5 minutes; p95 time to first token over a threshold per
      app; upstream down; log records dropped.
- [x] Shown as a banner and on an alerts page; optionally sent to Telegram (a bot token and chat id in `.env`, never
      logged: the URL holding the token is masked, as in BlackBox). Deduplicated, with a cooldown.

### 9.6 Dashboard
- [x] A budgets page with usage bars and reset times; queue depth and wait charts per class; circuit state; the
      alerts page.

## Tests
- [x] Rate limits (a real clock: the checks don't depend on elapsed time beyond the burst).
- [x] Budget accounting: hits don't count, restart restores counters, reset at a day boundary in a set time zone.
- [x] Scheduler with paused tokio time: priority order, aging, shedding at the wait limit, cancellation while
      queued, joined requests taking no slot (the last two through the gateway in `tests/limits.rs`).
- [x] Circuit breaker transitions, against a fake Ollama that is stopped and started.
- [x] Alert deduplication and cooldown; Telegram against a fake HTTP server.

## Done when
- [x] A load test with three apps of different priorities (fake Ollama with realistic generation times) shows
      interactive p95 queue wait under its target while batch requests are shed: 2.4 s at p95 against a 30 s limit,
      with 52 of 63 batch requests shed at their 10 s limit (2 requests a second on one slot, 90 s).
- [x] An exhausted budget returns 429 with the right reset time, and the dashboard shows it.
- [x] With Ollama stopped, requests fail fast within one probe interval, and recover when it starts. Opening takes
      `failures_to_open` failed connects (probes or requests); after that a request is refused in about 1 ms. The
      circuit closed about 2 s after Ollama came back.

## What was built (2026-10-26)

- `limits` (`rate`: governor per app; `budget`: day and month usage, a `Sink`; `scheduler`: the slot queue;
  `health`: the circuit; `cap`: the output cap; `AppLimits`, `Refusal`, `LimitNote`), `alerts` (rules, the
  fire/escalate/resolve state, `telegram`), `web::limits` with `limits.html`, `alerts.html` and the banner.
- The handler's order: rate limit (before the body is read), body, cache hit, join an existing flight, circuit and
  budget, lead a flight, output cap and the OpenAI usage rewrite, **wait for a slot**, circuit and budget again, the
  upstream call. A flight's leader waits for the slot; requests that join it share it.
- Per app settings, falling back to `[apps.default]`: `priority`, `requests_per_minute`, `burst`, `requests_per_day`,
  `tokens_per_day`, `tokens_per_month`, `gpu_seconds_per_day`, `max_output_tokens`. New sections: `[queue]`,
  `[budgets]`, `[health]`, `[alerts]`.
- `max_output_tokens`, an output cap added to generation requests that set none, came from phase 5's runaway rewrite
  (16,283 tokens). Capped answers end with `done_reason: length` and aren't cached (phase 5's cache rule).
- Migration 4: `budget_usage`; `priority`, `refusal` and `output_cap` on each exchange; queue waits and model loads
  in the rollups. Prometheus: `jetway_refused_total`, `jetway_queue_wait_seconds`, `jetway_queue_depth`,
  `jetway_upstream_slots_in_use`, `jetway_circuit_open`, `jetway_model_loads_total`. JSON: `/api/limits`,
  `/api/alerts`.
- reqwest now has its `rustls` feature (Telegram is HTTPS). It builds `aws-lc-sys`, which took about 40 s here.
- The latency Jetway adds didn't change measurably: 0.165 to 0.175 ms at p50 against 0.166 ms for the commit
  before, benchmarked in the same session.
- Found on the way:
  - **Exchanges cancelled before the upstream answered weren't recorded at all** (since phase 1): hyper drops the
    handler when its client leaves, so nothing reached the sinks. A queue makes that window minutes long. A guard
    now records them as `client_cancelled` with status 499.
  - **A flight whose leader was dropped before its upstream call kept its followers waiting forever.** The leader's
    `Lead` guard now fails the flight on drop, so joined requests get a 503 at once.
  - The health probe made the fake Ollama's request counts wrong in tests; probes now send
    `User-Agent: jetway-probe/<version>` (easy to spot in Ollama's log too), and the fake's `requests()` leaves
    them out.
  - reqwest's error text ("error sending request for url (...)") said nothing about why; alerts and the status now
    show the innermost cause, or "couldn't connect (timed out)" for Windows' slow refusal.

### Rust notes
- **Cancellation is a drop.** An async function stops wherever it is awaiting when its future is dropped, and no
  code after that `await` runs. Cleanup therefore lives in `Drop` impls: `Permit` frees its slot, `WaitGuard` leaves
  the queue (or hands on a slot granted to a waiter that's gone), `Lead` fails a flight, `CancelGuard` records the
  exchange ([limits/scheduler.rs](../../src/limits/scheduler.rs), [server/mod.rs](../../src/server/mod.rs)). Each has
  a way to disarm it on the normal path (`armed`, `Option::take`).
- **Handing over a slot with a oneshot channel.** A waiter parks on a `oneshot::Receiver<Grant>`; whoever frees a
  slot picks the best waiter and sends it a `Grant`. If the waiter has gone, `send` returns the grant, so the slot is
  still free. `Grant` has no `Drop` on purpose: only a `Permit`, made once the waiter receives the grant, frees a
  slot, so nothing is freed twice.
- **Tests on paused time.** `#[tokio::test(start_paused = true)]` freezes tokio's clock and jumps it forward
  whenever every task is waiting on a timer, so "wait 70 s, then check aging" runs in milliseconds and gives the same
  result every time.
- **Atomics for the hot path, a mutex for the details.** `Health::is_open` is one `AtomicBool` load per request;
  the version and error text sit behind a `Mutex` that only the probe and the dashboard touch. `Ordering::Relaxed`
  is enough because the flag is a hint, not a guard for other memory.
- **Calendar arithmetic with jiff.** `Zoned::start_of_day()`, `first_of_month()` and `checked_add(1.month())` give
  the period ends in the configured time zone, including months of different lengths and daylight-saving days,
  without hand-written date maths ([limits/budget.rs](../../src/limits/budget.rs)).
- **Cargo features are unified.** Turning on reqwest's `rustls-no-provider` (to use `ring` instead of aws-lc) would
  apply to every reqwest client in the build, tests included, and each would panic unless the program installs a
  crypto provider first. So Jetway takes the standard `rustls` feature.
- **No `set_var`.** In edition 2024 `std::env::set_var` is `unsafe` (other threads may read the environment), and
  this crate forbids unsafe code, so Telegram's settings are read from the environment or straight from `.env`
  rather than loaded into the environment ([alerts/telegram.rs](../../src/alerts/telegram.rs)).
- **Keeping a secret out of logs.** `Telegram` doesn't derive `Debug`, errors go through `reqwest::Error::without_url`
  (the URL holds the token), and any remaining text has the token replaced by `***`.
