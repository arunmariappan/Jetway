//! The exact cache and joined requests (plan D9, D14, phase 3). Entries live in memory (`moka`, bounded by bytes)
//! and in SQLite, so a restart keeps them: a memory miss falls back to an indexed read. The cache is also a sink: it
//! stores the answer of every cacheable exchange that ends well.

pub mod answer;
pub mod flight;
pub mod key;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use serde::Serialize;
use serde_json::Value;

use crate::config::{AppCacheConfig, CacheConfig, Config, Replay, SampledPolicy};
use crate::protocol::{Framing, Protocol};
use crate::proxy::record::{Exchange, Finished, Outcome, Sink};
use crate::proxy::usage::UsageFilter;
use crate::server::routes::PathClass;
use crate::store::Store;
use answer::Answer;
pub use answer::Entry;
use flight::Flights;
pub use key::CacheKey;

pub const CACHE_HEADER: &str = "x-jetway-cache";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheResult {
    /// Served from the cache.
    Hit,
    /// Looked up, not found; the answer is stored if it ends well.
    Miss,
    /// The request asked to skip the cache (`Cache-Control`, `X-Jetway-Cache: off`).
    Bypass,
    /// Served from an identical request in flight.
    Joined,
    /// Not cacheable: the cache is off for the app, the body isn't JSON, or the request is sampled and the app skips
    /// sampled requests.
    Off,
}

impl CacheResult {
    pub const ALL: [CacheResult; 5] = [Self::Hit, Self::Miss, Self::Bypass, Self::Joined, Self::Off];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Bypass => "bypass",
            Self::Joined => "joined",
            Self::Off => "off",
        }
    }
}

/// What the cache did for one exchange; stored as JSON in the exchange's `cache` column.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CacheNote {
    pub result: CacheResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// For a hit, the exchange that produced the answer; for a joined request, the one that made the upstream call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_s: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_completion_tokens: Option<u64>,
    /// Ollama's `total_duration` of the answer a hit replaced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_gpu_ms: Option<u64>,
    #[serde(skip)]
    pub key: Option<CacheKey>,
    /// Store the answer if the exchange ends well.
    #[serde(skip)]
    pub store: bool,
}

