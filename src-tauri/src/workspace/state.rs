use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;

use super::lock::WorkspaceLock;
use crate::ire::{IreSnapshot, IreStore};

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceState {
    pub path: PathBuf,
    pub name: String,
    /// `~/.ire/workspaces/<id>/` — home for runtime artifacts (local.db,
    /// mcp.json, mcp.sock, .lock). Not exposed to the frontend.
    #[serde(skip)]
    pub home_data_dir: PathBuf,
}

impl WorkspaceState {
    pub fn from_path(path: PathBuf, home_data_dir: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace")
            .to_string();
        Self {
            path,
            name,
            home_data_dir,
        }
    }
}

pub struct WorkspaceHandle {
    pub state: WorkspaceState,
    /// Per-file metadata `.ire/` had at the last checkpoint reconcile. Lives
    /// here so it is created and dropped with the workspace it describes.
    pub ire_snapshot: IreSnapshot,
    _lock: WorkspaceLock,
}

impl WorkspaceHandle {
    pub fn new(state: WorkspaceState, lock: WorkspaceLock) -> Self {
        // Primed from disk: the caller emits the hydrate burst right after, so
        // the first checkpoint must not report the same state as a change.
        let ire_snapshot = IreSnapshot::primed(&IreStore::new(state.path.clone()));
        Self {
            state,
            ire_snapshot,
            _lock: lock,
        }
    }
}

#[derive(Default)]
pub struct ActiveWorkspace(pub Mutex<Option<WorkspaceHandle>>);
