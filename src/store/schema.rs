//! Schema migrations: an ordered list of SQL scripts, with SQLite's `user_version` as the applied count.

use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[V1, V2, V3, V4];

/// The schema version a fully migrated database has.
pub const LATEST: usize = MIGRATIONS.len();

const V1: &str = r"
CREATE TABLE exchanges (
    id                    TEXT PRIMARY KEY,      -- ULID, sorts by time
    started_at            INTEGER NOT NULL,      -- epoch milliseconds, UTC
    app                   TEXT NOT NULL,
    protocol              TEXT NOT NULL,         -- ollama | openai
    method                TEXT NOT NULL,
    path                  TEXT NOT NULL,         -- after the /a/{app} prefix, with the query
    class                 TEXT NOT NULL,         -- generation | embedding | info | model_management | unknown
    model                 TEXT,                  -- as requested
    model_sent            TEXT,                  -- after aliases and routing (phase 8)
    stream                INTEGER,
    think                 TEXT,
    temperature           REAL,
    seed                  INTEGER,
    max_tokens            INTEGER,
    messages              INTEGER,
    has_images            INTEGER NOT NULL DEFAULT 0,
    has_tools             INTEGER NOT NULL DEFAULT 0,
    has_format            INTEGER NOT NULL DEFAULT 0,
    status                INTEGER NOT NULL,
    outcome               TEXT NOT NULL,
    error                 TEXT,
    request_bytes         INTEGER NOT NULL,
    response_bytes        INTEGER NOT NULL,
    upstream_sent_us      INTEGER,               -- offsets from receipt, microseconds
    headers_us            INTEGER,
    first_chunk_us        INTEGER,
    first_token_us        INTEGER,
    first_thinking_us     INTEGER,
    done_us               INTEGER,
    queue_wait_us         INTEGER,               -- phase 9
    prompt_tokens         INTEGER,
    completion_tokens     INTEGER,               -- Ollama's eval_count, which includes thinking tokens
    cached_prompt_tokens  INTEGER,
    load_ns               INTEGER,               -- Ollama's durations, nanoseconds
    prompt_eval_ns        INTEGER,
    eval_ns               INTEGER,
    total_ns              INTEGER,
    done_reason           TEXT,
    response_model        TEXT,
    trace_id              TEXT,
    parent_span_id        TEXT,
    truncated             INTEGER NOT NULL DEFAULT 0,
    incomplete            INTEGER NOT NULL DEFAULT 0,
    cache                 TEXT,                  -- JSON, phase 3
    guards                TEXT,                  -- JSON, phase 7
    route                 TEXT                   -- JSON, phase 8
);
CREATE INDEX exchanges_started ON exchanges (started_at);
CREATE INDEX exchanges_app_started ON exchanges (app, started_at);

CREATE TABLE bodies (
    exchange_id   TEXT PRIMARY KEY REFERENCES exchanges (id) ON DELETE CASCADE,
    request       BLOB NOT NULL,                 -- zstd
    response      BLOB NOT NULL,                 -- zstd
    request_len   INTEGER NOT NULL,
    response_len  INTEGER NOT NULL
);

CREATE TABLE rollups_minute (
    minute             INTEGER NOT NULL,          -- epoch milliseconds at the start of the minute
    app                TEXT NOT NULL,
    model              TEXT NOT NULL,
    requests           INTEGER NOT NULL,
    ok                 INTEGER NOT NULL,
    upstream_errors    INTEGER NOT NULL,
    cancelled          INTEGER NOT NULL,
    timeouts           INTEGER NOT NULL,
    rejected           INTEGER NOT NULL,
    prompt_tokens      INTEGER NOT NULL,
    completion_tokens  INTEGER NOT NULL,
    latency            BLOB NOT NULL,             -- hdrhistogram V2, microseconds
    ttft               BLOB NOT NULL,             -- time to first token, microseconds
    added              BLOB NOT NULL,             -- Jetway's own time before the upstream call, microseconds
    tokens_per_second  BLOB NOT NULL,             -- output tokens per second, x100
    PRIMARY KEY (minute, app, model)
);

CREATE TABLE meta (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);
";

/// Phase 3: the exact cache, and cache counts in the rollups.
const V2: &str = r"
CREATE TABLE cache_entries (
    key           BLOB PRIMARY KEY,            -- blake3 of the canonical request
    app           TEXT NOT NULL,
    path          TEXT NOT NULL,
    stream        INTEGER NOT NULL,            -- whether raw is a stream
    content_type  TEXT NOT NULL,
    source        TEXT NOT NULL,               -- the exchange that produced the answer
    created_at    INTEGER NOT NULL,            -- epoch milliseconds
    expires_at    INTEGER NOT NULL,
    size          INTEGER NOT NULL,            -- raw bytes
    hits          INTEGER NOT NULL DEFAULT 0,
    last_hit_at   INTEGER,
    answer        TEXT NOT NULL,               -- JSON: the protocol-neutral answer
    raw           BLOB NOT NULL                -- zstd: the bytes the first client received
);
CREATE INDEX cache_entries_expires ON cache_entries (expires_at);
CREATE INDEX cache_entries_app ON cache_entries (app);

ALTER TABLE rollups_minute ADD COLUMN cache_hits INTEGER NOT NULL DEFAULT 0;
ALTER TABLE rollups_minute ADD COLUMN cache_joined INTEGER NOT NULL DEFAULT 0;
ALTER TABLE rollups_minute ADD COLUMN saved_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE rollups_minute ADD COLUMN saved_gpu_ms INTEGER NOT NULL DEFAULT 0;
";

/// Phase 4: the cacheable count, so restored rollups give the dashboard its cache hit rate.
const V3: &str = r"
ALTER TABLE rollups_minute ADD COLUMN cacheable INTEGER NOT NULL DEFAULT 0;
";

/// Phase 9: budget usage, what the limits did to each exchange, and queue waits and model loads in the rollups.
const V4: &str = r"
CREATE TABLE budget_usage (
    app         TEXT NOT NULL,
    period      TEXT NOT NULL,                 -- day | month
    key         TEXT NOT NULL,                 -- 2026-10-26 or 2026-10, in budgets.time_zone
    requests    INTEGER NOT NULL,
    tokens      INTEGER NOT NULL,
    gpu_ms      INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,              -- epoch milliseconds
    PRIMARY KEY (app, period, key)
);

ALTER TABLE exchanges ADD COLUMN priority TEXT;      -- interactive | normal | batch
ALTER TABLE exchanges ADD COLUMN refusal TEXT;       -- rate_limit | budget | queue | circuit_open
ALTER TABLE exchanges ADD COLUMN output_cap INTEGER; -- the output cap Jetway added

ALTER TABLE rollups_minute ADD COLUMN queue_wait BLOB;  -- hdrhistogram V2, microseconds, upstream calls only
ALTER TABLE rollups_minute ADD COLUMN model_loads INTEGER NOT NULL DEFAULT 0;
";

pub fn version(connection: &Connection) -> rusqlite::Result<usize> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    Ok(usize::try_from(version).unwrap_or(0))
}

/// Applies the migrations not yet applied, each in its own transaction.
pub fn migrate(connection: &mut Connection) -> rusqlite::Result<usize> {
    let applied = version(connection)?;
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
        let tx = connection.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (index + 1) as i64)?;
        tx.commit()?;
    }
    version(connection)
}
