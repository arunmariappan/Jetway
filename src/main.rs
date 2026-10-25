use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use jetway::config::Config;

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
    /// Load and validate the configuration, then print the merged result.
    CheckConfig,
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
    // The default file may be absent (built-in defaults apply); a file named on the command line must exist.
    let required = cli.config.as_os_str() != DEFAULT_CONFIG;
    let config = Config::load(&cli.config, required)?;
    match cli.command {
        Command::CheckConfig => {
            print!("{}", toml::to_string_pretty(&config)?);
            Ok(())
        }
    }
}
