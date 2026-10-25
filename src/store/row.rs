//! Rows of the request log and their SQL.

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::ListFilter;
use crate::proxy::record::Finished;

/// One exchange as stored (plan phase 2.2).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExchangeRow {
    pub id: String,
    pub started_at_ms: i64,
    pub app: String,
    pub protocol: String,
    pub method: String,
    pub path: String,
    pub class: String,
    pub model: Option<String>,
    pub model_sent: Option<String>,
    pub stream: Option<bool>,
    pub think: Option<String>,
    pub temperature: Option<f64>,
    pub seed: Option<i64>,
    pub max_tokens: Option<i64>,
    pub messages: Option<i64>,
    pub has_images: bool,
    pub has_tools: bool,
    pub has_format: bool,
    pub status: u16,
    pub outcome: String,
    pub error: Option<String>,
    pub request_bytes: i64,
    pub response_bytes: i64,
    pub upstream_sent_us: Option<i64>,
    pub headers_us: Option<i64>,
    pub first_chunk_us: Option<i64>,
    pub first_token_us: Option<i64>,
    pub first_thinking_us: Option<i64>,
    pub done_us: Option<i64>,
    pub queue_wait_us: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub cached_prompt_tokens: Option<i64>,
    pub load_ns: Option<i64>,
    pub prompt_eval_ns: Option<i64>,
    pub eval_ns: Option<i64>,
    pub total_ns: Option<i64>,
    pub done_reason: Option<String>,
    pub response_model: Option<String>,
    pub trace_id: Option<String>,
    pub parent_span_id: Option<String>,
    pub truncated: bool,
    pub incomplete: bool,
    /// JSON: what the cache did ([`crate::cache::CacheNote`]).
    pub cache: Option<String>,
}

