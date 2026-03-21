pub use dejiny::{db, format};
mod blacklist;
mod import;
mod init;
mod record;
mod replay;
mod search;
mod store;
mod summarize;
mod terminal;
mod util;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::generate;

const NAME: &str = "dejiny";

#[derive(Parser)]
#[command(name = NAME, about = "Shell history manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, clap::ValueEnum)]
enum Shell {
    Elvish,
    Fish,
    Nushell,
    PowerShell,
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
        #[arg(long, conflicts_with = "input")]
        text: bool,
        /// Output recorded input keystrokes as plain text
        #[arg(long, conflicts_with = "text")]
        input: bool,
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
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
    #[command(hide = true)]
    Summarize { id: i64 },
    /// Manage summary blacklist patterns
    Blacklist {
        #[command(subcommand)]
        action: BlacklistAction,
    },
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
        Commands::Replay { id, speed, text, input } => replay::replay(id, speed, text, input),
        Commands::Completions { shell } => {
            let mut command = Cli::command();
            let stdout = &mut std::io::stdout();
            match shell {
                Shell::Elvish => generate(clap_complete::Shell::Elvish, &mut command, NAME, stdout),
                Shell::Fish => generate(clap_complete::Shell::Fish, &mut command, NAME, stdout),
                Shell::Nushell => {
                    generate(clap_complete_nushell::Nushell, &mut command, NAME, stdout)
                }
                Shell::PowerShell => {
                    generate(clap_complete::Shell::PowerShell, &mut command, NAME, stdout)
                }
            }
        }
        Commands::Summarize { id } => summarize::summarize(id),
        Commands::Blacklist { action } => match action {
            BlacklistAction::Add { pattern } => blacklist::add(&pattern),
            BlacklistAction::Remove { pattern } => blacklist::remove(&pattern),
            BlacklistAction::List => blacklist::list(),
        },
    }
}
