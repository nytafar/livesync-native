//! `lsync account ...`, `lsync vault ...`, and `lsync configure` on a vault
//! in `lsync.toml`: edit the file with its comments kept, keep passwords in
//! the secret store, and tell a running service (`service::config_changed`).

use crate::config::{Overrides, Selected, Setup};
use crate::prompt;
use crate::service::{self, Change};
use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use livesync_host::config::{check_name, server_key, Account, Config, Mode, Vault, USERINFO};
use livesync_host::edit::{string_array, ConfigFile};
use livesync_host::migrate;
use livesync_host::run::split_userinfo;
use livesync_host::secret::{Availability, Secret, SecretRef, SecretStores, StoreKind};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Subcommand, Debug)]
pub enum AccountCmd {
    /// Add an account: a server (--url), a user (--user) and where its
    /// password is kept. The password is asked for, or read with
    /// --password-stdin; never from the command line
    Add {
        name: String,
        /// Where the password is: file:KEY (secrets.toml, the default,
        /// file:NAME), credential:NAME (systemd credentials) or env:VAR
        #[arg(long)]
        secret: Option<String>,
        /// Read the password from stdin instead of asking
        #[arg(long)]
        password_stdin: bool,
        /// No password now (set it later with `lsync account passwd`). An
        /// account without --user is a server without authentication and
        /// never has one
        #[arg(long, conflicts_with = "password_stdin")]
        no_password: bool,
        /// Store the password even though the file: key already holds one,
        /// or is another account's (whose password then changes too)
        #[arg(long, conflicts_with = "no_password")]
        replace_secret: bool,
    },
    /// List the accounts, where each password is kept, and which vaults use them
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Change an account's password (asked for, or --password-stdin)
    Passwd {
        name: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Remove an account no vault uses, and its password in secrets.toml
    Rm { name: String },
}

#[derive(Subcommand, Debug)]
pub enum VaultCmd {
    /// Add a vault: an account (--account), a database (--db), a folder
    /// (--folder), and optionally a folder scope (--include/--exclude),
    /// --device-name, --ignore-regex, --state-dir, --max-size-mb, --concurrency
    Add {
        name: String,
        /// The account (may be left out when there is only one)
        #[arg(long)]
        account: Option<String>,
        /// two-way (default) or pull-only
        #[arg(long)]
        mode: Option<String>,
        /// Add it disabled (the service does not run it)
        #[arg(long)]
        disabled: bool,
    },
    /// List the vaults
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Change a vault's settings, with the same options as `vault add`
    Edit {
        name: String,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        mode: Option<String>,
        /// Remove an optional setting (state_dir, ignore_regex, device_name,
        /// max_size_mb, concurrency); repeatable
        #[arg(long, value_name = "KEY")]
        unset: Vec<String>,
    },
    /// Let the service run the vault
    Enable { name: String },
    /// Keep the vault configured, but have the service leave it alone
    Disable { name: String },
    /// Remove the vault from lsync.toml (its folder and its index are kept)
    Rm { name: String },
}

/// Which of the global options were given on the command line, as opposed
/// to the environment: `vault add` and `vault edit` take only those.
pub struct Given<'a> {
    pub o: &'a Overrides,
    pub on_command_line: &'a dyn Fn(&str) -> bool,
}

impl Given<'_> {
    fn has(&self, id: &str) -> bool {
        (self.on_command_line)(id)
    }

    fn refuse_password(&self) -> Result<()> {
        if self.has("password") {
            bail!("passwords are never taken from the command line here; you will be asked, or use --password-stdin");
        }
        Ok(())
    }
}

fn open(ctx: &Setup) -> Result<ConfigFile> {
    // Only the format here: a file that is invalid as a whole can still be
    // mended (`vault rm`); every change is checked as a whole when saved.
    if ctx.config_file.exists() && !ctx.is_accounts_file()? {
        bail!(
            "{} is a profile file; the account and vault commands edit an lsync.toml (leave out --config, or point it at one)",
            ctx.config_file.display()
        );
    }
    ConfigFile::open(&ctx.config_file).map_err(|e| anyhow!(e))
}

