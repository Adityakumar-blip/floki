//! Workspaces, accounts, teams and sync with the Floki server.

use crate::cloud::{self, Client, PushError};
use crate::model::{Account, Remote, SharedData, Workspace, WorkspaceMeta};
use crate::theme::Tokens;
use crate::ui::*;
use crate::{modal_card, modal_field, Modal, Popup, Floki, Tab};
use futures::channel::oneshot;
use gpui_kit::component::input::InputState;
use gpui_kit::prelude::*;
use gpui_kit::*;
use std::time::Duration;

const SYNC_EVERY: Duration = Duration::from_secs(3);
/// Checking for other people's changes keeps a scale-to-zero server and database awake, so do it less often than pushing.
// ponytail: polling; switch to server-sent events if near-instant updates or free-tier quotas start to matter.
const PULL_EVERY: Duration = Duration::from_secs(15);

/// Where a new or shared workspace lives.
#[derive(Clone, PartialEq)]
pub enum Target {
    /// Only on this computer.
    Local,
    /// On the server, shared with nobody until people are invited.
    Personal,
    Team(String),
    NewTeam,
}

pub enum CloudModal {
    SignIn {
        server: Entity<InputState>,
        name: Entity<InputState>,
        email: Entity<InputState>,
        password: Entity<InputState>,
        signup: bool,
        custom_server: bool,
        error: Option<String>,
        busy: bool,
    },
    /// Creates a workspace, or (with `share`) moves the active local one to the server.
    Workspace {
        share: bool,
        name: Entity<InputState>,
        new_team: Entity<InputState>,
        target: Target,
        teams: Vec<cloud::Team>,
        error: Option<String>,
        busy: bool,
    },
    Members {
        /// `workspaces` or `teams`.
        scope: &'static str,
        id: String,
        title: String,
        note: Option<String>,
        admin: bool,
        email: Entity<InputState>,
        role: &'static str,
        data: Option<cloud::Members>,
        error: Option<String>,
        busy: bool,
    },
    Teams {
        teams: Option<Vec<cloud::Team>>,
        name: Entity<InputState>,
        error: Option<String>,
        busy: bool,
    },
    RenameWorkspace(Entity<InputState>),
    Verify {
        code: Entity<InputState>,
        error: Option<String>,
        busy: bool,
    },
}

fn roles(scope: &str) -> &'static [&'static str] {
    if scope == "teams" {
        &["admin", "member"]
    } else {
        &["admin", "editor", "viewer"]
    }
}

fn role_label(role: &str) -> &'static str {
    match role {
        "admin" => "Admin",
        "editor" => "Editor",
        "viewer" => "Viewer",
        _ => "Member",
    }
}

fn input(window: &mut Window, cx: &mut Context<Floki>, placeholder: &str, value: &str) -> Entity<InputState> {
    let (placeholder, value) = (placeholder.to_string(), value.to_string());
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
}

