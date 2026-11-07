//! Changing `lsync.toml` in place: comments, blank lines and key order are
//! kept, a change is checked as a whole before it is written, and the file
//! is replaced atomically under a lock, so two commands at once never lose
//! each other's change.

use crate::config::{Account, Config, Dirs, Vault};
use livesync_engine::platform::{LockError, LockFile};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use toml_edit::{DocumentMut, Item, Table};

/// Write a file atomically with mode 0600 (in a 0700 folder when it has to
/// create it): a crash leaves the old file or the new one, never half.
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.exists() {
            let mut b = std::fs::DirBuilder::new();
            b.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                b.mode(0o700);
            }
            b.create(dir)?;
        }
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    {
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
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Some(dir) = path.parent() {
        // The rename is durable once the folder is.
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// How long a change waits for another one to finish.
const LOCK_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(10)
};

/// An exclusive lock on a config file, held while it is read, changed and
/// written (`.<name>.lock` beside it).
pub struct ConfigLock {
    _lock: LockFile,
}

impl ConfigLock {
    pub fn acquire(path: &Path) -> Result<ConfigLock, String> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let lock_path = path.with_file_name(format!(".{name}.lock"));
        if let Some(dir) = path.parent() {
            if !dir.exists() {
                write_private_dir(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
            }
        }
        let end = Instant::now() + LOCK_WAIT;
        loop {
            match LockFile::try_acquire(&lock_path) {
                Ok(l) => return Ok(ConfigLock { _lock: l }),
                Err(LockError::Busy(_)) if Instant::now() < end => std::thread::sleep(Duration::from_millis(50)),
                Err(LockError::Busy(_)) => {
                    return Err(format!(
                        "{} is being changed by another lsync; try again",
                        path.display()
                    ))
                }
                Err(e) => return Err(format!("locking {}: {e}", lock_path.display())),
            }
        }
    }
}

fn write_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

const HEADER: &str = "# lsync accounts and vaults. Passwords are not kept here: see `lsync account ls`.\n# `lsync account ...` and `lsync vault ...` edit this file and keep your comments.\n";

/// `lsync.toml` opened for a change.
pub struct ConfigFile {
    path: PathBuf,
    doc: DocumentMut,
    /// For a new file: the comment that goes above its first entry.
    header: Option<String>,
    _lock: ConfigLock,
}

impl ConfigFile {
    /// Open (or start) the file, locked until this is dropped.
    pub fn open(path: &Path) -> Result<ConfigFile, String> {
        let lock = ConfigLock::acquire(path)?;
        let (doc, header) = match std::fs::read_to_string(path) {
            Ok(t) => (
                t.parse::<DocumentMut>()
                    .map_err(|e| format!("{}: {}", path.display(), crate::toml_error(&t, e.message(), e.span())))?,
                None,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (DocumentMut::new(), Some(HEADER.to_string())),
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        Ok(ConfigFile {
            path: path.to_path_buf(),
            doc,
            header,
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// More comment lines for the top of a new file (no `#` needed).
    pub fn add_header_note(&mut self, note: &str) {
        if let Some(h) = &mut self.header {
            h.push('\n');
            for line in note.lines() {
                h.push_str("# ");
                h.push_str(line);
                h.push('\n');
            }
        }
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// The model as the edited text says, checked.
    pub fn config(&self, dirs: &Dirs) -> Result<Config, String> {
        Config::parse(&self.doc.to_string(), dirs)
    }

    fn section(&mut self, kind: &str) -> &mut Table {
        if !self.doc.contains_key(kind) {
            let mut t = Table::new();
            t.set_implicit(true);
            self.doc.insert(kind, Item::Table(t));
        }
        self.doc[kind].as_table_mut().expect("a section is a table")
    }

    fn entry(&mut self, kind: &str, name: &str) -> Option<&mut Table> {
        self.doc.get_mut(kind)?.as_table_mut()?.get_mut(name)?.as_table_mut()
    }

    pub fn has(&self, kind: &str, name: &str) -> bool {
        self.doc
            .get(kind)
            .and_then(|t| t.as_table())
            .is_some_and(|t| t.contains_key(name))
    }

    fn insert<T: serde::Serialize>(
        &mut self,
        kind: &str,
        name: &str,
        value: &T,
        comment: Option<&str>,
    ) -> Result<(), String> {
        if self.has(kind, name) {
            return Err(format!("there is already a {kind} named {name:?}"));
        }
        let text = toml::to_string(value).map_err(|e| e.to_string())?;
        let mut t = text
            .parse::<DocumentMut>()
            .map_err(|e| e.to_string())?
            .as_table()
            .clone();
        t.set_implicit(false);
        let mut prefix = match self.header.take() {
            Some(h) => format!("{h}\n"),
            None => String::from("\n"),
        };
        if let Some(c) = comment {
            for line in c.lines() {
                prefix.push_str("# ");
                prefix.push_str(line);
                prefix.push('\n');
            }
        }
        t.decor_mut().set_prefix(prefix);
        self.section(kind).insert(name, Item::Table(t));
        Ok(())
    }

    pub fn add_account(&mut self, name: &str, a: &Account, comment: Option<&str>) -> Result<(), String> {
        self.insert("account", name, a, comment)
    }

    pub fn add_vault(&mut self, name: &str, v: &Vault, comment: Option<&str>) -> Result<(), String> {
        self.insert("vault", name, v, comment)
    }

    /// Set (`Some`) or remove (`None`) one key of an account or vault. A
    /// replaced value keeps its line's comments.
    pub fn set(&mut self, kind: &str, name: &str, key: &str, value: Option<toml_edit::Value>) -> Result<(), String> {
        let t = self
            .entry(kind, name)
            .ok_or_else(|| format!("there is no {kind} named {name:?}"))?;
        match value {
            None => {
                t.remove(key);
            }
            Some(mut v) => match t.get_mut(key) {
                // Assigned in place: the key, and the comments above it, stay.
                Some(slot) => {
                    if let Some(old) = slot.as_value() {
                        *v.decor_mut() = old.decor().clone();
                    }
                    *slot = Item::Value(v);
                }
                None => {
                    t.insert(key, Item::Value(v));
                }
            },
        }
        Ok(())
    }

    pub fn remove(&mut self, kind: &str, name: &str) -> Result<(), String> {
        let removed = self
            .doc
            .get_mut(kind)
            .and_then(|t| t.as_table_mut())
            .and_then(|t| t.remove(name));
        match removed {
            Some(_) => Ok(()),
            None => Err(format!("there is no {kind} named {name:?}")),
        }
    }

    /// Check the whole file as it would be written, then write it.
    pub fn save(&self, dirs: &Dirs) -> Result<Config, String> {
        let cfg = self.config(dirs)?;
        write_private(&self.path, &self.doc.to_string())
            .map_err(|e| format!("writing {}: {e}", self.path.display()))?;
        Ok(cfg)
    }

    pub fn text(&self) -> String {
        self.doc.to_string()
    }
}

/// A string array value for `include`/`exclude`.
pub fn string_array(items: &[String]) -> toml_edit::Value {
    let mut a = toml_edit::Array::new();
    for i in items {
        a.push(i.as_str());
    }
    toml_edit::Value::Array(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(d: &Path) -> Dirs {
        Dirs {
            home: Some(d.to_path_buf()),
            config: d.join("config/lsync"),
            state: d.join("state/lsync"),
        }
    }

    #[test]
    fn edits_keep_comments_and_layout() {
        let d = tempfile::tempdir().unwrap();
        let dirs = dirs(d.path());
        let path = dirs.config_file();
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(
            &path,
            "# my machine\n\n[account.home]\n# the home server\nurl = \"https://couch.example.net\"  # behind the proxy\nusername = \"me\"\n\n[vault.notes]\naccount = \"home\"\n# which db\ndatabase = \"notes\"\nfolder = \"~/Notes\"\n",
        )
        .unwrap();
        let mut f = ConfigFile::open(&path).unwrap();
        f.set("vault", "notes", "database", Some("notes2".into())).unwrap();
        f.set("account", "home", "url", Some("https://couch.example.org".into()))
            .unwrap();
        f.add_vault("work", &Vault::new("home", "work", "~/Work"), Some("added by a test"))
            .unwrap();
        f.save(&dirs).unwrap();
        // The lock is held until the file is dropped.
        assert!(ConfigFile::open(&path).is_err());
        drop(f);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# my machine\n"), "{text}");
        assert!(
            text.contains("# the home server\nurl = \"https://couch.example.org\"  # behind the proxy\n"),
            "{text}"
        );
        assert!(text.contains("# which db\ndatabase = \"notes2\"\n"), "{text}");
        assert!(
            text.contains("\n# added by a test\n[vault.work]\naccount = \"home\"\n"),
            "{text}"
        );

        let mut f = ConfigFile::open(&path).unwrap();
        f.remove("vault", "work").unwrap();
        f.set("vault", "notes", "exclude", Some(string_array(&["Archive".into()])))
            .unwrap();
        f.save(&dirs).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("vault.work") && !text.contains("added by a test"),
            "{text}"
        );
        assert!(text.contains("exclude = [\"Archive\"]"), "{text}");
    }

    #[test]
    fn an_invalid_change_is_not_written() {
        let d = tempfile::tempdir().unwrap();
        let dirs = dirs(d.path());
        let path = dirs.config_file();
        let mut f = ConfigFile::open(&path).unwrap();
        f.add_vault("v", &Vault::new("nobody", "db", "/srv/v"), None).unwrap();
        let e = f.save(&dirs).unwrap_err();
        assert!(e.contains("no account"), "{e}");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let dirs = dirs(d.path());
        let mut f = ConfigFile::open(&dirs.config_file()).unwrap();
        f.add_account(
            "home",
            &Account {
                id: None,
                url: "https://couch.example.net".into(),
                username: None,
                secret: None,
            },
            None,
        )
        .unwrap();
        f.save(&dirs).unwrap();
        let md = std::fs::metadata(dirs.config_file()).unwrap();
        assert_eq!(md.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(&dirs.config).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
