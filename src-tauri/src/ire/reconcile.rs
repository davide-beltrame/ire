//! Checkpoint reconciliation of `.ire/` against the app's in-memory state.
//!
//! IRE does not watch the filesystem. It re-reads `.ire/` at points where its
//! own writes have already settled — after a completed tool call and when the
//! window regains focus — and emits the same `workspace-event`s a mutation
//! would, so a direct file edit or a `git checkout` shows up in the panels.
//!
//! Two gates keep a pass cheap. Per-file `(mtime, size)` from the previous pass
//! decides whether a file is read at all; the content hash of what was read
//! decides whether an event is emitted. The second gate matters for `git`
//! operations, which rewrite mtimes for every file they touch even when the
//! bytes are identical.
//!
//! Only `ire.json` and `resources/*.md` are scanned — the two things the
//! frontend mirrors in memory. `_SYSTEM.md`, `long-term.md` and `short-term/`
//! are re-read from disk on every agent turn and `cache/` is gitignored churn,
//! so none of them can drift.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::json;
use tauri::{AppHandle, Manager};

use super::store::{emit_sections, hash, resource_meta, IreContent, IreStore};
use crate::events;
use crate::workspace::state::ActiveWorkspace;

/// What the previous pass saw for one file: the `(mtime, size)` gate plus the
/// hash of the content it read.
struct FileMeta {
    mtime: Option<SystemTime>,
    size: u64,
    hash: String,
}

/// One externally-originated change found by a pass.
enum Change {
    /// `ire.json` parsed and its content differs — notes/focus/ideas.
    Ire(IreContent),
    /// A `resources/<slug>.md` payload, shaped like `list_resources`.
    Resource(serde_json::Value),
    /// A `resources/<slug>.md` that is no longer on disk.
    ResourceDeleted(String),
}

/// Per-file metadata from the last reconcile. Owned by `WorkspaceHandle`, so it
/// is created and dropped with the workspace it describes.
#[derive(Default)]
pub struct IreSnapshot {
    files: HashMap<PathBuf, FileMeta>,
}

impl IreSnapshot {
    /// Record the current state of `.ire/` without reporting it. Used at
    /// workspace open, where the hydrate burst has already sent everything.
    pub fn primed(store: &IreStore) -> Self {
        let mut snapshot = Self::default();
        let _ = snapshot.scan(store);
        snapshot
    }

    /// Stat `ire.json` and `resources/*.md`, read only what moved, and report
    /// what actually changed since the previous pass.
    fn scan(&mut self, store: &IreStore) -> Vec<Change> {
        let mut changes = Vec::new();

        let ire_path = store.ire_path();
        if let Some((mtime, size)) = gate(&ire_path) {
            if !self.matches_gate(&ire_path, mtime, size) {
                let raw = fs::read_to_string(&ire_path).unwrap_or_default();
                // Parse before recording. A half-written or hand-corrupted
                // file leaves the snapshot untouched so the next pass retries
                // it, and the panels keep the last state that parsed. This is
                // stricter than `read_ire`, which treats an empty file as
                // defaults — blanking the panels is exactly what a truncated
                // `ire.json` must not do.
                match serde_json::from_str::<IreContent>(&raw) {
                    Ok(content) => {
                        let same = self.record(&ire_path, mtime, size, sections_hash(&content));
                        if !same {
                            changes.push(Change::Ire(content));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "reconcile: ire.json did not parse, keeping last known state");
                    }
                }
            }
        }

        // One `read_dir` plus a `stat` per entry — enough to notice adds,
        // deletes and in-place edits without reading a file whose metadata
        // didn't move.
        let listing = fs::read_dir(&store.resources_dir);
        let listed = listing.is_ok();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        if let Ok(entries) = listing {
            for path in entries.flatten().map(|e| e.path()).filter(|p| is_resource(p)) {
                let Some((mtime, size)) = gate(&path) else {
                    continue;
                };
                seen.insert(path.clone());
                if self.matches_gate(&path, mtime, size) {
                    continue;
                }
                let Ok(content) = fs::read_to_string(&path) else {
                    continue;
                };
                let rel = rel_resource_path(&path);
                if self.record(&path, mtime, size, hash(&content)) {
                    continue;
                }
                let (title, sources) = resource_meta(&content, &rel);
                changes.push(Change::Resource(
                    json!({ "path": rel, "title": title, "sources": sources }),
                ));
            }
        }

        // Only sweep deletions when the directory actually listed — an
        // unreadable `resources/` must not read as "every resource is gone".
        if listed {
            let gone: Vec<PathBuf> = self
                .files
                .keys()
                .filter(|p| p.starts_with(&store.resources_dir) && !seen.contains(*p))
                .cloned()
                .collect();
            for path in gone {
                self.files.remove(&path);
                changes.push(Change::ResourceDeleted(rel_resource_path(&path)));
            }
        }

        changes
    }

