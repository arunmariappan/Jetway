# Phase 9: Limits, budgets and scheduling

**Goal:** each app gets its fair share of one GPU. Rate limits and budgets stop runaway clients, a priority queue
keeps interactive requests fast while batch work waits, requests that would wait too long are refused instead of
piling up, and a dead Ollama is reported at once instead of timing out request by request.

**Size:** M · **Depends on:** 3

## Tasks

### 9.1 Rate limits
- [ ] Per app `requests_per_minute` (and an optional burst) with `governor`'s GCRA limiter, checked first, before
      any parsing beyond the app name.
- [ ] Refused: 429 with `Retry-After`, in the client's error format.

### 9.2 Budgets (D13)
- [ ] Per app, over a day and a month: tokens (prompt, completion and thinking, upstream calls only), GPU-seconds
      (Ollama's `total_duration`) and requests. Cache hits and joined requests use none and count as saved.
- [ ] Counters persisted in SQLite (one row per app and period, updated each minute and at shutdown) and restored at
      startup.
- [ ] Checked before the upstream call: an app with nothing left is refused. A request that starts with budget left
      may overshoot it; this is documented rather than prevented, since an answer's length isn't known in advance.
- [ ] Refused: 429, for example `budget exceeded: paperpilot used 3,000,000 of 3,000,000 tokens today; resets at
      00:00`, with `Retry-After`. Periods reset in `budgets.time_zone` (default: the system's).

### 9.3 Scheduler (D14)
- [ ] `upstream.slots` requests at a time (the value spike S1 found). Others wait in a priority queue: `interactive`
      before `normal` before `batch`, first come first served within a class, with aging so batch work isn't starved
      forever.
- [ ] `max_queue_wait_seconds` per class; a request that would exceed it is refused with 503 and `Retry-After`
      (load shedding). A client that disconnects while queued leaves the queue without an upstream call.
- [ ] Joined requests (phase 3) and cache hits never take a slot.
- [ ] Queue wait is on the exchange, in `X-Jetway-Queue-Wait`, and on the dashboard.

### 9.4 Upstream health
- [ ] `GET /api/version` every 5 seconds. After 3 failed connects in a row the circuit opens: requests fail at once
      with 503 "Ollama is down" in the client's format; a probe every 5 seconds closes it again.
- [ ] Why it matters on this PC (found in phase 1): Windows reports a refused loopback connection only after about
      2 seconds of retries, so without the circuit every request to a stopped Ollama waits 2 s. Also try
      `TCP_MAXRTMS` on the upstream socket to shorten those retries, and keep whichever works.
- [ ] Cold starts: a `load_duration` over 1 second marks the exchange as a model load, shown on the dashboard.

### 9.5 Alerts
- [ ] Rules: budget at 80% and 100%; error rate over 5% for 5 minutes; p95 time to first token over a threshold per
      app; upstream down; log records dropped.
- [ ] Shown as a banner and on an alerts page; optionally sent to Telegram (a bot token and chat id in `.env`, never
      logged: the URL holding the token is masked, as in BlackBox). Deduplicated, with a cooldown.

### 9.6 Dashboard
- [ ] A budgets page with usage bars and reset times; queue depth and wait charts per class; circuit state; the
      alerts page.

## Tests
- [ ] Rate limits with a fake clock.
- [ ] Budget accounting: hits don't count, restart restores counters, reset at a day boundary in a set time zone.
- [ ] Scheduler with paused tokio time: priority order, aging, shedding at the wait limit, cancellation while
      queued, joined requests taking no slot.
- [ ] Circuit breaker transitions, against a fake Ollama that is stopped and started.
- [ ] Alert deduplication and cooldown; Telegram against a fake HTTP server.

## Done when
- [ ] A load test with three apps of different priorities (fake Ollama with realistic generation times) shows
      interactive p95 queue wait under its target while batch requests are shed.
- [ ] An exhausted budget returns 429 with the right reset time, and the dashboard shows it.
- [ ] With Ollama stopped, requests fail fast within one probe interval, and recover when it starts.
