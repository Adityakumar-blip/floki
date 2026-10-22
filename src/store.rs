use crate::model::{AppData, SharedData, Workspace, WorkspaceMeta};
use crate::storage;
use gpui_kit::*;

/// The active workspace plus the workspace list; each update is written to disk.
pub struct Store {
    pub ws: Workspace,
    pub app: AppData,
    pub save_error: Option<String>,
    /// Set when an edit was undone because the workspace is view-only.
    pub refused: bool,
    /// Bumped on every shared change, so a sync that started earlier knows not to clear `dirty`.
    pub edits: u64,
}

impl Store {
    pub fn load() -> Self {
        let app = storage::load_app();
        Self { ws: storage::load_workspace(&app.active), app, save_error: None, refused: false, edits: 0 }
    }

    pub fn meta(&self) -> &WorkspaceMeta {
        self.app.workspaces.iter().find(|w| w.id == self.app.active).expect("active workspace exists")
    }

    fn meta_mut(&mut self) -> &mut WorkspaceMeta {
        let active = self.app.active.clone();
        self.app.workspaces.iter_mut().find(|w| w.id == active).expect("active workspace exists")
    }

    pub fn update<R>(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Workspace) -> R) -> R {
        let before = self.meta().remote.is_some().then(|| self.ws.shared());
        self.refused = false;
        let out = f(&mut self.ws);
        if let Some(before) = before {
            if before != self.ws.shared() {
                if self.meta().remote.as_ref().is_some_and(|r| r.role == "viewer") {
                    // Viewers can look but not change; undo here so every edit path is covered.
                    self.ws.apply_shared(before);
                    self.save_error = storage::save_workspace(&self.app.active, &self.ws).err().map(|e| e.to_string());
                    self.refused = true;
                    cx.notify();
                    return out;
                }
                self.edits += 1;
                if let Some(remote) = &mut self.meta_mut().remote {
                    remote.dirty = true;
                }
                let _ = storage::save_app(&self.app);
            }
        }
        // ponytail: whole-file rewrite per change; fine for local workspaces of a few MB.
        self.save_error = storage::save_workspace(&self.app.active, &self.ws).err().map(|e| e.to_string());
        cx.notify();
        out
    }

    pub fn update_app<R>(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut AppData) -> R) -> R {
        let out = f(&mut self.app);
        self.save_error = storage::save_app(&self.app).err().map(|e| e.to_string());
        cx.notify();
        out
    }

    /// Replaces the shared part of the active workspace with the server's copy.
    pub fn apply_remote(&mut self, data: SharedData, version: i64, cx: &mut Context<Self>) {
        self.ws.apply_shared(data);
        if let Some(remote) = &mut self.meta_mut().remote {
            remote.version = version;
            remote.dirty = false;
        }
        let _ = storage::save_app(&self.app);
        self.save_error = storage::save_workspace(&self.app.active, &self.ws).err().map(|e| e.to_string());
        cx.notify();
    }

    /// Adds a workspace; `ws` keeps theme and settings from the current one, since those are app-wide.
    pub fn add_workspace(&mut self, meta: WorkspaceMeta, mut ws: Workspace, cx: &mut Context<Self>) {
        ws.theme = self.ws.theme;
        ws.settings = self.ws.settings.clone();
        let _ = storage::save_workspace(&meta.id, &ws);
        self.update_app(cx, |app| app.workspaces.push(meta));
    }

    pub fn switch(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut ws = storage::load_workspace(id);
        // ponytail: theme and settings live in each workspace file and are carried across; move to AppData if they diverge.
        ws.theme = self.ws.theme;
        ws.settings = self.ws.settings.clone();
        self.ws = ws;
        self.edits += 1;
        self.refused = false;
        self.update_app(cx, |app| app.active = id.into());
    }

    pub fn remove_workspace(&mut self, id: &str, cx: &mut Context<Self>) {
        storage::delete_workspace(id);
        self.update_app(cx, |app| app.workspaces.retain(|w| w.id != id));
    }
}