    fn matches_gate(&self, path: &Path, mtime: Option<SystemTime>, size: u64) -> bool {
        self.files
            .get(path)
            .is_some_and(|f| f.mtime == mtime && f.size == size)
    }

    /// Store what this pass read. Returns whether the content hash is the one
    /// the previous pass already reported.
    fn record(&mut self, path: &Path, mtime: Option<SystemTime>, size: u64, hash: String) -> bool {
        let same = self.files.get(path).is_some_and(|f| f.hash == hash);
        self.files
            .insert(path.to_path_buf(), FileMeta { mtime, size, hash });
        same
    }
}

/// Reconcile `.ire/` with the app's in-memory state and emit what moved. A
/// no-op when no workspace is open.
///
/// Experiments are read from `ire.json` but never emitted here: their rows
/// carry live tab linkage owned by the experiment runner, and re-emitting the
/// git-tracked copy would drop it.
pub fn reconcile(app: &AppHandle) {
    let active = app.state::<ActiveWorkspace>();
    let Ok(mut guard) = active.0.lock() else {
        return;
    };
    let Some(handle) = guard.as_mut() else {
        return;
    };
    let store = IreStore::new(handle.state.path.clone());
    let changes = handle.ire_snapshot.scan(&store);
    drop(guard);

    if changes.is_empty() {
        return;
    }
    tracing::info!(changes = changes.len(), "reconciled external .ire changes");
    for change in changes {
        match change {
            Change::Ire(content) => emit_sections(app, &content),
            Change::Resource(resource) => {
                events::emit_resource_changed(app, events::EventSource::Mutation, &resource)
            }
            Change::ResourceDeleted(path) => events::emit_resource_deleted(app, &path),
        }
    }
}

/// `(mtime, size)` for a file, or `None` when it can't be stat'd. A missing or
/// unreadable file reads as "no information", never as a change, so a vanished
/// `ire.json` can't blank the panels.
fn gate(path: &Path) -> Option<(Option<SystemTime>, u64)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

/// Hash only the sections reconcile emits. The experiment runner rewrites
/// `ire.json` on every status transition, and hashing the whole file would turn
/// that churn into notes/focus/ideas events carrying no change.
fn sections_hash(content: &IreContent) -> String {
    let sections = json!({
        "notes": content.notes,
        "focus": content.focus,
        "ideas": content.ideas,
    });
    hash(&sections.to_string())
}

/// Same filter as `list_resources`: `*.md`, skipping `_index.md` and dotfiles.
fn is_resource(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("md")
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.starts_with('_') && !n.starts_with('.'))
}