fn us(d: Option<Duration>) -> Option<i64> {
    d.map(|d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

fn int(n: Option<u64>) -> Option<i64> {
    n.map(|n| i64::try_from(n).unwrap_or(i64::MAX))
}

/// The trace id and parent span id of a W3C `traceparent` (`00-<trace>-<span>-<flags>`).
pub fn parse_traceparent(value: &str) -> Option<(String, String)> {
    let mut parts = value.trim().split('-');
    let (_version, trace, span) = (parts.next()?, parts.next()?, parts.next()?);
    let hex = |s: &str, len| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit());
    (hex(trace, 32) && hex(span, 16)).then(|| (trace.to_ascii_lowercase(), span.to_ascii_lowercase()))
}

impl ExchangeRow {
    pub fn from_finished(f: &Finished) -> Self {
        let request = f.exchange.summary.as_ref();
        let response = &f.summary;
        let trace = f.exchange.traceparent.as_deref().and_then(parse_traceparent);
        let t = f.timings;
        Self {
            id: f.exchange.id.to_string(),
            started_at_ms: f.exchange.started_at.as_millisecond(),
            app: f.exchange.app.clone(),
            protocol: f.exchange.protocol.as_str().to_string(),
            method: f.exchange.method.to_string(),
            path: f.exchange.path.clone(),
            class: f.exchange.class.as_str().to_string(),
            model: request.and_then(|r| r.model.clone()),
            model_sent: request.and_then(|r| r.model.clone()),
            stream: request.map(|r| r.stream),
            think: request.and_then(|r| r.think.clone()),
            temperature: request.and_then(|r| r.temperature),
            seed: request.and_then(|r| r.seed),
            max_tokens: request.and_then(|r| r.max_tokens),
            messages: request.map(|r| r.messages as i64),
            has_images: request.is_some_and(|r| r.has_images),
            has_tools: request.is_some_and(|r| r.has_tools),
            has_format: request.is_some_and(|r| r.has_format),
            status: f.status,
            outcome: f.outcome.as_str().to_string(),
            error: f.error.clone().or_else(|| response.error.clone()),
            request_bytes: f.exchange.request.len() as i64,
            response_bytes: int(Some(f.response_bytes)).unwrap_or(0),
            upstream_sent_us: us(t.upstream_sent),
            headers_us: us(t.headers),
            first_chunk_us: us(t.first_chunk),
            first_token_us: us(t.first_token),
            first_thinking_us: us(t.first_thinking),
            done_us: us(t.done),
            queue_wait_us: None,
            prompt_tokens: int(response.prompt_tokens),
            completion_tokens: int(response.completion_tokens),
            cached_prompt_tokens: int(response.cached_prompt_tokens),
            load_ns: int(response.load_ns),
            prompt_eval_ns: int(response.prompt_eval_ns),
            eval_ns: int(response.eval_ns),
            total_ns: int(response.total_ns),
            done_reason: response.done_reason.clone(),
            response_model: response.model.clone(),
            trace_id: trace.as_ref().map(|(t, _)| t.clone()),
            parent_span_id: trace.map(|(_, s)| s),
            truncated: f.truncated,
            incomplete: f.incomplete,
            cache: f.exchange.cache.as_ref().map(crate::cache::CacheNote::to_json),
        }
    }
}

const COLUMNS: &str = "id, started_at, app, protocol, method, path, class, model, model_sent, stream, think, temperature,
    seed, max_tokens, messages, has_images, has_tools, has_format, status, outcome, error, request_bytes, response_bytes,
    upstream_sent_us, headers_us, first_chunk_us, first_token_us, first_thinking_us, done_us, queue_wait_us,
    prompt_tokens, completion_tokens, cached_prompt_tokens, load_ns, prompt_eval_ns, eval_ns, total_ns, done_reason,
    response_model, trace_id, parent_span_id, truncated, incomplete, cache";

pub fn insert(connection: &Connection, r: &ExchangeRow) -> rusqlite::Result<()> {
    connection.execute(
        &format!(
            "INSERT OR REPLACE INTO exchanges ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
             ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34,
             ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44)"
        ),
        params![
            r.id,
            r.started_at_ms,
            r.app,
            r.protocol,
            r.method,
            r.path,
            r.class,
            r.model,
            r.model_sent,
            r.stream,
            r.think,
            r.temperature,
            r.seed,
            r.max_tokens,
            r.messages,
            r.has_images,
            r.has_tools,
            r.has_format,
            r.status,
            r.outcome,
            r.error,
            r.request_bytes,
            r.response_bytes,
            r.upstream_sent_us,
            r.headers_us,
            r.first_chunk_us,
            r.first_token_us,
            r.first_thinking_us,
            r.done_us,
            r.queue_wait_us,
            r.prompt_tokens,
            r.completion_tokens,
            r.cached_prompt_tokens,
            r.load_ns,
            r.prompt_eval_ns,
            r.eval_ns,
            r.total_ns,
            r.done_reason,
            r.response_model,
            r.trace_id,
            r.parent_span_id,
            r.truncated,
            r.incomplete,
            r.cache,
        ],
    )?;
    Ok(())
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<ExchangeRow> {
    Ok(ExchangeRow {
        id: row.get(0)?,
        started_at_ms: row.get(1)?,
        app: row.get(2)?,
        protocol: row.get(3)?,
        method: row.get(4)?,
        path: row.get(5)?,
        class: row.get(6)?,
        model: row.get(7)?,
        model_sent: row.get(8)?,
        stream: row.get(9)?,
        think: row.get(10)?,
        temperature: row.get(11)?,
        seed: row.get(12)?,
        max_tokens: row.get(13)?,
        messages: row.get(14)?,
        has_images: row.get(15)?,
        has_tools: row.get(16)?,
        has_format: row.get(17)?,
        status: row.get(18)?,
        outcome: row.get(19)?,
        error: row.get(20)?,
        request_bytes: row.get(21)?,
        response_bytes: row.get(22)?,
        upstream_sent_us: row.get(23)?,
        headers_us: row.get(24)?,
        first_chunk_us: row.get(25)?,
        first_token_us: row.get(26)?,
        first_thinking_us: row.get(27)?,
        done_us: row.get(28)?,
        queue_wait_us: row.get(29)?,
        prompt_tokens: row.get(30)?,
        completion_tokens: row.get(31)?,
        cached_prompt_tokens: row.get(32)?,
        load_ns: row.get(33)?,
        prompt_eval_ns: row.get(34)?,
        eval_ns: row.get(35)?,
        total_ns: row.get(36)?,
        done_reason: row.get(37)?,
        response_model: row.get(38)?,
        trace_id: row.get(39)?,
        parent_span_id: row.get(40)?,
        truncated: row.get(41)?,
        incomplete: row.get(42)?,
        cache: row.get(43)?,
    })
}

pub fn list(connection: &Connection, filter: &ListFilter) -> rusqlite::Result<Vec<ExchangeRow>> {
    let limit = i64::try_from(filter.limit.clamp(1, 10_000)).unwrap_or(100);
    let mut rows = if let Some(after) = &filter.after {
        let sql = format!(
            "SELECT {COLUMNS} FROM exchanges WHERE id > ?1 AND (?2 IS NULL OR app = ?2) AND (?3 IS NULL OR outcome = ?3)
             ORDER BY id LIMIT ?4"
        );
        let mut statement = connection.prepare(&sql)?;
        statement
            .query_map(params![after, filter.app, filter.outcome, limit], from_row)?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        let sql = format!(
            "SELECT {COLUMNS} FROM exchanges WHERE (?1 IS NULL OR app = ?1) AND (?2 IS NULL OR outcome = ?2)
             ORDER BY id DESC LIMIT ?3"
        );
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement
            .query_map(params![filter.app, filter.outcome, limit], from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.reverse();
        rows
    };
    rows.shrink_to_fit();
    Ok(rows)
}

type CompressedBodies = (Vec<u8>, Vec<u8>);

pub fn get(connection: &Connection, id: &str) -> rusqlite::Result<Option<(ExchangeRow, Option<CompressedBodies>)>> {
    let row =
        connection.query_row(&format!("SELECT {COLUMNS} FROM exchanges WHERE id = ?1"), [id], from_row).optional()?;
    let Some(row) = row else { return Ok(None) };
    let bodies = connection
        .query_row("SELECT request, response FROM bodies WHERE exchange_id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(Some((row, bodies)))
}

/// One minute of one app and model (phase 2.4), with serialised histograms.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RollupRow {
    pub minute_ms: i64,
    pub app: String,
    pub model: String,
    pub requests: i64,
    pub ok: i64,
    pub upstream_errors: i64,
    pub cancelled: i64,
    pub timeouts: i64,
    pub rejected: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_hits: i64,
    pub cache_joined: i64,
    pub saved_tokens: i64,
    pub saved_gpu_ms: i64,
    pub latency: Vec<u8>,
    pub ttft: Vec<u8>,
    pub added: Vec<u8>,
    pub tokens_per_second: Vec<u8>,
}

pub fn upsert_rollup(connection: &Connection, r: &RollupRow) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT OR REPLACE INTO rollups_minute (minute, app, model, requests, ok, upstream_errors, cancelled, timeouts,
         rejected, prompt_tokens, completion_tokens, latency, ttft, added, tokens_per_second, cache_hits,
         cache_joined, saved_tokens, saved_gpu_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            r.minute_ms,
            r.app,
            r.model,
            r.requests,
            r.ok,
            r.upstream_errors,
            r.cancelled,
            r.timeouts,
            r.rejected,
            r.prompt_tokens,
            r.completion_tokens,
            r.latency,
            r.ttft,
            r.added,
            r.tokens_per_second,
            r.cache_hits,
            r.cache_joined,
            r.saved_tokens,
            r.saved_gpu_ms
        ],
    )?;
    Ok(())
}

