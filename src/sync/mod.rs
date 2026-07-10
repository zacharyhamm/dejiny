//! Cross-instance history sync: broadcast newly stored commands to the
//! nodes listed in the config file, and receive theirs via `dejiny sync
//! listen`. History only — recordings and summaries never leave the machine.

mod listen;
mod outbox;
mod proto;

pub use outbox::enqueue;

use crate::config::{Node, SyncConfig};
use crate::db::{log_error, open_db};
use rusqlite::Connection;
use std::io::{BufRead, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// Flushes always run detached from the shell, so waiting out a competing
/// writer (shell hook, listener, another flush) beats erroring at 500ms.
const FLUSH_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct FlushStats {
    pub delivered: u64,
    pub failed_nodes: Vec<String>,
    pub deferred_nodes: Vec<String>,
}

/// Push all pending outbox entries to their nodes. Per-node failures are
/// recorded for retry and never propagated — a dead peer must not fail the
/// command that triggered the flush. `force` retries nodes still in backoff
/// (used by the manual `dejiny sync flush`).
pub fn flush_outbox(
    conn: &mut Connection,
    cfg: &SyncConfig,
    force: bool,
) -> anyhow::Result<FlushStats> {
    conn.busy_timeout(FLUSH_BUSY_TIMEOUT)?;
    outbox::prune(conn, cfg)?;
    let host = hostname::get()?.to_string_lossy().into_owned();
    let mut stats = FlushStats {
        delivered: 0,
        failed_nodes: Vec::new(),
        deferred_nodes: Vec::new(),
    };
    for node in &cfg.nodes {
        let pending = outbox::pending(conn, &node.name)?;
        if pending.is_empty() {
            continue;
        }
        if !force && outbox::in_backoff(conn, &node.name)? {
            stats.deferred_nodes.push(node.name.clone());
            continue;
        }
        match send_to_node(conn, node, cfg.key.as_bytes(), &host, &pending) {
            Ok(n) => stats.delivered += n,
            Err(e) => {
                log::debug!("sync flush: node {} ({}): {e}", node.name, node.addr);
                outbox::mark_attempt(conn, &node.name)?;
                stats.failed_nodes.push(node.name.clone());
            }
        }
    }
    Ok(stats)
}

/// Send pending commands to one node in MAX_BATCH-sized frames, deleting
/// each frame's outbox rows only after the peer acknowledges it.
fn send_to_node(
    conn: &mut Connection,
    node: &Node,
    key: &[u8],
    host: &str,
    pending: &[(i64, proto::CmdMsg)],
) -> anyhow::Result<u64> {
    let mut stream = connect(&node.addr)?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    stream.set_read_timeout(Some(ACK_TIMEOUT))?;
    let mut reader = std::io::BufReader::new(stream.try_clone()?);

    let mut delivered = 0u64;
    for chunk in pending.chunks(proto::MAX_BATCH) {
        let env = proto::Envelope {
            v: proto::PROTO_VERSION,
            host: host.to_string(),
            cmds: chunk.iter().map(|(_, cmd)| cmd.clone()).collect(),
        };
        let line = proto::encode(key, &env)?;
        stream.write_all(line.as_bytes())?;

        let mut ack = String::new();
        (&mut reader).take(16).read_line(&mut ack)?;
        if ack.trim_end() != "ok" {
            anyhow::bail!("no acknowledgement (got {ack:?})");
        }

        let ids: Vec<i64> = chunk.iter().map(|(id, _)| *id).collect();
        outbox::delete_delivered(conn, &ids)?;
        delivered += chunk.len() as u64;
    }
    Ok(delivered)
}

fn connect(addr: &str) -> anyhow::Result<TcpStream> {
    let mut last_err = anyhow::anyhow!("{addr}: no addresses resolved");
    for sock_addr in addr.to_socket_addrs()? {
        match TcpStream::connect_timeout(&sock_addr, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = anyhow::anyhow!("{addr}: {e}"),
        }
    }
    Err(last_err)
}

/// Re-exec ourselves as a detached `dejiny sync flush --auto` so network
/// latency never blocks the caller (used from the foreground `dejiny record`
/// path). `--auto` keeps the retry backoff in effect, unlike a manual flush.
pub fn spawn_flush() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let _ = Command::new(exe)
        .args(["sync", "flush", "--auto"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn require_config() -> SyncConfig {
    match crate::config::require() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("dejiny: {e}");
            std::process::exit(1);
        }
    }
}

pub fn listen_cmd(daemon: bool) {
    let cfg = require_config();
    if let Err(e) = listen::run(&cfg, daemon) {
        eprintln!("dejiny: sync listen: {e}");
        log_error(&format!("sync listen: {e}"));
        std::process::exit(1);
    }
}

pub fn stop_cmd() {
    if let Err(e) = listen::stop() {
        eprintln!("dejiny: sync stop: {e}");
        std::process::exit(1);
    }
}

pub fn flush_cmd(auto: bool) {
    let cfg = require_config();
    let mut conn = match open_db() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dejiny: failed to open database: {e}");
            std::process::exit(1);
        }
    };
    match flush_outbox(&mut conn, &cfg, !auto) {
        Ok(stats) => {
            println!("delivered {} entries", stats.delivered);
            for node in &stats.deferred_nodes {
                println!("node {node}: in retry backoff, deferred");
            }
            for node in &stats.failed_nodes {
                println!("node {node}: unreachable, will retry");
            }
        }
        Err(e) => {
            eprintln!("dejiny: sync flush: {e}");
            std::process::exit(1);
        }
    }
}

pub fn status_cmd() {
    let cfg = require_config();
    let conn = match open_db() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("dejiny: failed to open database: {e}");
            std::process::exit(1);
        }
    };
    match listen::daemon_pid() {
        Some(pid) => println!("daemon: running (pid {pid})"),
        None => println!("daemon: not running"),
    }
    for node in &cfg.nodes {
        match outbox::node_status(&conn, &node.name) {
            Ok(st) if st.pending == 0 => {
                println!("node {} ({}): up to date", node.name, node.addr);
            }
            Ok(st) => {
                let last = st
                    .last_attempt
                    .map(crate::util::format_timestamp)
                    .unwrap_or_else(|| "never".to_string());
                println!(
                    "node {} ({}): {} pending, {} attempts, last attempt {}",
                    node.name, node.addr, st.pending, st.attempts, last
                );
            }
            Err(e) => eprintln!("dejiny: node {}: {e}", node.name),
        }
    }
}

pub fn keygen_cmd() {
    let mut buf = [0u8; 32];
    if let Err(e) = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
    {
        eprintln!("dejiny: failed to read /dev/urandom: {e}");
        std::process::exit(1);
    }
    println!("{}", hex::encode(buf));
    eprintln!(
        "Set this as `key` under [sync] in {} on every node, then restrict access:",
        crate::config::config_path().display()
    );
    eprintln!("  chmod 600 {}", crate::config::config_path().display());
}