fn rel_resource_path(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    format!("resources/{name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::Duration;

    fn store() -> (tempfile::TempDir, IreStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = IreStore::new(dir.path().to_path_buf());
        fs::create_dir_all(&store.resources_dir).unwrap();
        (dir, store)
    }

    fn write_ire(store: &IreStore, notes: &str) {
        let content = format!(
            "{{\"notes\":\"{notes}\",\"focus\":{{\"research_question\":\"\",\"this_week\":\"\"}},\"ideas\":[],\"experiments\":[]}}\n"
        );
        fs::write(store.ire_path(), content).unwrap();
    }

    fn notes_of(changes: &[Change]) -> Vec<String> {
        changes
            .iter()
            .filter_map(|c| match c {
                Change::Ire(content) => Some(content.notes.clone()),
                _ => None,
            })
            .collect()
    }

    fn resource_paths(changes: &[Change]) -> Vec<String> {
        changes
            .iter()
            .filter_map(|c| match c {
                Change::Resource(r) => Some(r["path"].as_str().unwrap_or_default().to_string()),
                _ => None,
            })
            .collect()
    }

    fn deleted_paths(changes: &[Change]) -> Vec<String> {
        changes
            .iter()
            .filter_map(|c| match c {
                Change::ResourceDeleted(p) => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    /// Pin a file's mtime so a rewrite can be made invisible to the gate.
    fn set_mtime(path: &Path, mtime: SystemTime) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    #[test]
    fn priming_reports_nothing_and_absorbs_current_state() {
        let (_d, s) = store();
        write_ire(&s, "seeded");
        fs::write(s.resources_dir.join("a.md"), "# A\n").unwrap();

        let mut snap = IreSnapshot::primed(&s);
        assert!(snap.scan(&s).is_empty());
    }

    #[test]
    fn external_ire_edit_is_reported_once() {
        let (_d, s) = store();
        write_ire(&s, "before");
        let mut snap = IreSnapshot::primed(&s);

        write_ire(&s, "after an external edit");
        assert_eq!(notes_of(&snap.scan(&s)), ["after an external edit"]);
        // Second pass sees settled metadata and stays quiet.
        assert!(snap.scan(&s).is_empty());
    }

    #[test]
    fn rewriting_identical_content_reports_nothing() {
        // What `git checkout` does: bump the mtime of files whose bytes did
        // not change.
        let (_d, s) = store();
        write_ire(&s, "steady");
        fs::write(s.resources_dir.join("a.md"), "# A\n").unwrap();
        let mut snap = IreSnapshot::primed(&s);

        write_ire(&s, "steady");
        fs::write(s.resources_dir.join("a.md"), "# A\n").unwrap();
        assert!(snap.scan(&s).is_empty());
    }

    #[test]
    fn unchanged_metadata_is_never_re_read() {
        let (_d, s) = store();
        write_ire(&s, "aaaaaa");
        let pinned = fs::metadata(s.ire_path()).unwrap().modified().unwrap();
        let mut snap = IreSnapshot::primed(&s);

        // Same byte length, different content, mtime forced back: the gate is
        // the only thing that could have suppressed this.
        write_ire(&s, "bbbbbb");
        set_mtime(&s.ire_path(), pinned);
        assert!(snap.scan(&s).is_empty());

        // Move the mtime and the same content is picked up.
        set_mtime(&s.ire_path(), pinned + Duration::from_secs(1));
        assert_eq!(notes_of(&snap.scan(&s)), ["bbbbbb"]);
    }

    #[test]
    fn experiment_churn_in_ire_json_stays_quiet() {
        let (_d, s) = store();
        write_ire(&s, "steady");
        let mut snap = IreSnapshot::primed(&s);

        // What the runner writes on every experiment status transition. The
        // sections reconcile emits are untouched, so nothing should fire.
        fs::write(
            s.ire_path(),
            "{\"notes\":\"steady\",\"focus\":{\"research_question\":\"\",\"this_week\":\"\"},\"ideas\":[],\
             \"experiments\":[{\"uuid\":\"e1\",\"name\":\"train\",\"command\":\"make\",\
             \"status\":\"running\",\"started_at\":\"2026-01-01T00:00:00Z\"}]}\n",
        )
        .unwrap();
        assert!(snap.scan(&s).is_empty());
    }

    #[test]
    fn malformed_ire_json_reports_nothing_and_retries() {
        let (_d, s) = store();
        write_ire(&s, "good");
        let mut snap = IreSnapshot::primed(&s);

        fs::write(s.ire_path(), "{ not json").unwrap();
        assert!(snap.scan(&s).is_empty());
        // The bad file stays un-recorded, so the repair is still detected.
        write_ire(&s, "repaired");
        assert_eq!(notes_of(&snap.scan(&s)), ["repaired"]);
    }

    #[test]
    fn missing_ire_json_reports_nothing() {
        let (_d, s) = store();
        write_ire(&s, "good");
        let mut snap = IreSnapshot::primed(&s);

        fs::remove_file(s.ire_path()).unwrap();
        assert!(snap.scan(&s).is_empty());
    }

    #[test]
    fn added_and_deleted_resources_are_reported() {
        let (_d, s) = store();
        fs::write(s.resources_dir.join("a.md"), "---\ntitle: \"A\"\n---\n").unwrap();
        let mut snap = IreSnapshot::primed(&s);

        fs::write(s.resources_dir.join("b.md"), "---\ntitle: \"B\"\n---\n").unwrap();
        assert_eq!(resource_paths(&snap.scan(&s)), ["resources/b.md"]);

        fs::remove_file(s.resources_dir.join("a.md")).unwrap();
        assert_eq!(deleted_paths(&snap.scan(&s)), ["resources/a.md"]);
        assert!(snap.scan(&s).is_empty());
    }

    #[test]
    fn generated_index_and_dotfiles_are_ignored() {
        let (_d, s) = store();
        let mut snap = IreSnapshot::primed(&s);

        fs::write(s.resources_dir.join("_index.md"), "- regenerated\n").unwrap();
        fs::write(s.resources_dir.join(".hidden.md"), "x\n").unwrap();
        assert!(snap.scan(&s).is_empty());
    }

    #[test]
    fn cache_churn_is_outside_the_scan() {
        let (_d, s) = store();
        let mut snap = IreSnapshot::primed(&s);

        let logs = s.ire_dir.join("cache/experiments/abc");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("stdout.log"), "a lot of output\n").unwrap();
        assert!(snap.scan(&s).is_empty());
    }
}
