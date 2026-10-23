// Release builds on Windows open no console window behind the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod cloud;
mod grpc;
mod importers;
mod snippets;
mod http;
mod kv;
mod model;
mod request;
mod storage;
mod teams;
mod store;
mod theme;
mod ui;
mod ws;

use chrono::{Local, TimeZone};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::TitleBar;
use gpui_kit::prelude::*;
use gpui_kit::*;
use kv::{KvChanged, KvEditor};
use model::{parse_curl, Settings, GLOBALS_ID, Environment, Folder, Item, KeyValue, Request, SavedRequest, ThemeMode, Workspace};
use request::{RequestEditor, RequestEvent};
use std::borrow::Cow;
use std::path::PathBuf;
use store::Store;
use theme::{Tokens, FONT, MONO};
use ui::*;

actions!(
    floki,
    [Send, Save, NewTab, CloseTab, NextTab, PrevTab, ToggleSidebar, FocusUrl, Dismiss, OpenImport]
);

gpui_kit::assets::icon_assets!(ExtraIcons, [Clock, Layers, Code, Save, Send, FolderPlus, ArrowUpRight, ArrowDownLeft, Plug, Users, UserPlus, LogOut]);

struct AppAssets;

const LOGO: &str = "floki.svg";

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == LOGO {
            return Ok(Some(Cow::Borrowed(include_bytes!("../assets/floki.svg"))));
        }
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SidebarView {
    Collections,
    Environments,
    History,
}

#[derive(Clone, Copy, PartialEq)]
enum Popup {
    New,
    Environment,
    Settings,
    TabMenu,
    Row(u64),
    Workspaces,
    Account,
    WorkspaceMenu,
}

enum Tab {
    Request {
        editor: Entity<RequestEditor>,
        _sub: Subscription,
    },
    Environment {
        id: u64,
        editor: Entity<KvEditor>,
        _sub: Subscription,
    },
    /// A saved response, shown read-only in its own tab.
    Example {
        request: u64,
        ix: usize,
        body: Entity<EditorState>,
    },
}

enum Modal {
    SaveAs {
        editor: Entity<RequestEditor>,
        name: Entity<InputState>,
        new_collection: Entity<InputState>,
        collection: Option<u64>,
        close_after: bool,
    },
    ConfirmClose(usize),
    Import {
        text: Entity<EditorState>,
        error: Option<String>,
    },
    Rename {
        id: u64,
        input: Entity<InputState>,
    },
    Delete {
        id: u64,
        what: String,
    },
    /// Proxy, CA certificate, client certificate, client key.
    Settings([Entity<InputState>; 4]),
    Cloud(teams::CloudModal),
}

type RowAction = Box<dyn Fn(&mut Floki, &mut Window, &mut Context<Floki>)>;

struct Floki {
    store: Entity<Store>,
    tabs: Vec<Tab>,
    active: usize,
    view: SidebarView,
    sidebar_open: bool,
    filter: Entity<InputState>,
    popup: Option<Popup>,
    modal: Option<Modal>,
    notice: Option<String>,
    focus: FocusHandle,
    tab_scroll: ScrollHandle,
    /// Tab last scrolled into view, so manual scrolling isn't undone every frame.
    scrolled_to: std::cell::Cell<Option<usize>>,
    /// A push or pull is in flight.
    syncing: bool,
    last_pull: Option<std::time::Instant>,
    /// Workspaces on the server, for the Workspaces menu.
    server_workspaces: Vec<cloud::WorkspaceInfo>,
    _subs: Vec<Subscription>,
}

