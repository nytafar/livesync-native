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
    write_then_rename(&tmp, path, |f| {
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
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        hand_to_folder_owner(&f, dir)?;
    }
    let done = fill(&mut f).and_then(|()| {
        drop(f);
        std::fs::rename(tmp, path)
    });
    if done.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    done
}

/// The owner a file root writes into `dir` should have: the folder's owner,
/// when that is another user. A system service's config folder belongs to
/// the service's user (`lsync service install --system`), so a
/// `sudo lsync --system vault add` leaves files that the service can read.
/// `None` for everyone else, and for a folder of root's.
#[cfg(unix)]
pub fn folder_owner_for_root(dir: &Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let md = std::fs::metadata(dir).ok()?;
    (md.uid() != 0).then(|| (md.uid(), md.gid()))
}

/// Give the plain file `name` in `dir` to `uid:gid`, as root may safely do
/// in a folder another user can write: the name is opened without following
/// a symlink, and only a regular file with a single link is changed, so a
/// hard link that user planted to a file of root's (`ln /etc/shadow x`) is
/// left alone. `Ok(false)` when there is no such file, or it is not one to
/// change.
#[cfg(unix)]
pub fn chown_plain_file(dir: &Path, name: &str, uid: u32, gid: u32) -> std::io::Result<bool> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::MetadataExt;
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("not a file name: {name:?}"),
        ));
    }
    let cname = std::ffi::CString::new(name).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let d = std::fs::File::open(dir)?;
    // SAFETY: a valid folder fd and a NUL-terminated name. O_NONBLOCK: a
    // FIFO planted under the name does not block the open.
    let fd = unsafe {
        libc::openat(
            d.as_raw_fd(),
            cname.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::ENOENT) | Some(libc::ELOOP) => Ok(false),
            _ => Err(e),
        };
    }
    // SAFETY: openat returned a new fd that nothing else owns.
    let f = unsafe { std::fs::File::from_raw_fd(fd) };
    let md = f.metadata()?;
    if !md.file_type().is_file() || md.nlink() != 1 {
        return Ok(false);
    }
    std::os::unix::fs::fchown(&f, Some(uid), Some(gid))?;
    Ok(true)
}

#[cfg(unix)]
fn hand_to_folder_owner(f: &std::fs::File, dir: &Path) -> std::io::Result<()> {
    if let Some((uid, gid)) = folder_owner_for_root(dir) {
        std::os::unix::fs::fchown(f, Some(uid), Some(gid))?;
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
        // Root hands over only a lock file it creates here, never one that
        // was there (the folder's owner could have made it a hard link).
        let existed = std::fs::symlink_metadata(&lock_path).is_ok();
        let end = Instant::now() + LOCK_WAIT;
        loop {
            match LockFile::try_acquire(&lock_path) {
                Ok(l) => {
                    // As the files beside it (see `folder_owner_for_root`).
                    #[cfg(unix)]
                    if let (false, Some(dir)) = (existed, path.parent()) {
                        if let Some((uid, gid)) = folder_owner_for_root(dir) {
                            let _ = chown_plain_file(dir, &format!(".{name}.lock"), uid, gid);
                        }
                    }
                    return Ok(ConfigLock { _lock: l });
                }
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

    #[cfg(unix)]
    #[test]
    fn root_hands_over_only_plain_single_link_files() {
        use std::os::unix::fs::MetadataExt;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("conf");
        std::fs::create_dir(&dir).unwrap();
        let outside = d.path().join("shadow");
        std::fs::write(&outside, "root's").unwrap();
        std::fs::write(dir.join("lsync.toml"), "x").unwrap();
        // What the folder's owner could plant for root to chown.
        std::os::unix::fs::symlink(&outside, dir.join("secrets.toml")).unwrap();
        std::fs::hard_link(&outside, dir.join("hard")).unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let uid = unsafe { libc::geteuid() };
        let gid = another_group().unwrap_or_else(|| unsafe { libc::getegid() });
        let before = std::fs::metadata(&outside).unwrap().gid();
        assert!(chown_plain_file(&dir, "lsync.toml", uid, gid).unwrap());
        assert!(!chown_plain_file(&dir, "secrets.toml", uid, gid).unwrap(), "a symlink");
        assert!(!chown_plain_file(&dir, "hard", uid, gid).unwrap(), "a second link");
        assert!(!chown_plain_file(&dir, "sub", uid, gid).unwrap(), "a folder");
        assert!(!chown_plain_file(&dir, "missing", uid, gid).unwrap());
        assert!(chown_plain_file(&dir, "../shadow", uid, gid).is_err());
        assert_eq!(std::fs::metadata(dir.join("lsync.toml")).unwrap().gid(), gid);
        assert_eq!(std::fs::metadata(&outside).unwrap().gid(), before, "untouched");
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
        let e = write_then_rename(&tmp, &path, |f| {
            f.write_all(b"a = \"secret\"\n")?;
            Err(std::io::Error::other("disk full"))
        });
        assert!(e.is_err());
        assert!(!tmp.exists() && !path.exists());
        // The rename fails: the target is a folder with something in it.
        std::fs::create_dir_all(path.join("x")).unwrap();
        assert!(write_then_rename(&tmp, &path, |f| f.write_all(b"a = \"secret\"\n")).is_err());
        assert!(!tmp.exists());
        // A leftover temp file is replaced, never written into.
        std::fs::remove_dir_all(&path).unwrap();
        std::fs::write(&tmp, "old").unwrap();
        write_then_rename(&tmp, &path, |f| f.write_all(b"new")).unwrap();
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
