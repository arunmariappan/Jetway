//! The request log (plan D8, phase 2.3): one SQLite file in WAL mode. A single writer thread owns the only write
//! connection and takes work from a bounded channel in batches; the request path never waits for it. When the
//! channel is full, the record is dropped and counted. Reads use a small pool of read-only connections on the
//! blocking thread pool.

mod row;
mod schema;
pub mod sink;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use bytes::Bytes;
use rusqlite::{Connection, OpenFlags, params};
use tokio::sync::{mpsc, oneshot};

pub use row::{ExchangeRow, RollupRow};

const CHANNEL_OPS: usize = 4096;
const BATCH: usize = 200;
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("couldn't create {}: {source}", .path.display())]
    Directory { path: PathBuf, source: std::io::Error },
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("the store's writer has stopped")]
    WriterGone,
    #[error("a blocking read task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("a stored body couldn't be decompressed: {0}")]
    Decompress(std::io::Error),
}

/// Request and response bodies as sent; compressed by the writer.
#[derive(Debug, Clone)]
pub struct Bodies {
    pub request: Bytes,
    pub response: Bytes,
}

enum WriteOp {
    Exchange(Box<ExchangeRow>, Option<Bodies>),
    Rollups(Vec<RollupRow>),
    Prune { exchanges_before_ms: i64, rollups_before_ms: i64, done: oneshot::Sender<(usize, usize)> },
    Flush(oneshot::Sender<()>),
}

/// Filters for listing exchanges.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub app: Option<String>,
    pub outcome: Option<String>,
    /// Only exchanges after this id (ULIDs sort by time), oldest first; without it, the latest, oldest first.
    pub after: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Default)]
pub struct Counters {
    /// Records dropped because the writer's queue was full.
    pub dropped: AtomicU64,
    /// Batches whose transaction failed.
    pub write_errors: AtomicU64,
}

struct ReadPool {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
}

impl ReadPool {
    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, StoreError> {
        let taken = self.idle.lock().unwrap_or_else(PoisonError::into_inner).pop();
        let connection = match taken {
            Some(c) => c,
            None => {
                let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
                let c = Connection::open_with_flags(&self.path, flags)?;
                c.busy_timeout(std::time::Duration::from_secs(5))?;
                c
            }
        };
        let result = f(&connection);
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < 8 {
            idle.push(connection);
        }
        Ok(result?)
    }
}

/// The request log. Cheap to share behind an `Arc`.
pub struct Store {
    tx: mpsc::Sender<WriteOp>,
    readers: Arc<ReadPool>,
    pub counters: Arc<Counters>,
    path: PathBuf,
    writer: Mutex<Option<JoinHandle<()>>>,
}

fn open_writer(path: &Path) -> Result<Connection, StoreError> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|source| StoreError::Directory { path: dir.to_path_buf(), source })?;
    }
    let mut connection = Connection::open(path)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    schema::migrate(&mut connection)?;
    Ok(connection)
}

