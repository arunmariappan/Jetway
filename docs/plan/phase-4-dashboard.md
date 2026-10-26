# Phase 4: Dashboard and admin API

**Goal:** a live view of everything going through Jetway: latency, time to first token, throughput, tokens, cache
hits, errors, apps and each exchange in detail, served by Jetway itself on the admin port with no JavaScript written
and no internet needed. **The MVP is complete at the end of this phase.**

**Size:** M · **Depends on:** 2, 3 · **Status:** done (2026-10-25)

## Tasks

### 4.1 Admin server (D7)
- [x] One axum router on `127.0.0.1:8301` (`web::router`): pages, fragments, the live feed, the admin JSON API,
      assets, `/metrics` and `/healthz`.
- [x] Askama 0.16 templates in `templates/` (checked at compile time) with `askama_web` for axum; one base layout;
      light and dark themes from `prefers-color-scheme`, with `data-theme` overriding either way.
- [x] Assets embedded with `rust-embed`: `htmx.min.js` 2.0.11 and `htmx-ext-sse` 2.2.4 (vendored, both 0BSD, licence
      files and provenance in `assets/vendor/`), `jetway.css`. Served with the right content type, a SHA-256 ETag
      (304 on a match) and `Cache-Control: no-cache`, so a browser revalidates and picks up a new build at once (an
      hour's `max-age` kept old CSS after an upgrade); the vendored notes aren't served.

### 4.2 Pages
- [x] **Overview:** stat tiles (requests and per minute, latency p50 with p95, first token p50, output tokens per
      second, cache hit rate with saved tokens and GPU time, error rate, in progress with Jetway's memory) and four
      charts (latency p50/p95/p99, first token p50/p95, requests by outcome, cache results), over 15 minutes, an
      hour or a day (15-minute slots), for all apps or one. The body refreshes every 5 seconds (htmx), keeping the
      old render dimmed while it loads.
- [x] **Live:** finished exchanges arrive as table rows over server-sent events (newest first, filtered by app),
      and the in-progress list refreshes every second.
- [x] **Exchange detail:** request and response fields, a timing waterfall (Jetway → waiting for the upstream →
      first token → rest of the answer) with every offset and Ollama's own durations, the cache note (with a link
      to the source of a hit), and both bodies (JSON pretty-printed, at most 64 KB shown).
- [x] **Apps:** a table per app over the window, each linking to the overview filtered to that app.
- [x] **Cache:** entries and bytes in memory, stored entries and hits, flights; per-app stored entries with clear
      buttons (POST with an htmx confirmation; a plain form post works too); the most-hit entries with links to their
      sources.

### 4.3 Charts
- [x] **Changed from the plan:** hand-written SVG (`web::charts`) instead of `plotters`, whose SVG backend writes
      fixed colour attributes; with CSS classes the theme switches without redrawing, and there's no dependency.
- [x] Built to the data-viz method: 2px lines with round joins and 8px end dots ringed in the surface colour;
      columns at most 24px wide with a 4px rounded top, square at the baseline, a 2px surface gap between stacked
      segments; solid hairline grids; clean tick values; direct end labels only where they don't collide (the
      legend and tooltip carry the rest); a legend whenever there are two or more series.
- [x] Colours by job, validated with the palette checker in both modes: percentiles are ordinal (one blue,
      light → dark), cache results the first three categorical slots, errors the critical status colour, and
      cancelled-or-refused the de-emphasis grey (it fails only the chroma floor, which is the point of a grey).
      Light-mode aqua sits at 2.74:1, so the table view is the required relief.
- [x] Hover without JavaScript: each time slot has a full-height hit area with a native tooltip listing every
      series and a CSS crosshair on hover or keyboard focus; every chart has a table view.

### 4.4 Admin JSON API
- [x] `GET /api/overview?window=&app=` (tiles, rows, gauges, upstream status), `/api/exchanges?app=&outcome=&before=&limit=`,
      `/api/exchanges/{id}` (with bodies), `/api/apps?window=`, `/api/cache`; `POST /api/cache/clear` (`{"app": ...}`
      optional). Clearing empties the memory cache and deletes the stored entries of that app (or all); other
      apps' entries come back from the store on their next lookup.

