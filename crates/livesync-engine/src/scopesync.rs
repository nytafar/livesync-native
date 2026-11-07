//! Applying a changed folder scope (spec "Folder scope"):
//!
//! - **Narrowing** stops tracking the paths that fell out: their index rows
//!   are dropped, their files stay unless `remove_out_of_scope_copies` (and
//!   then only clean ones). The remote is never touched.
//! - **Widening** fetches the metadata of the newly visible documents with
//!   `_all_docs` (by ID range when the prefixes allow it) and applies it like
//!   a pull batch. `since` is not reset.
//!
//! The scope the index last synced with is recorded in `meta.scope` only
//! after the catch-up completed, so an interrupted catch-up runs again.

use crate::engine::{meta_keys as meta, Engine, EngineError};
use crate::filters::{FilterState, IgnoreState, Saved, Vis};
use crate::index::State;
use crate::vaultfs::{self, RemoveOutcome};
use livesync_model::path::{is_internal_id, path_to_id};
use livesync_model::scope::{Scope, Widening};
use livesync_remote::ChangeRow;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use tracing::{debug, info, warn};

type Result<T> = std::result::Result<T, EngineError>;

/// The vault path a document ID stands for, as far as scope matching goes
/// (IDs are lower-cased paths when names are case-insensitive, which the
/// case-insensitive scope match does not mind).
pub(crate) fn id_to_scope_path(id: &str) -> &str {
    id.strip_prefix('/').unwrap_or(id)
}

const PAGE: usize = 5000;

/// `_all_docs` key ranges (raw collation, inclusive) holding every document
/// ID whose path may lie under `prefix` when IDs are case-insensitive. IDs
/// are lower-cased paths, but older clients wrote mixed-case ones, which sort
/// elsewhere; so the ranges cover every ID that starts with the prefix's
/// first character in either case, and the caller filters by scope. Chunk
/// IDs (`h:`) are left out.
pub(crate) fn id_ranges(prefix: &str) -> Vec<(String, String)> {
    const TOP: char = '\u{10FFFF}';
    let id = path_to_id(prefix, true);
    if id.starts_with("/_") {
        // The leading-underscore rule: `_` has no case.
        return vec![("/_".into(), format!("/_{TOP}"))];
    }
    let Some(c) = id.chars().next() else {
        return vec![(String::new(), TOP.to_string())];
    };
    let variants: BTreeSet<String> = [
        c.to_lowercase().collect::<String>(),
        c.to_uppercase().collect(),
        c.to_string(),
    ]
    .into_iter()
    .collect();
    let mut out = Vec::new();
    for v in variants {
        if v == "h" {
            // Around the chunks (`h:...`).
            out.push(("h".into(), "h:".into()));
            out.push(("h;".into(), format!("h{TOP}")));
        } else {
            out.push((v.clone(), format!("{v}{TOP}")));
        }
    }
    out
}

impl Engine {
    /// Drop what fell out of the scope. Returns the scope the index was last
    /// synced with (to compute the widening afterwards).
    pub(crate) fn apply_scope_narrowing(&self) -> Result<Scope> {
        let scope = self.inner.cfg.scope.clone();
        let (since, stored) = self.with_index(|ix| Ok((ix.get_meta(meta::SINCE)?, ix.get_meta(meta::SCOPE)?)))?;
        let old = match (since, stored) {
            // Nothing synced yet: the full pull that follows covers the scope.
            (None, _) => scope.clone(),
            (Some(_), Some(s)) => serde_json::from_str(&s).unwrap_or_default(),
            // Indexes from before folder scopes synced everything.
            (Some(_), None) => Scope::all(),
        };
        let rows = self.with_index(|ix| ix.all_rows())?;
        // Rows that fell out with a scope change, and stray rows that were
        // never in the old scope either (older builds recorded skipped
        // dot-paths under excluded folders): both are dropped, but only the
        // first are news.
        let (mut dropped, mut stray, mut removed) = (0usize, 0usize, 0usize);
        for r in rows.iter().filter(|r| !scope.contains(&r.path)) {
            if self.inner.cfg.remove_out_of_scope_copies && matches!(r.state, State::Clean | State::Conflicted) {
                if let Some(h) = &r.disk_hash {
                    let target = self.inner.cfg.vault.join(&r.path);
                    match vaultfs::remove_if_matches(&self.inner.cfg.vault, &target, h) {
                        Ok(RemoveOutcome::Removed | RemoveOutcome::Trashed { .. }) => removed += 1,
                        Ok(RemoveOutcome::Raced { displaced }) => self.put_back(&r.path, displaced, None),
                        Ok(_) => {}
                        Err(e) => warn!("{}: cannot remove the local copy: {e}", r.path),
                    }
                }
            }
            self.with_index(|ix| ix.delete_path(&r.path))?;
            if old.contains(&r.path) {
                dropped += 1;
            } else {
                stray += 1;
            }
        }
        let pending = self.with_index(|ix| ix.pending(None))?;
        for p in pending.iter().filter(|p| !scope.contains(id_to_scope_path(&p.doc_id))) {
            self.with_index(|ix| ix.remove_pending(&p.doc_id))?;
        }
        if stray > 0 {
            debug!("dropped {stray} index row(s) that were never in the folder scope");
        }
        if dropped > 0 {
            info!(
                "folder scope is now {}: stopped tracking {dropped} file(s){}",
                scope.describe(),
                if removed > 0 {
                    format!(", removed {removed} local copies")
                } else {
                    ", local copies kept".into()
                }
            );
        }
        Ok(old)
    }

