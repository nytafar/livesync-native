//! Taking over a folder that is already in sync with the remote (spec D15,
//! `lsync adopt`).
//!
//! A vault folder another client kept in sync (the LiveSync plugin, the
//! official CLI) holds most of the remote's files already. A first sync from
//! an empty index would fetch every one of their chunks to compare them with
//! the files. `Engine::adopt` compares without that: it lists the remote's
//! file documents (metadata only, as `folders`), splits each local file the
//! way a push would, and calls the two identical when the chunk lists and
//! the type agree, as the push side does (`same_content`). Only where the
//! lists differ (another splitter, or another content) are the chunks
//! fetched, to compare the content itself as the first pull would.
//!
//! The report says exactly what the first sync will do with every file:
//!
//! - **identical**: adopted, with no transfer and no write;
//! - **differing**: no common base, so the first sync stores the local
//!   version as an independent root beside the remote's (`write_root`): a
//!   conflict with no common ancestor;
//! - **only local**: pushed as new documents;
//! - **only remote**: written into the folder;
//! - **deleted on the remote** while the file is still here: put back,
//!   removed locally, or kept as a conflict, by the reconcile rules of the
//!   first full pass (`write_unknown_ancestry`);
//! - **name differs** from the remote's spelling only in case or in Unicode
//!   normalisation (NFD here, NFC on the remote): refused, since the two
//!   names would become two files;
//! - what neither side syncs (too large, unwritable names, collisions).
//!
//! With `apply`, the identical files are recorded in the index as synced at
//! the remote's revision. Nothing is written to the remote or to the folder;
//! the first sync then finds them unchanged and fetches nothing for them.
//! A file is only recorded when every chunk of the remote's revision is on
//! the remote (or in the document's eden): a later push uploads only the
//! chunks its base lacks, so a missing one would never be put back.
//!
//! A real take-over fails closed: it refuses the plugin still enabled in the
//! folder (unless allowed, as two-way sync does), a folder that matches
//! nothing, and a weak match (see [`AdoptReport::weak_match`]) unless
//! [`AdoptOptions::accept_weak_match`] says the folder is the right one.

use crate::engine::{conflict_note, Engine, EngineError};
use crate::filters::IgnoreState;
use crate::index::{FileRow, Origin, RevisionRow, State};
use crate::push::Found;
use crate::vaultfs::{self, DiskState};
use livesync_model::doc::{classify_doc, Classified};
use livesync_model::gate::Refusal;
use livesync_model::path::is_internal_id;
use livesync_model::splitter::chunk_file;
use livesync_model::FileDoc;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;
use tracing::info;

type Result<T> = std::result::Result<T, EngineError>;

/// How `Engine::adopt` runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdoptOptions {
    /// Record the identical files as synced (otherwise only compare).
    pub apply: bool,
    /// Take the folder over although few of its files match the remote's.
    pub accept_weak_match: bool,
    /// The vault only pulls: the first sync is a pull, which pushes nothing
    /// and applies no deletion to a file it never synced (the notes say so).
    pub pull_only: bool,
}

impl AdoptOptions {
    /// Compare and report only.
    pub fn dry_run() -> AdoptOptions {
        AdoptOptions::default()
    }

    /// Record the identical files, unless the report refuses.
    pub fn apply() -> AdoptOptions {
        AdoptOptions {
            apply: true,
            ..AdoptOptions::default()
        }
    }
}