impl Floki {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = cx.new(|_| Store::load());
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subs = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(&filter, |_, _, _: &InputEvent, cx| cx.notify()),
        ];
        let mut app = Self {
            store,
            tabs: Vec::new(),
            active: 0,
            view: SidebarView::Collections,
            sidebar_open: true,
            filter,
            popup: None,
            modal: None,
            notice: None,
            focus: cx.focus_handle(),
            tab_scroll: ScrollHandle::new(),
            scrolled_to: Default::default(),
            syncing: false,
            last_pull: None,
            server_workspaces: Vec::new(),
            _subs: subs,
        };
        app.open_request(None, Request::default(), window, cx);
        app.start_sync(window, cx);
        app.refresh_server_workspaces(window, cx);
        app
    }

    fn ws<'a>(&self, cx: &'a App) -> &'a Workspace {
        &self.store.read(cx).ws
    }

    fn update_ws<R>(&self, cx: &mut Context<Self>, f: impl FnOnce(&mut Workspace) -> R) -> R {
        self.store.update(cx, |s, cx| s.update(cx, f))
    }

    fn active_editor(&self) -> Option<Entity<RequestEditor>> {
        match self.tabs.get(self.active) {
            Some(Tab::Request { editor, .. }) => Some(editor.clone()),
            _ => None,
        }
    }

    fn open_request(&mut self, saved_id: Option<u64>, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = saved_id {
            let existing = self.tabs.iter().position(|tab| {
                matches!(tab, Tab::Request { editor, .. } if editor.read(cx).saved_id == Some(id))
            });
            if let Some(ix) = existing {
                self.active = ix;
                cx.notify();
                return;
            }
        }
        let tab = self.request_tab(saved_id, request, window, cx);
        if let Tab::Request { editor, .. } = &tab {
            editor.read(cx).url_focus(cx).focus(window, cx);
        }
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    fn request_tab(&mut self, saved_id: Option<u64>, request: Request, window: &mut Window, cx: &mut Context<Self>) -> Tab {
        let store = self.store.clone();
        let editor = cx.new(|cx| RequestEditor::new(store, saved_id, request, window, cx));
        let sub = cx.subscribe_in(&editor, window, |this, editor, event: &RequestEvent, window, cx| match event {
            RequestEvent::Changed => cx.notify(),
            RequestEvent::SaveAs => this.open_save_as(editor.clone(), false, window, cx),
        });
        Tab::Request { editor, _sub: sub }
    }

    /// Editor tab for an environment, a collection's variables, or the globals.
    fn variables_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) -> Option<Tab> {
        let values = self.ws(cx).variable_set(id).map(|(_, v)| v.clone())?;
        let editor = cx.new(|cx| KvEditor::new(&values, ("Variable", "Value"), window, cx));
        let sub = cx.subscribe(&editor, move |this, editor, _: &KvChanged, cx| {
            let values = editor.read(cx).values(cx);
            this.update_ws(cx, |ws| {
                if let Some(set) = ws.variable_set_mut(id) {
                    *set = values;
                }
            });
        });
        Some(Tab::Environment { id, editor, _sub: sub })
    }

    fn open_environment(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self
            .tabs
            .iter()
            .position(|tab| matches!(tab, Tab::Environment { id: tid, .. } if *tid == id))
        {
            self.active = ix;
            cx.notify();
            return;
        }
        let Some(tab) = self.variables_tab(id, window, cx) else { return };
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    fn open_example(&mut self, request: u64, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pos) =
            self.tabs.iter().position(|tab| matches!(tab, Tab::Example { request: r, ix: i, .. } if *r == request && *i == ix))
        {
            self.active = pos;
            cx.notify();
            return;
        }
        let Some(example) = self.ws(cx).request(request).and_then(|(_, r)| r.examples.get(ix)).cloned() else { return };
        let response = http::Response::from(&example);
        let body = request::code_editor(&response.pretty(), response.language(), window, cx);
        self.tabs.push(Tab::Example { request, ix, body });
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    fn render_example_tab(&self, request: u64, ix: usize, body: &Entity<EditorState>, t: &Tokens, cx: &Context<Self>) -> AnyElement {
        let Some((_, r)) = self.ws(cx).request(request) else { return div().into_any_element() };
        let Some(example) = r.examples.get(ix) else { return div().into_any_element() };
        let saved = r.clone();
        let label = |text: &'static str| div().text_color(rgb(t.text_secondary)).font_weight(FontWeight::SEMIBOLD).child(text);
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .px_4()
            .pb_4()
            .bg(rgb(t.bg))
            .child(
                div()
                    .h(px(48.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(10.)).text_color(rgb(t.text_tertiary)).child("e.g."))
                    .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).text_size(px(14.)).child(example.name.clone()))
                    .child(div().text_color(rgb(t.text_tertiary)).child("Read-only"))
                    .child(outline_button("example-open-request", "Open request", t).on_click(cx.listener(
                        move |this, _, window, cx| this.open_request(Some(request), saved.clone(), window, cx),
                    ))),
            )
            .child(
                div()
                    .h(px(40.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(rgb(t.border))
                    .child(method_badge(&r.method, t))
                    .child(div().flex_1().min_w_0().truncate().child(r.url.clone())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(label("Response"))
                    .child(div().text_color(rgb(t.status(example.status))).child(format!(
                        "{} {}",
                        example.status,
                        http::Response::from(example).reason
                    ))),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(160.))
                    .border_1()
                    .border_color(rgb(t.border))
                    .rounded(px(4.))
                    .font_family(MONO)
                    .child(Editor::new(body).appearance(false).readonly(true).h_full()),
            )
            .when(!example.headers.is_empty(), |d| {
                d.child(label("Headers")).child(
                    div().id("example-headers").max_h(px(200.)).overflow_y_scroll().child(request::table(
                        t,
                        &["Key", "Value"],
                        example.headers.iter().map(|h| vec![h.key.clone(), h.value.clone()]).collect(),
                    )),
                )
            })
            .into_any_element()
    }

    fn close_tab(&mut self, ix: usize, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        if !force {
            if let Tab::Request { editor, .. } = &self.tabs[ix] {
                if editor.read(cx).is_dirty() {
                    self.active = ix;
                    self.modal = Some(Modal::ConfirmClose(ix));
                    cx.notify();
                    return;
                }
            }
        }
        self.tabs.remove(ix);
        if self.active > ix || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        if self.tabs.is_empty() {
            self.open_request(None, Request::default(), window, cx);
        }
        cx.notify();
    }

    fn open_save_as(&mut self, editor: Entity<RequestEditor>, close_after: bool, window: &mut Window, cx: &mut Context<Self>) {
        let title = editor.read(cx).request.title();
        let name = cx.new(|cx| InputState::new(window, cx).default_value(title));
        let new_collection = cx.new(|cx| InputState::new(window, cx).placeholder("New collection name"));
        let collection = self.ws(cx).collections.first().map(|c| c.id);
        name.read(cx).focus_handle(cx).focus(window, cx);
        self.modal = Some(Modal::SaveAs {
            editor,
            name,
            new_collection,
            collection,
            close_after,
        });
        cx.notify();
    }

    fn confirm_save_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::SaveAs {
            editor,
            name,
            new_collection,
            collection,
            close_after,
        }) = &self.modal
        else {
            return;
        };
        let (editor, close_after, target) = (editor.clone(), *close_after, *collection);
        let name = name.read(cx).value().trim().to_string();
        let new_name = new_collection.read(cx).value().trim().to_string();
        let mut request = editor.read(cx).request.clone();
        request.name = if name.is_empty() { request.title() } else { name };
        let id = self.update_ws(cx, |ws| {
            let collection = match target {
                Some(id) if new_name.is_empty() => id,
                _ => ws.add_collection(if new_name.is_empty() { "New Collection".into() } else { new_name }),
            };
            ws.add_request(collection, request.clone())
        });
        editor.update(cx, |e, cx| {
            e.request.name = request.name.clone();
            e.mark_saved(id, cx);
        });
        self.modal = None;
        if close_after {
            if let Some(ix) = self.tab_of(&editor) {
                self.close_tab(ix, true, window, cx);
            }
        }
        cx.notify();
    }

    fn tab_of(&self, editor: &Entity<RequestEditor>) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| matches!(tab, Tab::Request { editor: e, .. } if e == editor))
    }

    fn open_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx.new(|cx| EditorState::new(window, cx).language("json"));
        text.read(cx).focus_handle(cx).focus(window, cx);
        self.modal = Some(Modal::Import { text, error: None });
        self.popup = None;
        cx.notify();
    }

    /// Imports a cURL command, a Postman collection, or a Postman environment.
    fn import_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) -> std::result::Result<String, String> {
        let trimmed = text.trim();
        if trimmed.starts_with("curl") {
            let request = parse_curl(trimmed)?;
            self.open_request(None, request, window, cx);
            return Ok("Imported cURL request".into());
        }
        if let Some(spec) = importers::parse(trimmed) {
            let spec = spec?;
            let name = spec.name.clone();
            self.update_ws(cx, |ws| {
                let id = ws.add_collection(spec.name);
                let mut items = Vec::new();
                for (folder, requests) in spec.folders {
                    let requests: Vec<Item> = requests
                        .into_iter()
                        .map(|request| Item::Request(SavedRequest { id: ws.alloc_id(), request }))
                        .collect();
                    if folder.is_empty() {
                        items.extend(requests);
                    } else {
                        items.push(Item::Folder(Folder { id: ws.alloc_id(), name: folder, items: requests, expanded: false }));
                    }
                }
                if let Some(c) = ws.collections.iter_mut().find(|c| c.id == id) {
                    c.items = items;
                    c.variables = spec.variables;
                }
            });
            self.view = SidebarView::Collections;
            return Ok(format!("Imported collection \"{name}\""));
        }
        let value: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("Paste a cURL command, Postman JSON, or an OpenAPI, RAML or WSDL document ({e})"))?;
        if value.get("values").is_some() && value.get("info").is_none() {
            let name = value["name"].as_str().unwrap_or("Imported environment").to_string();
            let values: Vec<KeyValue> = value["values"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter_map(|r| {
                            Some(KeyValue {
                                key: r["key"].as_str()?.into(),
                                value: r["value"].as_str().unwrap_or_default().into(),
                                enabled: r["enabled"].as_bool().unwrap_or(true),
                                file: false,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let env_name = name.clone();
            self.update_ws(cx, |ws| {
                let id = ws.alloc_id();
                ws.environments.push(Environment { id, name: env_name, values });
            });
            self.view = SidebarView::Environments;
            return Ok(format!("Imported environment \"{name}\""));
        }
        let id = self.update_ws(cx, |ws| ws.import_postman(trimmed))?;
        self.view = SidebarView::Collections;
        let name = self
            .ws(cx)
            .collections
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        Ok(format!("Imported collection \"{name}\""))
    }

    fn finish_import(&mut self, result: std::result::Result<String, String>, cx: &mut Context<Self>) {
        match result {
            Ok(message) => {
                self.notice = Some(message);
                self.modal = None;
            }
            Err(error) => {
                if let Some(Modal::Import { error: slot, .. }) = &mut self.modal {
                    *slot = Some(error);
                } else {
                    self.notice = Some(error);
                }
            }
        }
        cx.notify();
    }

    fn choose_import_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                let mut ok = Vec::new();
                let mut errors = Vec::new();
                for path in paths {
                    let result = std::fs::read_to_string(&path)
                        .map_err(|e| e.to_string())
                        .and_then(|text| this.import_text(&text, window, cx));
                    match result {
                        Ok(m) => ok.push(m),
                        Err(e) => errors.push(format!("{}: {e}", path.display())),
                    }
                }
                let result = if errors.is_empty() { Ok(ok.join(", ")) } else { Err(errors.join("\n")) };
                this.finish_import(result, cx);
            });
        })
        .detach();
    }

    fn export_collection(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(json) = self.ws(cx).export_postman(id) else { return };
        let name = self
            .ws(cx)
            .collections
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let dir = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let path = cx.prompt_for_new_path(&dir, Some(&format!("{name}.postman_collection.json")));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = path.await else { return };
            let result = std::fs::write(&path, json);
            let _ = this.update(cx, |this, cx| {
                this.notice = Some(match result {
                    Ok(()) => format!("Exported to {}", path.display()),
                    Err(e) => format!("Export failed: {e}"),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let s = self.ws(cx).settings.clone();
        let input = |value: String, hint: &str, window: &mut Window, cx: &mut Context<Self>| {
            let hint = hint.to_string();
            cx.new(|cx| InputState::new(window, cx).placeholder(hint).default_value(value))
        };
        self.modal = Some(Modal::Settings([
            input(s.proxy, "http://user:pass@host:8080 (empty uses the system proxy)", window, cx),
            input(s.ca_cert, "/path/to/ca.pem", window, cx),
            input(s.client_cert, "/path/to/client.crt (PEM)", window, cx),
            input(s.client_key, "/path/to/client.key (PEM)", window, cx),
        ]));
        self.popup = None;
        cx.notify();
    }

    fn start_rename(&mut self, id: u64, current: String, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.popup = None;
        self.modal = Some(Modal::Rename { id, input });
        cx.notify();
    }

    fn confirm_rename(&mut self, cx: &mut Context<Self>) {
        let Some(Modal::Rename { id, input }) = &self.modal else { return };
        let (id, name) = (*id, input.read(cx).value().trim().to_string());
        if !name.is_empty() {
            self.update_ws(cx, |ws| ws.rename(id, name.clone()));
            let editors: Vec<_> = self
                .tabs
                .iter()
                .filter_map(|tab| match tab {
                    Tab::Request { editor, .. } if editor.read(cx).saved_id == Some(id) => Some(editor.clone()),
                    _ => None,
                })
                .collect();
            for editor in editors {
                editor.update(cx, |e, cx| e.set_name(name.clone(), cx));
            }
        }
        self.modal = None;
        cx.notify();
    }

    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(Modal::Delete { id, .. }) = &self.modal else { return };
        let id = *id;
        self.update_ws(cx, |ws| {
            ws.remove(id);
            ws.environments.retain(|e| e.id != id);
            if ws.active_environment == Some(id) {
                ws.active_environment = None;
            }
        });
        // Open request tabs keep their draft but become unsaved.
        let orphans: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|tab| match tab {
                Tab::Request { editor, .. } => {
                    let sid = editor.read(cx).saved_id?;
                    self.ws(cx).request(sid).is_none().then(|| editor.clone())
                }
                _ => None,
            })
            .collect();
        for editor in orphans {
            editor.update(cx, |e, _| e.saved_id = None);
        }
        let ws = self.store.read(cx).ws.clone();
        self.tabs.retain(|tab| match tab {
            Tab::Environment { id: env, .. } => *env != id,
            Tab::Example { request, .. } => ws.request(*request).is_some(),
            _ => true,
        });
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        self.modal = None;
        cx.notify();
    }

    fn new_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.update_ws(cx, |ws| {
            let id = ws.alloc_id();
            let n = ws.environments.len() + 1;
            ws.environments.push(Environment {
                id,
                name: format!("New Environment {n}"),
                values: Vec::new(),
            });
            id
        });
        self.view = SidebarView::Environments;
        self.popup = None;
        self.open_environment(id, window, cx);
    }

    fn new_collection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = format!("New Collection {}", self.ws(cx).collections.len() + 1);
        let id = self.update_ws(cx, |ws| ws.add_collection(name.clone()));
        self.view = SidebarView::Collections;
        self.start_rename(id, name, window, cx);
    }

    fn add_request_to(&mut self, collection: u64, window: &mut Window, cx: &mut Context<Self>) {
        let request = Request {
            name: "New Request".into(),
            ..Request::default()
        };
        let id = self.update_ws(cx, |ws| ws.add_request(collection, request.clone()));
        self.popup = None;
        self.open_request(Some(id), request, window, cx);
    }

    fn toggle_popup(&mut self, popup: Popup, cx: &mut Context<Self>) {
        self.popup = if self.popup == Some(popup) { None } else { Some(popup) };
        cx.notify();
    }

    fn on_send(&mut self, _: &Send, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |e, cx| e.send(window, cx));
        }
    }

    fn on_save(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |e, cx| e.save(cx));
        }
    }

    fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.open_request(None, Request::default(), window, cx);
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab(self.active, false, window, cx);
    }

    fn on_next_tab(&mut self, _: &NextTab, _: &mut Window, cx: &mut Context<Self>) {
        self.active = (self.active + 1) % self.tabs.len().max(1);
        cx.notify();
    }

    fn on_prev_tab(&mut self, _: &PrevTab, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.tabs.len().max(1);
        self.active = (self.active + n - 1) % n;
        cx.notify();
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    fn on_focus_url(&mut self, _: &FocusUrl, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor() {
            let handle = editor.read(cx).url_focus(cx);
            handle.focus(window, cx);
        }
    }

    fn on_dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.popup.take().is_some() || self.modal.take().is_some() {
            cx.notify();
        } else {
            cx.propagate();
        }
    }

    fn on_import(&mut self, _: &OpenImport, window: &mut Window, cx: &mut Context<Self>) {
        self.open_import(window, cx);
    }

    fn render_header(&self, t: &Tokens, cx: &Context<Self>) -> Div {
        let mode = t.mode;
        let dark = mode == ThemeMode::Dark;
        let settings_menu = menu(t)
            .child(
                menu_item("theme", t)
                    .child(icon(if dark { "icons/sun.svg" } else { "icons/moon.svg" }, 14., t.text_secondary))
                    .child(if dark { "Switch to light theme" } else { "Switch to dark theme" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let next = if dark { ThemeMode::Light } else { ThemeMode::Dark };
                        this.update_ws(cx, |ws| ws.theme = next);
                        theme::apply(next, cx);
                        this.popup = None;
                    })),
            )
            .child(
                menu_item("http-settings", t)
                    .child(icon("icons/settings.svg", 14., t.text_secondary))
                    .child("Proxy and certificates")
                    .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
            )
            .child(
                menu_item("data-folder", t)
                    .child(icon("icons/folder-open.svg", 14., t.text_secondary))
                    .child("Show data folder")
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.reveal_path(&storage::data_dir());
                        this.popup = None;
                        cx.notify();
                    })),
            );
        div()
            .h(px(48.))
            .flex_none()
            .px_3()
            .flex()
            .items_center()
            .bg(rgb(t.bg))
            .border_b_1()
            .border_color(rgb(t.border))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(img(LOGO).size(px(22.)).flex_none())
                    .child(div().text_size(px(14.)).text_color(rgb(t.text)).child("Floki"))
                    .child(div().w(px(12.)))
                    .child(self.render_workspaces_button(t, cx)),
            )
            .child(
                div()
                    .id("global-search")
                    .w(px(288.))
                    .h(px(32.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .rounded(px(4.))
                    .bg(rgb(t.bg_tertiary))
                    .cursor_text()
                    .text_color(rgb(t.text_secondary))
                    .child(icon("icons/search.svg", 14., t.text_secondary))
                    .child("Search Floki")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.sidebar_open = true;
                        this.filter.read(cx).focus_handle(cx).focus(window, cx);
                        cx.notify();
                    })),
            )
            .child(
                div().flex_1().flex().items_center().justify_end().gap_2().child(self.render_account_controls(t, cx)).child(
                    div()
                        .child(
                            icon_button("settings", "icons/settings.svg", t)
                                .size(px(32.))
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_popup(Popup::Settings, cx))),
                        )
                        .relative()
                        .when(self.popup == Some(Popup::Settings), |d| d.child(popover(settings_menu))),
                ),
            )
    }

    fn render_rail(&self, t: &Tokens, cx: &Context<Self>) -> impl IntoElement {
        let item = |id: &'static str, path: &'static str, label: &'static str, view: SidebarView| {
            let on = self.view == view;
            div()
                .id(id)
                .w(px(72.))
                .py_2()
                .flex()
                .flex_col()
                .items_center()
                .gap_1()
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(10.))
                .whitespace_nowrap()
                .when(on, |d| d.bg(rgb(t.hover)).text_color(rgb(t.text)))
                .when(!on, |d| d.text_color(rgb(t.text_secondary)))
                .hover(|s| s.text_color(rgb(t.text)))
                .child(icon(path, 16., if on { t.text } else { t.text_secondary }))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.view = view;
                    cx.notify();
                }))
        };
        div()
            .w(px(80.))
            .flex_none()
            .h_full()
            .pt_2()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .border_r_1()
            .border_color(rgb(t.border))
            .child(item("rail-collections", "icons/folder-closed.svg", "Collections", SidebarView::Collections))
            .child(item("rail-envs", "icons/layers.svg", "Environments", SidebarView::Environments))
            .child(item("rail-history", "icons/clock.svg", "History", SidebarView::History))
    }

    fn row_menu(&self, id: u64, entries: Vec<(&'static str, RowAction)>, t: &Tokens, cx: &Context<Self>) -> Div {
        let open = self.popup == Some(Popup::Row(id));
        let items = entries.into_iter().enumerate().map(|(ix, (label, action))| {
            menu_item(("row-menu", ix), t)
                .when(label == "Delete", |d| d.text_color(rgb(t.error)))
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.popup = None;
                    action(this, window, cx);
                    cx.notify();
                }))
        });
        div()
            .flex_none()
            .when(!open, |d| d.invisible().group_hover("tree-row", |s| s.visible()))
            .child(
                icon_button(("row-more", id), "icons/ellipsis.svg", t)
                    .size(px(20.))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_popup(Popup::Row(id), cx)
                    })),
            )
            .relative()
            .when(open, |d| d.child(popover(menu(t).children(items))))
    }

    fn tree_row(&self, id: impl Into<ElementId>, depth: usize, t: &Tokens) -> Stateful<Div> {
        div()
            .id(id)
            .group("tree-row")
            .h(px(28.))
            .pl(px(8. + depth as f32 * 16.))
            .pr_2()
            .flex()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .hover(|s| s.bg(rgb(t.hover)))
    }

    fn rename_action(id: u64, name: String) -> RowAction {
        Box::new(move |this, window, cx| this.start_rename(id, name.clone(), window, cx))
    }

    fn delete_action(id: u64, what: &'static str) -> RowAction {
        Box::new(move |this, _, cx| {
            this.modal = Some(Modal::Delete { id, what: what.into() });
            cx.notify();
        })
    }

    fn render_items(&self, items: &[Item], depth: usize, query: &str, t: &Tokens, cx: &Context<Self>, out: &mut Vec<AnyElement>) {
        let active_saved = self.active_editor().and_then(|e| e.read(cx).saved_id);
        for item in items {
            match item {
                Item::Folder(f) => {
                    if !query.is_empty() && !matches_tree(&f.items, query) {
                        continue;
                    }
                    let open = f.expanded || !query.is_empty();
                    let id = f.id;
                    let menu = self.row_menu(
                        id,
                        vec![
                            ("Rename", Self::rename_action(id, f.name.clone())),
                            ("Delete", Self::delete_action(id, "this folder and its requests")),
                        ],
                        t,
                        cx,
                    );
                    out.push(
                        self.tree_row(("tree", id), depth, t)
                            .child(icon(if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }, 14., t.text_secondary))
                            .child(icon(if open { "icons/folder-open.svg" } else { "icons/folder.svg" }, 14., t.text_secondary))
                            .child(div().flex_1().min_w_0().truncate().child(f.name.clone()))
                            .child(menu)
                            .on_click(cx.listener(move |this, _, _, cx| this.update_ws(cx, |ws| ws.toggle_expanded(id))))
                            .into_any_element(),
                    );
                    if open {
                        self.render_items(&f.items, depth + 1, query, t, cx, out);
                    }
                }
                Item::Request(r) => {
                    if !query.is_empty() && !matches_request(&r.request, query) {
                        continue;
                    }
                    let id = r.id;
                    let request = r.request.clone();
                    let menu = self.row_menu(
                        id,
                        vec![
                            ("Rename", Self::rename_action(id, r.request.name.clone())),
                            ("Delete", Self::delete_action(id, "this request")),
                        ],
                        t,
                        cx,
                    );
                    out.push(
                        self.tree_row(("tree", id), depth, t)
                            .when(active_saved == Some(id), |d| d.bg(rgb(t.hover)))
                            .child(method_badge(&r.request.method, t).ml(px(14.)))
                            .child(div().flex_1().min_w_0().truncate().child(r.request.title()))
                            .child(menu)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_request(Some(id), request.clone(), window, cx)
                            }))
                            .into_any_element(),
                    );
                    let open_example = match self.tabs.get(self.active) {
                        Some(Tab::Example { request, ix, .. }) if *request == id => Some(*ix),
                        _ => None,
                    };
                    for (ix, example) in r.request.examples.iter().enumerate() {
                        out.push(
                            self.tree_row(("example", id * 1000 + ix as u64), depth + 1, t)
                                .when(open_example == Some(ix), |d| d.bg(rgb(t.hover)))
                                .child(div().ml(px(14.)).text_size(px(10.)).text_color(rgb(t.text_tertiary)).child("e.g."))
                                .child(div().flex_1().min_w_0().truncate().child(example.name.clone()))
                                .child(icon_button(("example-delete", id * 1000 + ix as u64), "icons/close.svg", t).size(px(20.)).on_click(
                                    cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.update_ws(cx, |ws| ws.request_mut(id).map(|r| r.examples.remove(ix)));
                                        // Indexes shift after a removal, so close every example tab of this request.
                                        this.tabs.retain(|tab| !matches!(tab, Tab::Example { request, .. } if *request == id));
                                        this.active = this.active.min(this.tabs.len().saturating_sub(1));
                                        if this.tabs.is_empty() {
                                            this.open_request(None, Request::default(), window, cx);
                                        }
                                        for tab in &this.tabs {
                                            if let Tab::Request { editor, .. } = tab {
                                                if editor.read(cx).saved_id == Some(id) {
                                                    editor.update(cx, |e, cx| e.remove_example(ix, cx));
                                                }
                                            }
                                        }
                                    }),
                                ))
                                .on_click(cx.listener(move |this, _, window, cx| this.open_example(id, ix, window, cx)))
                                .into_any_element(),
                        );
                    }
                }
            }
        }
    }

    fn render_collections(&self, query: &str, t: &Tokens, cx: &Context<Self>) -> Vec<AnyElement> {
        let ws = self.ws(cx);
        let mut out = Vec::new();
        for c in &ws.collections {
            let name_hit = c.name.to_lowercase().contains(query);
            if !query.is_empty() && !name_hit && !matches_tree(&c.items, query) {
                continue;
            }
            let open = c.expanded || !query.is_empty();
            let id = c.id;
            let menu = self.row_menu(
                id,
                vec![
                    ("Add request", Box::new(move |this: &mut Self, w: &mut Window, cx: &mut Context<Self>| this.add_request_to(id, w, cx)) as RowAction),
                    ("Rename", Self::rename_action(id, c.name.clone())),
                    ("Edit variables", Box::new(move |this: &mut Self, w: &mut Window, cx: &mut Context<Self>| this.open_environment(id, w, cx))),
                    ("Export", Box::new(move |this: &mut Self, w: &mut Window, cx: &mut Context<Self>| this.export_collection(id, w, cx))),
                    ("Delete", Self::delete_action(id, "this collection and everything in it")),
                ],
                t,
                cx,
            );
            out.push(
                self.tree_row(("tree", id), 0, t)
                    .child(icon(if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }, 14., t.text_secondary))
                    .child(div().flex_1().min_w_0().truncate().child(c.name.clone()))
                    .child(menu)
                    .on_click(cx.listener(move |this, _, _, cx| this.update_ws(cx, |ws| ws.toggle_expanded(id))))
                    .into_any_element(),
            );
            if open {
                let q = if name_hit { "" } else { query };
                self.render_items(&c.items, 1, q, t, cx, &mut out);
            }
        }
        if ws.collections.is_empty() {
            out.push(
                empty_state(
                    t,
                    "Create a collection for your requests",
                    "A collection lets you group related requests and share common variables and authorization.",
                )
                .child(outline_button("create-collection", "Create Collection", t).on_click(
                    cx.listener(|this, _, window, cx| this.new_collection(window, cx)),
                ))
                .into_any_element(),
            );
        }
        out
    }

    fn render_environments(&self, query: &str, t: &Tokens, cx: &Context<Self>) -> Vec<AnyElement> {
        let ws = self.ws(cx);
        let open_env = match self.tabs.get(self.active) {
            Some(Tab::Environment { id, .. }) => Some(*id),
            _ => None,
        };
        let globals = self
            .tree_row("tree-globals", 0, t)
            .when(open_env == Some(GLOBALS_ID), |d| d.bg(rgb(t.hover)))
            .child(div().flex_1().child("Globals"))
            .on_click(cx.listener(|this, _, window, cx| this.open_environment(GLOBALS_ID, window, cx)))
            .into_any_element();
        let mut out: Vec<AnyElement> = std::iter::once(globals)
            .chain(ws.environments.iter().filter(|e| query.is_empty() || e.name.to_lowercase().contains(query)).map(|e| {
                let id = e.id;
                let is_active = ws.active_environment == Some(id);
                let toggle: RowAction = Box::new(move |this, _, cx| {
                    this.update_ws(cx, |ws| ws.active_environment = if is_active { None } else { Some(id) });
                });
                let menu = self.row_menu(
                    id,
                    vec![
                        (if is_active { "Deactivate" } else { "Set active" }, toggle),
                        ("Rename", Self::rename_action(id, e.name.clone())),
                        ("Delete", Self::delete_action(id, "this environment")),
                    ],
                    t,
                    cx,
                );
                self.tree_row(("tree", id), 0, t)
                    .when(open_env == Some(id), |d| d.bg(rgb(t.hover)))
                    .child(div().flex_1().min_w_0().truncate().child(e.name.clone()))
                    .when(is_active, |d| d.child(icon("icons/circle-check.svg", 14., t.success)))
                    .child(menu)
                    .on_click(cx.listener(move |this, _, window, cx| this.open_environment(id, window, cx)))
                    .into_any_element()
            }))
            .collect();
        if ws.environments.is_empty() {
            out.push(
                empty_state(
                    t,
                    "Create an environment",
                    "An environment is a set of variables that lets you switch the context of your requests.",
                )
                .child(outline_button("create-env", "Create Environment", t).on_click(
                    cx.listener(|this, _, window, cx| this.new_environment(window, cx)),
                ))
                .into_any_element(),
            );
        }
        out
    }

    fn render_history(&self, query: &str, t: &Tokens, cx: &Context<Self>) -> Vec<AnyElement> {
        let ws = self.ws(cx);
        let today = Local::now().date_naive();
        let mut out = Vec::new();
        let mut last_day = None;
        for (ix, entry) in ws.history.iter().enumerate() {
            if !query.is_empty() && !matches_request(&entry.request, query) {
                continue;
            }
            let day = Local
                .timestamp_millis_opt(entry.at_unix_ms)
                .single()
                .map(|d| d.date_naive());
            if day != last_day {
                last_day = day;
                let label = match day {
                    Some(d) if d == today => "Today".to_string(),
                    Some(d) if Some(d) == today.pred_opt() => "Yesterday".to_string(),
                    Some(d) => d.format("%B %-d").to_string(),
                    None => "Unknown".into(),
                };
                out.push(
                    div()
                        .h(px(28.))
                        .px_2()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_color(rgb(t.text_secondary))
                        .child(icon("icons/chevron-down.svg", 14., t.text_secondary))
                        .child(label)
                        .into_any_element(),
                );
            }
            let request = entry.request.clone();
            out.push(
                self.tree_row(("history", ix), 1, t)
                    .child(method_badge(&entry.request.method, t))
                    .child(div().flex_1().min_w_0().truncate().child(entry.request.url.clone()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_request(None, request.clone(), window, cx)
                    }))
                    .into_any_element(),
            );
        }
        if ws.history.is_empty() {
            out.push(
                empty_state(t, "You haven't sent any requests", "Any request you send in this workspace will appear here.")
                    .into_any_element(),
            );
        }
        out
    }

    fn render_sidebar(&self, t: &Tokens, cx: &Context<Self>) -> impl IntoElement {
        let query = self.filter.read(cx).value().trim().to_lowercase();
        let rows = match self.view {
            SidebarView::Collections => self.render_collections(&query, t, cx),
            SidebarView::Environments => self.render_environments(&query, t, cx),
            SidebarView::History => self.render_history(&query, t, cx),
        };
        let new_menu = menu(t)
            .child(
                menu_item("new-request", t)
                    .child(icon("icons/send.svg", 14., t.text_secondary))
                    .child("HTTP Request")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.popup = None;
                        this.open_request(None, Request::default(), window, cx);
                    })),
            )
            .child(
                menu_item("new-collection", t)
                    .child(icon("icons/folder-plus.svg", 14., t.text_secondary))
                    .child("Collection")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.popup = None;
                        this.new_collection(window, cx);
                    })),
            )
            .child(
                menu_item("new-env", t)
                    .child(icon("icons/layers.svg", 14., t.text_secondary))
                    .child("Environment")
                    .on_click(cx.listener(|this, _, window, cx| this.new_environment(window, cx))),
            );
        let view = self.view;
        let clear_history = (view == SidebarView::History && !self.ws(cx).history.is_empty()).then(|| {
            icon_button("clear-history", "icons/delete.svg", t)
                .on_click(cx.listener(|this, _, _, cx| this.update_ws(cx, |ws| ws.history.clear())))
        });
        let panel = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(40.))
                    .flex_none()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(t.border))
                    .child(self.render_workspace_title(t, cx))
                    .child(
                        div()
                            .child(small_button("new", "New", t).on_click(cx.listener(|this, _, _, cx| this.toggle_popup(Popup::New, cx))))
                            .relative()
                            .when(self.popup == Some(Popup::New), |d| d.child(popover(new_menu))),
                    )
                    .child(small_button("import", "Import", t).on_click(cx.listener(|this, _, window, cx| this.open_import(window, cx)))),
            )
            .child(
                div()
                    .h(px(40.))
                    .flex_none()
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(icon_button("sidebar-add", "icons/plus.svg", t).on_click(cx.listener(move |this, _, window, cx| match view {
                        SidebarView::Collections => this.new_collection(window, cx),
                        SidebarView::Environments => this.new_environment(window, cx),
                        SidebarView::History => this.open_request(None, Request::default(), window, cx),
                    })))
                    .child(
                        div()
                            .flex_1()
                            .h(px(24.))
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_1()
                            .rounded(px(2.))
                            .border_1()
                            .border_color(rgb(t.border))
                            .child(icon("icons/search.svg", 12., t.text_tertiary))
                            .child(div().flex_1().child(Input::new(&self.filter).appearance(false).text_size(px(12.)))),
                    )
                    .children(clear_history),
            )
            .child(div().id("sidebar-rows").flex_1().min_h_0().overflow_y_scroll().pb_2().children(rows));
        div()
            .size_full()
            .flex()
            .bg(rgb(t.bg_secondary))
            .border_r_1()
            .border_color(rgb(t.border))
            .child(self.render_rail(t, cx))
            .child(panel)
    }

    fn render_tabs(&self, t: &Tokens, cx: &Context<Self>) -> impl IntoElement {
        if self.scrolled_to.replace(Some(self.active)) != Some(self.active) {
            self.tab_scroll.scroll_to_item(self.active);
        }
        let ws = self.ws(cx);
        let tabs = self.tabs.iter().enumerate().map(|(ix, tab)| {
            let active = ix == self.active;
            let (method, title, dirty) = match tab {
                Tab::Request { editor, .. } => {
                    let e = editor.read(cx);
                    (Some(e.request.method.clone()), e.title(), e.is_dirty())
                }
                Tab::Environment { id, .. } => (None, ws.variable_set(*id).map(|(n, _)| n).unwrap_or_default(), false),
                Tab::Example { request, ix, .. } => (
                    Some("e.g.".to_string()),
                    ws.request(*request).and_then(|(_, r)| r.examples.get(*ix)).map(|e| e.name.clone()).unwrap_or_default(),
                    false,
                ),
            };
            let close = div()
                .id(("tab-close", ix))
                .relative()
                .size(px(18.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .hover(|s| s.bg(rgb(t.border_strong)))
                .map(|d| {
                    if dirty {
                        d.child(dot(t.brand).group_hover("tab", |s| s.invisible())).child(
                            div()
                                .absolute()
                                .invisible()
                                .group_hover("tab", |s| s.visible())
                                .child(icon("icons/close.svg", 12., t.text_secondary)),
                        )
                    } else {
                        d.when(!active, |d| d.invisible().group_hover("tab", |s| s.visible()))
                            .child(icon("icons/close.svg", 12., t.text_secondary))
                    }
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.close_tab(ix, false, window, cx);
                }));
            div()
                .id(("tab", ix))
                .group("tab")
                .relative()
                .h(px(40.))
                .w(px(180.))
                .flex_none()
                .px_2()
                .flex()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .border_r_1()
                .border_color(rgb(t.border))
                .when(active, |d| {
                    d.bg(rgb(t.bg))
                        .text_color(rgb(t.text))
                        .child(div().absolute().top_0().left_0().right_0().h(px(2.)).bg(rgb(t.brand)))
                })
                .when(!active, |d| d.text_color(rgb(t.text_secondary)).hover(|s| s.bg(rgb(t.hover))))
                .map(|d| match &method {
                    Some(m) => d.child(method_badge(m, t)),
                    None => d.child(icon("icons/layers.svg", 14., t.text_secondary)),
                })
                .child(div().flex_1().min_w_0().truncate().child(title))
                .child(close)
                .on_mouse_down(MouseButton::Middle, cx.listener(move |this, _, window, cx| this.close_tab(ix, false, window, cx)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active = ix;
                    cx.notify();
                }))
        });
        let env_name = ws
            .active_environment
            .and_then(|id| ws.environments.iter().find(|e| e.id == id))
            .map(|e| e.name.clone())
            .unwrap_or_else(|| "No environment".into());
        let mut env_items = vec![menu_item("env-none", t)
            .child(div().flex_1().child("No environment"))
            .when(ws.active_environment.is_none(), |d| d.child(icon("icons/check.svg", 14., t.text_secondary)))
            .on_click(cx.listener(|this, _, _, cx| {
                this.popup = None;
                this.update_ws(cx, |ws| ws.active_environment = None);
            }))];
        env_items.extend(ws.environments.iter().map(|e| {
            let id = e.id;
            menu_item(("env-pick", id), t)
                .child(div().flex_1().child(e.name.clone()))
                .when(ws.active_environment == Some(id), |d| d.child(icon("icons/check.svg", 14., t.text_secondary)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.popup = None;
                    this.update_ws(cx, |ws| ws.active_environment = Some(id));
                }))
        }));
        let tab_menu = menu(t)
            .child(menu_item("close-tab", t).child("Close current tab").on_click(cx.listener(|this, _, window, cx| {
                this.popup = None;
                this.close_tab(this.active, false, window, cx);
            })))
            .child(menu_item("close-saved", t).child("Close all unchanged tabs").on_click(cx.listener(|this, _, window, cx| {
                this.popup = None;
                this.tabs.retain(|tab| match tab {
                    Tab::Request { editor, .. } => editor.read(cx).is_dirty(),
                    Tab::Environment { .. } | Tab::Example { .. } => false,
                });
                this.active = 0;
                if this.tabs.is_empty() {
                    this.open_request(None, Request::default(), window, cx);
                }
                cx.notify();
            })));
        div()
            .h(px(40.))
            .flex_none()
            .flex()
            .bg(rgb(t.bg_secondary))
            .border_b_1()
            .border_color(rgb(t.border))
            .child(
                div()
                    .id("tab-strip")
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .overflow_x_scroll()
                    .track_scroll(&self.tab_scroll)
                    .children(tabs)
                    .child(
                        div()
                            .flex_none()
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(icon_button("new-tab", "icons/plus.svg", t).on_click(
                                cx.listener(|this, _, window, cx| this.open_request(None, Request::default(), window, cx)),
                            ))
                            .child(
                                div()
                                    .child(icon_button("tab-menu", "icons/ellipsis.svg", t).on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_popup(Popup::TabMenu, cx)),
                                    ))
                                    .relative()
                                    .when(self.popup == Some(Popup::TabMenu), |d| d.child(popover(tab_menu))),
                            ),
                    ),
            )
            .child(
                div()
                    .id("env-select")
                    .relative()
                    .w(px(220.))
                    .flex_none()
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_l_1()
                    .border_color(rgb(t.border))
                    .cursor_pointer()
                    .text_color(rgb(if ws.active_environment.is_some() { t.text } else { t.text_secondary }))
                    .hover(|s| s.bg(rgb(t.hover)))
                    .child(div().truncate().child(env_name))
                    .child(icon("icons/chevron-down.svg", 14., t.text_secondary))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_popup(Popup::Environment, cx)))
                    .when(self.popup == Some(Popup::Environment), |d| {
                        d.child(popover(menu(t).w(px(220.)).children(env_items)))
                    }),
            )
    }

    fn render_env_tab(&self, id: u64, editor: &Entity<KvEditor>, t: &Tokens, cx: &Context<Self>) -> AnyElement {
        let ws = self.ws(cx);
        let Some((name, _)) = ws.variable_set(id) else {
            return div().into_any_element();
        };
        let is_env = ws.environments.iter().any(|e| e.id == id);
        let active = ws.active_environment == Some(id);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(t.bg))
            .child(
                div()
                    .h(px(48.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(icon("icons/layers.svg", 16., t.text_secondary))
                    .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).text_size(px(14.)).child(name.clone()))
                    .when(is_env, |d| {
                        d.child(
                            ghost_button("env-rename", t)
                                .child("Rename")
                                .on_click(cx.listener(move |this, _, window, cx| this.start_rename(id, name.clone(), window, cx))),
                        )
                        .child(
                            outline_button("env-activate", if active { "Active" } else { "Set active" }, t)
                                .when(active, |d| d.text_color(rgb(t.success)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.update_ws(cx, |ws| ws.active_environment = if active { None } else { Some(id) });
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .id("env-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_4()
                    .child(
                        div()
                            .pb_2()
                            .text_color(rgb(t.text_secondary))
                            .child(match (is_env, id == GLOBALS_ID) {
                                (_, true) => "Globals apply to every request. Collection and environment variables with the same name override them.",
                                (true, _) => "Use variables in requests as {{variable}}. Values are stored locally in plain text.",
                                _ => "Collection variables apply to requests in this collection. The active environment overrides them.",
                            }),
                    )
                    .child(editor.clone()),
            )
            .into_any_element()
    }

    fn render_modal(&self, t: &Tokens, cx: &Context<Self>) -> Option<AnyElement> {
        let modal = self.modal.as_ref()?;
        let card = |title: &'static str, width: f32| modal_card(title, width, t, cx);
        let footer = || div().px_4().py_3().flex().justify_end().gap_2();
        let cancel = || {
            outline_button("modal-cancel", "Cancel", t).on_click(cx.listener(|this, _, _, cx| {
                this.modal = None;
                cx.notify();
            }))
        };
        let field = |input: &Entity<InputState>| modal_field(input, t);
        let body = match modal {
            Modal::SaveAs { name, new_collection, collection, .. } => {
                let ws = self.ws(cx);
                let selected = *collection;
                card("Save request", 520.)
                    .child(
                        div()
                            .px_4()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().text_color(rgb(t.text_secondary)).child("Request name"))
                            .child(field(name))
                            .child(div().pt_2().text_color(rgb(t.text_secondary)).child("Save to"))
                            .child(
                                div()
                                    .id("save-targets")
                                    .max_h(px(240.))
                                    .overflow_y_scroll()
                                    .border_1()
                                    .border_color(rgb(t.border))
                                    .rounded(px(4.))
                                    .children(ws.collections.iter().map(|c| {
                                        let id = c.id;
                                        menu_item(("save-target", id), t)
                                            .when(selected == Some(id), |d| d.bg(rgb(t.hover)))
                                            .child(icon("icons/folder-closed.svg", 14., t.text_secondary))
                                            .child(c.name.clone())
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let Some(Modal::SaveAs { collection, .. }) = &mut this.modal {
                                                    *collection = Some(id);
                                                }
                                                cx.notify();
                                            }))
                                    }))
                                    .when(ws.collections.is_empty(), |d| {
                                        d.child(div().p_3().text_color(rgb(t.text_secondary)).child("No collections yet; name a new one below."))
                                    }),
                            )
                            .child(div().pt_2().text_color(rgb(t.text_secondary)).child("Or create a new collection"))
                            .child(field(new_collection)),
                    )
                    .child(footer().child(cancel()).child(
                        primary_button("modal-save", "Save", t)
                            .on_click(cx.listener(|this, _, window, cx| this.confirm_save_as(window, cx))),
                    ))
            }
            Modal::ConfirmClose(ix) => {
                let ix = *ix;
                card("Do you want to save changes?", 460.)
                    .child(
                        div()
                            .px_4()
                            .text_color(rgb(t.text_secondary))
                            .child("This tab has unsaved changes which will be lost if you choose to close it."),
                    )
                    .child(
                        footer()
                            .child(outline_button("dont-save", "Don't save", t).on_click(cx.listener(move |this, _, window, cx| {
                                this.modal = None;
                                this.close_tab(ix, true, window, cx);
                            })))
                            .child(cancel())
                            .child(primary_button("save-close", "Save changes", t).on_click(cx.listener(move |this, _, window, cx| {
                                this.modal = None;
                                let Some(Tab::Request { editor, .. }) = this.tabs.get(ix) else { return };
                                let editor = editor.clone();
                                if editor.read(cx).saved_id.is_some() {
                                    editor.update(cx, |e, cx| e.save(cx));
                                    this.close_tab(ix, true, window, cx);
                                } else {
                                    this.open_save_as(editor, true, window, cx);
                                }
                            }))),
                    )
            }
            Modal::Import { text, error } => card("Import", 640.)
                .child(
                    div()
                        .px_4()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_color(rgb(t.text_secondary))
                                .child("Paste a cURL command, a Postman collection (v2.1) or environment, or an OpenAPI, Swagger, RAML or WSDL document."),
                        )
                        .child(
                            div()
                                .h(px(260.))
                                .border_1()
                                .border_color(rgb(t.border_strong))
                                .rounded(px(4.))
                                .font_family(MONO)
                                .child(Editor::new(text).appearance(false).h_full()),
                        )
                        .when_some(error.clone(), |d, e| d.child(div().text_color(rgb(t.error)).child(e))),
                )
                .child(
                    footer()
                        .child(
                            outline_button("import-files", "Choose files…", t)
                                .on_click(cx.listener(|this, _, window, cx| this.choose_import_files(window, cx))),
                        )
                        .child(div().flex_1())
                        .child(cancel())
                        .child(primary_button("import-go", "Import", t).on_click(cx.listener(|this, _, window, cx| {
                            let Some(Modal::Import { text, .. }) = &this.modal else { return };
                            let content = text.read(cx).value().to_string();
                            let result = this.import_text(&content, window, cx);
                            this.finish_import(result, cx);
                        }))),
                ),
            Modal::Settings(inputs) => {
                let labels = ["Proxy server", "CA certificate", "Client certificate", "Client key"];
                card("Proxy and certificates", 560.)
                    .child(div().px_4().flex().flex_col().gap_2().children(labels.iter().zip(inputs).map(|(label, input)| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_color(rgb(t.text_secondary)).child(*label))
                            .child(field(input))
                    })).child(
                        div()
                            .text_color(rgb(t.text_tertiary))
                            .child("Applies to HTTP requests. The client certificate is sent to every host that asks for one."),
                    ))
                    .child(footer().child(cancel()).child(primary_button("settings-save", "Save", t).on_click(
                        cx.listener(|this, _, _, cx| {
                            let Some(Modal::Settings(inputs)) = &this.modal else { return };
                            let [proxy, ca_cert, client_cert, client_key] =
                                inputs.clone().map(|i| i.read(cx).value().trim().to_string());
                            this.update_ws(cx, |ws| ws.settings = Settings { proxy, ca_cert, client_cert, client_key });
                            this.modal = None;
                            cx.notify();
                        }),
                    )))
            }
            Modal::Cloud(m) => self.render_cloud_modal(m, t, cx),
            Modal::Rename { input, .. } => card("Rename", 420.)
                .child(div().px_4().child(field(input)))
                .child(footer().child(cancel()).child(
                    primary_button("rename-go", "Rename", t).on_click(cx.listener(|this, _, _, cx| this.confirm_rename(cx))),
                )),
            Modal::Delete { what, .. } => card("Delete", 420.)
                .child(
                    div()
                        .px_4()
                        .text_color(rgb(t.text_secondary))
                        .child(format!("Are you sure you want to delete {what}? This cannot be undone.")),
                )
                .child(
                    footer().child(cancel()).child(
                        div()
                            .id("delete-go")
                            .h(px(32.))
                            .px_3()
                            .flex()
                            .items_center()
                            .rounded(px(4.))
                            .cursor_pointer()
                            .bg(rgb(0xd91717))
                            .text_color(rgb(0xffffff))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Delete")
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_delete(cx))),
                    ),
                ),
        };
        Some(
            div()
                .id("modal-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgba(0x070b1499))
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.modal = None;
                    cx.notify();
                }))
                .child(body)
                .into_any_element(),
        )
    }

    fn render_status_bar(&self, t: &Tokens, cx: &Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let (message, color) = match (&store.save_error, &self.notice) {
            (Some(e), _) => (format!("Could not save workspace: {e}"), t.error),
            (None, _) if store.refused => ("You have view-only access to this workspace".into(), t.warning),
            (None, Some(n)) => (n.clone(), t.text_secondary),
            (None, None) => match &store.meta().remote {
                None => ("Local workspace".into(), t.text_secondary),
                Some(_) if store.app.account.is_none() => ("Sign in to sync this workspace".into(), t.warning),
                Some(r) if r.dirty => ("Syncing changes…".into(), t.text_secondary),
                Some(_) => ("Synced".into(), t.text_secondary),
            },
        };
        div()
            .h(px(24.))
            .flex_none()
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .bg(rgb(t.bg))
            .border_t_1()
            .border_color(rgb(t.border))
            .text_size(px(11.))
            .text_color(rgb(color))
            .child(
                icon_button("toggle-sidebar", "icons/panel-left.svg", t)
                    .size(px(20.))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_open = !this.sidebar_open;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("status-message")
                    .flex_1()
                    .truncate()
                    .when(store.meta().remote.is_some(), |d| {
                        d.cursor_pointer().on_click(cx.listener(|this, _, window, cx| this.sync_now(window, cx)))
                    })
                    .child(message),
            )
            .child(div().text_color(rgb(t.text_tertiary)).child("Ctrl+Enter send · Ctrl+S save · Ctrl+T new tab"))
    }
}

