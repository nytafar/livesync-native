//! Changing `lsync.toml` in place: comments, blank lines and key order are
//! kept, a change is checked as a whole before it is written, and the file
//! is replaced atomically under a lock, so two commands at once never lose
//! each other's change.

use crate::config::{Account, Config, Dirs, Migration, Vault};
use livesync_engine::platform::{LockError, LockFile};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use toml_edit::{DocumentMut, Item, Table};

/// Write a file atomically with mode 0600 (in a 0700 folder when it has to
/// create it): a crash leaves the old file or the new one, never half.
///
/// Root writing into a folder of root's that is shared read-only with a
/// group (0750, the system service's config folder) writes the file 0640
/// with that group, so the service can read what `sudo lsync --system ...`
/// wrote, and never change it (see [`shared_group`]).
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    write_private_as(path, text, root_uid())
}

/// `Some(0)` when this process is root (see [`shared_group`]).
fn root_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        (unsafe { libc::geteuid() } == 0).then_some(0)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// [`write_private`], with `root` as the uid that counts as root (the
/// tests' own uid stands in for it).
fn write_private_as(path: &Path, text: &str, root: Option<u32>) -> std::io::Result<()> {
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
    write_then_rename(&tmp, path, root, |f| {
        f.write_all(text.as_bytes())?;
        f.sync_all()
    })?;
    if let Some(dir) = path.parent() {
        // The rename is durable once the folder is.
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Write `tmp` (0600, created fresh) with `fill`, then rename it to `path`.
/// On any error the temp file is removed: it may hold secrets.
fn write_then_rename(
    tmp: &Path,
    path: &Path,
    root: Option<u32>,
    fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    // A leftover from a crashed run with the same pid: never write into a
    // file whose mode we did not set.
    match std::fs::remove_file(tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(tmp)?;
    let done = share_with_folder_group(&f, path.parent(), root)
        .and_then(|()| fill(&mut f))
        .and_then(|()| {
            drop(f);
            std::fs::rename(tmp, path)
        });
    if done.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    done
}

/// The group that may read the files root writes in `dir`: the folder's
/// group when the folder is root's (`root`, the uid that counts as root)
/// and its mode is exactly `rwxr-x---` with a group other than root's. Its
/// owner alone can change the folder, so nothing in that metadata comes
/// from anyone but root. `None` for every other folder, and when this
/// process is not root (`root` is `None`): a user's own files stay 0600.
#[cfg(unix)]
pub fn shared_group(dir: &Path, root: Option<u32>) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let root = root?;
    let md = std::fs::symlink_metadata(dir).ok()?;
    (md.is_dir() && md.uid() == root && md.gid() != 0 && md.mode() & 0o7777 == 0o750).then(|| md.gid())
}

/// The new file `f` (ours, just created) readable by the folder's group:
/// mode 0640 and that group, when [`shared_group`] says so.
fn share_with_folder_group(f: &std::fs::File, dir: Option<&Path>, root: Option<u32>) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(gid) = dir.and_then(|d| shared_group(d, root)) {
        use std::os::unix::fs::PermissionsExt;
        std::os::unix::fs::fchown(f, None, Some(gid))?;
        f.set_permissions(std::fs::Permissions::from_mode(0o640))?;
    }
    #[cfg(not(unix))]
    let _ = (f, dir, root);
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
            // Never through a symlink, and a new lock file is created with
            // O_EXCL: root takes this lock beside the system config.
            #[cfg(unix)]
            let taken = LockFile::try_acquire_no_follow(&lock_path);
            #[cfg(not(unix))]
            let taken = LockFile::try_acquire(&lock_path);
            match taken {
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
        Config::parse(&self.text(), dirs)
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

    /// Write `[migration]` (a new file only: it goes first, under the header).
    pub fn set_migration(&mut self, m: &Migration) -> Result<(), String> {
        let text = toml::to_string(m).map_err(|e| e.to_string())?;
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
        prefix.push_str("# Which profile files were migrated, and which were left (read as before).\n");
        t.decor_mut().set_prefix(prefix);
        self.doc.insert("migration", Item::Table(t));
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

    /// Remove an account or vault. The comment block right above its header
    /// goes with it; comments set off from it by a blank line (the file's
    /// header, the migration note, a note on a group of entries) stay,
    /// above the next entry or at the end of the file.
    pub fn remove(&mut self, kind: &str, name: &str) -> Result<(), String> {
        let order = self.table_order();
        let key_prefix = self
            .doc
            .get(kind)
            .and_then(|t| t.as_table())
            .and_then(|t| t.key(name))
            .map(|k| decor_prefix(k.leaf_decor()))
            .filter(|p| p.contains('\n'))
            .unwrap_or_default();
        let removed = self
            .doc
            .get_mut(kind)
            .and_then(|t| t.as_table_mut())
            .and_then(|t| t.remove(name));
        let Some(removed) = removed else {
            return Err(format!("there is no {kind} named {name:?}"));
        };
        let table_prefix = removed.as_table().map(|t| decor_prefix(t.decor())).unwrap_or_default();
        let kept = file_comments(&format!("{key_prefix}{table_prefix}"));
        if kept.is_empty() {
            return Ok(());
        }
        let here = order.iter().position(|p| p[0] == kind && p[1] == name);
        // Not the first entry: a blank line before the comments, as before.
        let kept = if here.is_some_and(|i| i > 0) {
            format!("\n{kept}")
        } else {
            kept
        };
        let next = here.and_then(|i| order.get(i + 1)).cloned();
        let next = next.and_then(|[k, n]| {
            let section = self.doc.get_mut(&k)?.as_table_mut()?;
            // The writer puts a key prefix with a line break before the
            // table's own prefix: the comments go before both.
            let key_has_lines = section
                .key(&n)
                .is_some_and(|k| decor_prefix(k.leaf_decor()).contains('\n'));
            if key_has_lines {
                let mut key = section.key_mut(&n)?;
                let old = decor_prefix(key.leaf_decor());
                key.leaf_decor_mut().set_prefix(join_comments(&kept, &old));
            } else {
                let t = section.get_mut(&n)?.as_table_mut()?;
                let old = decor_prefix(t.decor());
                t.decor_mut().set_prefix(join_comments(&kept, &old));
            }
            Some(())
        });
        if next.is_none() {
            let old = self.doc.trailing().as_str().unwrap_or("").to_string();
            // The entry came before what is already at the end.
            self.doc.set_trailing(format!("{kept}{old}"));
        }
        Ok(())
    }

    /// The `[kind.name]` tables in the order they are written.
    fn table_order(&self) -> Vec<[String; 2]> {
        let mut out: Vec<(isize, [String; 2])> = Vec::new();
        let mut last = 0;
        for (kind, item) in self.doc.iter() {
            let Some(section) = item.as_table() else { continue };
            if let Some(p) = section.position() {
                last = p;
            }
            for (name, entry) in section.iter() {
                if let Some(t) = entry.as_table() {
                    if let Some(p) = t.position() {
                        last = p;
                    }
                    out.push((last, [kind.to_string(), name.to_string()]));
                }
            }
        }
        // Stable, as the writer sorts them.
        out.sort_by_key(|(p, _)| *p);
        out.into_iter().map(|(_, n)| n).collect()
    }

    /// Check the whole file as it would be written, then write it.
    pub fn save(&self, dirs: &Dirs) -> Result<Config, String> {
        let cfg = self.config(dirs)?;
        write_private(&self.path, &self.text()).map_err(|e| format!("writing {}: {e}", self.path.display()))?;
        Ok(cfg)
    }

    /// The file as it would be written (a new file with no entry yet is
    /// its header alone).
    pub fn text(&self) -> String {
        match &self.header {
            Some(h) => format!("{h}{}", self.doc),
            None => self.doc.to_string(),
        }
    }
}

fn decor_prefix(d: &toml_edit::Decor) -> String {
    d.prefix().and_then(|p| p.as_str()).unwrap_or("").to_string()
}

/// Of the text above an entry's header, the part that belongs to the file
/// rather than the entry: everything up to the last blank line, when it
/// holds a comment ("" otherwise). Ends with one line break.
fn file_comments(prefix: &str) -> String {
    let lines: Vec<&str> = prefix.split_inclusive('\n').collect();
    let Some(blank) = lines.iter().rposition(|l| l.trim().is_empty() && l.ends_with('\n')) else {
        return String::new();
    };
    let head: String = lines[..blank].concat();
    if !head.lines().any(|l| l.trim_start().starts_with('#')) {
        return String::new();
    }
    let head = head.trim_start_matches(['\n', '\r']).trim_end();
    format!("{head}\n")
}

/// `comments` above an entry whose own prefix is `own`, a blank line between.
fn join_comments(comments: &str, own: &str) -> String {
    let own = own.trim_start_matches(['\n', '\r']);
    format!("{comments}\n{own}")
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

    /// A group of this user's other than its own, when it has one: the
    /// group a chown without root can still change files to.
    #[cfg(unix)]
    fn another_group() -> Option<u32> {
        // SAFETY: getgroups with a buffer of the size it reports.
        let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        let mut gs = vec![0 as libc::gid_t; n.max(0) as usize];
        let n = unsafe { libc::getgroups(gs.len() as i32, gs.as_mut_ptr()) };
        let primary = unsafe { libc::getegid() };
        gs.truncate(n.max(0) as usize);
        gs.into_iter().find(|g| *g != primary)
    }

    /// A file root writes is shared with a group only in a folder of
    /// root's that is exactly 0750 with that group: the system service's
    /// config folder. The tests' own uid stands in for root.
    #[cfg(unix)]
    #[test]
    fn root_shares_a_file_only_in_its_own_folder_shared_read_only() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("etc-lsync");
        std::fs::create_dir(&dir).unwrap();
        let me = unsafe { libc::geteuid() };
        let mine = unsafe { libc::getegid() };
        let gid = another_group().unwrap_or(mine);
        std::os::unix::fs::chown(&dir, None, Some(gid)).unwrap();
        let file = dir.join("secrets.toml");
        let written = |mode: u32, root: Option<u32>| {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            write_private_as(&file, "\"home\" = \"pw\"\n", root).unwrap();
            let md = std::fs::metadata(&file).unwrap();
            (md.mode() & 0o777, md.gid())
        };
        if gid != mine {
            assert_eq!(written(0o750, Some(me)), (0o640, gid), "the service's group reads it");
        } else {
            eprintln!("this user has one group only: the shared group was not compared");
        }
        // Group-writable (the group could swap files), private, open to
        // all, another owner's, or not root at all: private, as always.
        for (mode, root) in [
            (0o770, Some(me)),
            (0o700, Some(me)),
            (0o755, Some(me)),
            (0o750, Some(me + 1)),
            (0o750, None),
        ] {
            assert_eq!(written(mode, root).0, 0o600, "{mode:o} {root:?}");
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "\"home\" = \"pw\"\n");
    }

    /// Finding C of the review: root's config lock beside the system
    /// config never follows a symlink planted at its name, so a dangling
    /// one (to `/etc/nologin`) does not make root create its target.
    #[cfg(unix)]
    #[test]
    fn the_config_lock_never_follows_a_planted_symlink() {
        let d = tempfile::tempdir().unwrap();
        let dirs = dirs(d.path());
        std::fs::create_dir_all(&dirs.config).unwrap();
        let nologin = d.path().join("nologin");
        std::os::unix::fs::symlink(&nologin, dirs.config.join(".lsync.toml.lock")).unwrap();
        let e = ConfigFile::open(&dirs.config_file()).err().unwrap();
        assert!(e.contains("symbolic link"), "{e}");
        assert!(!nologin.exists(), "nothing created through the link");
        assert!(!dirs.config_file().exists());
        // Through the secret store's lock as well.
        std::os::unix::fs::symlink(&nologin, dirs.config.join(".secrets.toml.lock")).unwrap();
        assert!(ConfigLock::acquire(&dirs.secrets_file()).is_err());
        assert!(!nologin.exists());
    }

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
    fn removing_an_entry_keeps_the_file_comments() {
        let d = tempfile::tempdir().unwrap();
        let dirs = dirs(d.path());
        let path = dirs.config_file();
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(
            &path,
            "# file header\n# second line\n\n# the home server\n[account.home]\nurl = \"https://couch.example.net\"\n\n# group note\n\n# work vault\n[vault.work]\naccount = \"home\"\ndatabase = \"work\"\nfolder = \"/srv/work\"\n\n# notes vault\n[vault.notes]\naccount = \"home\"\ndatabase = \"notes\"\nfolder = \"/srv/notes\"\n",
        )
        .unwrap();
        let mut f = ConfigFile::open(&path).unwrap();
        f.remove("vault", "work").unwrap();
        f.save(&dirs).unwrap();
        let text = f.text();
        assert!(!text.contains("# work vault"), "{text}");
        assert!(
            text.contains("\n\n# group note\n\n# notes vault\n[vault.notes]"),
            "{text}"
        );
        f.remove("vault", "notes").unwrap();
        f.remove("account", "home").unwrap();
        f.save(&dirs).unwrap();
        drop(f);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# file header\n# second line\n"), "{text}");
        assert!(text.contains("# group note"), "{text}");
        assert!(!text.contains("home server") && !text.contains("notes vault"), "{text}");
        assert_eq!(Config::parse(&text, &dirs).unwrap(), Config::default());

        // A file lsync started: its header and migration note stay too.
        std::fs::remove_file(&path).unwrap();
        let mut f = ConfigFile::open(&path).unwrap();
        f.add_header_note("Migrated from a.toml.");
        f.add_account(
            "home",
            &Account {
                id: None,
                url: "https://couch.example.net".into(),
                username: None,
                secret: None,
            },
            Some("own comment"),
        )
        .unwrap();
        f.save(&dirs).unwrap();
        f.remove("account", "home").unwrap();
        let text = f.text();
        assert!(text.starts_with("# lsync accounts and vaults."), "{text}");
        assert!(text.contains("# Migrated from a.toml.\n"), "{text}");
        assert!(!text.contains("own comment"), "{text}");
    }

    #[test]
    fn a_failed_write_leaves_no_temp_file() {
        let d = tempfile::tempdir().unwrap();
        let tmp = d.path().join(".secrets.toml.tmp-1");
        let path = d.path().join("secrets.toml");
        let e = write_then_rename(&tmp, &path, None, |f| {
            f.write_all(b"a = \"secret\"\n")?;
            Err(std::io::Error::other("disk full"))
        });
        assert!(e.is_err());
        assert!(!tmp.exists() && !path.exists());
        // The rename fails: the target is a folder with something in it.
        std::fs::create_dir_all(path.join("x")).unwrap();
        assert!(write_then_rename(&tmp, &path, None, |f| f.write_all(b"a = \"secret\"\n")).is_err());
        assert!(!tmp.exists());
        // A leftover temp file is replaced, never written into.
        std::fs::remove_dir_all(&path).unwrap();
        std::fs::write(&tmp, "old").unwrap();
        write_then_rename(&tmp, &path, None, |f| f.write_all(b"new")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!tmp.exists());
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
