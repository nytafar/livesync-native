//! Profile configuration: flags override environment, which overrides the
//! config file. The config file lives outside any repo
//! (`$XDG_CONFIG_HOME/lsync/<profile>.toml`, mode 0600).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FileConfig {
    pub url: Option<String>,
    pub database: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub vault: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    /// Local `syncIgnoreRegEx` in upstream format (`|[]|`-separated).
    pub ignore_regex: Option<String>,
    pub max_size_mb: Option<u64>,
    pub concurrency: Option<usize>,
}

/// Values given on the command line or through `LSYNC_*` variables.
#[derive(Debug, Default, Clone, clap::Args)]
pub struct Overrides {
    /// CouchDB server URL (http:// or https://)
    #[arg(long, env = "LSYNC_URL", global = true)]
    pub url: Option<String>,
    /// Database name
    #[arg(long = "db", env = "LSYNC_DB", global = true)]
    pub database: Option<String>,
    /// CouchDB user
    #[arg(long = "user", env = "LSYNC_USER", global = true)]
    pub username: Option<String>,
    /// CouchDB password (prefer LSYNC_PASSWORD or the config file)
    #[arg(long, env = "LSYNC_PASSWORD", global = true, hide_env_values = true)]
    pub password: Option<String>,
    /// Vault folder to materialise into
    #[arg(long, env = "LSYNC_VAULT", global = true)]
    pub vault: Option<PathBuf>,
    /// Directory for the sync index (default: $XDG_STATE_HOME/lsync/<profile>)
    #[arg(long, env = "LSYNC_STATE_DIR", global = true)]
    pub state_dir: Option<PathBuf>,
    /// Local syncIgnoreRegEx (upstream format, patterns joined by `|[]|`)
    #[arg(long, env = "LSYNC_IGNORE_REGEX", global = true)]
    pub ignore_regex: Option<String>,
    /// Override syncMaxSizeInMB from the remote's PREFERRED tweaks
    #[arg(long, env = "LSYNC_MAX_SIZE_MB", global = true)]
    pub max_size_mb: Option<u64>,
    /// Chunk-fetch requests in flight
    #[arg(long, env = "LSYNC_CONCURRENCY", global = true)]
    pub concurrency: Option<usize>,
}

impl FileConfig {
    pub fn merged(mut self, o: &Overrides) -> Self {
        macro_rules! take {
            ($f:ident) => {
                if o.$f.is_some() {
                    self.$f = o.$f.clone();
                }
            };
        }
        take!(url);
        take!(database);
        take!(username);
        take!(password);
        take!(vault);
        take!(state_dir);
        take!(ignore_regex);
        take!(max_size_mb);
        take!(concurrency);
        self
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

pub fn default_config_path(profile: &str) -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("lsync").join(format!("{profile}.toml"))
}

pub fn default_state_dir(profile: &str) -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("lsync").join(profile)
}

pub fn load(path: &Path) -> Result<FileConfig> {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save(path: &Path, cfg: &FileConfig) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(cfg)?;
    let tmp = path.with_extension("toml.tmp");
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// A fully resolved profile.
pub struct Resolved {
    pub remote: livesync_remote::RemoteConfig,
    pub vault: PathBuf,
    pub state_dir: PathBuf,
    pub ignore_regex: Option<String>,
    pub max_size_mb: Option<u64>,
    pub concurrency: Option<usize>,
}

pub fn resolve(cfg: &FileConfig, profile: &str, need_remote: bool) -> Result<Resolved> {
    let Some(vault) = cfg.vault.clone() else {
        bail!("no vault folder configured (use --vault, LSYNC_VAULT or `lsync configure --vault`)")
    };
    let url = cfg.url.clone().unwrap_or_default();
    let database = cfg.database.clone().unwrap_or_default();
    if need_remote && (url.is_empty() || database.is_empty()) {
        bail!("no remote configured (use --url/--db, LSYNC_URL/LSYNC_DB or `lsync configure`)");
    }
    Ok(Resolved {
        remote: livesync_remote::RemoteConfig {
            url,
            database,
            username: cfg.username.clone(),
            password: cfg.password.clone(),
        },
        vault,
        state_dir: cfg.state_dir.clone().unwrap_or_else(|| default_state_dir(profile)),
        ignore_regex: cfg.ignore_regex.clone().filter(|s| !s.is_empty()),
        max_size_mb: cfg.max_size_mb,
        concurrency: cfg.concurrency,
    })
}
