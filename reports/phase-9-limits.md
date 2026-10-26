# Phase 9: limits, budgets and the queue, measured

Measured on 2026-10-26 on the development PC (Ryzen 7 3700X, 16 GB RAM, Windows 11) against the **fake Ollama**:
no GPU was used (GPU work is paused after the PC shut down during phase 5). Jetway was a debug build for the load
test and a release build for the overhead benchmark.

## Three apps, one slot

`jetway load --rate 2 --duration 90 --repeat 0 --fake` against `jetway serve` with `upstream.slots = 1`, the cache
off (so every request needs the slot), and three apps:

| App | Priority | Longest wait |
|---|---|---|
| `demo-chat` | interactive | 30 s |
| `demo-agent` | normal | 120 s |
| `demo-batch` | batch | 10 s |

The fake answers each request with a scripted first-byte delay of 40–600 ms and 5–40 chunks 5–30 ms apart, about
0.7 s a request, so one slot serves about 1.4 requests a second. Two a second is a steady overload, as when a batch
job runs alongside people using an app.

| Class | Requests | Went upstream | Shed (503) | Queue wait p50 | p95 | Max |
|---|---|---|---|---|---|---|
| interactive | 66 | 66 | 0 | 0.45 s | 2.38 s | 3.07 s |
| normal | 52 | 52 | 0 | 3.22 s | 7.50 s | 8.89 s |
| batch | 63 | 11 | 52 | 7.43 s | 9.76 s | 9.76 s |

From the request log (`queue_wait_us` and `refusal` per exchange); `/metrics` agreed (`jetway_refused_total
{app="demo-batch",reason="queue"} 52`). Two of the upstream calls got the fake's scripted 500s.

- **Interactive requests stay quick under overload.** Their wait is mostly the request already running: a slot is
  never taken back, so an interactive request can still wait out one whole answer.
- **Batch work gives way.** 52 of its 63 requests reached their 10 s limit and were refused with `Retry-After: 30`,
  instead of the queue growing without end. With a longer batch limit, aging (one class per 30 s waited) would let
  them through between bursts.
- **Normal requests take what is left.** Their p95 of 7.5 s is the cost of serving interactive work first.

![The Limits page after the run: tiles for the upstream, slots, waiting requests, queue wait p95, model loads and refusals; a stacked chart of requests waiting per class each minute and a line chart of queue wait p95 per class; the budgets table with a usage bar; per-app priorities and limits; refusals by reason](../docs/images/limits.png)

The "36 model loads" are real in the fake's data: its recorded streaming chat answer came from a cold start
(`load_duration` 10.1 s), so every streamed chat counts as one.

## Ollama going away and coming back

`jetway serve` was started with the fake Ollama stopped (probe every 5 s; the circuit opens after 3 failed
connects), and the fake was started again later on the same port. Times from Jetway's log:

| Time (UTC) | What happened |
|---|---|
| 06:00:07 | Jetway starts; nothing answers on the upstream port. Until the circuit opens, a request waits for the connect to fail: **2.0 s** on Windows (502). |
| 06:00:22 | First alert check: `[warning] ollama is not answering (timed out)`. |
| 06:00:23 | Third failed probe (each takes 2 s to give up): **circuit open**. |
| 06:00:37 | Next alert check: escalated to `[critical] ollama is down; requests fail fast` (sent again: escalation is news). A request now gets **503 in 1.3 ms** (measured in the first run) with `Retry-After: 5`, in the client's format; the status line says "ollama is down · failing fast". |
| 06:00:56.76 | The fake starts. |
| 06:00:56.86 | A probe gets an answer: **circuit closed** 0.1 s later (at most one probe interval). Requests succeed again. |
| 06:01:07 | Next alert check: `[resolved] ollama is down`. |

The same sequence in a first run, with the fake stopped under a running Jetway, gave the same results. Between the
two runs Jetway was restarted, and budget usage came back from SQLite (280 tokens used by `demo-batch`).
(The log above shows "timed out"; the message now reads "couldn't connect (timed out)".)

## Overhead

`cargo bench --bench overhead` (release, the whole of `jetway serve`, fake Ollama with no delays, concurrency 1).
Phase 9 adds to every metered request: the app's limits, the rate-limit and budget checks, a slot from the
scheduler, a copy of the exchange for the cancellation guard and the health update.

To tell phase 9's cost from the state of the machine, the commit before it (`ae608c3`) was benchmarked in a
separate worktree in the same session, then phase 9 twice:

| Added by Jetway, per request | Before phase 9 | Phase 9, run 1 | Phase 9, run 2 | Target |
|---|---|---|---|---|
| p50 | 0.166 ms | 0.165 ms | 0.175 ms | < 0.3 ms |
| p99 | 0.262 ms | 0.266 ms | 0.283 ms | < 1 ms |
| Cache hit, p50 (whole round trip) | 0.173 ms | 0.208 ms | 0.183 ms | |

The difference is within the run-to-run spread (the per-request work is a few map lookups, two short mutex holds and
a copy of the exchange). Every number is higher than in phase 1 (0.095 ms added at p50) because the machine was
busier: talking to the fake directly took 0.166 ms at p50 instead of 0.108 ms.