impl CacheNote {
    fn new(result: CacheResult, reason: Option<&'static str>) -> Self {
        Self {
            result,
            reason,
            source: None,
            age_s: None,
            saved_prompt_tokens: None,
            saved_completion_tokens: None,
            saved_gpu_ms: None,
            key: None,
            store: false,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// The cache's decision for a request.
#[derive(Debug, Clone)]
pub struct Decision {
    pub note: CacheNote,
    /// Look the key up before going upstream.
    pub lookup: bool,
    /// The key for joining identical requests in flight.
    pub join: Option<CacheKey>,
}

/// The settings that apply to one app.
#[derive(Debug, Clone, Copy)]
struct Policy {
    exact: bool,
    sampled: SampledPolicy,
    replay: Replay,
    join: bool,
}

pub struct Cache {
    config: CacheConfig,
    apps: BTreeMap<String, AppCacheConfig>,
    memory: moka::sync::Cache<CacheKey, Arc<Entry>>,
    store: Option<Arc<Store>>,
    pub flights: Flights,
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn header_has(headers: &HeaderMap, name: &str, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|part| part.trim().eq_ignore_ascii_case(token))
}

fn content_type(framing: Framing) -> &'static str {
    match framing {
        Framing::Ndjson => "application/x-ndjson",
        Framing::Sse => "text/event-stream",
        Framing::Whole => "application/json; charset=utf-8",
    }
}

impl Cache {
    pub fn new(config: &Config, store: Option<Arc<Store>>) -> Self {
        let memory = moka::sync::Cache::builder()
            .max_capacity(config.cache.max_memory_bytes)
            .weigher(|_key: &CacheKey, entry: &Arc<Entry>| entry.weight())
            .time_to_live(Duration::from_secs(config.cache.ttl_hours * 3600))
            .build();
        let apps = config.apps.iter().map(|(name, app)| (name.clone(), app.cache.clone())).collect();
        Self { config: config.cache.clone(), apps, memory, store, flights: Flights::default() }
    }

    fn policy(&self, app: &str) -> Policy {
        let o = self.apps.get(app).cloned().unwrap_or_default();
        let c = &self.config;
        Policy {
            exact: o.exact.unwrap_or(c.exact),
            sampled: o.sampled.unwrap_or(c.sampled),
            replay: o.replay.unwrap_or(c.replay),
            join: o.join.unwrap_or(c.join),
        }
    }

    pub fn replay(&self, app: &str) -> Replay {
        self.policy(app).replay
    }

    /// Decides what the cache does for a request; `None` for requests the cache never looks at (model info, model
    /// management). `body` is the parsed request body.
    pub fn decide(&self, exchange: &Exchange, body: Option<&Value>, headers: &HeaderMap) -> Option<Decision> {
        if !matches!(exchange.class, PathClass::Generation | PathClass::Embedding) {
            return None;
        }
        let off =
            |reason| Some(Decision { note: CacheNote::new(CacheResult::Off, Some(reason)), lookup: false, join: None });
        let policy = self.policy(&exchange.app);
        let (Some(body), Some(summary)) = (body, exchange.summary.as_ref()) else { return off("the body isn't JSON") };
        if !policy.exact {
            return off("the cache is off for this app");
        }
        let sampled = summary.seed.is_none() && summary.temperature != Some(0.0);
        if sampled && policy.sampled == SampledPolicy::Skip {
            return off("sampled requests are skipped for this app");
        }

        let path = exchange.path.split('?').next().unwrap_or(&exchange.path);
        let scope = (!self.config.shared).then_some(exchange.app.as_str());
        let exact = key::exact_key(scope, path, body);
        let mut note = CacheNote::new(CacheResult::Miss, None);
        note.key = Some(exact);
        note.store = true;

        if header_has(headers, CACHE_HEADER, "off") || header_has(headers, "cache-control", "no-store") {
            note.result = CacheResult::Bypass;
            note.reason = Some("the request asked not to use the cache");
            note.store = false;
            return Some(Decision { note, lookup: false, join: None });
        }
        if header_has(headers, "cache-control", "no-cache") {
            note.result = CacheResult::Bypass;
            note.reason = Some("the request asked for a fresh answer");
            return Some(Decision { note, lookup: false, join: None });
        }
        let join = policy.join.then(|| key::join_key(&exact, summary.stream, summary.include_usage));
        Some(Decision { note, lookup: true, join })
    }

    /// The entry for `key`, from memory or the store, if it hasn't expired.
    pub async fn lookup(&self, key: &CacheKey) -> Option<Arc<Entry>> {
        let now = now_ms();
        if let Some(entry) = self.memory.get(key) {
            if entry.expires_ms > now {
                return Some(entry);
            }
            self.memory.invalidate(key);
            return None;
        }
        let store = self.store.as_ref()?;
        let bytes = key.0;
        let found = store.cache_get(bytes, now).await.ok().flatten()?;
        let entry = Arc::new(found);
        self.memory.insert(*key, Arc::clone(&entry));
        Some(entry)
    }

    /// Notes a hit on the entry (counted in the store).
    pub fn hit(&self, key: &CacheKey) {
        if let Some(store) = &self.store {
            store.cache_hit(key.0, now_ms());
        }
    }

    /// Entries and bytes held in memory.
    pub fn memory_usage(&self) -> (u64, u64) {
        self.memory.run_pending_tasks();
        (self.memory.entry_count(), self.memory.weighted_size())
    }

    /// Forgets every entry in memory (the store is cleared separately).
    pub fn clear_memory(&self) {
        self.memory.invalidate_all();
    }

    fn entry_for(&self, f: &Finished) -> Option<Entry> {
        let summary = f.exchange.summary.as_ref()?;
        let complete = f.outcome == Outcome::Ok
            && f.status == 200
            && f.summary.done
            && f.summary.error.is_none()
            && !f.truncated
            && !f.incomplete;
        // The response's framing must match what the request asked for.
        let stream = f.framing.is_stream();
        let wanted = summary.stream && f.exchange.class == PathClass::Generation;
        if !complete || stream != wanted {
            return None;
        }
        let mut raw = f.response.clone();
        if f.exchange.protocol == Protocol::OpenAi && f.framing == Framing::Sse {
            // Stored without a usage event; one is added for clients that ask (plan D21).
            let mut filter = UsageFilter::default();
            let filtered = filter.push(raw);
            let mut bytes = filtered.sent.to_vec();
            bytes.extend_from_slice(&filter.finish());
            raw = bytes.into();
        }
        let now = now_ms();
        let ttl_ms = i64::try_from(self.config.ttl_hours.saturating_mul(3_600_000)).unwrap_or(i64::MAX);
        Some(Entry {
            app: f.exchange.app.clone(),
            path: f.exchange.path.split('?').next().unwrap_or(&f.exchange.path).to_string(),
            stream,
            content_type: content_type(f.framing).into(),
            raw,
            answer: Answer::from_summary(&f.summary),
            source: f.exchange.id.to_string(),
            created_ms: now,
            expires_ms: now.saturating_add(ttl_ms),
        })
    }
}

impl Sink for Cache {
    fn finished(&self, f: Finished) {
        let Some(note) = &f.exchange.cache else { return };
        let Some(key) = note.key else { return };
        if !note.store || !matches!(note.result, CacheResult::Miss | CacheResult::Bypass) {
            return;
        }
        if let Some(entry) = self.entry_for(&f) {
            if let Some(store) = &self.store {
                store.cache_put(key.0, entry.clone());
            }
            self.memory.insert(key, Arc::new(entry));
        }
    }
}

/// The note for a hit on `entry`.
pub fn hit_note(key: CacheKey, entry: &Entry) -> CacheNote {
    let mut note = CacheNote::new(CacheResult::Hit, None);
    note.key = Some(key);
    note.source = Some(entry.source.clone());
    note.age_s = Some((now_ms() - entry.created_ms).max(0) / 1000);
    note.saved_prompt_tokens = entry.answer.prompt_tokens;
    note.saved_completion_tokens = entry.answer.completion_tokens;
    note.saved_gpu_ms = entry.answer.total_ns.map(|ns| ns / 1_000_000);
    note
}

/// The note for a request that joined `leader`'s flight.
pub fn joined_note(key: Option<CacheKey>, leader: &ulid::Ulid) -> CacheNote {
    let mut note = CacheNote::new(CacheResult::Joined, None);
    note.key = key;
    note.source = Some(leader.to_string());
    note
}
