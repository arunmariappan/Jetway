use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use jetway::app;
use jetway::config::Config;
use jetway::dev::fake_ollama;
use jetway::proxy::record::LogSink;
use jetway::server;
use jetway::store::{ExchangeRow, ListFilter, Store};
use tracing_subscriber::EnvFilter;

const DEFAULT_CONFIG: &str = "jetway.toml";

#[derive(Parser)]
#[command(name = "jetway", version, about = "An LLM gateway in front of Ollama")]
struct Cli {
    /// Configuration file. JETWAY__* environment variables override it (JETWAY__SERVER__PORT=9000).
    #[arg(long, global = true, default_value = DEFAULT_CONFIG)]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the gateway and the admin listener.
    Serve,
    /// Load and validate the configuration, then print the merged result.
    CheckConfig,
    /// Look at recorded exchanges.
    Requests {
        #[command(subcommand)]
        command: RequestsCommand,
    },
    /// Look at or clear the exact cache's stored entries.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Maintain the request log.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Run a fake Ollama that answers from recorded responses (for development and load tests).
    FakeOllama {
        #[arg(long, default_value_t = 8399)]
        port: u16,
        /// Default script as JSON, e.g. '{"ttfb_ms":200,"gap_ms":20}'; a request's x-fake-script header overrides it.
        #[arg(long)]
        script: Option<String>,
    },
}

#[derive(Subcommand)]
enum RequestsCommand {
    /// Print the latest exchanges, then (with --follow) new ones as they arrive.
    Tail {
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        outcome: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, short)]
        follow: bool,
    },
    /// Print one exchange in full.
    Show {
        id: String,
        /// Include the request and response bodies.
        #[arg(long)]
        bodies: bool,
    },
}