impl Floki {
    /// Runs `job` on a worker thread, then `done` back on the UI thread.
    fn run<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        job: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let (tx, rx) = oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(value) = rx.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                done(this, value, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn account(&self, cx: &App) -> Option<Account> {
        self.store.read(cx).app.account.clone()
    }

    fn client(&self, cx: &App) -> Option<Client> {
        self.account(cx).map(|a| Client::new(&a.server, Some(&a.token)))
    }

    /// Mutable access to the modal's fields; `None` once the modal was closed or replaced.
    fn cloud_modal(&mut self) -> Option<&mut CloudModal> {
        match &mut self.modal {
            Some(Modal::Cloud(m)) => Some(m),
            _ => None,
        }
    }

    fn set_busy(&mut self, busy: bool, error: Option<String>) {
        if let Some(
            CloudModal::SignIn { busy: b, error: e, .. }
            | CloudModal::Workspace { busy: b, error: e, .. }
            | CloudModal::Members { busy: b, error: e, .. }
            | CloudModal::Teams { busy: b, error: e, .. }
            | CloudModal::Verify { busy: b, error: e, .. },
        ) = self.cloud_modal()
        {
            *b = busy;
            *e = error;
        }
    }

    pub(crate) fn start_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor().timer(SYNC_EVERY).await;
            if this.update_in(cx, |this, window, cx| this.sync_tick(false, window, cx)).is_err() {
                break;
            }
        })
        .detach();
    }


    pub(crate) fn open_sign_in(&mut self, signup: bool, window: &mut Window, cx: &mut Context<Self>) {
        let server = self.account(cx).map(|a| a.server).unwrap_or_else(cloud::default_server);
        // Only self-hosters need to see the address; everyone else signs in to the default server.
        let custom = server != cloud::default_server();
        let password = cx.new(|cx| InputState::new(window, cx).placeholder("Password").masked(true));
        self.modal = Some(Modal::Cloud(CloudModal::SignIn {
            server: input(window, cx, "Server URL", &server),
            name: input(window, cx, "Your name", ""),
            email: input(window, cx, "Email", ""),
            password,
            signup,
            custom_server: custom,
            error: None,
            busy: false,
        }));
        self.popup = None;
        cx.notify();
    }

    fn submit_sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(CloudModal::SignIn { server, name, email, password, signup, .. }) = self.cloud_modal() else { return };
        let signup = *signup;
        let [server, name, email, password] =
            [server, name, email, password].map(|i| i.read(cx).value().trim().to_string());
        if email.is_empty() || password.is_empty() {
            return self.set_busy(false, Some("Enter your email and password".into()));
        }
        self.set_busy(true, None);
        let target = server.clone();
        self.run(
            window,
            cx,
            move || {
                let client = Client::new(&target, None);
                if signup { client.signup(&name, &email, &password) } else { client.login(&email, &password) }
            },
            move |this, result, window, cx| this.finish_sign_in(server, result, window, cx),
        );
    }

    fn google_sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(CloudModal::SignIn { server, .. }) = self.cloud_modal() else { return };
        let server = server.read(cx).value().trim().to_string();
        self.set_busy(true, Some("Continue in your browser…".into()));
        let target = server.clone();
        self.run(window, cx, move || cloud::google_start(&target), move |this, flow, window, cx| match flow {
            Err(e) => this.set_busy(false, Some(e)),
            Ok(flow) => {
                cx.open_url(&flow.url);
                let target = server.clone();
                this.run(window, cx, move || cloud::google_finish(&target, flow), move |this, result, window, cx| {
                    this.finish_sign_in(server, result, window, cx)
                });
            }
        });
    }

    fn finish_sign_in(&mut self, server: String, result: Result<cloud::Session, String>, window: &mut Window, cx: &mut Context<Self>) {
        match result {
            Err(e) => self.set_busy(false, Some(e)),
            Ok(session) => {
                let account = Account {
                    server: server.trim_end_matches('/').into(),
                    token: session.token,
                    email: session.user.email,
                    name: session.user.name,
                    verified: session.user.verified,
                };
                self.notice = Some(format!("Signed in as {}", account.email));
                let verified = account.verified;
                self.store.update(cx, |s, cx| s.update_app(cx, |app| app.account = Some(account)));
                self.modal = None;
                self.refresh_server_workspaces(window, cx);
                if !verified {
                    self.open_verify(window, cx);
                }
            }
        }
    }

    fn open_verify(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = Some(Modal::Cloud(CloudModal::Verify { code: input(window, cx, "6-digit code", ""), error: None, busy: false }));
        self.popup = None;
        cx.notify();
    }

    fn submit_verify(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(CloudModal::Verify { code, .. })) = (self.client(cx), self.cloud_modal()) else { return };
        let code = code.read(cx).value().trim().to_string();
        self.set_busy(true, None);
        self.run(window, cx, move || client.verify(&code), |this, result, window, cx| match result {
            Err(e) => this.set_busy(false, Some(e)),
            Ok(()) => {
                this.store.update(cx, |s, cx| {
                    s.update_app(cx, |app| {
                        if let Some(a) = &mut app.account {
                            a.verified = true;
                        }
                    })
                });
                this.modal = None;
                this.notice = Some("Email verified. Invites sent to this address are now in your Workspaces menu.".into());
                this.refresh_server_workspaces(window, cx);
            }
        });
    }

    fn resend_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else { return };
        self.set_busy(true, None);
        self.run(window, cx, move || client.resend_code(), |this, result, _, _| {
            this.set_busy(false, Some(match result {
                Ok(()) => "A new code is on its way".into(),
                Err(e) => e,
            }))
        });
    }

    fn sign_out(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(client) = self.client(cx) {
            self.run(window, cx, move || client.logout(), |_, _, _, _| {});
        }
        self.store.update(cx, |s, cx| s.update_app(cx, |app| app.account = None));
        self.server_workspaces.clear();
        self.popup = None;
        self.notice = Some("Signed out. Shared workspaces stay on this computer but stop syncing.".into());
    }


    /// Loads the server's workspace list and refreshes names and roles of linked copies.
    pub(crate) fn refresh_server_workspaces(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else { return };
        self.run(window, cx, move || client.workspaces(), |this, result, _, cx| match result {
            Err(e) => this.notice = Some(e),
            Ok(list) => this.apply_server_list(list, cx),
        });
    }

    /// Takes names, roles and teams of linked workspaces from the server's list, saving only when something changed.
    fn apply_server_list(&mut self, list: Vec<cloud::WorkspaceInfo>, cx: &mut Context<Self>) {
        let mut app = self.store.read(cx).app.clone();
        for meta in &mut app.workspaces {
            let Some(remote) = &mut meta.remote else { continue };
            if let Some(info) = list.iter().find(|w| w.id == remote.id) {
                meta.name = info.name.clone();
                remote.role = info.role.clone();
                remote.team = info.team_name.clone();
            }
        }
        if app != self.store.read(cx).app {
            self.store.update(cx, |s, cx| s.update_app(cx, |a| a.workspaces = app.workspaces));
        }
        self.server_workspaces = list;
    }

    fn switch_workspace(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = None;
        if self.store.read(cx).app.active == id {
            return cx.notify();
        }
        let unsaved = self.tabs.iter().any(|tab| matches!(tab, Tab::Request { editor, .. } if editor.read(cx).is_dirty()));
        if unsaved {
            self.notice = Some("Save or close tabs with unsaved changes before switching workspaces".into());
            return cx.notify();
        }
        self.tabs.clear();
        self.store.update(cx, |s, cx| s.switch(&id, cx));
        self.open_request(None, crate::Request::default(), window, cx);
        self.sync_tick(true, window, cx);
    }

    /// Downloads a server workspace that has no local copy yet, then opens it.
    fn open_server_workspace(&mut self, info: cloud::WorkspaceInfo, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else { return };
        self.popup = None;
        self.notice = Some(format!("Opening \"{}\"…", info.name));
        let id = info.id.clone();
        self.run(window, cx, move || client.workspace(&id), move |this, result, window, cx| match result {
            Err(e) => this.notice = Some(e),
            Ok(remote) => {
                let mut ws = Workspace::default();
                ws.apply_shared(serde_json::from_value(remote.data).unwrap_or_default());
                let local = this.store.read(cx).app.new_local_id();
                let meta = WorkspaceMeta {
                    id: local.clone(),
                    name: info.name,
                    remote: Some(Remote { id: info.id, version: remote.version, role: info.role, team: info.team_name, dirty: false }),
                };
                this.store.update(cx, |s, cx| s.add_workspace(meta, ws, cx));
                this.notice = None;
                this.switch_workspace(local, window, cx);
            }
        });
    }

    pub(crate) fn open_workspace_modal(&mut self, share: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = None;
        if share && self.account(cx).is_none() {
            return self.open_sign_in(false, window, cx);
        }
        let current = self.store.read(cx).meta().name.clone();
        self.modal = Some(Modal::Cloud(CloudModal::Workspace {
            share,
            name: input(window, cx, "Workspace name", if share { &current } else { "" }),
            new_team: input(window, cx, "Team name", ""),
            target: if share { Target::Personal } else { Target::Local },
            teams: Vec::new(),
            error: None,
            busy: false,
        }));
        if let Some(client) = self.client(cx) {
            self.run(window, cx, move || client.me(), |this, result, _, _| {
                if let (Ok(me), Some(CloudModal::Workspace { teams, .. })) = (result, this.cloud_modal()) {
                    *teams = me.teams;
                }
            });
        }
        cx.notify();
    }

    fn submit_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(CloudModal::Workspace { share, name, new_team, target, .. }) = self.cloud_modal() else { return };
        let (share, target) = (*share, target.clone());
        let name = name.read(cx).value().trim().to_string();
        let team_name = new_team.read(cx).value().trim().to_string();
        if name.is_empty() {
            return self.set_busy(false, Some("Enter a workspace name".into()));
        }
        if target == Target::NewTeam && team_name.is_empty() {
            return self.set_busy(false, Some("Enter a team name".into()));
        }
        if target == Target::Local {
            let local = self.store.read(cx).app.new_local_id();
            let meta = WorkspaceMeta { id: local.clone(), name, remote: None };
            self.store.update(cx, |s, cx| s.add_workspace(meta, Workspace::default(), cx));
            self.modal = None;
            return self.switch_workspace(local, window, cx);
        }
        let Some(client) = self.client(cx) else { return self.open_sign_in(false, window, cx) };
        let data = if share { self.store.read(cx).ws.shared() } else { SharedData::default() };
        self.set_busy(true, None);
        self.run(
            window,
            cx,
            move || {
                let team = match target {
                    Target::Team(id) => Some(id),
                    Target::NewTeam => Some(client.create_team(&team_name)?.id),
                    _ => None,
                };
                client.create_workspace(&name, team.as_deref(), &data)
            },
            move |this, result, window, cx| match result {
                Err(e) => this.set_busy(false, Some(e)),
                Ok(info) => {
                    let remote = Remote { id: info.id.clone(), version: info.version, role: info.role.clone(), team: info.team_name.clone(), dirty: false };
                    this.modal = None;
                    if share {
                        this.store.update(cx, |s, cx| {
                            let active = s.app.active.clone();
                            s.update_app(cx, |app| {
                                if let Some(meta) = app.workspaces.iter_mut().find(|w| w.id == active) {
                                    meta.name = info.name.clone();
                                    meta.remote = Some(remote);
                                }
                            })
                        });
                        this.open_members("workspaces", info.id, info.name, true, window, cx);
                    } else {
                        let local = this.store.read(cx).app.new_local_id();
                        let meta = WorkspaceMeta { id: local.clone(), name: info.name, remote: Some(remote) };
                        this.store.update(cx, |s, cx| s.add_workspace(meta, Workspace::default(), cx));
                        this.switch_workspace(local, window, cx);
                    }
                    this.refresh_server_workspaces(window, cx);
                }
            },
        );
    }

    pub(crate) fn invite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let meta = self.store.read(cx).meta().clone();
        match meta.remote {
            None => self.open_workspace_modal(true, window, cx),
            Some(remote) => {
                let note = remote.team.map(|team| format!("Everyone in the {team} team can also open this workspace."));
                self.open_members("workspaces", remote.id, meta.name, remote.role == "admin", window, cx);
                if let Some(CloudModal::Members { note: n, .. }) = self.cloud_modal() {
                    *n = note;
                }
            }
        }
    }

    fn rename_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.store.read(cx).meta().name.clone();
        self.modal = Some(Modal::Cloud(CloudModal::RenameWorkspace(input(window, cx, "Workspace name", &name))));
        self.popup = None;
        cx.notify();
    }

    fn submit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(CloudModal::RenameWorkspace(input)) = self.cloud_modal() else { return };
        let name = input.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        let meta = self.store.read(cx).meta().clone();
        self.modal = None;
        if let (Some(remote), Some(client)) = (&meta.remote, self.client(cx)) {
            if remote.role != "admin" {
                self.notice = Some("Only workspace admins can rename it".into());
                return;
            }
            let id = remote.id.clone();
            let new_name = name.clone();
            self.run(window, cx, move || client.rename_workspace(&id, &new_name), |this, result, _, _| {
                if let Err(e) = result {
                    this.notice = Some(e);
                }
            });
        }
        self.store.update(cx, |s, cx| {
            s.update_app(cx, |app| {
                if let Some(m) = app.workspaces.iter_mut().find(|w| w.id == meta.id) {
                    m.name = name;
                }
            })
        });
    }

    /// Local workspaces are deleted; shared ones are deleted for everyone by an admin, otherwise only the local copy goes.
    fn delete_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.popup = None;
        let app = self.store.read(cx).app.clone();
        let meta = self.store.read(cx).meta().clone();
        let Some(next) = app.workspaces.iter().find(|w| w.id != meta.id).map(|w| w.id.clone()) else {
            self.notice = Some("Create another workspace before deleting this one".into());
            return cx.notify();
        };
        if let (Some(remote), Some(client)) = (&meta.remote, self.client(cx)) {
            if remote.role == "admin" {
                let id = remote.id.clone();
                self.run(window, cx, move || client.delete_workspace(&id), |this, result, window, cx| {
                    if let Err(e) = result {
                        this.notice = Some(e);
                    }
                    this.refresh_server_workspaces(window, cx);
                });
            }
        }
        self.switch_workspace(next, window, cx);
        if self.store.read(cx).app.active != meta.id {
            self.store.update(cx, |s, cx| s.remove_workspace(&meta.id, cx));
        }
    }


    pub(crate) fn open_members(&mut self, scope: &'static str, id: String, title: String, admin: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = Some(Modal::Cloud(CloudModal::Members {
            scope,
            id,
            title,
            note: None,
            admin,
            email: input(window, cx, "Email address", ""),
            role: if scope == "teams" { "member" } else { "editor" },
            data: None,
            error: None,
            busy: false,
        }));
        self.popup = None;
        self.load_members(window, cx);
    }

    fn load_members(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(CloudModal::Members { scope, id, .. })) = (self.client(cx), self.cloud_modal()) else { return };
        let (scope, id) = (*scope, id.clone());
        self.run(window, cx, move || client.members(scope, &id), |this, result, _, _| {
            if let Some(CloudModal::Members { data, error, busy, .. }) = this.cloud_modal() {
                *busy = false;
                match result {
                    Ok(m) => *data = Some(m),
                    Err(e) => *error = Some(e),
                }
            }
        });
    }

    /// Runs a membership change, then reloads the list.
    fn change_members(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&Client, &'static str, &str) -> Result<String, String> + Send + 'static,
    ) {
        let (Some(client), Some(CloudModal::Members { scope, id, .. })) = (self.client(cx), self.cloud_modal()) else { return };
        let (scope, id) = (*scope, id.clone());
        self.set_busy(true, None);
        self.run(window, cx, move || change(&client, scope, &id), |this, result, window, cx| {
            match result {
                Ok(message) => {
                    this.set_busy(true, None);
                    if !message.is_empty() {
                        this.notice = Some(message);
                    }
                }
                Err(e) => this.set_busy(false, Some(e)),
            }
            this.load_members(window, cx);
            this.refresh_server_workspaces(window, cx);
        });
    }

    fn submit_invite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(CloudModal::Members { email, role, .. }) = self.cloud_modal() else { return };
        let (address, role) = (email.read(cx).value().trim().to_string(), *role);
        if !address.contains('@') {
            return self.set_busy(false, Some("Enter an email address".into()));
        }
        email.update(cx, |s, cx| s.set_value("", window, cx));
        self.change_members(window, cx, move |client, scope, id| {
            let status = client.add_member(scope, id, &address, role)?;
            Ok(if status == "invited" {
                format!("Invited {address}; they get access when they sign up")
            } else {
                format!("Added {address}")
            })
        });
    }

    fn open_teams(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = Some(Modal::Cloud(CloudModal::Teams { teams: None, name: input(window, cx, "New team name", ""), error: None, busy: false }));
        self.popup = None;
        self.load_teams(window, cx);
    }

    fn load_teams(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.client(cx) else { return };
        self.run(window, cx, move || client.me(), |this, result, _, _| {
            if let Some(CloudModal::Teams { teams, error, busy, .. }) = this.cloud_modal() {
                *busy = false;
                match result {
                    Ok(me) => *teams = Some(me.teams),
                    Err(e) => *error = Some(e),
                }
            }
        });
    }

    fn create_team(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(CloudModal::Teams { name, .. })) = (self.client(cx), self.cloud_modal()) else { return };
        let team = name.read(cx).value().trim().to_string();
        if team.is_empty() {
            return;
        }
        name.update(cx, |s, cx| s.set_value("", window, cx));
        self.set_busy(true, None);
        self.run(window, cx, move || client.create_team(&team), |this, result, window, cx| match result {
            Err(e) => this.set_busy(false, Some(e)),
            Ok(_) => this.load_teams(window, cx),
        });
    }


    pub(crate) fn sync_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notice = Some(if self.client(cx).is_some() { "Syncing…".into() } else { "Sign in to sync this workspace".into() });
        self.refresh_server_workspaces(window, cx);
        self.sync_tick(true, window, cx);
        cx.notify();
    }

    /// Pushes local changes, or pulls the server's when they are newer. Runs every few seconds.
    /// `force` checks the server now instead of waiting for the next pull.
    pub(crate) fn sync_tick(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.syncing {
            return;
        }
        let Some(client) = self.client(cx) else { return };
        let store = self.store.read(cx);
        let meta = store.meta().clone();
        let Some(remote) = meta.remote else { return };
        self.syncing = true;
        let local = meta.id;
        if remote.dirty && remote.role != "viewer" {
            let (data, edits) = (store.ws.shared(), store.edits);
            let (id, base) = (remote.id.clone(), remote.version);
            self.run(window, cx, move || client.push(&id, base, &data), move |this, result, window, cx| {
                this.syncing = false;
                if result.is_ok() {
                    this.clear_sync_notice();
                }
                match result {
                    Ok(version) => this.store.update(cx, |s, cx| {
                        let clean = s.edits == edits && s.app.active == local;
                        s.update_app(cx, |app| {
                            if let Some(r) = app.workspaces.iter_mut().find(|w| w.id == local).and_then(|w| w.remote.as_mut()) {
                                r.version = version;
                                r.dirty &= !clean;
                            }
                        })
                    }),
                    Err(PushError::Conflict) if this.store.read(cx).app.active == local => this.resolve_conflict(false, window, cx),
                    Err(PushError::Forbidden) if this.store.read(cx).app.active == local => {
                        this.resolve_conflict(true, window, cx);
                        this.refresh_server_workspaces(window, cx);
                    }
                    Err(PushError::Conflict | PushError::Forbidden) => {}
                    Err(PushError::Other(e)) => this.notice = Some(format!("Sync failed: {e}")),
                }
            });
        } else {
            if !force && self.last_pull.is_some_and(|t| t.elapsed() < PULL_EVERY) {
                self.syncing = false;
                return;
            }
            self.last_pull = Some(std::time::Instant::now());
            let id = remote.id.clone();
            // The list carries renames and role changes too, so one request per tick covers everything.
            let job = move || {
                let list = client.workspaces()?;
                let newer = list.iter().any(|w| w.id == id && w.version > remote.version);
                let data = if newer { Some(client.workspace(&id)?) } else { None };
                Ok::<_, String>((list, data))
            };
            self.run(window, cx, job, move |this, result, window, cx| {
                this.syncing = false;
                let result = result.map(|(list, data)| {
                    this.apply_server_list(list, cx);
                    data
                });
                match result {
                    Ok(Some(data)) => {
                        this.pull(&local, data, window, cx);
                        this.clear_sync_notice();
                    }
                    Ok(None) => this.clear_sync_notice(),
                    Err(e) => this.notice = Some(format!("Sync failed: {e}")),
                }
            });
        }
    }

    fn clear_sync_notice(&mut self) {
        if self.notice.as_deref() == Some("Syncing…") || self.notice.as_deref().is_some_and(|n| n.starts_with("Sync failed")) {
            self.notice = None;
        }
    }

    fn pull(&mut self, local: &str, data: cloud::WorkspaceData, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        // Something changed meanwhile; the next tick pushes it and the server reports the conflict.
        if store.app.active != local || store.meta().remote.as_ref().is_some_and(|r| r.dirty) {
            return;
        }
        let shared = serde_json::from_value(data.data).unwrap_or_default();
        self.store.update(cx, |s, cx| s.apply_remote(shared, data.version, cx));
        self.reload_tabs(window, cx);
    }

    /// Keeps the local edits as a separate local workspace, then takes the server's copy.
    /// `denied`: the push was refused because access was reduced, not because someone else pushed first.
    fn resolve_conflict(&mut self, denied: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), store) = (self.client(cx), self.store.read(cx)) else { return };
        let meta = store.meta().clone();
        let Some(remote) = meta.remote.clone() else { return };
        let copy = WorkspaceMeta { id: store.app.new_local_id(), name: format!("{} (conflicted copy)", meta.name), remote: None };
        let mine = store.ws.clone();
        self.notice = Some(if denied {
            format!("You can no longer edit \"{}\". Your changes were kept as \"{}\".", meta.name, copy.name)
        } else {
            format!("Someone else changed \"{}\" at the same time. Your version was kept as \"{}\".", meta.name, copy.name)
        });
        self.store.update(cx, |s, cx| s.add_workspace(copy, mine, cx));
        self.syncing = true;
        self.run(window, cx, move || client.workspace(&remote.id), move |this, result, window, cx| {
            this.syncing = false;
            match result {
                Ok(data) => {
                    let shared = serde_json::from_value(data.data).unwrap_or_default();
                    this.store.update(cx, |s, cx| s.apply_remote(shared, data.version, cx));
                    this.reload_tabs(window, cx);
                }
                // Access was removed entirely: keep the copy here as a plain local workspace.
                Err(_) if denied => this.store.update(cx, |s, cx| {
                    let local = meta.id.clone();
                    s.update_app(cx, |app| {
                        if let Some(m) = app.workspaces.iter_mut().find(|w| w.id == local) {
                            m.remote = None;
                        }
                    })
                }),
                Err(e) => this.notice = Some(e),
            }
        });
    }

    /// Brings open tabs in line with workspace data that changed underneath them.
    fn reload_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ws = self.store.read(cx).ws.clone();
        let mut remove = Vec::new();
        for ix in 0..self.tabs.len() {
            match &self.tabs[ix] {
                Tab::Request { editor, .. } => {
                    let Some(id) = editor.read(cx).saved_id else { continue };
                    match ws.request(id) {
                        None => editor.update(cx, |e, _| e.saved_id = None),
                        Some((_, r)) if !editor.read(cx).is_dirty() && editor.read(cx).request != *r => {
                            self.tabs[ix] = self.request_tab(Some(id), r.clone(), window, cx)
                        }
                        _ => {}
                    }
                }
                Tab::Environment { id, editor, .. } => match ws.variable_set(*id) {
                    None => remove.push(ix),
                    Some((_, values)) if editor.read(cx).values(cx) != *values => {
                        if let Some(tab) = self.variables_tab(*id, window, cx) {
                            self.tabs[ix] = tab;
                        }
                    }
                    _ => {}
                },
                Tab::Example { request, ix: i, .. } => {
                    if ws.request(*request).and_then(|(_, r)| r.examples.get(*i)).is_none() {
                        remove.push(ix);
                    }
                }
            }
        }
        for ix in remove.into_iter().rev() {
            self.tabs.remove(ix);
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        if self.tabs.is_empty() {
            self.open_request(None, crate::Request::default(), window, cx);
        }
        cx.notify();
    }


    /// Header controls: the Workspaces menu on the left.
    pub(crate) fn render_workspaces_button(&self, t: &Tokens, cx: &Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let active = store.app.active.clone();
        let linked: Vec<String> = store.app.workspaces.iter().filter_map(|w| Some(w.remote.as_ref()?.id.clone())).collect();
        let row = |id: String, name: String, kind: String, current: bool| {
            menu_item(SharedString::from(format!("ws-{id}")), t)
                .child(icon(if kind == "Team" { "icons/users.svg" } else { "icons/user.svg" }, 14., t.text_secondary))
                .child(div().flex_1().min_w_0().truncate().child(name))
                .child(div().text_color(rgb(t.text_tertiary)).child(kind))
                .when(current, |d| d.child(icon("icons/check.svg", 14., t.text_secondary)))
        };
        let mut list = menu(t).w(px(320.)).child(
            div().px_3().py_1().text_color(rgb(t.text_tertiary)).font_weight(FontWeight::SEMIBOLD).child("Workspaces"),
        );
        for w in &store.app.workspaces {
            let kind = match &w.remote {
                None => "Local".to_string(),
                Some(Remote { team: Some(_), .. }) => "Team".into(),
                Some(_) => "Personal".into(),
            };
            let id = w.id.clone();
            list = list.child(row(w.id.clone(), w.name.clone(), kind, w.id == active).on_click(
                cx.listener(move |this, _, window, cx| this.switch_workspace(id.clone(), window, cx)),
            ));
        }
        let more: Vec<_> = self.server_workspaces.iter().filter(|w| !linked.contains(&w.id)).cloned().collect();
        if !more.is_empty() {
            list = list.child(
                div().px_3().pt_2().pb_1().text_color(rgb(t.text_tertiary)).font_weight(FontWeight::SEMIBOLD).child("Shared with you"),
            );
            for info in more {
                let kind = if info.team_name.is_some() { "Team" } else { "Personal" }.to_string();
                list = list.child(row(format!("remote-{}", info.id), info.name.clone(), kind, false).on_click(
                    cx.listener(move |this, _, window, cx| this.open_server_workspace(info.clone(), window, cx)),
                ));
            }
        }
        list = list.child(div().h(px(1.)).my_1().bg(rgb(t.border))).child(
            menu_item("ws-create", t)
                .child(icon("icons/plus.svg", 14., t.text_secondary))
                .child("Create Workspace")
                .on_click(cx.listener(|this, _, window, cx| this.open_workspace_modal(false, window, cx))),
        );
        div()
            .id("workspaces-menu")
            .relative()
            .h(px(32.))
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(t.hover)))
            .text_color(rgb(t.text))
            .child("Workspaces")
            .child(icon("icons/chevron-down.svg", 14., t.text_secondary))
            .on_click(cx.listener(|this, _, window, cx| {
                if this.popup != Some(Popup::Workspaces) {
                    this.refresh_server_workspaces(window, cx);
                }
                this.toggle_popup(Popup::Workspaces, cx);
            }))
            .when(self.popup == Some(Popup::Workspaces), |d| d.child(popover(list)))
    }

    /// Header controls on the right: Invite and the account menu.
    pub(crate) fn render_account_controls(&self, t: &Tokens, cx: &Context<Self>) -> impl IntoElement {
        let account = self.account(cx);
        let shared = self.store.read(cx).meta().remote.is_some();
        let invite = outline_button("invite", if shared { "Invite" } else { "Share" }, t)
            .h(px(28.))
            .child(icon("icons/user-plus.svg", 14., t.text_secondary))
            .on_click(cx.listener(|this, _, window, cx| this.invite(window, cx)));
        let account_part = match account {
            None => div().child(
                primary_button("sign-in", "Sign In", t)
                    .h(px(28.))
                    .on_click(cx.listener(|this, _, window, cx| this.open_sign_in(false, window, cx))),
            )
            .into_any_element(),
            Some(a) => {
                let initial = a.name.chars().next().or(a.email.chars().next()).unwrap_or('?').to_uppercase().to_string();
                let account_menu = menu(t)
                    .w(px(240.))
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .flex()
                            .flex_col()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(a.name.clone()))
                            .child(div().text_color(rgb(t.text_secondary)).child(a.email.clone()))
                            .child(div().text_color(rgb(t.text_tertiary)).child(a.server.clone())),
                    )
                    .child(div().h(px(1.)).my_1().bg(rgb(t.border)))
                    .when(!a.verified, |d| {
                        d.child(
                            menu_item("account-verify", t)
                                .text_color(rgb(t.warning))
                                .child(icon("icons/circle-alert.svg", 14., t.warning))
                                .child("Verify your email")
                                .on_click(cx.listener(|this, _, window, cx| this.open_verify(window, cx))),
                        )
                    })
                    .child(
                        menu_item("account-teams", t)
                            .child(icon("icons/users.svg", 14., t.text_secondary))
                            .child("Teams")
                            .on_click(cx.listener(|this, _, window, cx| this.open_teams(window, cx))),
                    )
                    .child(
                        menu_item("account-sign-out", t)
                            .child(icon("icons/log-out.svg", 14., t.text_secondary))
                            .child("Sign out")
                            .on_click(cx.listener(|this, _, window, cx| this.sign_out(window, cx))),
                    );
                div()
                    .id("account")
                    .relative()
                    .size(px(28.))
                    .rounded(px(14.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .bg(rgb(t.info))
                    .text_color(rgb(0xffffff))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(initial)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_popup(Popup::Account, cx)))
                    .when(self.popup == Some(Popup::Account), |d| {
                        d.child(div().absolute().right_0().top_full().child(popover(account_menu)))
                    })
                    .into_any_element()
            }
        };
        div().flex().items_center().gap_2().child(invite).child(account_part)
    }

    /// Sidebar title: workspace name, its kind, and a menu to rename or delete it.
    pub(crate) fn render_workspace_title(&self, t: &Tokens, cx: &Context<Self>) -> impl IntoElement {
        let meta = self.store.read(cx).meta().clone();
        let badge = match &meta.remote {
            None => "Local".to_string(),
            Some(r) => format!("{}{}", if r.team.is_some() { "Team · " } else { "" }, role_label(&r.role)),
        };
        let delete_label = match &meta.remote {
            Some(r) if r.role != "admin" => "Remove from this computer",
            Some(_) => "Delete for everyone",
            None => "Delete workspace",
        };
        let ws_menu = menu(t)
            .child(menu_item("ws-rename", t).child("Rename").on_click(cx.listener(|this, _, window, cx| this.rename_workspace(window, cx))))
            .child(menu_item("ws-share", t).child(if meta.remote.is_some() { "Manage access" } else { "Share" }).on_click(
                cx.listener(|this, _, window, cx| {
                    this.popup = None;
                    this.invite(window, cx)
                }),
            ))
            .when(meta.remote.is_some(), |d| {
                d.child(menu_item("ws-sync", t).child("Sync now").on_click(cx.listener(|this, _, window, cx| {
                    this.popup = None;
                    this.sync_now(window, cx)
                })))
            })
            .child(menu_item("ws-delete", t).child(delete_label).on_click(cx.listener(|this, _, window, cx| this.delete_workspace(window, cx))));
        div()
            .id("ws-title")
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .child(div().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(meta.name.clone()))
            .child(div().flex_none().text_size(px(10.)).text_color(rgb(t.text_tertiary)).child(badge))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_popup(Popup::WorkspaceMenu, cx)))
            .when(self.popup == Some(Popup::WorkspaceMenu), |d| d.child(popover(ws_menu)))
    }

    pub(crate) fn render_cloud_modal(&self, modal: &CloudModal, t: &Tokens, cx: &Context<Self>) -> Stateful<Div> {
        let footer = || div().px_4().py_3().flex().items_center().justify_end().gap_2();
        let label = |text: &'static str| div().text_color(rgb(t.text_secondary)).child(text);
        let error_line = |error: &Option<String>| {
            error.clone().map(|e| div().px_4().pt_2().text_color(rgb(t.error)).child(e))
        };
        let close = || {
            outline_button("modal-cancel", "Cancel", t).on_click(cx.listener(|this, _, _, cx| {
                this.modal = None;
                cx.notify();
            }))
        };
        match modal {
            CloudModal::SignIn { server, name, email, password, signup, custom_server, error, busy } => {
                let signup = *signup;
                let custom = *custom_server;
                modal_card(if signup { "Create your account" } else { "Sign in to Floki" }, 440., t, cx)
                    .child(
                        div()
                            .px_4()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .when(signup, |d| d.child(label("Name")).child(modal_field(name, t)))
                            .child(label("Email"))
                            .child(modal_field(email, t))
                            .child(label("Password"))
                            .child(modal_field(password, t))
                            .when(custom, |d| d.child(label("Server")).child(modal_field(server, t)))
                            .child(
                                div()
                                    .id("toggle-server")
                                    .cursor_pointer()
                                    .text_color(rgb(t.text_tertiary))
                                    .child(if custom { "Use the Floki server" } else { "Use a self-hosted server" })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(CloudModal::SignIn { server, custom_server, .. }) = this.cloud_modal() {
                                            *custom_server = !custom;
                                            if custom {
                                                server.update(cx, |s, cx| s.set_value(cloud::default_server(), window, cx));
                                            }
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                outline_button("google", "Continue with Google", t)
                                    .mt_2()
                                    .justify_center()
                                    .on_click(cx.listener(|this, _, window, cx| this.google_sign_in(window, cx))),
                            ),
                    )
                    .children(error_line(error))
                    .child(
                        footer()
                            .child(
                                div()
                                    .id("toggle-signup")
                                    .flex_1()
                                    .cursor_pointer()
                                    .text_color(rgb(t.link))
                                    .child(if signup { "I already have an account" } else { "Create an account" })
                                    .on_click(cx.listener(move |this, _, window, cx| this.open_sign_in(!signup, window, cx))),
                            )
                            .child(close())
                            .child(
                                primary_button("sign-in-go", if *busy { "Working…" } else if signup { "Create Account" } else { "Sign In" }, t)
                                    .on_click(cx.listener(|this, _, window, cx| this.submit_sign_in(window, cx))),
                            ),
                    )
            }
            CloudModal::Workspace { share, name, new_team, target, teams, error, busy } => {
                let signed_in = self.account(cx).is_some();
                let option = |id: SharedString, value: Target, title: String, detail: &'static str| {
                    let on = *target == value;
                    div()
                        .id(id)
                        .p_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(rgb(if on { t.brand } else { t.border }))
                        .cursor_pointer()
                        .child(div().flex_1().flex().flex_col().child(div().font_weight(FontWeight::SEMIBOLD).child(title)).child(
                            div().text_color(rgb(t.text_secondary)).child(detail),
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if value != Target::Local && this.account(cx).is_none() {
                                return this.open_sign_in(false, window, cx);
                            }
                            if let Some(CloudModal::Workspace { target, .. }) = this.cloud_modal() {
                                *target = value.clone();
                            }
                            cx.notify();
                        }))
                };
                let mut options = div().flex().flex_col().gap_2();
                if !*share {
                    options = options.child(option("t-local".into(), Target::Local, "Only on this computer".into(), "Not synced. You can share it later."));
                }
                options = options.child(option(
                    "t-personal".into(),
                    Target::Personal,
                    "Personal".into(),
                    if signed_in { "Synced to your account; invite people individually." } else { "Sign in to sync and share." },
                ));
                for team in teams {
                    options = options.child(option(
                        SharedString::from(format!("t-team-{}", team.id)),
                        Target::Team(team.id.clone()),
                        format!("Team: {}", team.name),
                        "Everyone in the team can open and edit it.",
                    ));
                }
                if signed_in {
                    options = options.child(option("t-new-team".into(), Target::NewTeam, "New team…".into(), "Create a team and put this workspace in it."));
                }
                modal_card(if *share { "Share workspace" } else { "Create workspace" }, 500., t, cx)
                    .child(
                        div()
                            .px_4()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(label("Name"))
                            .child(modal_field(name, t))
                            .child(label(if *share { "Move it to" } else { "Who can access" }))
                            .child(options)
                            .when(*target == Target::NewTeam, |d| d.child(label("Team name")).child(modal_field(new_team, t))),
                    )
                    .children(error_line(error))
                    .child(footer().child(close()).child(
                        primary_button("ws-go", if *busy { "Working…" } else if *share { "Share" } else { "Create" }, t)
                            .on_click(cx.listener(|this, _, window, cx| this.submit_workspace(window, cx))),
                    ))
            }
            CloudModal::Members { scope, title, note, admin, email, role, data, error, busy, .. } => {
                let scope = *scope;
                let admin = *admin;
                let role_chip = |id: SharedString, current: String, user: Option<String>| {
                    let chip = div()
                        .id(id)
                        .px_2()
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .gap_1()
                        .rounded(px(4.))
                        .text_color(rgb(t.text_secondary))
                        .child(role_label(&current));
                    if !admin {
                        return chip;
                    }
                    chip.cursor_pointer()
                        .border_1()
                        .border_color(rgb(t.border))
                        .child(icon("icons/chevrons-up-down.svg", 12., t.text_tertiary))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let list = roles(scope);
                            let next = list[(list.iter().position(|r| *r == current).unwrap_or(0) + 1) % list.len()];
                            match user.clone() {
                                Some(user) => this.change_members(window, cx, move |c, s, id| c.set_role(s, id, &user, next).map(|_| String::new())),
                                None => {
                                    if let Some(CloudModal::Members { role, .. }) = this.cloud_modal() {
                                        *role = next;
                                    }
                                    cx.notify();
                                }
                            }
                        }))
                };
                let remove = |id: SharedString, run: Box<dyn Fn(&Client, &'static str, &str) -> Result<(), String> + Send + Sync>| {
                    let run = std::sync::Arc::new(run);
                    icon_button(id, "icons/close.svg", t).size(px(24.)).on_click(cx.listener(move |this, _, window, cx| {
                        let run = run.clone();
                        this.change_members(window, cx, move |c, s, id| run(c, s, id).map(|_| String::new()))
                    }))
                };
                let row = |left: Div, right: Div| div().h(px(36.)).flex().items_center().gap_2().border_b_1().border_color(rgb(t.border)).child(left.flex_1().min_w_0()).child(right);
                let list = match data {
                    None => div().py_3().text_color(rgb(t.text_secondary)).child(if *busy { "Loading…" } else { "" }).into_any_element(),
                    Some(m) => div()
                        .id("members")
                        .max_h(px(280.))
                        .overflow_y_scroll()
                        .children(m.members.iter().map(|member| {
                            let user = member.user_id.clone();
                            row(
                                div().flex().flex_col().child(div().truncate().child(member.name.clone())).child(
                                    div().truncate().text_color(rgb(t.text_secondary)).child(member.email.clone()),
                                ),
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(role_chip(SharedString::from(format!("role-{user}")), member.role.clone(), Some(user.clone())))
                                    .when(admin, |d| {
                                        let user = user.clone();
                                        d.child(remove(
                                            SharedString::from(format!("remove-{user}")),
                                            Box::new(move |c, s, id| c.remove_member(s, id, &user)),
                                        ))
                                    }),
                            )
                        }))
                        .children(m.invites.iter().map(|invite| {
                            let address = invite.email.clone();
                            row(
                                div().flex().flex_col().child(div().truncate().child(invite.email.clone())).child(
                                    div().text_color(rgb(t.text_tertiary)).child("Invite pending"),
                                ),
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(div().text_color(rgb(t.text_secondary)).child(role_label(&invite.role)))
                                    .when(admin, |d| {
                                        d.child(remove(
                                            SharedString::from(format!("cancel-{address}")),
                                            Box::new(move |c, s, id| c.cancel_invite(s, id, &address)),
                                        ))
                                    }),
                            )
                        }))
                        .into_any_element(),
                };
                let heading = if scope == "teams" { format!("Team: {title}") } else { format!("Share \"{title}\"") };
                modal_card("Manage access", 520., t, cx)
                    .child(
                        div()
                            .px_4()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(heading))
                            .when_some(note.clone(), |d, n| d.child(div().text_color(rgb(t.text_secondary)).child(n)))
                            .when(admin, |d| {
                                d.child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(div().flex_1().child(modal_field(email, t)))
                                        .child(role_chip("invite-role".into(), role.to_string(), None))
                                        .child(
                                            primary_button("invite-go", "Invite", t)
                                                .on_click(cx.listener(|this, _, window, cx| this.submit_invite(window, cx))),
                                        ),
                                )
                            })
                            .when(!admin, |d| {
                                d.child(div().text_color(rgb(t.text_tertiary)).child("Only admins can invite people or change roles."))
                            })
                            .child(list),
                    )
                    .children(error_line(error))
                    .child(footer().child(
                        primary_button("members-done", "Done", t).on_click(cx.listener(|this, _, _, cx| {
                            this.modal = None;
                            cx.notify();
                        })),
                    ))
            }
            CloudModal::Teams { teams, name, error, busy } => modal_card("Teams", 460., t, cx)
                .child(
                    div()
                        .px_4()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(match teams {
                            None => div().text_color(rgb(t.text_secondary)).child("Loading…"),
                            Some(list) if list.is_empty() => {
                                div().text_color(rgb(t.text_secondary)).child("You are not in any team yet. Create one below.")
                            }
                            Some(list) => div().flex().flex_col().children(list.iter().map(|team| {
                                let (id, name, admin) = (team.id.clone(), team.name.clone(), team.role == "admin");
                                menu_item(SharedString::from(format!("team-{id}")), t)
                                    .child(icon("icons/users.svg", 14., t.text_secondary))
                                    .child(div().flex_1().child(team.name.clone()))
                                    .child(div().text_color(rgb(t.text_tertiary)).child(role_label(&team.role)))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_members("teams", id.clone(), name.clone(), admin, window, cx)
                                    }))
                            })),
                        })
                        .child(
                            div().pt_2().flex().items_center().gap_2().child(div().flex_1().child(modal_field(name, t))).child(
                                outline_button("team-create", if *busy { "Working…" } else { "Create team" }, t)
                                    .on_click(cx.listener(|this, _, window, cx| this.create_team(window, cx))),
                            ),
                        ),
                )
                .children(error_line(error))
                .child(footer().child(close())),
            CloudModal::Verify { code, error, busy } => modal_card("Verify your email", 440., t, cx)
                .child(
                    div()
                        .px_4()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(label("Enter the 6-digit code sent to your email. Until then, teammates' invites wait for you."))
                        .child(modal_field(code, t)),
                )
                .children(error_line(error))
                .child(
                    footer()
                        .child(
                            div()
                                .id("resend-code")
                                .flex_1()
                                .cursor_pointer()
                                .text_color(rgb(t.link))
                                .child("Send a new code")
                                .on_click(cx.listener(|this, _, window, cx| this.resend_code(window, cx))),
                        )
                        .child(close())
                        .child(
                            primary_button("verify-go", if *busy { "Working…" } else { "Verify" }, t)
                                .on_click(cx.listener(|this, _, window, cx| this.submit_verify(window, cx))),
                        ),
                ),
            CloudModal::RenameWorkspace(input) => modal_card("Rename workspace", 420., t, cx)
                .child(div().px_4().child(modal_field(input, t)))
                .child(footer().child(close()).child(
                    primary_button("ws-rename-go", "Rename", t).on_click(cx.listener(|this, _, window, cx| this.submit_rename(window, cx))),
                )),
        }
    }
}
