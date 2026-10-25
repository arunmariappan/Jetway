//! Test harness: a fake Ollama and a gateway in-process on free ports, and a sink that collects exchange records.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use jetway::config::Config;
use jetway::dev::fake_ollama::{self, FakeOllama, Script};
use jetway::proxy::record::{Finished, Sink};
use jetway::server::{self, Gateway};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

#[derive(Clone, Default)]
pub struct Collect(Arc<Mutex<Vec<Finished>>>);

impl Sink for Collect {
    fn finished(&self, finished: Finished) {
        self.0.lock().unwrap().push(finished);
    }
}

impl Collect {
    pub fn all(&self) -> Vec<Finished> {
        self.0.lock().unwrap().clone()
    }

    /// Waits until at least `n` exchanges have finished.
    pub async fn wait_for(&self, n: usize) -> Vec<Finished> {
        for _ in 0..400 {
            let all = self.all();
            if all.len() >= n {
                return all;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("expected {n} finished exchanges, got {:?}", self.all());
    }
}

pub struct Harness {
    pub fake: FakeOllama,
    pub gateway: Arc<Gateway>,
    pub url: String,
    pub records: Collect,
    pub client: reqwest::Client,
    stop: Option<oneshot::Sender<()>>,
    pub server: Option<JoinHandle<std::io::Result<()>>>,
}

impl Harness {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    pub async fn start_with(edit: impl FnOnce(&mut Config)) -> Self {
        let fake = fake_ollama::spawn_local(Script::default()).await.unwrap();
        let mut config = Config::default();
        config.upstream.url = fake.url();
        config.server.shutdown_grace_seconds = 2;
        // Pass-through tests send identical requests on purpose; cache tests turn the cache on.
        config.cache.exact = false;
        edit(&mut config);
        let records = Collect::default();
        let gateway = Gateway::new(config, Arc::new(records.clone()), None).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(server::run(listener, Arc::clone(&gateway), async move {
            let _ = stopped.await;
        }));
        Self { fake, gateway, url, records, client: client(), stop: Some(stop), server: Some(server) }
    }

    /// Asks the gateway to shut down (it then lets in-flight requests finish).
    pub fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    pub fn post(&self, path: &str, body: serde_json::Value) -> reqwest::RequestBuilder {
        self.client.post(format!("{}{path}", self.url)).json(&body)
    }

    pub fn direct(&self, path: &str, body: serde_json::Value) -> reqwest::RequestBuilder {
        self.client.post(format!("{}{path}", self.fake.url())).json(&body)
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().tcp_nodelay(true).build().unwrap()
}

pub fn script(json: &str) -> (&'static str, String) {
    (fake_ollama::SCRIPT_HEADER, json.to_string())
}

pub fn chat(stream: bool) -> serde_json::Value {
    serde_json::json!({
        "model": "qwen3.5:4b",
        "messages": [{"role": "user", "content": "Say hello in five words."}],
        "stream": stream,
    })
}

/// The whole of `jetway serve` (store, metrics, admin listener) against a fake Ollama, with a temporary database.
pub struct Stack {
    pub fake: FakeOllama,
    pub running: Option<jetway::app::Running>,
    pub url: String,
    pub admin_url: String,
    pub records: Collect,
    pub client: reqwest::Client,
    pub dir: tempfile::TempDir,
}

impl Stack {
    pub async fn start_with(edit: impl FnOnce(&mut Config)) -> Self {
        let fake = fake_ollama::spawn_local(Script::default()).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.upstream.url = fake.url();
        config.server.shutdown_grace_seconds = 2;
        config.store.path = dir.path().join("jetway.db");
        config.cache.exact = false;
        edit(&mut config);
        let records = Collect::default();
        let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let admin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let running = jetway::app::start(config, gateway, admin, vec![Arc::new(records.clone())]).await.unwrap();
        let url = format!("http://{}", running.gateway_addr);
        let admin_url = format!("http://{}", running.admin_addr);
        Self { fake, running: Some(running), url, admin_url, records, client: client(), dir }
    }

    pub fn running(&self) -> &jetway::app::Running {
        self.running.as_ref().unwrap()
    }

    pub fn post(&self, path: &str, body: serde_json::Value) -> reqwest::RequestBuilder {
        self.client.post(format!("{}{path}", self.url)).json(&body)
    }

    /// Waits for `n` finished exchanges and for the store to have written them.
    pub async fn settle(&self, n: usize) -> Vec<Finished> {
        let finished = self.records.wait_for(n).await;
        self.running().store.flush().await.unwrap();
        finished
    }
}
