# dejiny

A shell history manager that records terminal sessions and lets you search and replay them.

dejiny stores every command you run in a SQLite database along with its exit code, working directory, hostname, and timestamps. When recording is enabled, it also captures the full terminal output so you can replay sessions later. Recordings are compressed with zstd and stored in chunks. An optional summarization feature uses Claude to generate short descriptions of recorded sessions.

## Installation

```
cargo install --path .
```

## Shell setup

Add the following to your shell configuration file:

**Zsh** (`~/.zshrc`):
```zsh
eval "$(dejiny init zsh)"
```

This installs hooks that automatically store each command after it runs and binds `Ctrl+R` to the interactive search UI.

- An init script also exists for `bash`, but I have not tested it yet, and have no idea if it works as expected. YMMV.
- Completions exist for many other shells too, provided by [`clap_complete`](https://docs.rs/clap_complete/latest/clap_complete/) and [`clap_complete_nushell`](https://docs.rs/clap_complete_nushell/latest/clap_complete_nushell/), but have also not be tested extensively


## Recording terminal sessions

To record a single command:

```
dejiny record -- ls -la
```

To record all commands automatically, set the `DEJINY_RECORD_ALL` environment variable:

```
export DEJINY_RECORD_ALL=1
```

When this is set, the shell hook wraps each command in `dejiny record` transparently. Shell builtins (`cd`, `export`, `alias`, etc.) and commands starting with a space are excluded.

## Searching history

Press `Ctrl+R` in your shell (after setup) to open the interactive search interface. You can also run it directly:

```
dejiny search
```

The search UI supports:

- Fuzzy matching as you type
- `Up`/`Down` or `Ctrl+P`/`Ctrl+N` to navigate results
- `PageUp`/`PageDown` for faster scrolling
- `Enter` to select a command and place it on your command line
- `Ctrl+O` to replay a recorded session directly from search
- `Ctrl+R` to toggle filtering to only recorded commands
- `Ctrl+W` to delete the last word from the query
- `Ctrl+U` to clear the query
- `Tab` to focus the summary panel (when a summary is available)
- `Esc` or `Ctrl+C` to cancel

Each result shows the exit code, a recording indicator, the command, the working directory, and how long ago it ran.

## Replaying sessions

Replay a specific recording by its database ID:

```
dejiny replay 42
```

Replay the most recent recording:

```
dejiny replay
```

Controls during interactive replay:

- `Space` to pause/resume
- `Left`/`Right` arrows to seek backward/forward by 5 seconds
- `q` or `Ctrl+C` to quit

Options:

- `--speed <multiplier>` -- playback speed (default `1.0`, use `0.0` for instant)
- `--text` -- print the session as plain text with metadata instead of interactive replay

## Summarization

After each recording finishes, dejiny spawns a background process that sends the terminal output to `claude` (the Claude CLI) to generate a short summary. Summaries are stored in the database and shown in the search UI.

To disable automatic summarization, set `DEJINY_NO_SUMMARY=1`.

### Blacklist

You can prevent summarization for commands matching regex patterns:

```
dejiny blacklist add '^ssh '
dejiny blacklist remove '^ssh '
dejiny blacklist list
```

## Data storage

All data is stored in `$XDG_DATA_HOME/dejiny/history.db` (defaults to `~/.local/share/dejiny/history.db`). The database uses WAL mode for concurrent access. Debug logs are written to `debug.log` and errors to `error.log` in the same directory.