    /// Fetch what became visible since `old`, then record the current scope.
    pub(crate) async fn scope_catch_up(&self, old: &Scope) -> Result<()> {
        let new = self.inner.cfg.scope.clone();
        if *old != new {
            let case_sensitive = self.inner.runtime.lock().unwrap().case_sensitive_ids;
            let ids = match new.widening_from(old) {
                Widening::None => vec![],
                Widening::Prefixes(ps) if !case_sensitive => {
                    let ranges: BTreeSet<(String, String)> = ps.iter().flat_map(|p| id_ranges(p)).collect();
                    let mut ids = BTreeSet::new();
                    for (start, end) in ranges {
                        ids.extend(self.scan_ids(Some(&start), Some(&end)).await?);
                    }
                    ids.into_iter().collect()
                }
                Widening::Prefixes(_) | Widening::Everything => self.scan_ids(None, None).await?,
            };
            let ids: Vec<String> = ids
                .into_iter()
                .filter(|id| {
                    let p = id_to_scope_path(id);
                    new.contains(p) && !old.contains(p)
                })
                .collect();
            info!(
                "folder scope widened to {}: fetching {} newly visible document(s)",
                new.describe(),
                ids.len()
            );
            for keys in ids.chunks(100) {
                let rows = self.inner.couch.all_docs_by_keys(keys).await?;
                let changes: Vec<ChangeRow> = rows
                    .into_iter()
                    .filter_map(|r| {
                        let doc = r.doc.filter(|d| !d.is_null())?;
                        Some(ChangeRow {
                            seq: Value::Null,
                            id: r.key.unwrap_or_default(),
                            changes: vec![],
                            deleted: false,
                            doc: Some(doc),
                        })
                    })
                    .collect();
                self.apply_batch(changes).await?;
            }
        }
        self.with_index(|ix| ix.set_meta(meta::SCOPE, &serde_json::to_string(&new).unwrap()))?;
        Ok(())
    }

    /// Read every ignore file a walk of the vault meets (the folders that
    /// sync, plus the own ignore files of folders that may be ignored as a
    /// whole), with a reader of its own.
    pub(crate) fn scan_ignore_files(&self, case_sensitive: bool) -> Result<BTreeMap<String, Saved>> {
        let settings = self.inner.cfg.filter_settings();
        if !settings.use_ignore_files {
            return Ok(BTreeMap::new());
        }
        let root = self.inner.cfg.vault.clone();
        let f = settings.build(&root, case_sensitive).map_err(EngineError::Other)?;
        let scope = &self.inner.cfg.scope;
        let mut stack = vec![String::new()];
        while let Some(dir) = stack.pop() {
            if !matches!(f.folder_state(&dir), IgnoreState::Synced { .. }) {
                continue;
            }
            let path = if dir.is_empty() { root.clone() } else { root.join(&dir) };
            let Ok(rd) = std::fs::read_dir(&path) else { continue };
            for e in rd.flatten() {
                if !e.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                let rel = if dir.is_empty() {
                    name.clone()
                } else {
                    format!("{dir}/{name}")
                };
                if name.starts_with('.') && !f.dot.covers(&rel) {
                    continue;
                }
                if scope.may_contain_under(&rel) {
                    stack.push(rel);
                }
            }
        }
        Ok(f.files.as_ref().map(|x| x.read_so_far()).unwrap_or_default())
    }