fn current(ctx: &Setup) -> Result<Config> {
    Ok(ctx.load_accounts()?.unwrap_or_default())
}

fn store_line(ctx: &Setup, r: &SecretRef) -> (String, String) {
    let stores = ctx.secrets();
    let state = match stores.store(r.kind) {
        Err(e) => format!("unavailable: {e}"),
        Ok(s) => match s.availability() {
            Availability::Locked(why) => format!("locked: {why}"),
            Availability::Unavailable(why) => format!("unavailable: {why}"),
            Availability::Ready => match s.get(&r.key) {
                Ok(Some(_)) => "set".into(),
                Ok(None) => "missing".into(),
                Err(e) => format!("unavailable: {e}"),
            },
        },
    };
    (stores.describe(r), state)
}

pub fn account(ctx: &Setup, cmd: AccountCmd, given: &Given) -> Result<u8> {
    match cmd {
        AccountCmd::Add {
            name,
            secret,
            password_stdin,
            no_password,
            replace_secret,
        } => {
            given.refuse_password()?;
            check_name("account", &name).map_err(|e| anyhow!(e))?;
            let url = given
                .has("url")
                .then(|| given.o.url.clone())
                .flatten()
                .ok_or_else(|| anyhow!("give the server with --url"))?;
            if split_userinfo(&url).is_some() {
                bail!("{USERINFO}");
            }
            let username = given
                .has("username")
                .then(|| given.o.username.clone())
                .flatten()
                .filter(|u| !u.is_empty());
            let r = match &secret {
                Some(s) => SecretRef::parse(s).map_err(|e| anyhow!("--secret: {e}"))?,
                None => SecretRef::file(&name),
            };
            let mut file = open(ctx)?;
            let account = Account {
                id: Some(new_id()),
                url: url.trim().to_string(),
                username: username.clone(),
                secret: Some(r.to_string()),
            };
            file.add_account(&name, &account, None).map_err(|e| anyhow!(e))?;
            // Checked before any password is stored.
            let cfg = file.config(&ctx.dirs).map_err(|e| anyhow!(e))?;
            let stores = ctx.secrets();
            if username.is_none() && password_stdin {
                bail!("account {name} has no user, so no password (a server without authentication); give --user USER for an account with one");
            }
            if r.kind != StoreKind::File && password_stdin {
                bail!(
                    "account {name}'s password comes from the {}; lsync does not store it",
                    stores.describe(&r)
                );
            }
            // A file: key another account uses, or one that already holds a
            // password, is never taken over unasked.
            let mut previous = None;
            if r.kind == StoreKind::File && username.is_some() {
                let others: Vec<&str> = cfg
                    .account
                    .iter()
                    .filter(|(n, a)| *n != &name && a.secret_ref(n).ok().as_ref() == Some(&r))
                    .map(|(n, _)| n.as_str())
                    .collect();
                previous = stores.get(&r)?;
                let shares_on_purpose = secret.is_some() && !others.is_empty();
                let taken = !others.is_empty() || previous.is_some();
                if !no_password && taken && !replace_secret {
                    if others.is_empty() {
                        bail!("the key {r} already holds a password that no account uses (left from an earlier account?); give this account another key with --secret file:KEY, or --replace-secret to replace it");
                    }
                    bail!(
                        "the key {r} is the password of account(s) {}; give this account a key of its own with --secret file:KEY, or --replace-secret to change the password they share",
                        others.join(", ")
                    );
                }
                if no_password && previous.is_some() && !shares_on_purpose {
                    bail!(
                        "the key {r} already holds a password{}, which this account would use; give it another key with --secret file:KEY, or leave out --no-password and give --replace-secret",
                        if others.is_empty() { String::new() } else { format!(" (account(s) {}'s)", others.join(", ")) }
                    );
                }
            }
            let mut stored = false;
            if r.kind == StoreKind::File && username.is_some() && !no_password {
                let pw = prompt::new_password(password_stdin, &format!("password for {name}"))?;
                stores.set(&r, &pw)?;
                stored = true;
            }
            if let Err(e) = file.save(&ctx.dirs) {
                // Undo only what this command did: a key it created goes, a
                // password it replaced comes back.
                if stored {
                    undo_password(&stores, &r, previous.as_ref());
                }
                bail!(e);
            }
            println!("added account {name} to {}", ctx.config_file.display());
            if username.is_none() {
                println!("  password  none (no user: a server without authentication)");
            } else {
                println!("  password  {}", stores.describe(&r));
                if r.kind != StoreKind::File {
                    println!("            (lsync reads it from there; it does not store it)");
                }
            }
            service::config_changed(&ctx.config_file, &Change::Account(name));
            Ok(0)
        }
        AccountCmd::Ls { json } => {
            let cfg = current(ctx)?;
            let mut out = vec![];
            for (name, a) in &cfg.account {
                let r = a.secret_ref(name).map_err(|e| anyhow!(e))?;
                let (where_, mut state) = store_line(ctx, &r);
                if a.username.as_deref().is_none_or(|u| u.is_empty()) {
                    state = "not used: no user".into();
                }
                out.push(json!({
                    "name": name,
                    "url": a.url,
                    "username": a.username,
                    "secret": r.to_string(),
                    "store": r.kind.prefix(),
                    "where": where_,
                    "password": state,
                    "vaults": cfg.vaults_of(name),
                }));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else if out.is_empty() {
                println!(
                    "no accounts in {} (add one with `lsync account add NAME --url URL --user USER`)",
                    ctx.config_file.display()
                );
            } else {
                for a in &out {
                    println!("{}", a["name"].as_str().unwrap_or_default());
                    println!("  url       {}", a["url"].as_str().unwrap_or_default());
                    println!("  user      {}", a["username"].as_str().unwrap_or("-"));
                    println!(
                        "  password  {} ({})",
                        a["where"].as_str().unwrap_or_default(),
                        a["password"].as_str().unwrap_or_default()
                    );
                    let vaults: Vec<&str> = a["vaults"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str())
                        .collect();
                    println!(
                        "  vaults    {}",
                        if vaults.is_empty() {
                            "-".into()
                        } else {
                            vaults.join(", ")
                        }
                    );
                }
            }
            Ok(0)
        }
        AccountCmd::Passwd { name, password_stdin } => {
            given.refuse_password()?;
            let cfg = current(ctx)?;
            let a = cfg
                .account
                .get(&name)
                .ok_or_else(|| anyhow!("no account named {name:?} in {}", ctx.config_file.display()))?;
            let r = a.secret_ref(&name).map_err(|e| anyhow!(e))?;
            let stores = ctx.secrets();
            if !stores.writable(&r) {
                bail!(
                    "account {name}'s password comes from the {}; change it there",
                    stores.describe(&r)
                );
            }
            let pw = prompt::new_password(password_stdin, &format!("new password for {name}"))?;
            stores.set(&r, &pw)?;
            println!("changed the password of {name} ({})", stores.describe(&r));
            service::config_changed(&ctx.config_file, &Change::Account(name));
            Ok(0)
        }
        AccountCmd::Rm { name } => {
            let mut file = open(ctx)?;
            let cfg = file.config(&ctx.dirs).map_err(|e| anyhow!(e))?;
            let Some(a) = cfg.account.get(&name) else {
                bail!("no account named {name:?} in {}", ctx.config_file.display());
            };
            let users = cfg.vaults_of(&name);
            if !users.is_empty() {
                bail!(
                    "account {name} is used by vault(s) {}; remove them first, or move them with `lsync vault edit VAULT --account OTHER`",
                    users.join(", ")
                );
            }
            let r = a.secret_ref(&name).map_err(|e| anyhow!(e))?;
            file.remove("account", &name).map_err(|e| anyhow!(e))?;
            file.save(&ctx.dirs).map_err(|e| anyhow!(e))?;
            // After the file: a crash in between leaves an unused secret, never
            // an account without one.
            let shared = cfg
                .account
                .iter()
                .any(|(n, o)| n != &name && o.secret_ref(n).ok().as_ref() == Some(&r));
            if r.kind == StoreKind::File && !shared {
                ctx.secrets().delete(&r)?;
            }
            println!("removed account {name}");
            service::config_changed(&ctx.config_file, &Change::Account(name));
            Ok(0)
        }
    }
}

/// Undo a password this command stored: the one it replaced comes back,
/// or the key goes if this command created it. Nothing else is touched.
fn undo_password(stores: &SecretStores, r: &SecretRef, previous: Option<&Secret>) {
    let _ = match previous {
        Some(old) => stores.set(r, old),
        None => stores.delete(r).map(|_| ()),
    };
}

fn new_id() -> String {
    use rand::Rng;
    format!("{:016x}", rand::thread_rng().gen::<u64>())
}

/// A folder as written to the file: `~/...` kept, anything else absolute.
fn folder_value(p: &Path) -> Result<String> {
    let s = p.display().to_string();
    if s == "~" || s.starts_with("~/") {
        return Ok(s);
    }
    Ok(crate::absolute(p)?.display().to_string())
}

/// The folder scope given, as (include, exclude); neither for all folders.
type ScopeLists = (Option<Vec<String>>, Option<Vec<String>>);

fn scope_edit(given: &Given) -> Option<ScopeLists> {
    let o = given.o;
    if given.has("include") && !o.include.is_empty() {
        Some((Some(o.include.clone()), None))
    } else if given.has("exclude") && !o.exclude.is_empty() {
        Some((None, Some(o.exclude.clone())))
    } else if given.has("all_folders") && o.all_folders {
        Some((None, None))
    } else {
        None
    }
}

fn refuse_account_options(given: &Given, cmd: &str) -> Result<()> {
    given.refuse_password()?;
    for (id, flag) in [("url", "--url"), ("username", "--user")] {
        if given.has(id) {
            bail!("{flag} belongs to an account: use `lsync account add` (or `lsync account ls` to see them); `lsync vault {cmd}` takes --account");
        }
    }
    Ok(())
}

pub fn vault(ctx: &Setup, cmd: VaultCmd, given: &Given) -> Result<u8> {
    let o = given.o;
    match cmd {
        VaultCmd::Add {
            name,
            account,
            mode,
            disabled,
        } => {
            refuse_account_options(given, "add")?;
            check_name("vault", &name).map_err(|e| anyhow!(e))?;
            let mut file = open(ctx)?;
            let cfg = file.config(&ctx.dirs).map_err(|e| anyhow!(e))?;
            let account = match account {
                Some(a) => a,
                None if cfg.account.len() == 1 => cfg.account.keys().next().cloned().unwrap_or_default(),
                None if cfg.account.is_empty() => {
                    bail!("no accounts yet; add one first with `lsync account add NAME --url URL --user USER`")
                }
                None => bail!(
                    "choose the account with --account ({})",
                    cfg.account.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            };
            let database = given
                .has("database")
                .then(|| o.database.clone())
                .flatten()
                .ok_or_else(|| anyhow!("give the database with --db"))?;
            let folder = given
                .has("folder")
                .then(|| o.folder.clone())
                .flatten()
                .ok_or_else(|| anyhow!("give the vault folder with --folder"))?;
            let mut v = Vault::new(&account, database.trim(), &folder_value(&folder)?);
            v.id = Some(new_id());
            if let Some((inc, exc)) = scope_edit(given) {
                v.include = inc;
                v.exclude = exc;
            }
            if let Some(m) = mode {
                v.mode = Mode::parse(&m).map_err(|e| anyhow!(e))?;
            }
            v.enabled = !disabled;
            if given.has("state_dir") {
                v.state_dir = o.state_dir.as_deref().map(folder_value).transpose()?;
            }
            if given.has("ignore_regex") {
                v.ignore_regex = o.ignore_regex.clone().filter(|s| !s.is_empty());
            }
            if given.has("device_name") {
                v.device_name = o.device_name.clone().filter(|s| !s.is_empty());
            }
            if given.has("max_size_mb") {
                v.max_size_mb = o.max_size_mb;
            }
            if given.has("concurrency") {
                v.concurrency = o.concurrency;
            }
            file.add_vault(&name, &v, None).map_err(|e| anyhow!(e))?;
            let cfg = file.save(&ctx.dirs).map_err(|e| anyhow!(e))?;
            println!("added vault {name} to {}", ctx.config_file.display());
            print_vault(ctx, &name, &cfg.vault[&name]);
            service::config_changed(&ctx.config_file, &Change::Vault(name));
            Ok(0)
        }
        VaultCmd::Ls { json } => {
            let cfg = current(ctx)?;
            let legacy: Vec<PathBuf> = if ctx.explicit.is_none() {
                migrate::profile_files(&ctx.dirs)
                    .into_iter()
                    .filter(|(n, _)| !cfg.vault.contains_key(n))
                    .map(|(_, p)| p)
                    .collect()
            } else {
                vec![]
            };
            if json {
                let vaults: Vec<serde_json::Value> = cfg
                    .vault
                    .iter()
                    .map(|(n, v)| {
                        json!({
                            "name": n,
                            "account": v.account,
                            "database": v.database,
                            "folder": v.folder_path(&ctx.dirs),
                            "state_dir": v.state_path(n, &ctx.dirs),
                            "scope": v.scope().map(|s| s.describe()).unwrap_or_default(),
                            "mode": v.mode.as_str(),
                            "enabled": v.enabled,
                            "device_name": v.device_name,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "vaults": vaults, "profiles_not_migrated": legacy }))?
                );
                return Ok(0);
            }
            if cfg.vault.is_empty() {
                println!(
                    "no vaults in {} (add one with `lsync vault add NAME --db DB --folder DIR`)",
                    ctx.config_file.display()
                );
            }
            for (n, v) in &cfg.vault {
                print_vault(ctx, n, v);
            }
            if !legacy.is_empty() {
                println!("profile files not migrated (read with --profile NAME):");
                for p in legacy {
                    println!("  {}", p.display());
                }
            }
            Ok(0)
        }
        VaultCmd::Edit {
            name,
            account,
            mode,
            unset,
        } => {
            refuse_account_options(given, "edit")?;
            let mut file = open(ctx)?;
            let before = file.config(&ctx.dirs).map_err(|e| anyhow!(e))?;
            let Some(old) = before.vault.get(&name).cloned() else {
                bail!("no vault named {name:?} in {}", ctx.config_file.display());
            };
            let mut changed = false;
            let mut set = |key: &str, value: Option<toml_edit::Value>| -> Result<()> {
                changed = true;
                file.set("vault", &name, key, value).map_err(|e| anyhow!(e))
            };
            if let Some(a) = account {
                set("account", Some(a.into()))?;
            }
            if let Some(m) = mode {
                let m = Mode::parse(&m).map_err(|e| anyhow!(e))?;
                set("mode", (m != Mode::TwoWay).then(|| m.as_str().into()))?;
            }
            if given.has("database") {
                if let Some(d) = &o.database {
                    set("database", Some(d.trim().into()))?;
                }
            }
            if given.has("folder") {
                if let Some(f) = &o.folder {
                    set("folder", Some(folder_value(f)?.into()))?;
                }
            }
            if let Some((inc, exc)) = scope_edit(given) {
                set("include", inc.as_deref().map(string_array))?;
                set("exclude", exc.as_deref().map(string_array))?;
            }
            if given.has("state_dir") {
                if let Some(s) = &o.state_dir {
                    set("state_dir", Some(folder_value(s)?.into()))?;
                }
            }
            if given.has("ignore_regex") {
                set(
                    "ignore_regex",
                    o.ignore_regex.clone().filter(|s| !s.is_empty()).map(Into::into),
                )?;
            }
            if given.has("device_name") {
                set(
                    "device_name",
                    o.device_name.clone().filter(|s| !s.is_empty()).map(Into::into),
                )?;
            }
            if given.has("max_size_mb") {
                set("max_size_mb", o.max_size_mb.map(|n| (n as i64).into()))?;
            }
            if given.has("concurrency") {
                set("concurrency", o.concurrency.map(|n| (n as i64).into()))?;
            }
            for key in &unset {
                match key.as_str() {
                    "state_dir" | "ignore_regex" | "device_name" | "max_size_mb" | "concurrency" => set(key, None)?,
                    other => bail!("--unset {other}: only state_dir, ignore_regex, device_name, max_size_mb and concurrency can be unset"),
                }
            }
            if !changed {
                bail!("nothing to change; give the settings to change (see `lsync vault edit --help`)");
            }
            let cfg = file.save(&ctx.dirs).map_err(|e| anyhow!(e))?;
            let new = &cfg.vault[&name];
            if new.state_path(&name, &ctx.dirs) == old.state_path(&name, &ctx.dirs)
                && (new.folder_path(&ctx.dirs) != old.folder_path(&ctx.dirs)
                    || new.database != old.database
                    || before.account[&old.account].server_key() != cfg.account[&new.account].server_key())
            {
                eprintln!(
                    "note: the index in {} was built for the old folder or database; lsync checks it against the new one at the next start",
                    new.state_path(&name, &ctx.dirs).display()
                );
            }
            println!("changed vault {name}");
            print_vault(ctx, &name, new);
            service::config_changed(&ctx.config_file, &Change::Vault(name));
            Ok(0)
        }
        VaultCmd::Enable { name } => set_enabled(ctx, &name, true),
        VaultCmd::Disable { name } => set_enabled(ctx, &name, false),
        VaultCmd::Rm { name } => {
            let mut file = open(ctx)?;
            if !file.has("vault", &name) {
                bail!("no vault named {name:?} in {}", ctx.config_file.display());
            }
            // What it had, when the file is valid as it is.
            let before = file.config(&ctx.dirs).ok().and_then(|c| c.vault.get(&name).cloned());
            file.remove("vault", &name).map_err(|e| anyhow!(e))?;
            file.save(&ctx.dirs).map_err(|e| anyhow!(e))?;
            match before {
                Some(v) => {
                    println!(
                        "removed vault {name}; its folder {} is left as it is",
                        v.folder_path(&ctx.dirs).display()
                    );
                    let state = v.state_path(&name, &ctx.dirs);
                    if state.exists() {
                        println!(
                            "  its index is kept in {} (delete it once you no longer need it; a vault added again with this name uses it)",
                            state.display()
                        );
                    }
                }
                None => println!("removed vault {name}; its folder and its index are left as they are"),
            }
            let profile = migrate::profile_path(&ctx.dirs, &name);
            if ctx.explicit.is_none() && profile.exists() {
                println!(
                    "  note: the profile file {} is still there, and is read again for {}",
                    profile.display(),
                    if name == "default" {
                        "a command that names no vault".to_string()
                    } else {
                        format!("--profile {name}")
                    }
                );
            }
            service::config_changed(&ctx.config_file, &Change::Vault(name));
            Ok(0)
        }
    }
}

fn set_enabled(ctx: &Setup, name: &str, on: bool) -> Result<u8> {
    let mut file = open(ctx)?;
    if !file.has("vault", name) {
        bail!("no vault named {name:?} in {}", ctx.config_file.display());
    }
    file.set("vault", name, "enabled", (!on).then(|| false.into()))
        .map_err(|e| anyhow!(e))?;
    file.save(&ctx.dirs).map_err(|e| anyhow!(e))?;
    println!("vault {name} {}", if on { "enabled" } else { "disabled" });
    service::config_changed(&ctx.config_file, &Change::Vault(name.to_string()));
    Ok(0)
}

fn print_vault(ctx: &Setup, name: &str, v: &Vault) {
    println!("{name}{}", if v.enabled { "" } else { "  (disabled)" });
    println!("  account   {}", v.account);
    println!("  database  {}", v.database);
    println!("  folder    {}", v.folder_path(&ctx.dirs).display());
    println!("  state     {}", v.state_path(name, &ctx.dirs).display());
    match v.scope() {
        Ok(s) => println!("  folders   {}", s.describe()),
        Err(e) => println!("  folders   INVALID: {e}"),
    }
    println!("  mode      {}", v.mode.as_str());
    if let Some(d) = &v.device_name {
        println!("  device    {d}");
    }
}

/// `lsync configure` for a vault in `lsync.toml` (or a new one): the given
/// settings (flags and environment, as `configure` always took them) go
/// into the vault and its account, the password into the account's store.
/// Returns the file written.
pub fn configure(ctx: &Setup, sel: &Selected, o: &Overrides, mut password: Option<Secret>) -> Result<PathBuf> {
    let name = sel.name.clone();
    check_name("vault", &name).map_err(|e| anyhow!(e))?;
    let mut file = open(ctx)?;
    let cfg = file.config(&ctx.dirs).map_err(|e| anyhow!(e))?;
    let old = cfg.vault.get(&name).cloned();
    let old_account = old
        .as_ref()
        .map(|v| (v.account.clone(), cfg.account[&v.account].clone()));
    let mut url = o
        .url
        .clone()
        .or_else(|| old_account.as_ref().map(|(_, a)| a.url.clone()))
        .ok_or_else(|| anyhow!("a new vault needs --url, --db and --folder"))?;
    let mut user = match &o.username {
        Some(u) => Some(u.clone()).filter(|s| !s.is_empty()),
        None => old_account.as_ref().and_then(|(_, a)| a.username.clone()),
    };
    // A user and password in the URL go where lsync.toml keeps them (the
    // account and its secret store), as the remote client would read them:
    // --user and the password given win.
    if let Some(u) = split_userinfo(&url) {
        url = u.url;
        if o.username.as_deref().is_none_or(|s| s.is_empty()) {
            user = u.username.or(user);
        }
        if password.is_none() {
            password = u.password.map(Secret::new);
        }
    }
    let stores = ctx.secrets();
    let wanted = (server_key(&url), user.clone().unwrap_or_default());
    let key_of = |a: &Account| (a.server_key(), a.username.clone().unwrap_or_default());
    // The account: unchanged, another one for this server and user, the
    // vault's own one edited, or a new one.
    let account = match &old_account {
        Some((n, a)) if key_of(a) == wanted => n.clone(),
        _ => match cfg.account.iter().find(|(_, a)| key_of(a) == wanted) {
            Some((n, _)) => n.clone(),
            None => match &old_account {
                Some((n, _)) if cfg.vaults_of(n) == vec![name.as_str()] => {
                    file.set("account", n, "url", Some(url.trim().into()))
                        .map_err(|e| anyhow!(e))?;
                    file.set("account", n, "username", user.clone().map(Into::into))
                        .map_err(|e| anyhow!(e))?;
                    n.clone()
                }
                _ => {
                    // A name whose key holds nothing yet: never a password
                    // left in secrets.toml by an earlier account.
                    let base = migrate::account_base(&url);
                    let mut n = None;
                    for c in (1..100).map(|i| if i == 1 { base.clone() } else { format!("{base}-{i}") }) {
                        let r = SecretRef::file(&c);
                        let used = cfg.account.contains_key(&c)
                            || cfg
                                .account
                                .iter()
                                .any(|(an, a)| a.secret_ref(an).ok().as_ref() == Some(&r));
                        if !used && stores.get(&r)?.is_none() {
                            n = Some(c);
                            break;
                        }
                    }
                    let n = n.ok_or_else(|| anyhow!("no free account name for the server {base}"))?;
                    file.add_account(
                        &n,
                        &Account {
                            id: Some(new_id()),
                            url: url.trim().to_string(),
                            username: user.clone(),
                            secret: Some(format!("file:{n}")),
                        },
                        None,
                    )
                    .map_err(|e| anyhow!(e))?;
                    n
                }
            },
        },
    };
    match &old {
        Some(v) => {
            if v.account != account {
                file.set("vault", &name, "account", Some(account.as_str().into()))
                    .map_err(|e| anyhow!(e))?;
            }
        }
        None => {
            let (Some(db), Some(folder)) = (&o.database, &o.folder) else {
                bail!("a new vault needs --url, --db and --folder");
            };
            let mut v = Vault::new(&account, db.trim(), &folder_value(folder)?);
            v.id = Some(new_id());
            file.add_vault(&name, &v, None).map_err(|e| anyhow!(e))?;
        }
    }
    let set = |file: &mut ConfigFile, key: &str, value: Option<toml_edit::Value>| {
        file.set("vault", &name, key, value).map_err(|e| anyhow!(e))
    };
    if let Some(d) = &o.database {
        set(&mut file, "database", Some(d.trim().into()))?;
    }
    if let Some(f) = &o.folder {
        set(&mut file, "folder", Some(folder_value(f)?.into()))?;
    }
    if let Some(s) = &o.state_dir {
        set(&mut file, "state_dir", Some(folder_value(s)?.into()))?;
    }
    if let Some(r) = &o.ignore_regex {
        set(
            &mut file,
            "ignore_regex",
            Some(r.clone()).filter(|s| !s.is_empty()).map(Into::into),
        )?;
    }
    if let Some(d) = &o.device_name {
        set(
            &mut file,
            "device_name",
            Some(d.clone()).filter(|s| !s.is_empty()).map(Into::into),
        )?;
    }
    if let Some(n) = o.max_size_mb {
        set(&mut file, "max_size_mb", Some((n as i64).into()))?;
    }
    if let Some(n) = o.concurrency {
        set(&mut file, "concurrency", Some((n as i64).into()))?;
    }
    if !o.include.is_empty() {
        set(&mut file, "include", Some(string_array(&o.include)))?;
        set(&mut file, "exclude", None)?;
    } else if !o.exclude.is_empty() {
        set(&mut file, "exclude", Some(string_array(&o.exclude)))?;
        set(&mut file, "include", None)?;
    } else if o.all_folders {
        set(&mut file, "include", None)?;
        set(&mut file, "exclude", None)?;
    }
    let checked = file.config(&ctx.dirs).map_err(|e| anyhow!(e))?;
    let r = checked.account[&account].secret_ref(&account).map_err(|e| anyhow!(e))?;
    let mut previous = None;
    if let Some(pw) = &password {
        if !stores.writable(&r) {
            bail!(
                "account {account}'s password comes from the {}; it is not stored by lsync",
                stores.describe(&r)
            );
        }
        previous = Some(stores.get(&r)?);
        stores.set(&r, pw)?;
    }
    if let Err(e) = file.save(&ctx.dirs) {
        // Undo the password: back to what it was, or gone if it is new.
        if let Some(previous) = previous {
            undo_password(&stores, &r, previous.as_ref());
        }
        bail!(e);
    }
    service::config_changed(&ctx.config_file, &Change::Vault(name));
    Ok(ctx.config_file.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use livesync_host::secret::FileStore;

    #[test]
    fn a_rollback_restores_a_replaced_password_and_removes_only_a_new_key() {
        let d = tempfile::tempdir().unwrap();
        let stores = SecretStores::new(vec![Box::new(FileStore::new(d.path().join("secrets.toml")))]);
        let (home, new) = (SecretRef::file("home"), SecretRef::file("new"));
        stores.set(&home, &Secret::new("pw-home")).unwrap();
        // Replaced, then the command failed: the old password is back.
        stores.set(&home, &Secret::new("pw-other")).unwrap();
        undo_password(&stores, &home, Some(&Secret::new("pw-home")));
        assert_eq!(stores.get(&home).unwrap().unwrap().expose(), "pw-home");
        // Created, then failed: only that key goes.
        stores.set(&new, &Secret::new("pw-new")).unwrap();
        undo_password(&stores, &new, None);
        assert_eq!(stores.get(&new).unwrap(), None);
        assert_eq!(stores.get(&home).unwrap().unwrap().expose(), "pw-home");
    }
}
