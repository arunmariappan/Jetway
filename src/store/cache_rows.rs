//! SQL for the exact cache's entries.

use bytes::Bytes;
use rusqlite::{Connection, OptionalExtension, params};

use crate::cache::Entry;
use crate::cache::answer::Answer;

/// One cache entry as listed (`jetway cache list`, the dashboard).
#[derive(Debug, Clone, PartialEq)]
pub struct CacheRow {
    pub key: String,
    pub app: String,
    pub path: String,
    pub stream: bool,
    pub source: String,
    pub created_ms: i64,
    pub expires_ms: i64,
    pub size: i64,
    pub hits: i64,
    pub last_hit_ms: Option<i64>,
    /// The start of the answer, for display.
    pub preview: String,
    pub completion_tokens: Option<u64>,
}

pub fn put(connection: &Connection, key: &[u8; 32], entry: &Entry, compressed_raw: &[u8]) -> rusqlite::Result<()> {
    let answer = serde_json::to_string(&entry.answer).unwrap_or_else(|_| "{}".into());
    connection.execute(
        "INSERT OR REPLACE INTO cache_entries (key, app, path, stream, content_type, source, created_at, expires_at,
         size, hits, last_hit_at, answer, raw)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, NULL, ?10, ?11)",
        params![
            &key[..],
            entry.app,
            entry.path,
            entry.stream,
            entry.content_type,
            entry.source,
            entry.created_ms,
            entry.expires_ms,
            entry.raw.len() as i64,
            answer,
            compressed_raw
        ],
    )?;
    Ok(())
}

/// The entry with its raw bytes still compressed.
pub fn get(connection: &Connection, key: &[u8; 32], now_ms: i64) -> rusqlite::Result<Option<(Entry, Vec<u8>)>> {
    connection
        .query_row(
            "SELECT app, path, stream, content_type, source, created_at, expires_at, answer, raw
             FROM cache_entries WHERE key = ?1 AND expires_at > ?2",
            params![&key[..], now_ms],
            |row| {
                let answer: String = row.get(7)?;
                let entry = Entry {
                    app: row.get(0)?,
                    path: row.get(1)?,
                    stream: row.get(2)?,
                    content_type: row.get(3)?,
                    source: row.get(4)?,
                    created_ms: row.get(5)?,
                    expires_ms: row.get(6)?,
                    answer: serde_json::from_str::<Answer>(&answer).unwrap_or_default(),
                    raw: Bytes::new(),
                };
                Ok((entry, row.get::<_, Vec<u8>>(8)?))
            },
        )
        .optional()
}

pub fn list(connection: &Connection, app: Option<&str>, limit: usize, now_ms: i64) -> rusqlite::Result<Vec<CacheRow>> {
    let mut statement = connection.prepare(
        "SELECT key, app, path, stream, source, created_at, expires_at, size, hits, last_hit_at, answer
         FROM cache_entries WHERE expires_at > ?1 AND (?2 IS NULL OR app = ?2)
         ORDER BY hits DESC, created_at DESC LIMIT ?3",
    )?;
    let limit = i64::try_from(limit.clamp(1, 10_000)).unwrap_or(100);
    statement
        .query_map(params![now_ms, app, limit], |row| {
            let key: Vec<u8> = row.get(0)?;
            let answer: String = row.get(10)?;
            let answer = serde_json::from_str::<Answer>(&answer).unwrap_or_default();
            let mut preview: String = answer.content.chars().take(80).collect();
            if answer.content.chars().count() > 80 {
                preview.push('…');
            }
            Ok(CacheRow {
                key: key.iter().map(|b| format!("{b:02x}")).collect(),
                app: row.get(1)?,
                path: row.get(2)?,
                stream: row.get(3)?,
                source: row.get(4)?,
                created_ms: row.get(5)?,
                expires_ms: row.get(6)?,
                size: row.get(7)?,
                hits: row.get(8)?,
                last_hit_ms: row.get(9)?,
                preview,
                completion_tokens: answer.completion_tokens,
            })
        })?
        .collect()
}