/// One file in an adopt report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AdoptEntry {
    /// The path as the remote spells it, or the local path when the remote
    /// has no document for it.
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_rev: Option<String>,
    /// What the first sync does with it, where that is not implied by the list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What `Engine::adopt` found, and what the first sync will do.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AdoptReport {
    /// Whether the identical files were recorded in the index.
    pub applied: bool,
    /// Live remote files this vault syncs (after scope and filters).
    pub remote_files: u64,
    /// Local files this vault syncs.
    pub local_files: u64,
    /// Same content locally and on the remote: adopted with no transfer.
    pub identical: u64,
    /// Of `identical`, those whose chunk lists differed (another splitter),
    /// so their chunks were fetched to compare the content.
    pub identical_by_content: u64,
    /// Of `identical`, the empty files (they say nothing about the folder).
    pub identical_empty: u64,
    /// Why the match is too weak to take for granted that this is the
    /// remote's folder, if it is: a real adopt then needs
    /// `accept_weak_match`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weak_match: Option<String>,
    /// Different content on both sides: conflicts with no common ancestor.
    pub differing: Vec<AdoptEntry>,
    /// Only here: pushed as new.
    pub only_local: Vec<AdoptEntry>,
    /// Only on the remote: written into the folder.
    pub only_remote: Vec<AdoptEntry>,
    /// Deleted on the remote, still here (see each note).
    pub deleted_remotely: Vec<AdoptEntry>,
    /// The remote's document has conflicting revisions already (whatever
    /// list the file is in).
    pub conflicted_remotely: Vec<String>,
    /// The local name differs from the remote's only in case (same document,
    /// on a filesystem that tells them apart) or in Unicode normalisation.
    pub name_case: Vec<AdoptEntry>,
    /// The remote's content could not be read to compare, or a chunk of an
    /// identical file is missing on the remote: not adopted; the first sync
    /// keeps the local file and retries the remote.
    pub undecided: Vec<AdoptEntry>,
    /// Neither side syncs these (too large, a name this filesystem cannot
    /// hold, two local names for one document).
    pub not_synced: Vec<AdoptEntry>,
    /// Remote documents the vault does not sync, by reason.
    pub skipped_remote: BTreeMap<String, u64>,
    /// Folders and files that could not be read (nothing under them is
    /// compared).
    pub unreadable: Vec<AdoptEntry>,
    pub chunks_fetched: u64,
    pub chunk_bytes: u64,
    /// Response bytes read from the remote, listing included.
    pub bytes_fetched: u64,
    pub elapsed_ms: u64,
}

impl AdoptReport {
    /// Why taking the folder over must not go ahead, if it must not. A weak
    /// match refuses unless `accept_weak_match`.
    pub fn refusal(&self, accept_weak_match: bool) -> Option<String> {
        if !self.name_case.is_empty() {
            return Some(format!(
                "{} local file(s) differ from the remote's name only in case or Unicode normalisation (first: {} here, {} on the remote); rename them to the remote's spelling, or move them out of the folder",
                self.name_case.len(),
                self.name_case[0].local_path.as_deref().unwrap_or_default(),
                self.name_case[0].path
            ));
        }
        if !self.unreadable.is_empty() {
            return Some(format!(
                "{} folder(s) or file(s) could not be read (first: {}: {}); fix that first, so nothing is compared half",
                self.unreadable.len(),
                display_path(&self.unreadable[0].path),
                self.unreadable[0].note.as_deref().unwrap_or_default()
            ));
        }
        if self.identical == 0 && self.local_files > 0 && self.remote_files > 0 {
            return Some(format!(
                "none of the {} local files matches the remote's {} files; is this the right folder and database?",
                self.local_files, self.remote_files
            ));
        }
        match &self.weak_match {
            Some(why) if !accept_weak_match => Some(format!(
                "{why}; is this the right folder and database? If it is, take it over with --accept-weak-match"
            )),
            _ => None,
        }
    }

    /// Whether the match is weak: fewer than half of the files on the
    /// smaller side are identical, empty files not counted.
    fn assess_match(&mut self) {
        let matched = self.identical - self.identical_empty;
        let smaller = self.local_files.min(self.remote_files);
        self.weak_match = (smaller > 0 && matched * 2 < smaller).then(|| {
            format!(
                "only {matched} of the {} local and {} remote files match ({} identical, {} of them empty)",
                self.local_files, self.remote_files, self.identical, self.identical_empty
            )
        });
    }
}

fn display_path(p: &str) -> &str {
    if p.is_empty() {
        "."
    } else {
        p
    }
}

/// A local file whose remote document has other chunks: compared by content.
struct Candidate {
    doc: FileDoc,
    found: Found,
    disk: DiskState,
}

/// A local file read and split as a push would.
struct Read {
    disk: DiskState,
    children: Vec<String>,
    kind: livesync_model::FileKind,
    lossy: bool,
}