### 4.5 Demo mode
- [x] `jetway load --rate 6 --duration 150 --fake` spreads traffic over three demo apps, both APIs, streamed and
      whole answers, with a share of repeated questions (cache hits) and, with `--fake`, scripted timings plus 2%
      errors and 1% dropped streams. Phase 10 turns it into the load tester.

## Tests
- [x] Every page and API route returns 200 with its key content from real traffic through the full stack
      (`tests/dashboard.rs`); an unknown exchange is a 404.
- [x] The SSE feed sends an `exchange` event when an exchange finishes, honouring the app filter.
- [x] Chart SVGs are well-formed, split lines at gaps, round only the top segment, and have a tooltip per slot
      (unit tests in `web::charts`).
- [x] Assets are served with the right types and ETags; no template has an `http://` or `https://` URL, and every
      asset a template references is embedded.
- [x] Clearing needs a POST (GET is 405); a plain form post gets a 303, an htmx one an `HX-Redirect`.
- [x] Rollups restored at startup match what was written; restored minutes aren't written again.

## Done when
- [x] With the fake Ollama and the load generator running, the dashboard shows live numbers that agree with
      `/api/overview` and `/metrics` (checked 2026-10-25 with 900 demo requests, then a restart that brought them
      back).
- [x] A screenshot of the overview page is in the README.
- [x] **MVP complete:** a pass-through proxy with streaming, an exact-match cache and a latency dashboard.

## What was built (2026-10-25)

- `web` (`mod`: routes and handlers; `pages`: page models and askama templates; `views`: buckets, tiles and rows
  from the rollups; `charts`: SVG; `feed`: the live broadcast; `assets`; `api`), `templates/` (8 files),
  `assets/jetway.css` and the vendored htmx, `proxy::inflight` (the in-progress registry replacing phase 1's
  counter), `Admin` state with a cached upstream probe, `dev::load` and `jetway load`.
- Found while looking at the first screenshots: three charts per row shrank their text to about 7px, so charts sit
  two per row; hits showed "0.0 ms", so sub-millisecond times keep two decimals.
- The dashboard reads the in-memory rollups, so `serve` now restores the last day of rollups from SQLite at startup
  (migration 3 adds the cacheable count they need for the hit rate).

### Rust notes
- **Compile-time templates.** `#[derive(Template)]` with `#[template(path = "overview.html")]` makes askama turn the
  template into Rust code at build time, so a typo in a field name is a compile error, not a blank page
  ([web/pages.rs](../../src/web/pages.rs)). Values are HTML-escaped unless a template says `|safe` (used only for
  SVG and fragments Jetway rendered itself).
- **Embedding files.** `#[derive(Embed)] #[folder = "assets/"]` (rust-embed) puts the files into the binary in
  release builds and reads them from disk in debug builds, with a SHA-256 per file for the ETag
  ([web/assets.rs](../../src/web/assets.rs)).
- **Streams from channels.** The live feed turns a `broadcast::Receiver` into a `Stream` with
  `futures::stream::unfold`, carrying the receiver and filter as state between items; a lagging page skips what it
  missed (`RecvError::Lagged`) instead of failing ([web/mod.rs](../../src/web/mod.rs)).
- **Generic extractors.** Handler arguments like `Query<Params>`, `Path<String>`, `Form<ClearForm>` and
  `State<Arc<Admin>>` are types axum knows how to build from a request; adding one to a function's signature is all
  it takes.
- **Writing into strings.** The charts build SVG with `write!(out, ...)` on a `String` (`std::fmt::Write`), which
  appends without allocating a temporary per element; the result is ignored with `let _ =` because writing to a
  `String` can't fail.
- **`dyn Fn` for small variations.** `overview_body` passes `&dyn Fn(&Row) -> Vec<String>` to one table-building
  closure, so four chart tables share one loop without a generic function per table.
