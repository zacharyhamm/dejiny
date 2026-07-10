pub use dejiny::{db, format};
mod blacklist;
mod config;
mod import;
mod init;
mod record;
mod replay;
mod search;
mod store;
mod summarize;
mod sync;
mod terminal;
mod util;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "dejiny", about = "Shell history manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Print shell hook script to stdout
    Init {
        /// Shell to generate hooks for (zsh or bash)
        shell: String,
    },
    /// Store a command in history
    Store {
        #[arg(long)]
        command: String,
        #[arg(long)]
        exit_code: i32,
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: String,
        #[arg(long)]
        cwd: String,
    },
    /// Fuzzy search command history
    Search {
        /// Initial search query (pre-populated from shell input)
        query: Option<String>,
    },
    /// Record a command's terminal session
    Record {
        /// Command to run (passed to $SHELL -c)
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Replay a recorded session
    Replay {
        /// Command history ID (defaults to most recent recording)
        id: Option<i64>,
        /// Playback speed multiplier
        #[arg(long, default_value = "1.0")]
        speed: f64,
        /// Output plain text with metadata instead of interactive replay
        #[arg(long)]
        text: bool,
    },
    /// Import history from existing shell history files
    Import {
        /// Path to zsh history file (default: ~/.zsh_history)
        #[arg(long)]
        zsh: Option<std::path::PathBuf>,
        /// Path to bash history file (default: ~/.bash_history)
        #[arg(long)]
        bash: Option<std::path::PathBuf>,
        /// Show what would be imported without writing to the database
        #[arg(long)]
        dry_run: bool,
    },
    #[command(hide = true)]
    Summarize { id: i64 },
    /// Manage summary blacklist patterns
    Blacklist {
        #[command(subcommand)]
        action: BlacklistAction,
    },
    /// Synchronize history with configured peer nodes
    Sync {
        #[command(subcommand)]
        action: SyncAction,
    },
}

#[derive(Subcommand)]
enum SyncAction {
    /// Listen for history from peer nodes
    Listen {
        /// Run in the background, managed via `dejiny sync stop`
        #[arg(long)]
        daemon: bool,
    },
    /// Stop the backgrounded sync daemon
    Stop,
    /// Push pending outbox entries to all peers
    Flush {
        /// Respect the retry backoff (used by automatic background flushes)
        #[arg(long, hide = true)]
        auto: bool,
    },
    /// Show daemon state and per-node outbox depth
    Status,
    /// Generate a preshared key for config.toml
    Keygen,
}

#[derive(Subcommand)]
enum BlacklistAction {
    /// Add a regex pattern to skip summarization for matching commands
    Add {
        /// Regex pattern to match against shell commands
        pattern: String,
    },
    /// Remove a pattern from the blacklist
    Remove {
        /// The exact pattern string to remove
        pattern: String,
    },
    /// List all blacklist patterns
    List,
}

fn init_logging() {
    use simplelog::{ConfigBuilder, LevelFilter, WriteLogger};
    let log_path = db::history_path().join("debug.log");
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let config = ConfigBuilder::new().set_time_format_rfc3339().build();
        let _ = WriteLogger::init(LevelFilter::Debug, config, file);
    }
}

fn main() {
    init_logging();
    let cli = Cli::parse();
    match cli.command {
        Commands::Init { shell } => init::init(&shell),
        Commands::Store {
            command,
            exit_code,
            start,
            end,
            cwd,
        } => store::store(&command, exit_code, &start, &end, &cwd),
        Commands::Search { query } => search::search(query),
        Commands::Record { command } => record::record(&command),
        Commands::Import { zsh, bash, dry_run } => import::import(zsh, bash, dry_run),
        Commands::Replay { id, speed, text } => replay::replay(id, speed, text),
        Commands::Summarize { id } => summarize::summarize(id),
        Commands::Blacklist { action } => match action {
            BlacklistAction::Add { pattern } => blacklist::add(&pattern),
            BlacklistAction::Remove { pattern } => blacklist::remove(&pattern),
            BlacklistAction::List => blacklist::list(),
        },
        Commands::Sync { action } => match action {
            SyncAction::Listen { daemon } => sync::listen_cmd(daemon),
            SyncAction::Stop => sync::stop_cmd(),
            SyncAction::Flush { auto } => sync::flush_cmd(auto),
            SyncAction::Status => sync::status_cmd(),
            SyncAction::Keygen => sync::keygen_cmd(),
        },
    }
}
