//! From `<profile>.toml` files to `lsync.toml` (spec D13).
//!
//! Runs while there is no `lsync.toml` beside the profile files: one account
//! per distinct server URL and user (and password), one vault per profile
//! with the profile's name. A vault named like the profile has the same
//! default state folder (`$XDG_STATE_HOME/lsync/<name>/`), and an explicit
//! `state_dir` is carried over, so every vault keeps its index and node ID
//! and nothing is fetched again.
//!
//! Crash safety: the passwords go into the secret store first, `lsync.toml`
//! is written last and atomically, and the profile files are never changed.
//! A crash before `lsync.toml` exists leaves the profiles as the source, and
//! the next run migrates again to the same names. Once it exists, nothing
//! more is migrated.
//!
//! The profile files stay as they were, so an older `lsync` still runs
//! from them after a downgrade (with the settings of the time of the
//! migration). They still hold their passwords; the migration says so.

use crate::config::{check_name, server_key, Account, Config, Dirs, Migration, Vault, CONFIG_FILE, SECRETS_FILE};
use crate::edit::ConfigFile;
use crate::legacy::{self, Profile};
use crate::run::split_userinfo;
use crate::secret::{Secret, SecretStore};
use livesync_model::scope::ScopeMode;
use std::path::{Path, PathBuf};

/// What a migration did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// New accounts, by name.
    pub accounts: Vec<String>,
    /// (vault name, the profile file it came from).
    pub vaults: Vec<(String, PathBuf)>,
    /// Profile files left as they are, and why (they are still read when
    /// named with `--profile`).
    pub skipped: Vec<(PathBuf, String)>,
    /// Accounts whose password went into the secret store.
    pub passwords: Vec<String>,
    /// Whether `lsync.toml` was written (with no vaults when none could
    /// be migrated, so that this runs once).
    pub written: bool,
}

/// The profile files in the config folder: `*.toml` other than `lsync.toml`
/// and `secrets.toml`, sorted by name.
pub fn profile_files(dirs: &Dirs) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(&dirs.config) else {
        return vec![];
    };
    let mut out: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".toml")?.to_string();
            if name == CONFIG_FILE || name == SECRETS_FILE || stem.is_empty() || stem.starts_with('.') {
                return None;
            }
            Some((stem, e.path()))
        })
        .collect();
    out.sort();
    out
}

/// Whether a run should migrate: no `lsync.toml`, and profile files.
pub fn needed(dirs: &Dirs) -> bool {
    !dirs.config_file().exists() && !profile_files(dirs).is_empty()
}

fn new_id() -> String {
    use rand::Rng;
    let n: u64 = rand::thread_rng().gen();
    format!("{n:016x}")
}