impl Engine {
    /// Compare the vault folder with the remote before its first sync (see
    /// the module docs); with `apply`, record the identical files as synced,
    /// unless `AdoptReport::refusal` says the take-over must not go ahead.
    /// Runs the gate (which never writes to the remote) and refuses an index
    /// that tracks files already, and with `apply` a folder whose LiveSync
    /// plugin is enabled (the plugin guard). Writes nothing to the remote or
    /// the folder.
    pub async fn adopt(&self, opts: AdoptOptions) -> Result<AdoptReport> {
        let start = Instant::now();
        if opts.apply {
            self.check_plugin_guard()?;
        }
        self.reset_report();
        self.connect_gate(false).await?;
        let tracked = self.with_index(|ix| ix.all_rows())?.len();
        if tracked > 0 {
            return Err(EngineError::Other(format!(
                "this vault's index already tracks {tracked} files: adopt is for a folder's first sync, and this one has had it (use `lsync sync`)"
            )));
        }
        let couch = self.inner.couch.clone();
        let rx0 = couch.bytes_received();
        let mut rep = AdoptReport::default();

        // The remote's file documents, as the first pull would sort them.
        let mut live: HashMap<String, FileDoc> = HashMap::new();
        let mut deleted: HashMap<String, FileDoc> = HashMap::new();
        let mut skipped_ids: HashMap<String, (FileDoc, String)> = HashMap::new();
        let filters = self.filters();
        filters.refresh();
        let scope = self.inner.cfg.scope.clone();
        for (start_key, end_key) in crate::folders::ranges() {
            let mut from = start_key.map(str::to_string);
            let mut cursor: Option<String> = None;
            loop {
                let rows = couch
                    .all_docs_page(from.as_deref(), end_key, crate::folders::PAGE, true)
                    .await?;
                let n = rows.len();
                let mut last = None;
                for r in rows {
                    let Some(id) = r.key else { continue };
                    if cursor.as_deref() == Some(id.as_str()) {
                        continue;
                    }
                    last = Some(id.clone());
                    let Some(doc) = r.doc.filter(|d| !d.is_null()) else {
                        continue;
                    };
                    let f = match classify_doc(&doc) {
                        Classified::File(f) => f,
                        Classified::Encrypted { id } => {
                            return Err(Refusal::EncryptedContent(format!("document {id}")).into())
                        }
                        Classified::LegacyNote { .. } => {
                            *rep.skipped_remote.entry("legacy-notes-doc".into()).or_default() += 1;
                            continue;
                        }
                        _ => continue,
                    };
                    if let Some(c) = f.children.iter().find(|c| c.starts_with("h:+")) {
                        return Err(Refusal::EncryptedContent(format!("chunk {c} of {}", f.path)).into());
                    }
                    if is_internal_id(&f.id) {
                        continue;
                    }
                    if !scope.contains(&f.path) {
                        *rep.skipped_remote.entry("out-of-scope".into()).or_default() += 1;
                        continue;
                    }
                    match filters.ignore_state(&f.path) {
                        IgnoreState::Ignored(_) => {
                            *rep.skipped_remote.entry("ignore-file".into()).or_default() += 1;
                            continue;
                        }
                        IgnoreState::Unknown { file, why } => {
                            if !f.deleted {
                                rep.undecided.push(AdoptEntry {
                                    path: f.path.clone(),
                                    remote_size: Some(f.size),
                                    remote_rev: Some(f.rev.clone()),
                                    note: Some(format!("ignore file {file} {why}; the first sync defers it")),
                                    ..Default::default()
                                });
                            }
                            continue;
                        }
                        IgnoreState::Synced { .. } => {}
                    }
                    if f.deleted {
                        deleted.insert(f.id.clone(), f);
                        continue;
                    }
                    match self.skip_reason(&f) {
                        Some(s) => {
                            let label = s.label().to_string();
                            *rep.skipped_remote.entry(label.clone()).or_default() += 1;
                            skipped_ids.insert(f.id.clone(), (f, label));
                        }
                        None => {
                            live.insert(f.id.clone(), f);
                        }
                    }
                }
                if n < crate::folders::PAGE {
                    break;
                }
                match last {
                    Some(l) => {
                        from = Some(l.clone());
                        cursor = Some(l);
                    }
                    None => break,
                }
            }
        }
        rep.remote_files = live.len() as u64;

        // The local files, as the first push pass would walk them.
        let this = self.clone();
        let walk = tokio::task::spawn_blocking(move || this.walk_visible(""))
            .await
            .map_err(EngineError::other)?;
        for (p, why) in &walk.unreadable {
            rep.unreadable.push(AdoptEntry {
                path: p.clone(),
                note: Some(why.clone()),
                ..Default::default()
            });
        }
        let mut by_id: BTreeMap<String, Vec<Found>> = BTreeMap::new();
        for f in walk.found {
            by_id.entry(self.doc_id_for(&f.rel)).or_default().push(f);
        }
        let max = self.inner.runtime.lock().unwrap().max_size_bytes;
        let split = self.inner.runtime.lock().unwrap().split;
        let folds = self.inner.fs.folds();

        let mut identical: Vec<(FileDoc, DiskState)> = Vec::new();
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut only_local: Vec<(String, Found)> = Vec::new();
        let mut seen_remote: HashSet<String> = HashSet::new();
        for (id, mut files) in by_id {
            files.sort_by(|a, b| a.rel.cmp(&b.rel));
            if files.len() > 1 {
                // The push keeps one of them (the remote's spelling, else
                // the first) and leaves the others out.
                let keep = live
                    .get(&id)
                    .and_then(|d| files.iter().position(|f| f.rel == d.path))
                    .unwrap_or(0);
                let kept = files.remove(keep);
                for f in &files {
                    rep.not_synced.push(AdoptEntry {
                        path: f.rel.clone(),
                        local_size: Some(f.size),
                        note: Some(format!("case collision with {}; not synced", kept.rel)),
                        ..Default::default()
                    });
                }
                files = vec![kept];
            }
            let found = files.pop().expect("one file");
            rep.local_files += 1;
            if let Some((doc, label)) = skipped_ids.get(&id) {
                rep.not_synced.push(AdoptEntry {
                    path: doc.path.clone(),
                    local_path: Some(found.rel.clone()).filter(|r| *r != doc.path),
                    local_size: Some(found.size),
                    remote_size: Some(doc.size),
                    note: Some(format!(
                        "the remote document is skipped here ({label}); neither side syncs it"
                    )),
                    ..Default::default()
                });
                continue;
            }
            if max.is_some_and(|m| found.size > m) {
                // Not pushed; a remote version under the limit is fetched,
                // but the local file differs, so it is kept (dirty).
                let remote = live.get(&id);
                if remote.is_some() {
                    seen_remote.insert(id.clone());
                }
                rep.not_synced.push(AdoptEntry {
                    path: found.rel.clone(),
                    local_size: Some(found.size),
                    remote_size: remote.map(|d| d.size),
                    remote_rev: remote.map(|d| d.rev.clone()),
                    note: Some(match remote {
                        Some(_) => {
                            "over the size limit here: not pushed, and the server's version is not applied over it"
                                .into()
                        }
                        None => "over the size limit; not synced".into(),
                    }),
                    ..Default::default()
                });
                continue;
            }
            if let Some(doc) = live.get(&id) {
                seen_remote.insert(id.clone());
                let same_name = doc.path == found.rel
                    || (folds && crate::index::fold_key(&doc.path) == crate::index::fold_key(&found.rel));
                if !same_name {
                    rep.name_case.push(AdoptEntry {
                        path: doc.path.clone(),
                        local_path: Some(found.rel.clone()),
                        local_size: Some(found.size),
                        remote_size: Some(doc.size),
                        remote_rev: Some(doc.rev.clone()),
                        note: None,
                    });
                    continue;
                }
                let read = match self.read_split(&found, &doc.path, &split).await {
                    Ok(r) => r,
                    Err(why) => {
                        rep.unreadable.push(AdoptEntry {
                            path: found.rel.clone(),
                            note: Some(why),
                            ..Default::default()
                        });
                        continue;
                    }
                };
                if !doc.conflicts.is_empty() {
                    rep.conflicted_remotely.push(doc.path.clone());
                }
                if !read.lossy && read.kind == doc.kind && read.children == doc.children {
                    identical.push((doc.clone(), read.disk));
                } else {
                    candidates.push(Candidate {
                        doc: doc.clone(),
                        found,
                        disk: read.disk,
                    });
                }
            } else if let Some(doc) = deleted.get(&id) {
                let read = match self.read_split(&found, &doc.path, &split).await {
                    Ok(r) => r,
                    Err(why) => {
                        rep.unreadable.push(AdoptEntry {
                            path: found.rel.clone(),
                            note: Some(why),
                            ..Default::default()
                        });
                        continue;
                    }
                };
                // As the reconcile pass of the first sync decides; a pull
                // applies a deletion only to a file it tracks.
                let local = json!({"children": read.children, "type": kind_name(read.kind)});
                let remote = json!({"children": doc.children, "type": kind_name(doc.kind)});
                let note = if opts.pull_only {
                    "kept here: a pull-only vault does not apply the deletion to a file it never synced"
                } else if found.mtime_ms > doc.mtime {
                    "changed here after the deletion: pushed back as a new version"
                } else if crate::push::same_content(&local, &remote) {
                    "the very version that was deleted: removed here (a copy is kept in the state folder)"
                } else {
                    "older than the deletion but different: kept as a conflicting revision beside it"
                };
                rep.deleted_remotely.push(AdoptEntry {
                    path: doc.path.clone(),
                    local_path: Some(found.rel.clone()).filter(|r| *r != doc.path),
                    local_size: Some(found.size),
                    remote_size: Some(doc.size),
                    remote_rev: Some(doc.rev.clone()),
                    note: Some(note.into()),
                });
            } else {
                only_local.push((id, found));
            }
        }

        // A local name that differs from an unmatched remote one only in case
        // or Unicode normalisation (another ID, e.g. NFD here and NFC on the
        // remote) would become a second file beside it: refused, as above.
        let mut unmatched: HashMap<String, &FileDoc> = HashMap::new();
        for (id, d) in &live {
            if !seen_remote.contains(id) {
                unmatched.entry(crate::index::fold_key(&d.path)).or_insert(d);
            }
        }
        let mut rest = Vec::with_capacity(only_local.len());
        for (id, found) in only_local {
            match unmatched.get(&crate::index::fold_key(&found.rel)) {
                Some(doc) => {
                    seen_remote.insert(doc.id.clone());
                    rep.name_case.push(AdoptEntry {
                        path: doc.path.clone(),
                        local_path: Some(found.rel.clone()),
                        local_size: Some(found.size),
                        remote_size: Some(doc.size),
                        remote_rev: Some(doc.rev.clone()),
                        note: None,
                    });
                }
                None => rest.push((id, found)),
            }
        }
        let only_local = rest;

        // Documents deleted outright (CouchDB tombstones) are not listed by
        // `_all_docs`: ask for the local-only ones by ID.
        let mut tombstoned: HashSet<String> = HashSet::new();
        for keys in only_local.chunks(100) {
            let keys: Vec<String> = keys.iter().map(|(id, _)| id.clone()).collect();
            for r in couch.all_docs_revs(&keys).await? {
                let gone = r
                    .value
                    .as_ref()
                    .and_then(|v| v.get("deleted"))
                    .and_then(|d| d.as_bool())
                    .unwrap_or(false);
                if let (true, Some(k)) = (gone, r.key) {
                    tombstoned.insert(k);
                }
            }
        }
        for (id, found) in only_local {
            rep.only_local.push(AdoptEntry {
                path: found.rel.clone(),
                local_size: Some(found.size),
                note: tombstoned.contains(&id).then(|| {
                    if opts.pull_only {
                        "deleted on the remote (its document is gone): kept here".into()
                    } else {
                        "deleted on the remote (its document is gone): pushed back as new".into()
                    }
                }),
                ..Default::default()
            });
        }
        let mut only_remote: Vec<&FileDoc> = live
            .iter()
            .filter(|(id, _)| !seen_remote.contains(*id))
            .map(|(_, d)| d)
            .collect();
        only_remote.sort_by(|a, b| a.path.cmp(&b.path));
        for d in only_remote {
            if !d.conflicts.is_empty() {
                rep.conflicted_remotely.push(d.path.clone());
            }
            rep.only_remote.push(AdoptEntry {
                path: d.path.clone(),
                remote_size: Some(d.size),
                remote_rev: Some(d.rev.clone()),
                ..Default::default()
            });
        }

        // Different chunks: compare the content, as the first pull would.
        let mut on_remote: HashSet<String> = HashSet::new();
        self.compare_content(candidates, &mut identical, &mut on_remote, &mut rep)
            .await?;
        // Every chunk of an identical file must be on the remote before the
        // file is recorded as synced with it.
        let identical = self.check_chunks(identical, on_remote, &mut rep).await?;
        rep.identical = identical.len() as u64;
        rep.identical_empty = identical.iter().filter(|(_, d)| d.size == 0).count() as u64;
        rep.assess_match();
        rep.conflicted_remotely.sort();
        for list in [
            &mut rep.differing,
            &mut rep.only_local,
            &mut rep.deleted_remotely,
            &mut rep.name_case,
            &mut rep.undecided,
            &mut rep.not_synced,
            &mut rep.unreadable,
        ] {
            list.sort_by(|a, b| a.path.cmp(&b.path));
        }
        {
            let pulled = self.report();
            rep.chunks_fetched = pulled.chunks_fetched;
            rep.chunk_bytes = pulled.chunk_bytes;
        }
        rep.bytes_fetched = couch.bytes_received() - rx0;

        // A refused take-over records nothing (`applied` stays false).
        if opts.apply && rep.refusal(opts.accept_weak_match).is_none() {
            let now = vaultfs::now_ms();
            self.with_index(|ix| {
                ix.atomically(|ix| {
                    for (doc, disk) in &identical {
                        let (state, note) = if doc.conflicts.is_empty() {
                            (State::Clean, None)
                        } else {
                            (State::Conflicted, Some(conflict_note(doc)))
                        };
                        ix.upsert(&FileRow {
                            path: doc.path.clone(),
                            doc_id: doc.id.clone(),
                            base_rev: Some(doc.rev.clone()),
                            base_children: doc.children.clone(),
                            disk_mtime: Some(disk.mtime_ms),
                            disk_size: Some(disk.size),
                            disk_hash: Some(disk.hash.clone()),
                            state,
                            note,
                        })?;
                        ix.record_revision(&RevisionRow::of_doc(
                            doc,
                            Some(disk.hash.clone()),
                            Origin::Remote,
                            true,
                            now,
                        ))?;
                    }
                    Ok(())
                })
            })?;
            rep.applied = true;
            info!("adopted {} files that match the remote", rep.identical);
        }
        rep.elapsed_ms = start.elapsed().as_millis() as u64;
        Ok(rep)
    }