fn modal_card(title: &'static str, width: f32, t: &Tokens, cx: &Context<Floki>) -> Stateful<Div> {
    div()
        .id("modal-card")
        .w(px(width))
        .max_h(px(640.))
        .flex()
        .flex_col()
        .bg(rgb(t.bg))
        .border_1()
        .border_color(rgb(t.border_strong))
        .rounded(px(8.))
        .shadow_lg()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            div()
                .h(px(48.))
                .px_4()
                .flex()
                .items_center()
                .justify_between()
                .child(div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child(title))
                .child(icon_button("modal-close", "icons/close.svg", t).on_click(cx.listener(|this, _, _, cx| {
                    this.modal = None;
                    cx.notify();
                }))),
        )
}

fn modal_field(input: &Entity<InputState>, t: &Tokens) -> Div {
    div()
        .h(px(32.))
        .px_2()
        .flex()
        .items_center()
        .rounded(px(4.))
        .border_1()
        .border_color(rgb(t.border_strong))
        .child(Input::new(input).appearance(false).text_size(px(12.)))
}

fn method_badge(method: &str, t: &Tokens) -> Div {
    let short = match method {
        "DELETE" => "DEL",
        "OPTIONS" => "OPT",
        "SOCKETIO" => "SIO",
        "GRPC" => "gRPC",
        m => m,
    };
    div()
        .w(px(36.))
        .flex_none()
        .flex()
        .justify_end()
        .text_size(px(9.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(t.method(method)))
        .child(short.to_string())
}

fn empty_state(t: &Tokens, title: &'static str, text: &'static str) -> Div {
    div()
        .p_4()
        .flex()
        .flex_col()
        .items_center()
        .gap_2()
        .text_center()
        .child(div().pt_6().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(t.text)).child(title))
        .child(div().text_color(rgb(t.text_secondary)).child(text))
}

