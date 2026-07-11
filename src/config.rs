use std::path::PathBuf;

#[derive(serde::Deserialize)]
struct ConfigFile {
    sync: Option<SyncConfig>,
}

#[derive(serde::Deserialize, Debug)]
pub struct SyncConfig {
    /// Preshared key; raw UTF-8 bytes are used as the HMAC key.
    pub key: String,
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default)]
    pub nodes: Vec<Node>,
}

#[derive(serde::Deserialize, Clone, Debug)]
pub struct Node {
    pub name: String,
    pub addr: String,
}

fn default_listen() -> String {
    "0.0.0.0:28657".to_string()
}

pub fn config_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            PathBuf::from(home).join(".config")
        });
    config_dir.join("dejiny").join("config.toml")
}

/// Load sync config for the store/record hot paths: absent config is a
/// silent None (sync disabled), broken config is logged and disables sync
/// rather than failing the command.
pub fn load() -> Option<SyncConfig> {
    match read_config(&config_path()) {
        Ok(cfg) => cfg,
        Err(e) => {
            crate::db::log_error(&format!("sync config: {e}"));
            None
        }
    }
}

/// Load sync config for explicit `dejiny sync` subcommands: errors and the
/// absence of a [sync] section are surfaced to the caller.
pub fn require() -> anyhow::Result<SyncConfig> {
    let path = config_path();
    if let Some(warning) = permission_warning(&path) {
        eprintln!("dejiny: warning: {warning}");
    }
    read_config(&path)?
        .ok_or_else(|| anyhow::anyhow!("no [sync] configuration found at {}", path.display()))
}

fn read_config(path: &std::path::Path) -> anyhow::Result<Option<SyncConfig>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("failed to read {}: {e}", path.display())),
    };
    let file: ConfigFile = toml::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", path.display()))?;
    let Some(sync) = file.sync else {
        return Ok(None);
    };
    if sync.key.is_empty() {
        anyhow::bail!("[sync] key must not be empty in {}", path.display());
    }
    if sync.nodes.is_empty() {
        anyhow::bail!("no [[sync.nodes]] configured in {}", path.display());
    }
    Ok(Some(sync))
}

fn permission_warning(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    let mode = meta.mode() & 0o777;
    (mode & 0o077 != 0).then(|| {
        format!(
            "{} is readable by other users (mode {mode:o}); it contains the sync key — run: chmod 600 {}",
            path.display(),
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        (dir, path)
    }

    #[test]
    fn full_config_parses() {
        let (_dir, path) = write_config(
            r#"
            [sync]
            key = "secret"
            listen = "127.0.0.1:1234"

            [[sync.nodes]]
            name = "desktop"
            addr = "10.0.0.2:28657"

            [[sync.nodes]]
            name = "laptop"
            addr = "laptop.tail1234.ts.net:28657"
            "#,
        );
        let cfg = read_config(&path).unwrap().unwrap();
        assert_eq!(cfg.key, "secret");
        assert_eq!(cfg.listen, "127.0.0.1:1234");
        assert_eq!(cfg.nodes.len(), 2);
        assert_eq!(cfg.nodes[0].name, "desktop");
        assert_eq!(cfg.nodes[1].addr, "laptop.tail1234.ts.net:28657");
    }

    #[test]
    fn default_listen_filled_in() {
        let (_dir, path) = write_config(
            r#"
            [sync]
            key = "secret"

            [[sync.nodes]]
            name = "desktop"
            addr = "10.0.0.2:28657"
            "#,
        );
        let cfg = read_config(&path).unwrap().unwrap();
        assert_eq!(cfg.listen, "0.0.0.0:28657");
    }

    #[test]
    fn missing_file_is_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = read_config(&dir.path().join("nope.toml")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn no_sync_section_is_none() {
        let (_dir, path) = write_config("# just a comment\n");
        assert!(read_config(&path).unwrap().is_none());
    }

    #[test]
    fn garbage_toml_is_error() {
        let (_dir, path) = write_config("[sync\nkey =");
        assert!(read_config(&path).is_err());
    }

    #[test]
    fn missing_key_is_error() {
        let (_dir, path) = write_config(
            r#"
            [sync]
            [[sync.nodes]]
            name = "desktop"
            addr = "10.0.0.2:28657"
            "#,
        );
        assert!(read_config(&path).is_err());
    }

    #[test]
    fn empty_nodes_is_error() {
        let (_dir, path) = write_config("[sync]\nkey = \"secret\"\n");
        assert!(read_config(&path).is_err());
    }

    #[test]
    fn permission_warning_on_loose_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = write_config("[sync]\nkey = \"secret\"\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(permission_warning(&path).is_some());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(permission_warning(&path).is_none());
    }
}