    /// Read a local file (unchanged while read) and split it as a push of
    /// `doc_path` would.
    async fn read_split(
        &self,
        found: &Found,
        doc_path: &str,
        split: &livesync_model::splitter::SplitParams,
    ) -> std::result::Result<Read, String> {
        let abs = self.inner.cfg.vault.join(&found.rel);
        let doc_path = doc_path.to_string();
        let split = *split;
        tokio::task::spawn_blocking(move || -> std::result::Result<Read, String> {
            let mtime = |md: &std::fs::Metadata| {
                md.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0)
            };
            let before = std::fs::metadata(&abs).map_err(|e| e.to_string())?;
            let bytes = std::fs::read(&abs).map_err(|e| e.to_string())?;
            let after = std::fs::metadata(&abs).map_err(|e| e.to_string())?;
            if before.len() != after.len() || mtime(&before) != mtime(&after) || bytes.len() as u64 != after.len() {
                return Err("changed while it was read; run adopt again once it is quiet".into());
            }
            let chunked = chunk_file(&doc_path, &bytes, &split);
            Ok(Read {
                disk: DiskState {
                    hash: vaultfs::hash_bytes(&bytes),
                    size: bytes.len() as u64,
                    mtime_ms: mtime(&after),
                    file_id: None,
                },
                children: chunked.chunks.into_iter().map(|(id, _)| id).collect(),
                kind: chunked.kind,
                lossy: chunked.lossy,
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Fetch the remote content of files whose chunk lists differ and sort
    /// them into identical and differing, in units of bounded size.
    async fn compare_content(
        &self,
        candidates: Vec<Candidate>,
        identical: &mut Vec<(FileDoc, DiskState)>,
        on_remote: &mut HashSet<String>,
        rep: &mut AdoptReport,
    ) -> Result<()> {
        const MAX_CHUNKS: usize = 100;
        const MAX_BYTES: u64 = 4 * 1024 * 1024;
        let mut units: Vec<Vec<Candidate>> = Vec::new();
        let mut cur: Vec<Candidate> = Vec::new();
        let (mut chunks, mut bytes) = (0usize, 0u64);
        for c in candidates {
            let (n, b) = (c.doc.children.len(), c.doc.size);
            if !cur.is_empty() && (chunks + n > MAX_CHUNKS || bytes + b > MAX_BYTES) {
                units.push(std::mem::take(&mut cur));
                chunks = 0;
                bytes = 0;
            }
            chunks += n;
            bytes += b;
            cur.push(c);
        }
        if !cur.is_empty() {
            units.push(cur);
        }
        for unit in units {
            let mut wanted: Vec<(String, u64)> = Vec::new();
            let mut seen = HashSet::new();
            for c in &unit {
                let per = if c.doc.children.is_empty() {
                    0
                } else {
                    c.doc.size * 4 / 3 / c.doc.children.len() as u64 + 64
                };
                for id in &c.doc.children {
                    if !c.doc.eden.contains_key(id) && seen.insert(id.clone()) {
                        wanted.push((id.clone(), per));
                    }
                }
            }
            let fetched = self.fetch_chunks(wanted).await?;
            on_remote.extend(fetched.keys().cloned());
            for c in unit {
                let entry = AdoptEntry {
                    path: c.doc.path.clone(),
                    local_path: Some(c.found.rel.clone()).filter(|r| *r != c.doc.path),
                    local_size: Some(c.disk.size),
                    remote_size: Some(c.doc.size),
                    remote_rev: Some(c.doc.rev.clone()),
                    note: None,
                };
                match self.assemble(&c.doc, &fetched) {
                    Ok(content) if vaultfs::hash_bytes(&content) == c.disk.hash => {
                        rep.identical_by_content += 1;
                        identical.push((c.doc, c.disk));
                    }
                    Ok(_) => rep.differing.push(entry),
                    Err(why) => rep.undecided.push(AdoptEntry {
                        note: Some(format!("{why}; the first sync keeps the local file and retries")),
                        ..entry
                    }),
                }
            }
        }
        Ok(())
    }

    /// Keep the identical files whose every chunk is on the remote (in
    /// `on_remote`, the document's eden, or listed live by `_all_docs`);
    /// the others go to `undecided`. Reads revisions only, no bodies.
    async fn check_chunks(
        &self,
        identical: Vec<(FileDoc, DiskState)>,
        mut on_remote: HashSet<String>,
        rep: &mut AdoptReport,
    ) -> Result<Vec<(FileDoc, DiskState)>> {
        const KEYS: usize = 100;
        let mut ask: Vec<String> = Vec::new();
        let mut asked: HashSet<&str> = HashSet::new();
        for (doc, _) in &identical {
            for c in &doc.children {
                if !doc.eden.contains_key(c) && !on_remote.contains(c) && asked.insert(c.as_str()) {
                    ask.push(c.clone());
                }
            }
        }
        for keys in ask.chunks(KEYS) {
            for r in self.inner.couch.all_docs_revs(keys).await? {
                let live = r.error.is_none()
                    && r.value.as_ref().is_some_and(|v| {
                        v.get("rev").and_then(|x| x.as_str()).is_some()
                            && !v.get("deleted").and_then(|d| d.as_bool()).unwrap_or(false)
                    });
                if let (true, Some(k)) = (live, r.key) {
                    on_remote.insert(k);
                }
            }
        }
        let mut kept = Vec::with_capacity(identical.len());
        for (doc, disk) in identical {
            let missing = doc
                .children
                .iter()
                .filter(|c| !doc.eden.contains_key(*c) && !on_remote.contains(*c))
                .count();
            if missing == 0 {
                kept.push((doc, disk));
                continue;
            }
            rep.undecided.push(AdoptEntry {
                path: doc.path.clone(),
                local_size: Some(disk.size),
                remote_size: Some(doc.size),
                remote_rev: Some(doc.rev.clone()),
                note: Some(format!(
                    "same content, but {missing} of {} chunks missing on the remote: not adopted; the first sync keeps the local file and retries",
                    doc.children.len()
                )),
                ..Default::default()
            });
        }
        Ok(kept)
    }
}

fn kind_name(k: livesync_model::FileKind) -> &'static str {
    match k {
        livesync_model::FileKind::Plain => "plain",
        livesync_model::FileKind::Binary => "newnote",
    }
}
