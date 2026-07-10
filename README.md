# dejiny

A shell history manager that records terminal sessions and lets you search and replay them.

dejiny stores every command you run in a SQLite database along with its exit code, working directory, hostname, and timestamps. When recording is enabled, it also captures the full terminal output so you can replay sessions later. Recordings are compressed with zstd and stored in chunks. An optional summarization feature uses Claude to generate short descriptions of recorded sessions.

## Requirements

- Rust (2024 edition)
- Bash or Zsh

## Installation

```
cargo install --path .
```

## Shell setup

Add one of the following to your shell configuration file:

**Zsh** (`~/.zshrc`):
```zsh
eval "$(dejiny init zsh)"
```

This installs hooks that automatically store each command after it runs and binds `Ctrl+R` to the interactive search UI.

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

## Syncing history between machines

dejiny can broadcast newly stored commands to other dejiny instances over TCP, so every machine on your LAN or tailnet shares one command history. Synced entries carry the hostname of the machine they ran on and show up in the search UI as `command @otherhost`. Only command history is synced — terminal recordings and summaries stay local.

Messages are authenticated with a preshared key (HMAC-SHA256), so only nodes holding the key can insert history. The transport itself is not encrypted; run it over a trusted network such as a tailnet.

### Configuration

Create `~/.config/dejiny/config.toml` on every node, listing all the other nodes:

```toml
[sync]
key = "generate one with: dejiny sync keygen"
listen = "0.0.0.0:28657"        # optional, this is the default

[[sync.nodes]]
name = "desktop"
addr = "desktop.tail1234.ts.net:28657"

[[sync.nodes]]
name = "laptop"
addr = "192.168.1.20:28657"
```

Every node must use the same `key`. Generate one and lock the file down:

```
dejiny sync keygen
chmod 600 ~/.config/dejiny/config.toml
```

Without a config file (or without a `[sync]` section) dejiny behaves exactly as before — no sync, no listener, no queueing.

### Receiving: the listener

Each node runs a listener to receive commands from its peers. Either background it directly:

```
dejiny sync listen --daemon    # start in the background
dejiny sync stop               # stop it
```

or run it in the foreground under a supervisor such as a systemd user unit (`~/.config/systemd/user/dejiny-sync.service`):

```ini
[Unit]
Description=dejiny history sync listener

[Service]
ExecStart=%h/.cargo/bin/dejiny sync listen
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

### Delivery, retries, and status

Each stored command is queued per node in a durable outbox and removed only once that node acknowledges it. Unreachable peers (laptop asleep, network down) simply accumulate a backlog that is retried — with exponential backoff — every time you run a command, so histories converge once the peer comes back. Duplicate deliveries are harmless: receivers dedupe on (command, timestamp, origin host). Outbox entries for a node that stays unreachable are dropped after 30 days.

```
dejiny sync status    # daemon state and per-node backlog
dejiny sync flush     # push the backlog right now, ignoring backoff
```

## Data storage

All data is stored in `$XDG_DATA_HOME/dejiny/history.db` (defaults to `~/.local/share/dejiny/history.db`). The database uses WAL mode for concurrent access. Debug logs are written to `debug.log` and errors to `error.log` in the same directory. The sync listener's PID file (`sync.pid`) also lives there.