impl Store {
    /// Opens (creating and migrating) the database and starts the writer thread.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::open_with_capacity(path, CHANNEL_OPS)
    }

    /// [`Store::open`] with a given queue size (tests use a small one to show dropping).
    pub fn open_with_capacity(path: &Path, capacity: usize) -> Result<Self, StoreError> {
        let connection = open_writer(path)?;
        let (tx, rx) = mpsc::channel(capacity.max(1));
        let counters = Arc::new(Counters::default());
        let writer_counters = Arc::clone(&counters);
        let writer = std::thread::Builder::new()
            .name("jetway-store-writer".into())
            .spawn(move || write_loop(connection, rx, &writer_counters))
            .map_err(|source| StoreError::Directory { path: path.to_path_buf(), source })?;
        Ok(Self {
            tx,
            readers: Arc::new(ReadPool { path: path.to_path_buf(), idle: Mutex::new(Vec::new()) }),
            counters,
            path: path.to_path_buf(),
            writer: Mutex::new(Some(writer)),
        })
    }

    /// Runs the migrations without starting a writer (`jetway db migrate`). Returns the schema version.
    pub fn migrate_only(path: &Path) -> Result<usize, StoreError> {
        let connection = open_writer(path)?;
        Ok(schema::version(&connection)?)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queues one exchange for writing. Never waits: a full queue drops the record and counts it.
    pub fn record(&self, row: ExchangeRow, bodies: Option<Bodies>) {
        if self.tx.try_send(WriteOp::Exchange(Box::new(row), bodies)).is_err() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn write_rollups(&self, rows: Vec<RollupRow>) {
        if !rows.is_empty() && self.tx.try_send(WriteOp::Rollups(rows)).is_err() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Waits until everything queued so far is written.
    pub async fn flush(&self) -> Result<(), StoreError> {
        let (done, wait) = oneshot::channel();
        self.tx.send(WriteOp::Flush(done)).await.map_err(|_| StoreError::WriterGone)?;
        wait.await.map_err(|_| StoreError::WriterGone)
    }

    /// Deletes exchanges (with their bodies) and rollups older than the given times. Returns the counts deleted.
    pub async fn prune(&self, exchanges_before_ms: i64, rollups_before_ms: i64) -> Result<(usize, usize), StoreError> {
        let (done, wait) = oneshot::channel();
        let op = WriteOp::Prune { exchanges_before_ms, rollups_before_ms, done };
        self.tx.send(op).await.map_err(|_| StoreError::WriterGone)?;
        wait.await.map_err(|_| StoreError::WriterGone)
    }

    /// Runs a read on the blocking pool with a read-only connection.
    pub async fn read<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let readers = Arc::clone(&self.readers);
        tokio::task::spawn_blocking(move || readers.with(f)).await?
    }

    pub async fn list(&self, filter: ListFilter) -> Result<Vec<ExchangeRow>, StoreError> {
        self.read(move |c| row::list(c, &filter)).await
    }

    /// One exchange and its bodies (decompressed), if stored.
    pub async fn get(&self, id: String) -> Result<Option<(ExchangeRow, Option<Bodies>)>, StoreError> {
        let found = self.read(move |c| row::get(c, &id)).await?;
        let Some((row, bodies)) = found else { return Ok(None) };
        let bodies = match bodies {
            Some((request, response)) => Some(Bodies {
                request: Bytes::from(zstd::decode_all(&request[..]).map_err(StoreError::Decompress)?),
                response: Bytes::from(zstd::decode_all(&response[..]).map_err(StoreError::Decompress)?),
            }),
            None => None,
        };
        Ok(Some((row, bodies)))
    }

    pub async fn count(&self) -> Result<u64, StoreError> {
        let count: i64 = self.read(|c| c.query_row("SELECT COUNT(*) FROM exchanges", [], |r| r.get(0))).await?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub async fn rollups_since(&self, since_ms: i64) -> Result<Vec<RollupRow>, StoreError> {
        self.read(move |c| row::rollups_since(c, since_ms)).await
    }
}

impl Drop for Store {
    /// Closing the channel ends the writer after it has written what was queued.
    fn drop(&mut self) {
        let (tx, _) = mpsc::channel(1);
        drop(std::mem::replace(&mut self.tx, tx));
        if let Some(writer) = self.writer.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = writer.join();
        }
    }
}

fn write_loop(mut connection: Connection, mut rx: mpsc::Receiver<WriteOp>, counters: &Counters) {
    while let Some(first) = rx.blocking_recv() {
        let mut batch = vec![first];
        while batch.len() < BATCH {
            match rx.try_recv() {
                Ok(op) => batch.push(op),
                Err(_) => break,
            }
        }
        if let Err(error) = apply(&mut connection, batch) {
            counters.write_errors.fetch_add(1, Ordering::Relaxed);
            tracing::error!(%error, "couldn't write to the request log");
        }
    }
}

fn compress(bytes: &[u8]) -> Vec<u8> {
    zstd::encode_all(bytes, ZSTD_LEVEL).unwrap_or_default()
}

fn apply(connection: &mut Connection, batch: Vec<WriteOp>) -> rusqlite::Result<()> {
    let mut replies = Vec::new();
    let tx = connection.transaction()?;
    for op in batch {
        match op {
            WriteOp::Exchange(row, bodies) => {
                row::insert(&tx, &row)?;
                if let Some(bodies) = bodies {
                    tx.execute(
                        "INSERT OR REPLACE INTO bodies (exchange_id, request, response, request_len, response_len)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            row.id,
                            compress(&bodies.request),
                            compress(&bodies.response),
                            bodies.request.len() as i64,
                            bodies.response.len() as i64
                        ],
                    )?;
                }
            }
            WriteOp::Rollups(rows) => {
                for rollup in rows {
                    row::upsert_rollup(&tx, &rollup)?;
                }
            }
            WriteOp::Prune { exchanges_before_ms, rollups_before_ms, done } => {
                let exchanges = tx.execute("DELETE FROM exchanges WHERE started_at < ?1", [exchanges_before_ms])?;
                let rollups = tx.execute("DELETE FROM rollups_minute WHERE minute < ?1", [rollups_before_ms])?;
                replies.push(Reply::Prune(done, (exchanges, rollups)));
            }
            WriteOp::Flush(done) => replies.push(Reply::Flush(done)),
        }
    }
    tx.commit()?;
    for reply in replies {
        match reply {
            Reply::Prune(done, counts) => {
                let _ = done.send(counts);
            }
            Reply::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
    Ok(())
}

enum Reply {
    Prune(oneshot::Sender<(usize, usize)>, (usize, usize)),
    Flush(oneshot::Sender<()>),
}
