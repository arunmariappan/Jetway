# Phase 4: Dashboard and admin API

**Goal:** a live view of everything going through Jetway: latency, time to first token, throughput, tokens, cache
hits, errors, apps and each exchange in detail, served by Jetway itself on the admin port with no JavaScript written
and no internet needed. **The MVP is complete at the end of this phase.**

**Size:** M · **Depends on:** 2, 3

## Tasks

### 4.1 Admin server (D7)
- [ ] An axum router on `127.0.0.1:8301`: pages, the admin JSON API and `/metrics`.
- [ ] Askama templates in `templates/` (checked at compile time), one base layout, light and dark themes from
      `prefers-color-scheme` through CSS variables.
- [ ] Assets embedded with `rust-embed`: `htmx.min.js` and the `htmx-ext-sse` extension (vendored, pinned versions,
      licences kept), one stylesheet. Served with the right content type, an ETag and a long cache lifetime.

### 4.2 Pages
- [ ] **Overview:** tiles for requests per minute, p50 and p95 latency, p50 time to first token, output tokens per
      second, cache hit rate, in-flight requests, upstream status with Ollama's version, Jetway's memory, dropped log
      records. Charts for latency percentiles, TTFT, throughput and outcomes, over 15 minutes, 1 hour or 24 hours.
      Refreshed every 5 seconds.
- [ ] **Live feed:** an SSE stream of exchanges as they finish, plus in-flight rows with their elapsed time; filters
      by app and outcome.
- [ ] **Exchange detail:** a timing waterfall (received → queued → sent upstream → first byte → first token → done),
      request and response bodies (pretty-printed, collapsed by default), cache result (with a link to the source of
      a hit or the exchange a request joined), later guard verdicts and route, trace id.
- [ ] **Apps:** a table per app (requests, tokens, hit rate, errors, p95); each app's page has its own charts.
- [ ] **Cache:** entry counts and bytes per app, most-hit entries, saved tokens and GPU time; clear buttons that ask
      for confirmation (`hx-confirm`) and use POST.

### 4.3 Charts
- [ ] `plotters` with its SVG backend, drawn on the server from the rollups: lines for percentiles, stacked bars for
      outcomes and cache results. Colours come from CSS classes, so the theme changes without redrawing.
- [ ] Charts are swapped in by HTMX on the refresh tick; the live feed uses the SSE extension.

### 4.4 Admin JSON API
- [ ] `GET /api/overview?window=1h`, `/api/exchanges?app=&outcome=&before=&limit=`, `/api/exchanges/{id}`,
      `/api/apps`, `/api/cache`; `POST /api/cache/clear`. The pages and the API use the same queries.

### 4.5 Demo mode
- [ ] `jetway fake-ollama` plus `jetway load --rate 5/s --mix fixtures` fill the dashboard without the GPU, for UI
      work and README screenshots.

## Tests
- [ ] Every page and API endpoint returns 200 with its key content, from a store seeded with known exchanges.
- [ ] The SSE feed sends an event when an exchange finishes.
- [ ] Chart SVGs are well-formed (snapshot of one chart from fixed data).
- [ ] Assets are served with the right types; no template contains an `http://` or `https://` URL (the dashboard
      must work offline).
- [ ] Clear buttons require POST; a GET changes nothing.

## Done when
- [ ] With the fake Ollama and the load generator running, the dashboard shows live numbers that agree with
      `/api/overview` and `/metrics`.
- [ ] A screenshot of the overview page is in the README.
- [ ] **MVP complete:** a pass-through proxy with streaming, an exact-match cache and a latency dashboard.