fn matches_request(r: &Request, query: &str) -> bool {
    r.name.to_lowercase().contains(query) || r.url.to_lowercase().contains(query)
}

fn matches_tree(items: &[Item], query: &str) -> bool {
    items.iter().any(|item| match item {
        Item::Request(r) => matches_request(&r.request, query),
        Item::Folder(f) => f.name.to_lowercase().contains(query) || matches_tree(&f.items, query),
    })
}

impl Render for Floki {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Tokens::get(cx);
        let content: AnyElement = match self.tabs.get(self.active) {
            Some(Tab::Request { editor, .. }) => editor.clone().into_any_element(),
            Some(Tab::Environment { id, editor, .. }) => self.render_env_tab(*id, editor, &t, cx),
            Some(Tab::Example { request, ix, body }) => self.render_example_tab(*request, *ix, body, &t, cx),
            None => div().into_any_element(),
        };
        let workbench = div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_tabs(&t, cx))
            .child(div().flex_1().min_h_0().child(content));
        let body = if self.sidebar_open {
            h_resizable("workbench-split")
                .child(
                    resizable_panel()
                        .size(px(372.))
                        .size_range(px(240.)..px(640.))
                        .child(self.render_sidebar(&t, cx)),
                )
                .child(resizable_panel().child(workbench))
                .into_any_element()
        } else {
            workbench.into_any_element()
        };
        div()
            .id("floki")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_send))
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_new_tab))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_focus_url))
            .on_action(cx.listener(Self::on_dismiss))
            .on_action(cx.listener(Self::on_import))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(t.bg))
            .font_family(FONT)
            .text_size(px(12.))
            .text_color(rgb(t.text))
            .map(|d| {
                let header = self.render_header(&t, cx);
                // GNOME/Wayland won't draw a title bar, so the header doubles as one with window controls.
                if cfg!(target_os = "linux") && matches!(window.window_decorations(), Decorations::Client { .. }) {
                    d.child(TitleBar::new().h(px(48.)).pl_0().bg(rgb(t.bg)).border_color(rgb(t.border)).child(header.flex_1().h_full().border_b_0()))
                } else {
                    d.child(header)
                }
            })
            .child(div().flex_1().min_h_0().child(body))
            .child(self.render_status_bar(&t, cx))
            .when(self.popup.is_some(), |d| {
                d.child(backdrop(window, cx.listener(|this, _, _, cx| {
                    this.popup = None;
                    cx.notify();
                })))
            })
            .children(self.render_modal(&t, cx))
    }
}