    /// Apply changed local filters (ignore files edited, settings changed
    /// since the last run): stop tracking what they now hide (the files and
    /// the remote documents stay), and fetch what they stopped hiding, as
    /// for a widened scope. Returns whether anything changed.
    pub(crate) async fn filters_catch_up(&self) -> Result<bool> {
        let _one = self.inner.filters_lock.lock().await;
        let case_sensitive = self.inner.runtime.lock().unwrap().case_sensitive_ids;
        let this = self.clone();
        let files = tokio::task::spawn_blocking(move || this.scan_ignore_files(case_sensitive))
            .await
            .map_err(EngineError::other)??;
        let new = FilterState::new(self.inner.cfg.filter_settings(), files);
        let (since, stored) = self.with_index(|ix| Ok((ix.get_meta(meta::SINCE)?, FilterState::load(ix)?)))?;
        let old = match (since, stored) {
            // Nothing synced yet: the full pull covers the filters as they are.
            (None, _) => new.clone(),
            (Some(_), Some(s)) => s,
            (Some(_), None) => FilterState::before_filters(&self.inner.cfg.filter_settings()),
        };
        if old == new {
            return Ok(false);
        }
        let scope = self.inner.cfg.scope.clone();
        // Narrowing: tracked paths the filters now hide are untracked.
        let live = self.filters();
        live.refresh();
        let mut untracked = 0usize;
        for r in self.with_index(|ix| ix.all_rows())? {
            if !matches!(r.state, State::Clean | State::Dirty | State::Conflicted) {
                continue;
            }
            if live.vis(&r.path, &scope) == Vis::Hidden && scope.contains(&r.path) {
                self.with_index(|ix| {
                    ix.atomically(|ix| {
                        ix.delete_path(&r.path)?;
                        ix.remove_pending(&r.doc_id)
                    })
                })?;
                untracked += 1;
            }
        }
        if untracked > 0 {
            info!("the local filters changed: stopped tracking {untracked} file(s) they now hide (files and remote copies kept)");
        }
        // Widening: what was hidden and is not now, fetched like a widened scope.
        let old_eval = old.evaluator(case_sensitive).map_err(EngineError::Other)?;
        let new_eval = new.evaluator(case_sensitive).map_err(EngineError::Other)?;
        let ids: Vec<String> = self
            .scan_ids(None, None)
            .await?
            .into_iter()
            .filter(|id| {
                let p = id_to_scope_path(id);
                scope.contains(p) && new_eval.vis(p, &scope) != Vis::Hidden && old_eval.vis(p, &scope) != Vis::Visible
            })
            .collect();
        let ids: Vec<String> = {
            let ix = self.inner.index.lock().unwrap();
            let mut keep = Vec::new();
            for id in ids {
                let tracked = ix
                    .by_doc(&id)?
                    .is_some_and(|r| matches!(r.state, State::Clean | State::Dirty | State::Conflicted));
                if !tracked {
                    keep.push(id);
                }
            }
            keep
        };
        if !ids.is_empty() {
            info!(
                "the local filters changed: fetching {} document(s) they no longer hide",
                ids.len()
            );
        }
        for keys in ids.chunks(100) {
            let rows = self.inner.couch.all_docs_by_keys(keys).await?;
            let changes: Vec<ChangeRow> = rows
                .into_iter()
                .filter_map(|r| {
                    let doc = r.doc.filter(|d| !d.is_null())?;
                    Some(ChangeRow {
                        seq: Value::Null,
                        id: r.key.unwrap_or_default(),
                        changes: vec![],
                        deleted: false,
                        doc: Some(doc),
                    })
                })
                .collect();
            self.apply_batch(changes).await?;
        }
        self.with_index(|ix| new.save(ix))?;
        Ok(true)
    }

    /// Document IDs in `[start, end]` (or all), without chunks and internal
    /// documents.
    async fn scan_ids(&self, start: Option<&str>, end: Option<&str>) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut from = start.map(str::to_string);
        // The last ID taken. A page starts at it (inclusive), so it comes
        // back first unless it was deleted meanwhile; only then is it skipped.
        let mut cursor: Option<String> = None;
        loop {
            let rows = self
                .inner
                .couch
                .all_docs_page(from.as_deref(), end, PAGE, false)
                .await?;
            let n = rows.len();
            let mut last = None;
            for r in rows {
                let Some(id) = r.key else { continue };
                if cursor.as_deref() == Some(id.as_str()) {
                    continue;
                }
                last = Some(id.clone());
                if id.starts_with("h:") || id.starts_with("_design/") || is_internal_id(&id) {
                    continue;
                }
                out.push(id);
            }
            if n < PAGE {
                return Ok(out);
            }
            match last {
                Some(l) => {
                    from = Some(l.clone());
                    cursor = Some(l);
                }
                None => return Ok(out),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_ranges_cover_both_cases_and_skip_chunks() {
        let r = id_ranges("notes");
        assert_eq!(
            r,
            vec![
                ("N".to_string(), "N\u{10FFFF}".to_string()),
                ("n".to_string(), "n\u{10FFFF}".to_string())
            ]
        );
        let inside = |id: &str| r.iter().any(|(a, b)| id >= a.as_str() && id <= b.as_str());
        assert!(inside("notes/a.md") && inside("Notes/Mixed.md") && inside("nOtes/x.md"));
        assert!(!inside("other/a.md"));
        let h = id_ranges("hobbies");
        let inside = |id: &str| h.iter().any(|(a, b)| id >= a.as_str() && id <= b.as_str());
        assert!(inside("hobbies/a.md") && inside("Hobbies/b.md"));
        assert!(!inside("h:abc123"), "chunks are not scanned");
        assert_eq!(
            id_ranges("_templates"),
            vec![("/_".to_string(), "/_\u{10FFFF}".to_string())]
        );
    }
}
