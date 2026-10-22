use crate::model::{AppData, Workspace, WorkspaceMeta};
use std::{fs, io, path::PathBuf};

pub fn data_dir() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("Floki");
    // Postwomen was the app's former name; carry its data over once.
    let old = base.join("Postwomen");
    if !dir.exists() && old.is_dir() {
        let _ = fs::rename(&old, &dir);
    }
    dir
}

fn app_path() -> PathBuf {
    data_dir().join("app.json")
}

fn workspace_path(id: &str) -> PathBuf {
    data_dir().join("workspaces").join(format!("{id}.json"))
}

/// Reads JSON, moving a corrupt file aside instead of letting a later save overwrite it.
fn read<T: serde::de::DeserializeOwned + Default>(path: PathBuf) -> T {
    let Ok(bytes) = fs::read(&path) else { return T::default() };
    serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        let _ = fs::rename(&path, path.with_extension("json.corrupt"));
        T::default()
    })
}

fn write<T: serde::Serialize>(path: PathBuf, data: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(data).map_err(io::Error::other)?)?;
    fs::rename(tmp, path)
}

/// The workspace list; the single-file layout of 0.1 becomes "My Workspace" (the old file is left in place).
pub fn load_app() -> AppData {
    let mut app: AppData = read(app_path());
    if app.workspaces.is_empty() {
        let legacy: Workspace = read(data_dir().join("postwomen.json"));
        let _ = save_workspace("1", &legacy);
        app.workspaces.push(WorkspaceMeta { id: "1".into(), name: "My Workspace".into(), remote: None });
        app.active = "1".into();
        let _ = save_app(&app);
    }
    if !app.workspaces.iter().any(|w| w.id == app.active) {
        app.active = app.workspaces[0].id.clone();
    }
    app
}

pub fn save_app(app: &AppData) -> io::Result<()> {
    write(app_path(), app)
}

pub fn load_workspace(id: &str) -> Workspace {
    read(workspace_path(id))
}

pub fn save_workspace(id: &str, ws: &Workspace) -> io::Result<()> {
    write(workspace_path(id), ws)
}

pub fn delete_workspace(id: &str) {
    let _ = fs::remove_file(workspace_path(id));
}
