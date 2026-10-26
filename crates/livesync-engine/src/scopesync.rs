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
use crate::index::State;
use crate::vaultfs::{self, RemoveOutcome};
use livesync_model::path::{is_internal_id, path_to_id};
use livesync_model::scope::{Scope, Widening};
use livesync_remote::ChangeRow;
use serde_json::Value;
use tracing::{info, warn};

type Result<T> = std::result::Result<T, EngineError>;

/// The vault path a document ID stands for, as far as scope matching goes
/// (IDs are lower-cased paths when names are case-insensitive, which the
/// case-insensitive scope match does not mind).
pub(crate) fn id_to_scope_path(id: &str) -> &str {
    id.strip_prefix('/').unwrap_or(id)
}

const PAGE: usize = 5000;

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
        let (mut dropped, mut removed) = (0usize, 0usize);
        for r in rows.iter().filter(|r| !scope.contains(&r.path)) {
            if self.inner.cfg.remove_out_of_scope_copies && matches!(r.state, State::Clean | State::Conflicted) {
                if let Some(h) = &r.disk_hash {
                    let target = self.inner.cfg.vault.join(&r.path);
                    match vaultfs::remove_if_matches(&self.inner.cfg.vault, &target, h) {
                        Ok(RemoveOutcome::Removed) => removed += 1,
                        Ok(RemoveOutcome::Raced { displaced }) => self.put_back(&r.path, displaced, None),
                        Ok(_) => {}
                        Err(e) => warn!("{}: cannot remove the local copy: {e}", r.path),
                    }
                }
            }
            self.with_index(|ix| ix.delete_path(&r.path))?;
            dropped += 1;
        }
        let pending = self.with_index(|ix| ix.pending(None))?;
        for p in pending.iter().filter(|p| !scope.contains(id_to_scope_path(&p.doc_id))) {
            self.with_index(|ix| ix.remove_pending(&p.doc_id))?;
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
                    let mut ids = Vec::new();
                    for p in ps {
                        let start = path_to_id(&p, true);
                        let end = format!("{start}/\u{10FFFF}");
                        ids.extend(self.scan_ids(Some(&start), Some(&end)).await?);
                    }
                    ids
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

    /// Document IDs in `[start, end]` (or all), without chunks and internal
    /// documents.
    async fn scan_ids(&self, start: Option<&str>, end: Option<&str>) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut from = start.map(str::to_string);
        let mut skip_first = false;
        loop {
            let rows = self
                .inner
                .couch
                .all_docs_page(from.as_deref(), end, PAGE + usize::from(skip_first), false)
                .await?;
            let n = rows.len();
            let mut last = None;
            for (i, r) in rows.into_iter().enumerate() {
                if skip_first && i == 0 {
                    continue;
                }
                let Some(id) = r.key else { continue };
                last = Some(id.clone());
                if id.starts_with("h:") || id.starts_with("_design/") || is_internal_id(&id) {
                    continue;
                }
                out.push(id);
            }
            if n < PAGE + usize::from(skip_first) {
                return Ok(out);
            }
            match last {
                Some(l) => {
                    from = Some(l);
                    skip_first = true;
                }
                None => return Ok(out),
            }
        }
    }
}