pub fn rollups_since(connection: &Connection, since_ms: i64) -> rusqlite::Result<Vec<RollupRow>> {
    let mut statement = connection.prepare(
        "SELECT minute, app, model, requests, ok, upstream_errors, cancelled, timeouts, rejected, prompt_tokens,
         completion_tokens, latency, ttft, added, tokens_per_second, cache_hits, cache_joined, saved_tokens,
         saved_gpu_ms FROM rollups_minute WHERE minute >= ?1 ORDER BY minute",
    )?;
    statement
        .query_map([since_ms], |row| {
            Ok(RollupRow {
                minute_ms: row.get(0)?,
                app: row.get(1)?,
                model: row.get(2)?,
                requests: row.get(3)?,
                ok: row.get(4)?,
                upstream_errors: row.get(5)?,
                cancelled: row.get(6)?,
                timeouts: row.get(7)?,
                rejected: row.get(8)?,
                prompt_tokens: row.get(9)?,
                completion_tokens: row.get(10)?,
                latency: row.get(11)?,
                ttft: row.get(12)?,
                added: row.get(13)?,
                tokens_per_second: row.get(14)?,
                cache_hits: row.get(15)?,
                cache_joined: row.get(16)?,
                saved_tokens: row.get(17)?,
                saved_gpu_ms: row.get(18)?,
            })
        })?
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparents() {
        let parsed = parse_traceparent("00-0AF7651916CD43DD8448EB211C80319C-b7ad6b7169203331-01").unwrap();
        assert_eq!(parsed, ("0af7651916cd43dd8448eb211c80319c".into(), "b7ad6b7169203331".into()));
        assert!(parse_traceparent("00-xyz-b7ad6b7169203331-01").is_none());
        assert!(parse_traceparent("garbage").is_none());
    }
}
