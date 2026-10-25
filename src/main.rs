use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use jetway::config::Config;
use jetway::dev::fake_ollama;
use jetway::proxy::record::LogSink;
use jetway::server::{self, Gateway};
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
    /// Run the gateway.
    Serve,
    /// Load and validate the configuration, then print the merged result.
    CheckConfig,
    /// Run a fake Ollama that answers from recorded responses (for development and load tests).
    FakeOllama {
        #[arg(long, default_value_t = 8399)]
        port: u16,
        /// Default script as JSON, e.g. '{"ttfb_ms":200,"gap_ms":20}'; a request's x-fake-script header overrides it.
        #[arg(long)]
        script: Option<String>,
    },
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
        Command::FakeOllama { .. } => unreachable!("handled above"),
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("couldn't start the async runtime")
}

fn init_logging() {
    let filter = EnvFilter::try_from_env("JETWAY_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ansi)
        .compact()
        .init();
}

async fn serve(config: Config) -> anyhow::Result<()> {
    init_logging();
    let listener = server::bind(&config)
        .await
        .with_context(|| format!("couldn't listen on {}:{}", config.server.host, config.server.port))?;
    let gateway = Gateway::new(config, Arc::new(LogSink)).context("couldn't build the upstream client")?;
    tracing::info!(
        "Jetway listening on http://{} and forwarding to {}",
        listener.local_addr()?,
        gateway.upstream_url()
    );
    server::run(listener, gateway, shutdown_signal()).await?;
    Ok(())
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        // Without a signal handler, run until killed.
        std::future::pending::<()>().await;
    }
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