fn main() {
    application().with_assets(AppAssets).run(|cx| {
        init(cx);
        cx.text_system()
            .add_fonts(vec![
                Cow::Borrowed(include_bytes!("../assets/fonts/inter-400.ttf").as_slice()),
                Cow::Borrowed(include_bytes!("../assets/fonts/inter-600.ttf").as_slice()),
                Cow::Borrowed(include_bytes!("../assets/fonts/ibm-plex-mono-400.ttf").as_slice()),
            ])
            .expect("bundled fonts are valid");
        theme::apply(storage::load_workspace(&storage::load_app().active).theme, cx);
        cx.bind_keys([
            KeyBinding::new("ctrl-enter", Send, None),
            KeyBinding::new("ctrl-s", Save, None),
            KeyBinding::new("ctrl-t", NewTab, None),
            KeyBinding::new("ctrl-w", CloseTab, None),
            KeyBinding::new("ctrl-tab", NextTab, None),
            KeyBinding::new("ctrl-shift-tab", PrevTab, None),
            KeyBinding::new("ctrl-\\", ToggleSidebar, None),
            KeyBinding::new("ctrl-l", FocusUrl, None),
            KeyBinding::new("ctrl-o", OpenImport, None),
            KeyBinding::new("escape", Dismiss, None),
        ]);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1440.), px(900.)), cx))),
            window_min_size: Some(size(px(960.), px(600.))),
            app_id: Some("floki".into()),
            // X11 only; Wayland takes the icon from floki.desktop and Windows from the .exe resource.
            icon: image::load_from_memory_with_format(include_bytes!("../assets/app-icon/floki-256.png"), image::ImageFormat::Png)
                .ok()
                .map(|i| std::sync::Arc::new(i.into_rgba8())),
            ..Default::default()
        };
        open_window(options, cx, |window, cx| {
            window.set_window_title("Floki");
            cx.new(|cx| Floki::new(window, cx))
        })
        .expect("Failed to open Floki window");
    });
}