/// A short account name from the server URL: its host (and port).
pub fn account_base(url: &str) -> String {
    let rest = url.trim().split_once("://").map_or(url.trim(), |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let mut s: String = host
        .chars()
        .map(|c| {
            // No dots: a dotted name needs quotes in `[account."..."]`.
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    s = s.trim_matches(|c: char| !c.is_ascii_alphanumeric()).to_string();
    s.truncate(48);
    if s.is_empty() {
        "server".into()
    } else {
        s
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string()
}

struct NewAccount {
    name: String,
    key: (String, String),
    account: Account,
    password: Option<String>,
}

fn incomplete(p: &Profile) -> Option<String> {
    let missing = |o: &Option<String>| o.as_deref().is_none_or(|s| s.trim().is_empty());
    if missing(&p.url) {
        return Some("it has no url (it may get it from --url or LSYNC_URL)".into());
    }
    if missing(&p.database) {
        return Some("it has no database".into());
    }
    match &p.vault {
        None => Some("it has no vault folder".into()),
        Some(v) if !v.is_absolute() => Some("its vault folder is not an absolute path".into()),
        _ => None,
    }
}

fn vault_from(p: &Profile, account: &str) -> Result<Vault, String> {
    let mut v = Vault::new(
        account,
        p.database.as_deref().unwrap_or_default().trim(),
        &p.vault.as_ref().map(|v| v.display().to_string()).unwrap_or_default(),
    );
    v.id = Some(new_id());
    v.state_dir = p.state_dir.as_ref().map(|s| s.display().to_string());
    let folders = p.scope_folders.clone().unwrap_or_default();
    match p.scope_mode()? {
        ScopeMode::All => {}
        ScopeMode::Include => v.include = Some(folders),
        ScopeMode::Exclude => v.exclude = Some(folders),
    }
    v.ignore_regex = p.ignore_regex.clone().filter(|s| !s.is_empty());
    v.device_name = p.device_name.clone().filter(|s| !s.is_empty());
    v.max_size_mb = p.max_size_mb;
    v.concurrency = p.concurrency;
    v.use_ignore_files = p.use_ignore_files;
    v.ignore_files = p.ignore_files.clone();
    v.sync_only_regex = p.sync_only_regex.clone().filter(|s| !s.is_empty());
    v.sync_dot_paths = p.sync_dot_paths.clone();
    v.trash = p.trash.clone();
    Ok(v)
}

/// Migrate the profile files in `dirs.config` into `lsync.toml`, putting the
/// passwords into `secrets` (the `file` store, `secret = "file:<account>"`).
/// `Ok(None)` when there is nothing to do.
///
/// `lsync.toml` is written even when no profile could become a vault (with
/// no entries, and a note saying which profiles were left and why), so the
/// migration runs and reports once, not on every run. The profiles left
/// are still read as before. A migration that fails (the secret store or
/// the file cannot be written) writes nothing and is tried again next run.
pub fn migrate(dirs: &Dirs, secrets: &dyn SecretStore) -> Result<Option<Report>, String> {
    let path = dirs.config_file();
    if path.exists() {
        return Ok(None);
    }
    let profiles = profile_files(dirs);
    if profiles.is_empty() {
        return Ok(None);
    }
    // Locked, then checked again: another lsync may have just migrated.
    let mut file = ConfigFile::open(&path)?;
    if file.exists() {
        return Ok(None);
    }
    let mut report = Report::default();
    // Accounts get their names once every profile is placed (see below);
    // until then they go by a placeholder.
    let mut accounts: Vec<NewAccount> = Vec::new();
    let mut cfg = Config::default();
    let mut vaults: Vec<(String, Vault, PathBuf)> = Vec::new();
    for (name, p_path) in &profiles {
        let skip = |report: &mut Report, why: String| report.skipped.push((p_path.clone(), why));
        if let Err(e) = check_name("vault", name) {
            skip(&mut report, e);
            continue;
        }
        let p = match legacy::load(p_path) {
            Ok(p) => p,
            Err(e) => {
                skip(&mut report, e);
                continue;
            }
        };
        if let Some(why) = incomplete(&p) {
            skip(&mut report, why);
            continue;
        }
        let (url, user, password) = credentials(&p);
        let key = (server_key(&url), user.clone());
        // The same server and user, and no other password: the same account.
        let found = accounts
            .iter_mut()
            .position(|a| a.key == key && (a.password.is_none() || password.is_none() || a.password == password));
        let (acc_index, created) = match found {
            Some(i) => (i, false),
            None => {
                let name = format!("new-account-{}", accounts.len());
                accounts.push(NewAccount {
                    name: name.clone(),
                    key,
                    account: Account {
                        id: Some(new_id()),
                        url: url.clone(),
                        username: Some(user.clone()).filter(|s| !s.is_empty()),
                        secret: None,
                    },
                    password: None,
                });
                (accounts.len() - 1, true)
            }
        };
        let vault = match vault_from(&p, &accounts[acc_index].name) {
            Ok(v) => v,
            Err(e) => {
                if created {
                    accounts.pop();
                }
                skip(&mut report, e);
                continue;
            }
        };
        // Checked with every vault taken so far: the folder rules.
        let mut trial = cfg.clone();
        let a = &accounts[acc_index];
        trial.account.insert(a.name.clone(), a.account.clone());
        trial.vault.insert(name.clone(), vault.clone());
        if let Err(e) = trial.validate(dirs) {
            // The account has no name yet: say "the server" instead.
            let placeholder = format!("[account.{}]: ", a.name);
            if created {
                accounts.pop();
            }
            let e = match e.strip_prefix(&placeholder) {
                Some(rest) => format!("its server: {rest}"),
                None => e,
            };
            skip(&mut report, e);
            continue;
        }
        cfg = trial;
        if accounts[acc_index].password.is_none() {
            accounts[acc_index].password = password;
        }
        vaults.push((name.clone(), vault, p_path.clone()));
    }
    // The names, now that each account's password is known: a rerun after
    // a crash picks the same ones (a key that holds this account's password
    // is its own).
    let mut named: Vec<NewAccount> = Vec::new();
    for mut a in accounts {
        let placeholder = std::mem::take(&mut a.name);
        let user = a.key.1.clone();
        a.name = account_name(&named, &a.account.url, &user, a.password.as_deref(), secrets)?;
        a.account.secret = Some(format!("file:{}", a.name));
        for (_, v, _) in vaults.iter_mut() {
            if v.account == placeholder {
                v.account = a.name.clone();
            }
        }
        named.push(a);
    }
    let accounts = named;
    // 1. The passwords, so that `lsync.toml` never names one that is missing.
    for a in &accounts {
        if let Some(pw) = &a.password {
            let have = secrets.get(&a.name).map_err(|e| e.to_string())?;
            if have.as_ref().map(|s| s.expose()) != Some(pw.as_str()) {
                secrets
                    .set(&a.name, &Secret::new(pw.clone()))
                    .map_err(|e| format!("storing the password of account {:?}: {e}", a.name))?;
            }
            report.passwords.push(a.name.clone());
        }
    }
    // 2. `lsync.toml`.
    let file_name = |p: &Path| p.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let mut note = String::new();
    if !vaults.is_empty() {
        let names: Vec<String> = vaults.iter().map(|(_, _, p)| file_name(p)).collect();
        note.push_str(&format!(
            "Migrated from {}.\nThose files are left as they were, for going back to an older lsync; this file\nis the one read now. They still hold their passwords: delete them once you no\nlonger need them.",
            names.join(", ")
        ));
    } else {
        note.push_str("No profile file could become a vault here.");
    }
    if !report.skipped.is_empty() {
        note.push_str(
            "\n\nNot migrated, and still read as before (with --profile NAME; default.toml when\nno vault is named):",
        );
        for (p, why) in &report.skipped {
            note.push_str(&format!("\n- {}: {}", file_name(p), why.replace('\n', " ")));
        }
        note.push_str("\nTo migrate them after a fix, add them with `lsync vault add`, or move this file\naway while it has no vaults; it is not tried again while this file exists.");
    }
    file.add_header_note(&note);
    file.set_migration(&Migration {
        migrated: vaults.iter().map(|(_, _, p)| file_name(p)).collect(),
        skipped: report.skipped.iter().map(|(p, _)| file_name(p)).collect(),
    })?;
    for a in &accounts {
        file.add_account(&a.name, &a.account, None)?;
        report.accounts.push(a.name.clone());
    }
    for (name, v, from) in &vaults {
        file.add_vault(name, v, Some(&format!("migrated from {}", file_name(from))))?;
        report.vaults.push((name.clone(), from.clone()));
    }
    let text = file.text();
    Config::parse(&text, dirs)?;
    crate::edit::write_private(&path, &text).map_err(|e| format!("writing {}: {e}", path.display()))?;
    report.written = true;
    Ok(Some(report))
}

/// The server, user and password of a profile, with a user and password
/// written into the URL moved out of it (as the remote client reads them:
/// the profile's own `username` and `password` win).
fn credentials(p: &Profile) -> (String, String, Option<String>) {
    let url = p.url.clone().unwrap_or_default().trim().to_string();
    let mut user = p.username.clone().unwrap_or_default();
    let mut password = p.password.clone().filter(|s| !s.is_empty());
    match split_userinfo(&url) {
        Some(u) => {
            if user.is_empty() {
                user = u.username.unwrap_or_default();
            }
            if password.is_none() {
                password = u.password;
            }
            (u.url, user, password)
        }
        None => (url, user, password),
    }
}

/// A name for a new account that no other new account has, and whose
/// `file:` key does not already hold another password.
fn account_name(
    taken: &[NewAccount],
    url: &str,
    user: &str,
    password: Option<&str>,
    secrets: &dyn SecretStore,
) -> Result<String, String> {
    let base = account_base(url);
    let mut candidates = vec![base.clone()];
    let u = sanitize(user);
    let stem = if u.is_empty() {
        base.clone()
    } else {
        format!("{base}-{u}")
    };
    if !u.is_empty() {
        candidates.push(stem.clone());
    }
    candidates.extend((2..100).map(|n| format!("{stem}-{n}")));
    for c in candidates {
        if taken.iter().any(|a| a.name == c) || check_name("account", &c).is_err() {
            continue;
        }
        let stored = secrets.get(&c).map_err(|e| e.to_string())?;
        match (stored, password) {
            (Some(s), Some(p)) if s.expose() != p => continue,
            (Some(_), None) => continue,
            _ => return Ok(c),
        }
    }
    Err(format!("no free account name for the server {base}"))
}

/// Where the profile a name refers to lives.
pub fn profile_path(dirs: &Dirs, name: &str) -> PathBuf {
    dirs.config.join(format!("{name}.toml"))
}

/// Whether `path` is one of the profile files of `dirs`.
pub fn is_profile_file(dirs: &Dirs, path: &Path) -> bool {
    profile_files(dirs).iter().any(|(_, p)| p == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_names_come_from_the_host() {
        assert_eq!(account_base("https://couch.example.net/"), "couch-example-net");
        assert_eq!(account_base("http://127.0.0.1:8411"), "127-0-0-1-8411");
        assert_eq!(account_base("https://u:p@couch.example.net/db"), "couch-example-net");
        assert_eq!(account_base("http://"), "server");
    }
}