#[derive(Subcommand)]
enum CacheCommand {
    /// Entries, bytes and hits per app.
    Stats,
    /// The most hit entries.
    List {
        #[arg(long)]
        app: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Delete stored entries (a running Jetway keeps what it holds in memory until it expires or restarts).
    Clear {
        #[arg(long)]
        app: Option<String>,
    },
}

#[derive(Subcommand)]
enum DbCommand {
    /// Create or update the schema (serve does this too).
    Migrate,
    /// Delete exchanges older than a number of days, e.g. --older-than 7d.
    Prune {
        #[arg(long, value_parser = parse_days)]
        older_than: i64,
    },
}

fn parse_days(text: &str) -> Result<i64, String> {
    text.trim_end_matches('d').parse::<i64>().map_err(|_| format!("{text:?} isn't a number of days like 7d"))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    if let Command::FakeOllama { port, script } = &cli.command {
        return runtime()?.block_on(fake(*port, script.as_deref()));
    }
    // The default file may be absent (built-in defaults apply); a file named on the command line must exist.
    let required = cli.config.as_os_str() != DEFAULT_CONFIG;
    let config = Config::load(&cli.config, required)?;
    match cli.command {
        Command::CheckConfig => {
            print!("{}", toml::to_string_pretty(&config)?);
            Ok(())
        }
        Command::Serve => runtime()?.block_on(serve(config)),
        Command::Requests { command } => runtime()?.block_on(requests(config, command)),
        Command::Db { command } => runtime()?.block_on(db(config, command)),
        Command::Cache { command } => runtime()?.block_on(cache(config, command)),
        Command::FakeOllama { .. } => unreachable!("handled above"),
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().context("couldn't start the async runtime")
}

fn init_logging() {
    let filter = EnvFilter::try_from_env("JETWAY_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
    tracing_subscriber::fmt().with_env_filter(filter).with_ansi(ansi).compact().init();
}

async fn serve(config: Config) -> anyhow::Result<()> {
    init_logging();
    let (host, port, admin_port) = (config.server.host, config.server.port, config.server.admin_port);
    let gateway = server::bind(&config).await.with_context(|| format!("couldn't listen on {host}:{port}"))?;
    let admin = server::bind_admin(&config).await.with_context(|| format!("couldn't listen on {host}:{admin_port}"))?;
    let running = app::start(config, gateway, admin, vec![Arc::new(LogSink)]).await?;
    tracing::info!(
        "Jetway listening on http://{} (admin http://{}) and forwarding to {}; request log {}",
        running.gateway_addr,
        running.admin_addr,
        running.gateway.upstream_url(),
        running.store.path().display()
    );
    shutdown_signal().await;
    running.stop();
    running.wait().await?;
    Ok(())
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        // Without a signal handler, run until killed.
        std::future::pending::<()>().await;
    }
}

fn ms(us: Option<i64>) -> String {
    us.map_or_else(|| "-".into(), |us| format!("{:.0}", us as f64 / 1000.0))
}

fn opt<T: ToString>(value: &Option<T>) -> String {
    value.as_ref().map_or_else(|| "-".into(), ToString::to_string)
}

fn print_row(r: &ExchangeRow) {
    let time = jiff::Timestamp::from_millisecond(r.started_at_ms).map_or_else(|_| "?".into(), |t| t.to_string());
    println!(
        "{time}  {}  {:<10} {:<6} {:<24} {:>3} {:<16} ttft {:>6} ms  total {:>7} ms  tokens {}/{}  {}",
        r.id,
        r.app,
        r.method,
        r.path,
        r.status,
        r.outcome,
        ms(r.first_token_us),
        ms(r.done_us),
        opt(&r.prompt_tokens),
        opt(&r.completion_tokens),
        r.error.as_deref().unwrap_or("")
    );
}

async fn requests(config: Config, command: RequestsCommand) -> anyhow::Result<()> {
    let store = Store::open(&config.store.path)?;
    match command {
        RequestsCommand::Tail { app, outcome, limit, follow } => {
            let mut filter = ListFilter { app, outcome, after: None, limit };
            loop {
                let rows = store.list(filter.clone()).await?;
                for row in &rows {
                    print_row(row);
                }
                if let Some(last) = rows.last() {
                    filter.after = Some(last.id.clone());
                } else if filter.after.is_none() {
                    // An empty log: follow from the start.
                    filter.after = Some(String::new());
                }
                if !follow {
                    return Ok(());
                }
                filter.limit = 1000;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        RequestsCommand::Show { id, bodies } => {
            let Some((row, stored)) = store.get(id.clone()).await? else {
                anyhow::bail!("no exchange {id}");
            };
            println!("{row:#?}");
            if bodies {
                match stored {
                    Some(b) => {
                        println!("--- request ({} bytes)\n{}", b.request.len(), String::from_utf8_lossy(&b.request));
                        println!("--- response ({} bytes)\n{}", b.response.len(), String::from_utf8_lossy(&b.response));
                    }
                    None => println!("(no bodies stored; see store.bodies)"),
                }
            }
            Ok(())
        }
    }
}

async fn cache(config: Config, command: CacheCommand) -> anyhow::Result<()> {
    let store = Store::open(&config.store.path)?;
    let now = jiff::Timestamp::now().as_millisecond();
    match command {
        CacheCommand::Stats => {
            let stats = store.cache_stats(now).await?;
            if stats.is_empty() {
                println!("no cache entries");
            }
            for s in stats {
                println!("{:<16} {:>7} entries  {:>10} bytes  {:>7} hits", s.app, s.entries, s.bytes, s.hits);
            }
        }
        CacheCommand::List { app, limit } => {
            for row in store.cache_list(app, limit, now).await? {
                let stream = if row.stream { "stream" } else { "whole" };
                println!(
                    "{}  {:<12} {:<22} {:<6} {:>5} hits  {:>8} bytes  {}",
                    &row.key[..16],
                    row.app,
                    row.path,
                    stream,
                    row.hits,
                    row.size,
                    row.preview
                );
            }
        }
        CacheCommand::Clear { app } => {
            let deleted = store.cache_clear(app).await?;
            println!("deleted {deleted} cache entries");
        }
    }
    Ok(())
}

async fn db(config: Config, command: DbCommand) -> anyhow::Result<()> {
    match command {
        DbCommand::Migrate => {
            let version = Store::migrate_only(&config.store.path)?;
            println!("{} is at schema version {version}", config.store.path.display());
        }
        DbCommand::Prune { older_than } => {
            let store = Store::open(&config.store.path)?;
            let before = jiff::Timestamp::now().as_millisecond() - older_than * 24 * 3600 * 1000;
            let (exchanges, _) = store.prune(before, i64::MIN).await?;
            println!("deleted {exchanges} exchanges older than {older_than} days");
        }
    }
    Ok(())
}

async fn fake(port: u16, script: Option<&str>) -> anyhow::Result<()> {
    init_logging();
    let script = match script {
        Some(json) => serde_json::from_str(json).context("--script isn't a valid script")?,
        None => fake_ollama::Script::default(),
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let fake = fake_ollama::spawn(listener, script)?;
    tracing::info!("fake Ollama listening on {}", fake.url());
    shutdown_signal().await;
    Ok(())
}
