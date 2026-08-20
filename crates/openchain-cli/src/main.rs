mod abi;
mod decode;
mod follow;
mod sql;
mod sync;

use clap::{Parser, Subcommand};
use eyre::Result;
use openchain_core::{Config, Dataset};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "openchain", version, about = "Open-source, self-hostable onchain analytics engine")]
struct Cli {
    /// Path to the config file
    #[arg(long, global = true, default_value = "openchain.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a default config file and create the ClickHouse schema
    Init {
        /// Overwrite an existing config file
        #[arg(long)]
        force: bool,
    },
    /// Backfill a block range into ClickHouse (resumable)
    Sync {
        /// Chain id (must exist in config, e.g. 1 for Ethereum)
        #[arg(long)]
        chain: u64,
        /// First block to sync; defaults to resuming after the last watermark
        #[arg(long)]
        from: Option<u64>,
        /// Last block to sync, or "latest" (default)
        #[arg(long)]
        to: Option<String>,
        /// Comma-separated datasets: blocks, transactions, logs
        #[arg(long, default_value = "blocks,transactions,logs")]
        datasets: String,
        /// Blocks per ClickHouse insert batch
        #[arg(long, default_value_t = 10)]
        chunk_size: u64,
        /// Concurrent chunk fetches in flight
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
    },
    /// Tail the chain head live with reorg handling
    Follow {
        /// Chain id (must exist in config)
        #[arg(long)]
        chain: u64,
        /// Comma-separated datasets: blocks, transactions, logs
        #[arg(long, default_value = "blocks,transactions,logs")]
        datasets: String,
        /// Seconds between head polls
        #[arg(long, default_value_t = 3)]
        poll_interval: u64,
        /// Reorg-safe hot window size in blocks
        #[arg(long, default_value_t = 64)]
        hot_window: u64,
    },
    /// Manage contract ABIs used for event decoding
    Abi {
        #[command(subcommand)]
        action: AbiAction,
    },
    /// Decode raw logs into decoded_events using registered ABIs (incremental)
    Decode {
        /// Chain id (must exist in config)
        #[arg(long)]
        chain: u64,
        /// Blocks per decode batch
        #[arg(long, default_value_t = 1000)]
        batch_blocks: u64,
    },
    /// Run a SQL query against the OpenChain database
    Sql {
        /// The SQL query to run
        query: String,
        /// ClickHouse output format (PrettyCompact, JSONEachRow, CSVWithNames, ...)
        #[arg(long, default_value = "PrettyCompact")]
        format: String,
    },
}

#[derive(Subcommand)]
enum AbiAction {
    /// Fetch a verified ABI from Sourcify (or load from --file) and register it
    Add {
        /// Contract address (0x...)
        address: String,
        /// Chain id
        #[arg(long)]
        chain: u64,
        /// Override the contract name
        #[arg(long)]
        name: Option<String>,
        /// Load the ABI from a local JSON file instead of Sourcify
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// List registered ABIs for a chain
    List {
        /// Chain id
        #[arg(long)]
        chain: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,openchain=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Init { force } => init(&cli.config, force).await,
        Command::Sync { chain, from, to, datasets, chunk_size, concurrency } => {
            let config = Config::load(&cli.config)?;
            let datasets = Dataset::parse_list(&datasets)?;
            sync::run(&config, chain, from, to, &datasets, chunk_size, concurrency).await
        }
        Command::Follow { chain, datasets, poll_interval, hot_window } => {
            let config = Config::load(&cli.config)?;
            let datasets = Dataset::parse_list(&datasets)?;
            follow::run(&config, chain, &datasets, poll_interval, hot_window).await
        }
        Command::Abi { action } => {
            let config = Config::load(&cli.config)?;
            match action {
                AbiAction::Add { address, chain, name, file } => {
                    abi::add(&config, chain, &address, name, file).await
                }
                AbiAction::List { chain } => abi::list(&config, chain).await,
            }
        }
        Command::Decode { chain, batch_blocks } => {
            let config = Config::load(&cli.config)?;
            decode::run(&config, chain, batch_blocks).await
        }
        Command::Sql { query, format } => {
            let config = Config::load(&cli.config)?;
            sql::run(&config, &query, &format).await
        }
    }
}

async fn init(path: &PathBuf, force: bool) -> Result<()> {
    if !path.exists() || force {
        let template = toml::to_string_pretty(&Config::default_template())?;
        std::fs::write(path, template)?;
        println!("wrote {}", path.display());
    } else {
        println!("{} already exists (use --force to overwrite)", path.display());
    }
    let config = Config::load(path)?;
    let sink = openchain_sink::Sink::new(&config.clickhouse);
    sink.ensure_schema().await?;
    println!("ClickHouse schema ready in database '{}'", config.clickhouse.database);
    Ok(())
}
