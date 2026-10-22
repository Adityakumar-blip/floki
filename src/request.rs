use crate::http::{self, format_size};
use crate::kv::{cell, KvChanged, KvEditor};
use crate::model::{
    parse_curl, parse_relaxed_json, resolve, Auth, Example, KeyValue, BinaryEncoding, BodyMode, HistoryEntry, RawLanguage, Request,
    GRPC, METHODS, SOCKET_IO, WEBSOCKET,
};
use crate::grpc;
use crate::snippets;
use crate::store::Store;
use crate::theme::{Tokens, MONO};
use crate::ui::*;
use crate::ws;
use futures::channel::oneshot;
use futures::StreamExt;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::resizable::{resizable_panel, v_resizable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq)]
enum Section {
    Params,
    Auth,
    Headers,
    Body,
    Settings,
    /// gRPC: server reflection or a .proto file.
    Definition,
    Docs,
}

#[derive(Clone, Copy, PartialEq)]
enum ResponseTab {
    Body,
    Cookies,
    Headers,
}

#[derive(Clone, Copy, PartialEq)]
enum BodyView {
    Pretty,
    Raw,
}

#[derive(Clone, Copy, PartialEq)]
enum Menu {
    Method,
    Auth,
    Language,
    GrpcMethod,
    CodeLanguage,
}

enum SendState {
    Idle,
    Sending,
    Done(Rc<http::Response>),
    Failed(String),
}

#[derive(Clone, Copy, PartialEq)]
enum WsStatus {
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Clone, Copy, PartialEq)]
enum WsKind {
    Sent,
    Received,
    Info,
    Error,
}

struct WsEntry {
    kind: WsKind,
    summary: String,
    detail: String,
    /// "Binary", or the Socket.IO event name.
    tag: Option<String>,
    at: String,
    open: bool,
}

const WS_LOG_LIMIT: usize = 1000;

/// Pretty-prints JSON messages; anything else is returned as is.
fn pretty_message(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .filter(|v| v.is_object() || v.is_array())
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| text.to_string())
}

/// Label for the request-kind box and header badge.
pub fn method_label(method: &str) -> &str {
    match method {
        SOCKET_IO => "Socket.IO",
        GRPC => "gRPC",
        m => m,
    }
}

/// Numeric socket settings edited as text, with how each one is stored.
const SOCKET_FIELDS: [(&str, &str, fn(&mut Request, &str) -> bool, fn(&Request) -> String); 5] = [
    (
        "Handshake request timeout",
        "How long to wait for the connection to be established, in ms. 0 waits forever.",
        |r, v| v.parse().map(|v| r.socket.handshake_timeout_ms = v).is_ok(),
        |r| r.socket.handshake_timeout_ms.to_string(),
    ),
    (
        "Reconnection attempts",
        "How many times to reconnect after the connection drops unexpectedly.",
        |r, v| v.parse().map(|v| r.socket.reconnect_attempts = v).is_ok(),
        |r| r.socket.reconnect_attempts.to_string(),
    ),
    (
        "Reconnection intervals",
        "Delay between reconnection attempts, in ms.",
        |r, v| v.parse().map(|v| r.socket.reconnect_interval_ms = v).is_ok(),
        |r| r.socket.reconnect_interval_ms.to_string(),
    ),
    (
        "Maximum message size",
        "Largest message to receive, in MB.",
        |r, v| v.parse().map(|v: u32| r.socket.max_message_mb = v.max(1)).is_ok(),
        |r| r.socket.max_message_mb.to_string(),
    ),
    (
        "Handshake path",
        "Socket.IO server path; the URL path selects the namespace.",
        |r, v| {
            r.socket.handshake_path = v.into();
            true
        },
        |r| r.socket.handshake_path.clone(),
    ),
];

pub enum RequestEvent {
    /// Title, method, or dirty state changed.
    Changed,
    /// Save was requested for a request that is not in a collection yet.
    SaveAs,
}

pub struct RequestEditor {
    store: Entity<Store>,
    pub saved_id: Option<u64>,
    pub request: Request,
    baseline: Request,
    section: Section,
    menu: Option<Menu>,
    url: Entity<InputState>,
    params: Entity<KvEditor>,
    headers: Entity<KvEditor>,
    form: Entity<KvEditor>,
    multipart: Entity<KvEditor>,
    body: Entity<EditorState>,
    graphql_query: Entity<EditorState>,
    graphql_variables: Entity<EditorState>,
    socket_event: Entity<InputState>,
    socket_fields: Vec<Entity<InputState>>,
    /// One input per `Auth::fields_mut` slot, reused across auth types.
    auth_inputs: Vec<Entity<InputState>>,
    oauth_status: Option<String>,
    docs: Entity<EditorState>,
    code_lang: usize,
    state: SendState,
    generation: u64,
    response_tab: ResponseTab,
    body_view: BodyView,
    viewer: Option<Entity<EditorState>>,
    pub show_code: bool,
    code: Option<Entity<EditorState>>,
    /// Outgoing channel of the open socket; dropping it closes the connection.
    ws: Option<std::sync::mpsc::Sender<ws::Command>>,
    ws_status: WsStatus,
    ws_session: u64,
    /// Kind the open connection was made with; switching kind disconnects.
    ws_method: String,
    ws_recorded: bool,
    ws_log: Vec<WsEntry>,
    /// Cancels the running gRPC call when dropped or fired.
    grpc_call: Option<oneshot::Sender<()>>,
    /// Final status of the last call and whether it was OK.
    grpc_status: Option<(String, bool)>,
    grpc_methods: Vec<grpc::MethodInfo>,
    /// Loading methods or the example message, or the error from doing so.
    grpc_loading: bool,
    grpc_error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RequestEvent> for RequestEditor {}

pub fn code_editor(text: &str, language: &str, window: &mut Window, cx: &mut App) -> Entity<EditorState> {
    let text = text.to_string();
    let language = language.to_string();
    cx.new(|cx| {
        EditorState::new(window, cx)
            .language(language)
            .line_number(true)
            .default_value(text)
    })
}

impl RequestEditor {
    pub fn new(
        store: Entity<Store>,
        saved_id: Option<u64>,
        request: Request,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let url = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Enter URL or paste text")
        });
        let params = cx.new(|cx| KvEditor::new(&[], ("Key", "Value"), window, cx));
        let headers = cx.new(|cx| KvEditor::new(&[], ("Key", "Value"), window, cx));
        let form = cx.new(|cx| KvEditor::new(&[], ("Key", "Value"), window, cx));
        let multipart = cx.new(|cx| KvEditor::new(&[], ("Key", "Value"), window, cx).with_files());
        let socket_event = cx.new(|cx| InputState::new(window, cx).placeholder("Event name"));
        let socket_fields: Vec<_> = SOCKET_FIELDS.iter().map(|_| cx.new(|cx| InputState::new(window, cx))).collect();
        let auth_inputs: Vec<_> = (0..8).map(|_| cx.new(|cx| InputState::new(window, cx))).collect();
        let docs = code_editor(&request.description, "text", window, cx);
        let body = code_editor("", request.raw_language.editor_language(), window, cx);
        let graphql_query = code_editor("", "text", window, cx);
        let graphql_variables = code_editor("", "json", window, cx);
        let mut subs = vec![
            cx.subscribe_in(&url, window, Self::on_url),
            cx.subscribe_in(&params, window, |this, _, _: &KvChanged, window, cx| {
                let values = this.params.read(cx).values(cx);
                this.request.set_params(values);
                let url = this.request.url.clone();
                this.url.update(cx, |s, cx| s.set_value(url, window, cx));
                this.changed(window, cx);
            }),
            cx.subscribe_in(&headers, window, |this, _, _: &KvChanged, window, cx| {
                this.request.headers = this.headers.read(cx).values(cx);
                this.changed(window, cx);
            }),
            cx.subscribe_in(&form, window, |this, _, _: &KvChanged, window, cx| {
                this.request.body_form = this.form.read(cx).values(cx);
                this.changed(window, cx);
            }),
            cx.subscribe_in(&multipart, window, |this, _, _: &KvChanged, window, cx| {
                this.request.body_multipart = this.multipart.read(cx).values(cx);
                this.changed(window, cx);
            }),
            cx.subscribe_in(&socket_event, window, |this, input, e: &InputEvent, window, cx| {
                if matches!(e, InputEvent::Change) {
                    this.request.socket_event = input.read(cx).value().to_string();
                    this.changed(window, cx);
                }
            }),
        ];
        for (ix, input) in socket_fields.iter().enumerate() {
            subs.push(cx.subscribe_in(input, window, move |this, input, e: &InputEvent, window, cx| {
                let value = input.read(cx).value().trim().to_string();
                if matches!(e, InputEvent::Change) && (SOCKET_FIELDS[ix].2)(&mut this.request, &value) {
                    this.changed(window, cx);
                }
            }));
        }
        subs.push(cx.subscribe_in(&docs, window, |this, e, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Change) {
                this.request.description = e.read(cx).value().to_string();
                this.changed(window, cx);
            }
        }));
        for input in &auth_inputs {
            subs.push(cx.subscribe_in(input, window, |this, _, e: &InputEvent, window, cx| {
                if matches!(e, InputEvent::Change) {
                    this.read_auth(cx);
                    this.changed(window, cx);
                }
            }));
        }
        let mut editor = Self {
            store,
            saved_id,
            baseline: request.clone(),
            request: Request::default(),
            section: if request.is_socket() || request.is_grpc() { Section::Body } else { Section::Params },
            menu: None,
            url,
            params,
            headers,
            form,
            multipart,
            body,
            graphql_query,
            graphql_variables,
            socket_event,
            socket_fields,
            auth_inputs,
            oauth_status: None,
            docs,
            code_lang: 0,
            state: SendState::Idle,
            generation: 0,
            response_tab: ResponseTab::Body,
            body_view: BodyView::Pretty,
            viewer: None,
            show_code: false,
            code: None,
            ws: None,
            ws_status: WsStatus::Disconnected,
            ws_session: 0,
            ws_method: String::new(),
            ws_recorded: false,
            ws_log: Vec::new(),
            grpc_call: None,
            grpc_status: None,
            grpc_methods: Vec::new(),
            grpc_loading: false,
            grpc_error: None,
            _subscriptions: subs,
        };
        editor.load(request, window, cx);
        editor
    }

    /// Puts every input in sync with `request`.
    fn load(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        let url = request.url.clone();
        self.url.update(cx, |s, cx| s.set_value(url, window, cx));
        self.params.update(cx, |e, cx| e.set_values(&request.params, window, cx));
        self.headers.update(cx, |e, cx| e.set_values(&request.headers, window, cx));
        self.form.update(cx, |e, cx| e.set_values(&request.body_form, window, cx));
        self.multipart.update(cx, |e, cx| e.set_values(&request.body_multipart, window, cx));
        let event = request.socket_event.clone();
        self.socket_event.update(cx, |s, cx| s.set_value(event, window, cx));
        for (input, field) in self.socket_fields.iter().zip(SOCKET_FIELDS) {
            let value = (field.3)(&request);
            input.update(cx, |s, cx| s.set_value(value, window, cx));
        }
        self.request = request;
        self.write_auth(window, cx);
        self.rebuild_body(window, cx);
        self.rebuild_graphql(window, cx);
    }

    fn rebuild_graphql(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.graphql_query = code_editor(&self.request.graphql_query, "text", window, cx);
        self.graphql_variables = code_editor(&self.request.graphql_variables, "json", window, cx);
        let subs = [
            cx.subscribe_in(&self.graphql_query, window, |this, e, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.request.graphql_query = e.read(cx).value().to_string();
                    this.changed(window, cx);
                }
            }),
            cx.subscribe_in(&self.graphql_variables, window, |this, e, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.request.graphql_variables = e.read(cx).value().to_string();
                    this.changed(window, cx);
                }
            }),
        ];
        self._subscriptions.extend(subs);
    }

    fn rebuild_body(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.body = code_editor(
            &self.request.body_raw,
            self.request.raw_language.editor_language(),
            window,
            cx,
        );
        let sub = cx.subscribe_in(&self.body, window, |this, body, e: &InputEvent, window, cx| {
            if matches!(e, InputEvent::Change) {
                this.request.body_raw = body.read(cx).value().to_string();
                this.changed(window, cx);
            }
        });
        self._subscriptions.push(sub);
    }

    fn write_auth(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self.request.auth.fields();
        for (ix, input) in self.auth_inputs.iter().enumerate() {
            let (label, value) = fields.get(ix).cloned().unwrap_or_default();
            input.update(cx, |s, cx| {
                s.set_placeholder(label, window, cx);
                s.set_value(value, window, cx)
            });
        }
    }

    fn read_auth(&mut self, cx: &App) {
        let inputs = &self.auth_inputs;
        for (ix, (_, value)) in self.request.auth.fields_mut().into_iter().enumerate() {
            *value = inputs[ix].read(cx).value().to_string();
        }
    }

    fn pick_body_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                this.request.body_file = path.to_string_lossy().into_owned();
                this.changed(window, cx);
            });
        })
        .detach();
    }

    fn get_oauth_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Auth::OAuth2(o) = self.request.auth.clone() else { return };
        let ws = &self.store.read(cx).ws;
        let (vars, settings) = (ws.variables(self.saved_id), ws.settings.clone());
        self.oauth_status = Some("Requesting token…".into());
        cx.notify();
        let (tx, rx) = oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(http::fetch_oauth2_token(&o, &vars, &settings));
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = rx.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(token) => {
                        if let Auth::OAuth2(o) = &mut this.request.auth {
                            o.access_token = token;
                        }
                        this.oauth_status = Some("Token added".into());
                        this.write_auth(window, cx);
                        this.changed(window, cx);
                    }
                    Err(e) => this.oauth_status = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn on_url(&mut self, _: &Entity<InputState>, event: &InputEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            InputEvent::Change => {
                let text = self.url.read(cx).value().to_string();
                if text == self.request.url {
                    return;
                }
                if text.trim_start().starts_with("curl ") {
                    if let Ok(mut parsed) = parse_curl(&text) {
                        parsed.name = self.request.name.clone();
                        self.load(parsed, window, cx);
                        self.changed(window, cx);
                        return;
                    }
                }
                let was_websocket = self.request.is_socket() || self.request.is_grpc();
                self.request.set_url(text);
                if (self.request.is_socket() || self.request.is_grpc()) && !was_websocket {
                    self.section = Section::Body;
                }
                let params = self.request.params.clone();
                self.params.update(cx, |e, cx| e.set_values(&params, window, cx));
                self.changed(window, cx);
            }
            // While connected, Enter in the URL must not send the composed message.
            InputEvent::PressEnter { .. } if self.ws_status == WsStatus::Disconnected => self.send(window, cx),
            _ => {}
        }
    }

    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ws_status != WsStatus::Disconnected && self.request.method != self.ws_method {
            self.disconnect(cx);
        }
        if self.show_code {
            self.refresh_code(window, cx);
        }
        cx.emit(RequestEvent::Changed);
        cx.notify();
    }

    pub fn is_dirty(&self) -> bool {
        self.request != self.baseline
    }

    pub fn title(&self) -> String {
        self.request.title()
    }

    pub fn url_focus(&self, cx: &App) -> FocusHandle {
        self.url.read(cx).focus_handle(cx)
    }

    pub fn set_name(&mut self, name: String, cx: &mut Context<Self>) {
        self.request.name = name.clone();
        self.baseline.name = name;
        cx.emit(RequestEvent::Changed);
        cx.notify();
    }

    /// Records that the current draft now lives in a collection.
    pub fn mark_saved(&mut self, id: u64, cx: &mut Context<Self>) {
        self.saved_id = Some(id);
        self.baseline = self.request.clone();
        cx.emit(RequestEvent::Changed);
        cx.notify();
    }

    /// Stores the current response under the request; saved requests are updated in place.
    fn save_example(&mut self, cx: &mut Context<Self>) {
        let SendState::Done(response) = &self.state else { return };
        let example = Example {
            name: format!("{} {}", self.request.title(), response.status),
            status: response.status,
            headers: response.headers.iter().map(|(k, v)| KeyValue::new(k, v)).collect(),
            body: response.text(),
        };
        self.request.examples.push(example.clone());
        if let Some(id) = self.saved_id {
            self.baseline.examples.push(example.clone());
            self.store.update(cx, |s, cx| {
                s.update(cx, |ws| ws.request_mut(id).map(|r| r.examples.push(example)))
            });
        }
        cx.emit(RequestEvent::Changed);
        cx.notify();
    }

    pub fn remove_example(&mut self, ix: usize, cx: &mut Context<Self>) {
        for examples in [&mut self.request.examples, &mut self.baseline.examples] {
            if ix < examples.len() {
                examples.remove(ix);
            }
        }
        cx.emit(RequestEvent::Changed);
        cx.notify();
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.saved_id else {
            cx.emit(RequestEvent::SaveAs);
            return;
        };
        let request = self.request.clone();
        let exists = self.store.update(cx, |s, cx| {
            s.update(cx, |ws| ws.request_mut(id).map(|r| *r = request).is_some())
        });
        if exists {
            self.mark_saved(id, cx);
        } else {
            self.saved_id = None;
            cx.emit(RequestEvent::SaveAs);
        }
    }

    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.request.is_grpc() {
            return self.invoke(cx);
        }
        if self.request.is_socket() {
            return match self.ws_status {
                WsStatus::Disconnected => self.connect(cx),
                WsStatus::Connected => self.send_message(cx),
                WsStatus::Connecting => {}
            };
        }
        if matches!(self.state, SendState::Sending) {
            return;
        }
        let request = self.request.clone();
        let vars = self.store.read(cx).ws.variables(self.saved_id);
        let settings = self.store.read(cx).ws.settings.clone();
        self.generation += 1;
        let generation = self.generation;
        self.state = SendState::Sending;
        self.menu = None;
        cx.notify();
        let (tx, rx) = oneshot::channel();
        let job = request.clone();
        std::thread::spawn(move || {
            let _ = tx.send(http::execute(&job, &vars, &settings));
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = rx.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                let entry = HistoryEntry {
                    request: Request {
                        name: String::new(),
                        ..request
                    },
                    status: result.as_ref().ok().map(|r| r.status),
                    at_unix_ms: chrono::Utc::now().timestamp_millis(),
                };
                this.store
                    .update(cx, |s, cx| s.update(cx, |ws| ws.push_history(entry)));
                match result {
                    Ok(response) => {
                        this.state = SendState::Done(Rc::new(response));
                        this.refresh_viewer(window, cx);
                    }
                    Err(error) => this.state = SendState::Failed(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let request = self.request.clone();
        let vars = self.store.read(cx).ws.variables(self.saved_id);
        let shown_url = resolve(request.url.trim(), &vars);
        self.ws_session += 1;
        let session = self.ws_session;
        self.ws_method = request.method.clone();
        self.ws_recorded = false;
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (ev_tx, mut ev_rx) = futures::channel::mpsc::unbounded();
        self.ws = Some(cmd_tx);
        self.ws_status = WsStatus::Connecting;
        self.menu = None;
        let job = request.clone();
        std::thread::spawn(move || ws::run(&job, &vars, cmd_rx, ev_tx));
        cx.spawn(async move |this, cx| {
            while let Some(event) = ev_rx.next().await {
                let current = this.update(cx, |this, cx| {
                    if this.ws_session != session {
                        return false;
                    }
                    this.on_ws_event(event, &request, &shown_url, cx);
                    true
                });
                if !matches!(current, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn on_ws_event(&mut self, event: ws::Event, request: &Request, url: &str, cx: &mut Context<Self>) {
        let first = !std::mem::replace(&mut self.ws_recorded, true);
        let history = |status: Option<u16>, cx: &mut Context<Self>| {
            if !first {
                return;
            }
            let entry = HistoryEntry {
                request: Request {
                    name: String::new(),
                    ..request.clone()
                },
                status,
                at_unix_ms: chrono::Utc::now().timestamp_millis(),
            };
            let store = self.store.clone();
            store.update(cx, |s, cx| s.update(cx, |ws| ws.push_history(entry)));
        };
        match event {
            ws::Event::Connected(status, headers) => {
                history(Some(101), cx);
                self.ws_status = WsStatus::Connected;
                let detail = std::iter::once(status)
                    .chain(headers.iter().map(|(k, v)| format!("{k}: {v}")))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.log(WsKind::Info, format!("Connected to {url}"), detail, None);
            }
            ws::Event::Received { text, binary, event } => {
                let tag = event.or(binary.then(|| "Binary".to_string()));
                self.log(WsKind::Received, text.clone(), pretty_message(&text), tag)
            }
            ws::Event::Reconnecting(message) => {
                self.ws_status = WsStatus::Connecting;
                self.log(WsKind::Error, message, String::new(), None);
            }
            ws::Event::Closed(reason) => {
                self.ws = None;
                self.ws_status = WsStatus::Disconnected;
                self.log(WsKind::Info, format!("{reason} from {url}"), String::new(), None);
            }
            ws::Event::Failed(error) => {
                if self.ws_status == WsStatus::Connecting {
                    history(None, cx);
                }
                self.ws = None;
                self.ws_status = WsStatus::Disconnected;
                self.log(WsKind::Error, error.clone(), error, None);
            }
        }
        cx.notify();
    }

    fn log(&mut self, kind: WsKind, summary: String, detail: String, tag: Option<String>) {
        // ponytail: every row renders each frame; virtualize the list if 1000 rows gets slow.
        if self.ws_log.len() >= WS_LOG_LIMIT {
            self.ws_log.remove(0);
        }
        let mut summary: String = summary.split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some((cut, _)) = summary.char_indices().nth(300) {
            summary.truncate(cut);
        }
        self.ws_log.push(WsEntry {
            kind,
            summary,
            detail,
            tag,
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            open: false,
        });
    }

    fn send_message(&mut self, cx: &mut Context<Self>) {
        let (WsStatus::Connected, Some(tx)) = (self.ws_status, &self.ws) else {
            return;
        };
        let vars = self.store.read(cx).ws.variables(self.saved_id);
        let text = resolve(&self.request.body_raw, &vars);
        let (command, tag) = if self.request.is_socket_io() {
            let event = resolve(self.request.socket_event.trim(), &vars);
            if event.is_empty() {
                return self.log(WsKind::Error, "Enter an event name to emit".into(), String::new(), None);
            }
            let tag = if self.request.socket_ack { format!("{event} (ack)") } else { event.clone() };
            let ack = self.request.socket_ack;
            (ws::Command::Emit { event, data: text.clone(), ack }, Some(tag))
        } else if let Some(encoding) = self.request.message_binary {
            match encoding.decode(&text) {
                Ok(bytes) => (ws::Command::Binary(bytes), Some("Binary".into())),
                Err(error) => return self.log(WsKind::Error, error, String::new(), None),
            }
        } else if text.is_empty() {
            return;
        } else {
            (ws::Command::Text(text.clone()), None)
        };
        if tx.send(command).is_ok() {
            self.log(WsKind::Sent, text.clone(), pretty_message(&text), tag);
        }
        cx.notify();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        match self.ws_status {
            // The worker reports Closed once the close handshake finishes.
            WsStatus::Connected => {
                if let Some(tx) = &self.ws {
                    let _ = tx.send(ws::Command::Close);
                }
            }
            WsStatus::Connecting => {
                self.ws_session += 1;
                self.ws = None;
                self.ws_status = WsStatus::Disconnected;
                self.log(WsKind::Info, "Connection cancelled".into(), String::new(), None);
            }
            WsStatus::Disconnected => {}
        }
        cx.notify();
    }

    fn grpc_target(&self, cx: &App) -> (String, grpc::Source, Vec<(String, String)>) {
        let vars = self.store.read(cx).ws.variables(self.saved_id);
        let url = resolve(self.request.url.trim(), &vars);
        let proto = resolve(self.request.grpc_proto.trim(), &vars);
        let source = if proto.is_empty() { grpc::Source::Reflection } else { grpc::Source::Proto(proto.into()) };
        let metadata = self
            .request
            .headers
            .iter()
            .filter(|h| h.enabled && !h.key.trim().is_empty())
            .map(|h| (resolve(h.key.trim(), &vars), resolve(&h.value, &vars)))
            .collect();
        (url, source, metadata)
    }

    /// Runs `job` on a worker thread and hands its result back on the UI thread.
    fn off_thread<T: Send + 'static>(
        &mut self,
        job: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut Self, T, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (tx, rx) = oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = rx.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                done(this, result, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn load_grpc_methods(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (url, source, metadata) = self.grpc_target(cx);
        self.grpc_loading = true;
        self.grpc_error = None;
        self.menu = Some(Menu::GrpcMethod);
        cx.notify();
        self.off_thread(
            move || grpc::list_methods(&url, &source, &metadata),
            |this, result, _, _| {
                this.grpc_loading = false;
                match result {
                    Ok(methods) => this.grpc_methods = methods,
                    Err(error) => this.grpc_error = Some(error),
                }
            },
            window,
            cx,
        );
    }

    fn use_example_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (url, source, metadata) = self.grpc_target(cx);
        let method = self.request.grpc_method.clone();
        self.grpc_loading = true;
        self.grpc_error = None;
        self.off_thread(
            move || grpc::example_message(&url, &source, &metadata, &method),
            |this, result, window, cx| {
                this.grpc_loading = false;
                match result {
                    Ok(example) => {
                        this.request.body_raw = example;
                        this.rebuild_body(window, cx);
                        this.changed(window, cx);
                    }
                    Err(error) => this.grpc_error = Some(error),
                }
            },
            window,
            cx,
        );
    }

    fn pick_proto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                this.request.grpc_proto = path.to_string_lossy().into_owned();
                this.grpc_methods.clear();
                this.changed(window, cx);
            });
        })
        .detach();
    }

    fn invoke(&mut self, cx: &mut Context<Self>) {
        if self.grpc_call.is_some() {
            return;
        }
        if self.request.grpc_method.is_empty() {
            self.grpc_error = Some("Select a method to invoke".into());
            return cx.notify();
        }
        let (url, source, metadata) = self.grpc_target(cx);
        let vars = self.store.read(cx).ws.variables(self.saved_id);
        let message = resolve(&self.request.body_raw, &vars);
        let method = self.request.grpc_method.clone();
        let request = self.request.clone();
        self.ws_session += 1;
        let session = self.ws_session;
        self.ws_log.clear();
        self.grpc_status = None;
        self.grpc_error = None;
        self.menu = None;
        self.log(WsKind::Sent, message.clone(), pretty_message(&message), Some(method.clone()));
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.grpc_call = Some(cancel_tx);
        let (ev_tx, mut ev_rx) = futures::channel::mpsc::unbounded();
        std::thread::spawn(move || grpc::invoke(&url, &source, &metadata, &method, &message, cancel_rx, ev_tx));
        cx.spawn(async move |this, cx| {
            while let Some(event) = ev_rx.next().await {
                let finished = matches!(event, grpc::Event::Done { .. } | grpc::Event::Failed(_));
                let current = this.update(cx, |this, cx| {
                    if this.ws_session != session {
                        return false;
                    }
                    this.on_grpc_event(event, cx);
                    if finished {
                        this.grpc_call = None;
                        let entry = HistoryEntry {
                            request: Request { name: String::new(), ..request.clone() },
                            status: None,
                            at_unix_ms: chrono::Utc::now().timestamp_millis(),
                        };
                        this.store.update(cx, |s, cx| s.update(cx, |ws| ws.push_history(entry)));
                    }
                    cx.notify();
                    true
                });
                if !matches!(current, Ok(true)) || finished {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn on_grpc_event(&mut self, event: grpc::Event, _: &mut Context<Self>) {
        let lines = |pairs: &[(String, String)]| pairs.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n");
        match event {
            grpc::Event::Metadata(headers) => {
                let detail = lines(&headers);
                self.log(WsKind::Info, format!("Response metadata ({})", headers.len()), detail, None)
            }
            grpc::Event::Message(json) => self.log(WsKind::Received, json.clone(), json, None),
            grpc::Event::Done { status, message, trailers } => {
                let ok = status.starts_with("0 ");
                let summary = if message.is_empty() { status.clone() } else { format!("{status}: {message}") };
                let kind = if ok { WsKind::Info } else { WsKind::Error };
                self.log(kind, summary, lines(&trailers), Some("Trailers".into()).filter(|_| !trailers.is_empty()));
                self.grpc_status = Some((status, ok));
            }
            grpc::Event::Failed(error) => {
                self.log(WsKind::Error, error.clone(), error, None);
                self.grpc_status = Some(("Failed".into(), false));
            }
        }
    }

    fn cancel_grpc(&mut self, cx: &mut Context<Self>) {
        // Dropping the sender cancels the call; its thread exits without further events.
        if self.grpc_call.take().is_some() {
            self.ws_session += 1;
            self.log(WsKind::Info, "Call cancelled".into(), String::new(), None);
            cx.notify();
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        // ponytail: the worker thread finishes in the background; its result is dropped.
        self.generation += 1;
        self.state = SendState::Idle;
        cx.notify();
    }

    fn refresh_viewer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let SendState::Done(response) = &self.state else {
            return;
        };
        let (text, language) = match self.body_view {
            BodyView::Pretty => (response.pretty(), response.language()),
            BodyView::Raw => (response.text(), "text"),
        };
        self.viewer = Some(code_editor(&text, language, window, cx));
    }

    fn refresh_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let vars = self.store.read(cx).ws.variables(self.saved_id);
        let snippet = snippets::generate(self.code_lang, &self.request, &vars);
        match &self.code {
            Some(code) => code.update(cx, |s, cx| s.set_value(snippet, window, cx)),
            None => self.code = Some(code_editor(&snippet, snippets::LANGUAGES[self.code_lang].1, window, cx)),
        }
    }

    pub fn toggle_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_code = !self.show_code;
        if self.show_code {
            self.refresh_code(window, cx);
        }
        cx.notify();
    }

    fn set_method(&mut self, method: &str, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(method, WEBSOCKET | SOCKET_IO | GRPC) && self.request.method != method {
            self.section = Section::Body;
        }
        self.request.method = method.into();
        self.menu = None;
        self.changed(window, cx);
    }

    fn toggle_menu(&mut self, menu: Menu, cx: &mut Context<Self>) {
        self.menu = if self.menu == Some(menu) { None } else { Some(menu) };
        cx.notify();
    }

    fn render_url_row(&self, t: &Tokens, cx: &mut Context<Self>) -> impl IntoElement {
        let method = self.request.method.clone();
        let method_menu = menu(t).children(METHODS.iter().map(|m| {
            menu_item(SharedString::from(format!("method-{m}")), t)
                .text_color(rgb(t.method(m)))
                .font_weight(FontWeight::SEMIBOLD)
                .child(*m)
                .on_click(cx.listener(move |this, _, window, cx| this.set_method(m, window, cx)))
        }))
        .child(div().h(px(1.)).my_1().bg(rgb(t.border)))
        .children([(WEBSOCKET, "WebSocket"), (SOCKET_IO, "Socket.IO"), (GRPC, "gRPC")].map(|(kind, label)| {
            menu_item(SharedString::from(format!("method-{kind}")), t)
                .font_weight(FontWeight::SEMIBOLD)
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| this.set_method(kind, window, cx)))
        }));
        let action = if self.request.is_grpc() {
            if self.grpc_call.is_some() {
                outline_button("grpc-cancel", "Cancel", t).on_click(cx.listener(|this, _, _, cx| this.cancel_grpc(cx)))
            } else {
                primary_button("invoke", "Invoke", t).on_click(cx.listener(|this, _, _, cx| this.invoke(cx)))
            }
        } else if !self.request.is_socket() {
            primary_button("send", "Send", t).on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
        } else if self.ws_status == WsStatus::Disconnected {
            primary_button("connect", "Connect", t).on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
        } else {
            let label = if self.ws_status == WsStatus::Connecting { "Cancel" } else { "Disconnect" };
            outline_button("disconnect", label, t).on_click(cx.listener(|this, _, _, cx| this.disconnect(cx)))
        };
        div()
            .px_4()
            .pb_2()
            .flex()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(40.))
                    .flex()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(rgb(t.border))
                    .bg(rgb(t.bg_tertiary))
                    .child(
                        div()
                            .id("method")
                            .w(px(110.))
                            .flex_none()
                            .h_full()
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .rounded_l(px(4.))
                            .bg(rgb(t.bg))
                            .border_r_1()
                            .border_color(rgb(t.border))
                            .cursor_pointer()
                            .text_color(rgb(t.method(&method)))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(method_label(&method).to_string())
                            .child(icon("icons/chevron-down.svg", 14., t.text_secondary))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Method, cx)))
                            .relative()
                            .when(self.menu == Some(Menu::Method), |d| d.child(popover(method_menu))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .flex()
                            .items_center()
                            .child(Input::new(&self.url).appearance(false).text_size(px(12.)).on_paste({
                                // The single-line input drops newlines on paste, which breaks
                                // multi-line `curl` commands, so parse the clipboard text directly.
                                let this = cx.entity().downgrade();
                                move |item, window, cx| {
                                    let Some(text) = item.text() else { return false };
                                    if !text.trim_start().starts_with("curl ") {
                                        return false;
                                    }
                                    let Ok(mut parsed) = parse_curl(&text) else { return false };
                                    this.update(cx, |e, cx| {
                                        parsed.name = e.request.name.clone();
                                        e.load(parsed, window, cx);
                                        e.changed(window, cx);
                                    })
                                    .is_ok()
                                }
                            })),
                    ),
            )
            .when(self.request.is_grpc(), |d| d.child(self.render_grpc_picker(t, cx)))
            .child(action.h(px(40.)).w(px(100.)).text_size(px(14.)))
    }

    fn render_grpc_picker(&self, t: &Tokens, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = &self.request.grpc_method;
        let body: AnyElement = if self.grpc_loading {
            div().p_2().text_color(rgb(t.text_secondary)).child("Loading methods...").into_any_element()
        } else if let Some(error) = &self.grpc_error {
            div().p_2().max_w(px(420.)).text_color(rgb(t.error)).child(error.clone()).into_any_element()
        } else if self.grpc_methods.is_empty() {
            div().p_2().text_color(rgb(t.text_secondary)).child("No methods found").into_any_element()
        } else {
            div()
                .id("grpc-methods")
                .max_h(px(360.))
                .overflow_y_scroll()
                .children(self.grpc_methods.iter().enumerate().map(|(ix, m)| {
                    let path = m.path.clone();
                    let kind = match (m.client_streaming, m.server_streaming) {
                        (false, false) => "Unary",
                        (false, true) => "Server streaming",
                        (true, false) => "Client streaming",
                        (true, true) => "Bidirectional",
                    };
                    menu_item(("grpc-method", ix), t)
                        .flex()
                        .justify_between()
                        .gap_4()
                        .when(&m.path == selected, |d| d.font_weight(FontWeight::SEMIBOLD))
                        .child(m.path.clone())
                        .child(div().text_color(rgb(t.text_tertiary)).child(kind))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.request.grpc_method = path.clone();
                            this.menu = None;
                            this.changed(window, cx);
                        }))
                }))
                .into_any_element()
        };
        div()
            .id("grpc-method")
            .relative()
            .w(px(260.))
            .flex_none()
            .h(px(40.))
            .px(px(10.))
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .rounded(px(4.))
            .border_1()
            .border_color(rgb(t.border))
            .bg(rgb(t.bg_tertiary))
            .cursor_pointer()
            .child(
                div()
                    .truncate()
                    .text_color(rgb(if selected.is_empty() { t.text_secondary } else { t.text }))
                    .child(if selected.is_empty() { "Select a method".to_string() } else { selected.clone() }),
            )
            .child(icon("icons/chevron-down.svg", 14., t.text_secondary))
            .on_click(cx.listener(|this, _, window, cx| {
                if this.menu == Some(Menu::GrpcMethod) {
                    this.menu = None;
                    cx.notify();
                } else {
                    this.load_grpc_methods(window, cx);
                }
            }))
            .when(self.menu == Some(Menu::GrpcMethod), |d| d.child(popover(menu(t).min_w(px(420.)).child(body))))
    }

    fn render_grpc_message(&self, t: &Tokens, cx: &mut Context<Self>) -> Div {
        let link = |id: &'static str, label: &'static str| {
            div().id(id).cursor_pointer().text_color(rgb(t.link)).font_weight(FontWeight::SEMIBOLD).child(label)
        };
        div()
            .child(
                div()
                    .h(px(40.))
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(link("grpc-example", "Use Example Message").on_click(
                        cx.listener(|this, _, window, cx| this.use_example_message(window, cx)),
                    ))
                    .when(self.grpc_loading, |d| d.child(div().text_color(rgb(t.text_secondary)).child("Loading...")))
                    .child(div().flex_1())
                    .child(link("grpc-beautify", "Beautify").on_click(cx.listener(|this, _, window, cx| {
                        if let Ok(pretty) = parse_relaxed_json(&this.request.body_raw).and_then(|v| {
                            serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
                        }) {
                            this.request.body_raw = pretty;
                            this.rebuild_body(window, cx);
                            this.changed(window, cx);
                        }
                    }))),
            )
            .when_some(self.grpc_error.clone().filter(|_| self.menu != Some(Menu::GrpcMethod)), |d, e| {
                d.child(div().pb_2().text_color(rgb(t.error)).child(e))
            })
            .child(
                div()
                    .h(px(260.))
                    .border_1()
                    .border_color(rgb(t.border))
                    .rounded(px(4.))
                    .font_family(MONO)
                    .child(Editor::new(&self.body).appearance(false).h_full()),
            )
    }

    fn render_definition(&self, t: &Tokens, cx: &mut Context<Self>) -> Div {
        let proto = self.request.grpc_proto.clone();
        let reflection = proto.trim().is_empty();
        let option = |id: &'static str, on: bool, label: &'static str, help: &'static str| {
            div()
                .id(id)
                .py_2()
                .flex()
                .items_start()
                .gap_2()
                .cursor_pointer()
                .child(
                    div()
                        .mt(px(2.))
                        .size(px(14.))
                        .flex_none()
                        .rounded(px(7.))
                        .border_1()
                        .border_color(rgb(if on { t.brand } else { t.text_tertiary }))
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(on, |d| d.child(div().size(px(6.)).rounded(px(3.)).bg(rgb(t.brand)))),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_color(rgb(t.text)).child(label))
                        .child(div().text_color(rgb(t.text_secondary)).child(help)),
                )
        };
        div()
            .pt_2()
            .child(
                option(
                    "def-reflection",
                    reflection,
                    "Using server reflection",
                    "Ask the server for its services. The server must enable gRPC server reflection.",
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.request.grpc_proto.clear();
                    this.grpc_methods.clear();
                    this.changed(window, cx);
                })),
            )
            .child(
                option(
                    "def-proto",
                    !reflection,
                    "Using a .proto file",
                    "Imports are resolved relative to the file's folder.",
                )
                .on_click(cx.listener(|this, _, window, cx| this.pick_proto(window, cx))),
            )
            .when(!reflection, |d| {
                d.child(
                    div()
                        .pl_6()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().font_family(MONO).truncate().child(proto))
                        .child(
                            outline_button("proto-change", "Change", t)
                                .on_click(cx.listener(|this, _, window, cx| this.pick_proto(window, cx))),
                        ),
                )
            })
    }

    fn render_sections(&self, t: &Tokens, cx: &mut Context<Self>) -> impl IntoElement {
        let r = &self.request;
        let header_count = r.headers.iter().filter(|h| h.enabled).count();
        let tab = |id: &'static str, section: Section, label: String, marker: bool, cx: &mut Context<Self>| {
            section_tab(id, label, self.section == section, t)
                .when(marker, |d| d.child(dot(t.success)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.section = section;
                    this.menu = None;
                    cx.notify();
                }))
        };
        let ws = r.is_socket();
        if r.is_grpc() {
            let tabs = div()
                .px_4()
                .flex()
                .gap_4()
                .child(tab("sec-message", Section::Body, "Message".into(), false, cx))
                .child(tab(
                    "sec-metadata",
                    Section::Headers,
                    if header_count > 0 { format!("Metadata ({header_count})") } else { "Metadata".into() },
                    false,
                    cx,
                ))
                .child(tab("sec-definition", Section::Definition, "Service definition".into(), false, cx));
            let content = match self.section {
                Section::Headers => div().child(self.headers.clone()).pt_2(),
                Section::Definition => self.render_definition(t, cx),
                _ => self.render_grpc_message(t, cx),
            };
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(tabs)
                .child(div().id("sections").flex_1().min_h_0().overflow_y_scroll().px_4().pb_4().child(content));
        }
        let tabs = div()
            .px_4()
            .flex()
            .gap_4()
            .when(ws, |d| d.child(tab("sec-message", Section::Body, "Message".into(), false, cx)))
            .child(tab("sec-params", Section::Params, "Params".into(), r.params.iter().any(|p| p.enabled), cx))
            .when(!ws, |d| d.child(tab("sec-auth", Section::Auth, "Authorization".into(), r.auth != Auth::None, cx)))
            .child(tab(
                "sec-headers",
                Section::Headers,
                if header_count > 0 { format!("Headers ({header_count})") } else { "Headers".into() },
                false,
                cx,
            ))
            .when(!ws, |d| d.child(tab("sec-body", Section::Body, "Body".into(), r.body_mode != BodyMode::None, cx)))
            .child(tab("sec-settings", Section::Settings, "Settings".into(), false, cx))
            .child(tab("sec-docs", Section::Docs, "Docs".into(), !r.description.trim().is_empty(), cx));
        let title = |text: &'static str| {
            div()
                .h(px(32.))
                .flex()
                .items_center()
                .text_color(rgb(t.text_secondary))
                .font_weight(FontWeight::SEMIBOLD)
                .child(text)
        };
        let section = match self.section {
            Section::Auth if ws => Section::Body,
            Section::Definition => Section::Params,
            section => section,
        };
        let content = match section {
            Section::Params => div().child(title("Query Params")).child(self.params.clone()),
            Section::Headers => div().child(title("Headers")).child(self.headers.clone()),
            Section::Auth => self.render_auth(t, cx),
            Section::Body => self.render_body(t, cx),
            Section::Settings | Section::Definition => self.render_settings(t, cx),
            Section::Docs => div().pt_2().child(
                div()
                    .h(px(260.))
                    .border_1()
                    .border_color(rgb(t.border))
                    .rounded(px(4.))
                    .child(Editor::new(&self.docs).appearance(false).h_full()),
            ),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(tabs)
            .child(
                div()
                    .id("sections")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_4()
                    .pb_4()
                    .child(content),
            )
    }

    fn render_auth(&self, t: &Tokens, cx: &mut Context<Self>) -> Div {
        let auth = &self.request.auth;
        let auth_menu = menu(t).children(Auth::LABELS.iter().enumerate().map(|(ix, label)| {
            menu_item(("auth-type", ix), t).child(*label).on_click(cx.listener(
                move |this, _, window, cx| {
                    this.request.auth = Auth::from_index(ix);
                    this.menu = None;
                    this.write_auth(window, cx);
                    this.changed(window, cx);
                },
            ))
        }));
        let help = match auth {
            Auth::None => "This request does not use any authorization.",
            Auth::Bearer { .. } => "The authorization header will be automatically generated when you send the request.",
            Auth::Basic { .. } => "The username and password are sent Base64-encoded in the Authorization header.",
            Auth::ApiKey { .. } => "The key/value pair is added to the header or query params when you send the request.",
            Auth::Digest { .. } => "The request is retried with a digest of the credentials after the server's challenge.",
            Auth::OAuth2(_) => "The access token is sent in the Authorization header. Get a new one from the token URL.",
            Auth::AwsV4 { .. } => "The request is signed with AWS Signature Version 4 when you send it.",
        };
        let field = |label: &'static str, input: &Entity<InputState>| {
            div()
                .flex()
                .items_center()
                .gap_4()
                .child(div().w(px(120.)).text_color(rgb(t.text_secondary)).child(label))
                .child(
                    div()
                        .flex_1()
                        .h(px(32.))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(rgb(t.border))
                        .child(Input::new(input).appearance(false).text_size(px(12.))),
                )
        };
        let option = |id: &'static str, label: &'static str, value: &'static str, cx: &mut Context<Self>| {
            div()
                .flex()
                .items_center()
                .gap_4()
                .child(div().w(px(120.)).text_color(rgb(t.text_secondary)).child(label))
                .child(
                    ghost_button(id, t)
                        .border_1()
                        .border_color(rgb(t.border))
                        .text_color(rgb(t.text))
                        .child(value)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            match &mut this.request.auth {
                                Auth::ApiKey { in_query, .. } => *in_query = !*in_query,
                                Auth::OAuth2(o) => {
                                    o.grant_type =
                                        if o.grant_type == "password" { "client_credentials" } else { "password" }.into()
                                }
                                _ => {}
                            }
                            this.write_auth(window, cx);
                            this.changed(window, cx);
                        })),
                )
        };
        let mut fields = div().flex().flex_col().gap_2().children(
            auth.fields().iter().zip(&self.auth_inputs).map(|((label, _), input)| field(label, input)),
        );
        match auth {
            Auth::ApiKey { in_query, .. } => {
                fields = fields.child(option("api-key-in", "Add to", if *in_query { "Query Params" } else { "Header" }, cx))
            }
            Auth::OAuth2(o) => {
                let grant = if o.grant_type == "password" { "Password Credentials" } else { "Client Credentials" };
                fields = fields.child(option("oauth-grant", "Grant Type", grant, cx)).child(
                    div()
                        .flex()
                        .items_center()
                        .gap_4()
                        .child(div().w(px(120.)))
                        .child(
                            primary_button("oauth-token", "Get New Access Token", t)
                                .on_click(cx.listener(|this, _, window, cx| this.get_oauth_token(window, cx))),
                        )
                        .when_some(self.oauth_status.clone(), |d, status| {
                            d.child(div().flex_1().min_w_0().truncate().text_color(rgb(t.text_secondary)).child(status))
                        }),
                );
            }
            _ => {}
        }
        div()
            .pt_3()
            .flex()
            .gap_6()
            .child(
                div()
                    .w(px(280.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .pr_6()
                    .border_r_1()
                    .border_color(rgb(t.border))
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(rgb(t.text_secondary)).child("Auth Type"))
                    .child(
                        div()
                            .id("auth-type")
                            .relative()
                            .h(px(32.))
                            .px_2()
                            .flex()
                            .items_center()
                            .justify_between()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(rgb(t.border))
                            .cursor_pointer()
                            .child(auth.label())
                            .child(icon("icons/chevron-down.svg", 14., t.text_secondary))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Auth, cx)))
                            .relative()
                            .when(self.menu == Some(Menu::Auth), |d| d.child(popover(auth_menu))),
                    )
                    .child(div().text_color(rgb(t.text_secondary)).child(help)),
            )
            .child(div().flex_1().child(fields))
    }

    fn render_body(&self, t: &Tokens, cx: &mut Context<Self>) -> Div {
        let ws = self.request.is_socket();
        let mode = if ws { BodyMode::Raw } else { self.request.body_mode };
        let radio = |id: &'static str, label: &'static str, value: BodyMode, cx: &mut Context<Self>| {
            let on = mode == value;
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .child(
                    div()
                        .size(px(14.))
                        .rounded(px(7.))
                        .border_1()
                        .border_color(rgb(if on { t.brand } else { t.text_tertiary }))
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(on, |d| d.child(div().size(px(6.)).rounded(px(3.)).bg(rgb(t.brand)))),
                )
                .child(label)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.request.body_mode = value;
                    // Most GraphQL servers ignore a GET body, so a GET here would silently return nothing.
                    if value == BodyMode::GraphQl && this.request.method == "GET" {
                        this.request.method = "POST".into();
                    }
                    this.changed(window, cx);
                }))
        };
        let lang_menu = menu(t)
            .children(RawLanguage::ALL.iter().map(|lang| {
                let lang = *lang;
                menu_item(SharedString::from(format!("lang-{}", lang.label())), t)
                    .child(lang.label())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request.raw_language = lang;
                        this.request.message_binary = None;
                        this.menu = None;
                        this.rebuild_body(window, cx);
                        this.changed(window, cx);
                    }))
            }))
            .when(self.request.is_websocket(), |d| {
                d.children([(BinaryEncoding::Base64, "Binary (Base64)"), (BinaryEncoding::Hex, "Binary (Hex)")].map(
                    |(encoding, label)| {
                        menu_item(SharedString::from(format!("lang-{label}")), t).child(label).on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.request.raw_language = RawLanguage::Text;
                                this.request.message_binary = Some(encoding);
                                this.menu = None;
                                this.rebuild_body(window, cx);
                                this.changed(window, cx);
                            },
                        ))
                    },
                ))
            });
        let binary = self.request.message_binary.filter(|_| self.request.is_websocket());
        let lang_label = match binary {
            Some(BinaryEncoding::Base64) => "Binary (Base64)",
            Some(BinaryEncoding::Hex) => "Binary (Hex)",
            None => self.request.raw_language.label(),
        };
        let bar = div()
            .h(px(40.))
            .flex()
            .items_center()
            .gap_4()
            .when(!ws, |d| {
                d.child(radio("body-none", "none", BodyMode::None, cx))
                    .child(radio("body-multipart", "form-data", BodyMode::FormData, cx))
                    .child(radio("body-form", "x-www-form-urlencoded", BodyMode::UrlEncoded, cx))
                    .child(radio("body-raw", "raw", BodyMode::Raw, cx))
                    .child(radio("body-graphql", "GraphQL", BodyMode::GraphQl, cx))
                    .child(radio("body-binary", "binary", BodyMode::Binary, cx))
            })
            .when(mode == BodyMode::Raw, |d| {
                d.child(
                    div()
                        .id("raw-lang")
                        .flex()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .text_color(rgb(t.link))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(lang_label)
                        .child(icon("icons/chevron-down.svg", 14., t.link))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Language, cx)))
                        .relative()
                        .when(self.menu == Some(Menu::Language), |d| d.child(popover(lang_menu))),
                )
                .when(self.request.raw_language == RawLanguage::Json && binary.is_none(), |d| {
                    d.child(div().flex_1()).child(
                        div()
                            .id("beautify")
                            .cursor_pointer()
                            .text_color(rgb(t.link))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Beautify")
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Ok(v) = parse_relaxed_json(&this.request.body_raw) {
                                    if let Ok(pretty) = serde_json::to_string_pretty(&v) {
                                        this.request.body_raw = pretty;
                                        this.rebuild_body(window, cx);
                                        this.changed(window, cx);
                                    }
                                }
                            })),
                    )
                })
            })
            .when(ws, |d| {
                let connected = self.ws_status == WsStatus::Connected;
                d.when(self.request.raw_language != RawLanguage::Json || binary.is_some(), |d| d.child(div().flex_1()))
                    .child(if connected {
                        primary_button("ws-send", "Send", t)
                            .on_click(cx.listener(|this, _, _, cx| this.send_message(cx)))
                    } else {
                        outline_button("ws-send", "Send", t).text_color(rgb(t.text_tertiary)).cursor_default()
                    })
            });
        let content = match mode {
            BodyMode::None => div()
                .h(px(80.))
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(t.text_secondary))
                .child("This request does not have a body"),
            BodyMode::UrlEncoded => div().child(self.form.clone()),
            BodyMode::Binary => {
                let name = std::path::Path::new(&self.request.body_file)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Select file".into());
                div().pt_1().flex().child(
                    outline_button("body-file", name, t)
                        .on_click(cx.listener(|this, _, window, cx| this.pick_body_file(window, cx))),
                )
            }
            BodyMode::FormData => div().child(self.multipart.clone()),
            BodyMode::GraphQl => {
                let pane = |title: &'static str, editor: &Entity<EditorState>| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_color(rgb(t.text_secondary)).font_weight(FontWeight::SEMIBOLD).child(title))
                        .child(
                            div()
                                .h(px(240.))
                                .border_1()
                                .border_color(rgb(t.border))
                                .rounded(px(4.))
                                .font_family(MONO)
                                .child(Editor::new(editor).appearance(false).h_full()),
                        )
                };
                div()
                    .flex()
                    .gap_4()
                    .child(pane("Query", &self.graphql_query))
                    .child(pane("GraphQL Variables", &self.graphql_variables))
            }
            BodyMode::Raw => div()
                .h(px(260.))
                .border_1()
                .border_color(rgb(t.border))
                .rounded(px(4.))
                .font_family(MONO)
                .child(Editor::new(&self.body).appearance(false).h_full()),
        };
        let noun = if ws { "Message" } else { "Body" };
        let socket_io = self.request.is_socket_io().then(|| {
            div()
                .pb_2()
                .flex()
                .items_center()
                .gap_4()
                .child(
                    div()
                        .w(px(240.))
                        .h(px(32.))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(rgb(t.border))
                        .child(Input::new(&self.socket_event).appearance(false).text_size(px(12.))),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(checkbox("socket-ack", self.request.socket_ack, t).on_click(cx.listener(
                            |this, _, window, cx| {
                                this.request.socket_ack = !this.request.socket_ack;
                                this.changed(window, cx);
                            },
                        )))
                        .child("Ack"),
                )
        });
        let json_warning = (mode == BodyMode::Raw
            && binary.is_none()
            && self.request.raw_language == RawLanguage::Json
            && !self.request.body_raw.trim().is_empty())
        .then(|| serde_json::from_str::<serde_json::Value>(&self.request.body_raw).err())
        .flatten()
        .map(|error| match parse_relaxed_json(&self.request.body_raw) {
            Ok(_) => format!("{noun} is not valid JSON, so the server may reject it. Click Beautify to convert it."),
            Err(_) => format!("{noun} is not valid JSON: {error}"),
        });
        let content = div()
            .children(socket_io)
            .children(json_warning.map(|w| div().pb_2().text_color(rgb(t.warning)).child(w)))
            .child(content);
        div().child(bar).child(content)
    }

    fn render_settings(&self, t: &Tokens, cx: &mut Context<Self>) -> Div {
        let row = |id: &'static str, title: &'static str, help: &'static str, on: bool, flip: fn(&mut Request)| {
            div()
                .py_3()
                .flex()
                .items_center()
                .gap_6()
                .border_b_1()
                .border_color(rgb(t.border))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_color(rgb(t.text)).child(title))
                        .child(div().text_color(rgb(t.text_secondary)).child(help)),
                )
                .child(switch(id, on, t).on_click(cx.listener(move |this, _, window, cx| {
                    flip(&mut this.request);
                    this.changed(window, cx);
                })))
        };
        div()
            .pt_2()
            .child(row(
                "set-ssl",
                "Enable SSL certificate verification",
                "Verify SSL certificates when sending a request. Verification failures will result in the request being aborted.",
                self.request.verify_ssl,
                |r| r.verify_ssl = !r.verify_ssl,
            ))
            .when(self.request.is_socket(), |d| {
                d.children(SOCKET_FIELDS.iter().zip(&self.socket_fields).enumerate().filter_map(
                    |(ix, ((title, help, ..), input))| {
                        (ix < 4 || self.request.is_socket_io()).then(|| {
                            div()
                                .py_3()
                                .flex()
                                .items_center()
                                .gap_6()
                                .border_b_1()
                                .border_color(rgb(t.border))
                                .child(
                                    div()
                                        .flex_1()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(div().text_color(rgb(t.text)).child(*title))
                                        .child(div().text_color(rgb(t.text_secondary)).child(*help)),
                                )
                                .child(
                                    div()
                                        .w(px(120.))
                                        .h(px(32.))
                                        .px_2()
                                        .flex()
                                        .items_center()
                                        .rounded(px(4.))
                                        .border_1()
                                        .border_color(rgb(t.border))
                                        .child(Input::new(input).appearance(false).text_size(px(12.))),
                                )
                        })
                    },
                ))
            })
            .when(!self.request.is_socket(), |d| {
                d.child(row(
                    "set-redirects",
                    "Automatically follow redirects",
                    "Follow HTTP 3xx responses as redirects.",
                    self.request.follow_redirects,
                    |r| r.follow_redirects = !r.follow_redirects,
                ))
            })
    }

    fn render_response(&self, t: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let label_bar = |text: &'static str| {
            div()
                .h(px(32.))
                .px_2()
                .flex()
                .items_center()
                .text_size(px(14.))
                .text_color(rgb(t.text_tertiary))
                .child(text)
        };
        let centered = || {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(rgb(t.text_secondary))
        };
        let response = match &self.state {
            SendState::Idle => {
                return div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(label_bar("Response"))
                    .child(
                        centered()
                            .child(icon("icons/send.svg", 48., t.text_tertiary))
                            .child("Click Send to get a response"),
                    )
                    .into_any_element()
            }
            SendState::Sending => {
                return div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(label_bar("Response"))
                    .child(
                        centered().child("Sending request...").child(
                            outline_button("cancel", "Cancel", t)
                                .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                        ),
                    )
                    .into_any_element()
            }
            SendState::Failed(error) => {
                return div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(label_bar("Response"))
                    .child(
                        centered()
                            .child(div().text_size(px(14.)).text_color(rgb(t.text)).child("Could not send request"))
                            .child(
                                div()
                                    .max_w(px(640.))
                                    .px_3()
                                    .py_2()
                                    .rounded(px(4.))
                                    .bg(rgb(t.bg_secondary))
                                    .text_color(rgb(t.error))
                                    .font_family(MONO)
                                    .child(format!("Error: {error}")),
                            ),
                    )
                    .into_any_element()
            }
            SendState::Done(response) => response.clone(),
        };

        let tab = |id: &'static str, label: String, value: ResponseTab, cx: &mut Context<Self>| {
            section_tab(id, label, self.response_tab == value, t).on_click(cx.listener(move |this, _, _, cx| {
                this.response_tab = value;
                cx.notify();
            }))
        };
        let meta_sep = || div().text_color(rgb(t.text_tertiary)).child("•");
        let status_bar = div()
            .h(px(40.))
            .px_4()
            .flex()
            .items_center()
            .gap_4()
            .child(tab("res-body", "Body".into(), ResponseTab::Body, cx))
            .child(tab(
                "res-cookies",
                if response.cookies.is_empty() { "Cookies".into() } else { format!("Cookies ({})", response.cookies.len()) },
                ResponseTab::Cookies,
                cx,
            ))
            .child(tab("res-headers", format!("Headers ({})", response.headers.len()), ResponseTab::Headers, cx))
            .child(div().flex_1())
            .child(
                div()
                    .px_1()
                    .rounded(px(2.))
                    .text_color(rgb(t.status(response.status)))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{} {}", response.status, response.reason)),
            )
            .child(meta_sep())
            .child(div().text_color(rgb(t.success)).child(format!("{} ms", response.elapsed_ms)))
            .child(meta_sep())
            .child(div().text_color(rgb(t.success)).child(format_size(response.size)))
            .when(response.truncated, |d| {
                d.child(meta_sep()).child(div().text_color(rgb(t.warning)).child("Truncated at 10 MB"))
            })
            .child(meta_sep())
            .child(
                div()
                    .id("save-example")
                    .cursor_pointer()
                    .text_color(rgb(t.link))
                    .child("Save as example")
                    .on_click(cx.listener(|this, _, _, cx| this.save_example(cx))),
            );

        let body: AnyElement = match self.response_tab {
            ResponseTab::Body => {
                let seg = |id: &'static str, label: &'static str, view: BodyView, cx: &mut Context<Self>| {
                    let on = self.body_view == view;
                    div()
                        .id(id)
                        .h(px(24.))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .when(on, |d| d.bg(rgb(t.hover)).text_color(rgb(t.text)))
                        .when(!on, |d| d.text_color(rgb(t.text_secondary)))
                        .child(label)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.body_view = view;
                            this.refresh_viewer(window, cx);
                            cx.notify();
                        }))
                };
                let copy_text = response.text();
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .px_4()
                            .pb_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .p(px(2.))
                                    .flex()
                                    .rounded(px(4.))
                                    .bg(rgb(t.bg_secondary))
                                    .child(seg("view-pretty", "Pretty", BodyView::Pretty, cx))
                                    .child(seg("view-raw", "Raw", BodyView::Raw, cx)),
                            )
                            .child(
                                div()
                                    .text_color(rgb(t.text_secondary))
                                    .child(response.language().to_uppercase()),
                            )
                            .child(div().flex_1())
                            .child(icon_button("copy-response", "icons/copy.svg", t).on_click(
                                move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone())),
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .mx_4()
                            .mb_2()
                            .font_family(MONO)
                            .when_some(self.viewer.as_ref(), |d, viewer| {
                                d.child(Editor::new(viewer).appearance(false).readonly(true).h_full())
                            }),
                    )
                    .into_any_element()
            }
            ResponseTab::Headers => table(
                t,
                &["Key", "Value"],
                response.headers.iter().map(|(k, v)| vec![k.clone(), v.clone()]).collect(),
            ),
            ResponseTab::Cookies => {
                if response.cookies.is_empty() {
                    centered().child("No cookies received from the server").into_any_element()
                } else {
                    table(
                        t,
                        &["Name", "Value", "Domain", "Path", "Expires", "HttpOnly", "Secure"],
                        response
                            .cookies
                            .iter()
                            .map(|c| {
                                vec![
                                    c.name.clone(),
                                    c.value.clone(),
                                    c.domain.clone(),
                                    c.path.clone(),
                                    c.expires.clone(),
                                    c.http_only.to_string(),
                                    c.secure.to_string(),
                                ]
                            })
                            .collect(),
                    )
                }
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(status_bar)
            .child(body)
            .into_any_element()
    }

    fn render_messages(&self, t: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let grpc = self.request.is_grpc();
        let (status, color): (String, u32) = if grpc {
            match (&self.grpc_call, &self.grpc_status) {
                (Some(_), _) => ("Invoking...".into(), t.warning),
                (None, Some((status, ok))) => (status.clone(), if *ok { t.success } else { t.error }),
                (None, None) => ("Not invoked".into(), t.text_tertiary),
            }
        } else {
            match self.ws_status {
                WsStatus::Disconnected => ("Disconnected".into(), t.text_tertiary),
                WsStatus::Connecting => ("Connecting...".into(), t.warning),
                WsStatus::Connected => ("Connected".into(), t.success),
            }
        };
        let bar = div()
            .h(px(40.))
            .px_4()
            .flex()
            .items_center()
            .gap_2()
            .child(div().text_size(px(14.)).text_color(rgb(t.text_secondary)).child("Response"))
            .child(div().flex_1())
            .child(dot(color))
            .child(div().text_color(rgb(color)).child(status))
            .when(!self.ws_log.is_empty(), |d| {
                d.child(
                    ghost_button("ws-clear", t)
                        .child(icon("icons/delete.svg", 14., t.text_secondary))
                        .child("Clear")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ws_log.clear();
                            cx.notify();
                        })),
                )
            });
        if self.ws_log.is_empty() {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(bar)
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .text_color(rgb(t.text_secondary))
                        .child(icon("icons/plug.svg", 48., t.text_tertiary))
                        .child(if grpc {
                            "Select a method and click Invoke to get a response"
                        } else {
                            "Connect to a server to send and receive messages"
                        }),
                )
                .into_any_element();
        }
        // Newest first, so the latest message is visible without scrolling.
        let rows = self.ws_log.iter().enumerate().rev().map(|(ix, entry)| {
            let (path, tint) = match entry.kind {
                WsKind::Sent => ("icons/arrow-up-right.svg", t.success),
                WsKind::Received => ("icons/arrow-down-left.svg", t.info),
                WsKind::Info => ("icons/info.svg", t.text_secondary),
                WsKind::Error => ("icons/circle-alert.svg", t.error),
            };
            let expandable = !entry.detail.is_empty();
            let copy = entry.detail.clone();
            div()
                .border_b_1()
                .border_color(rgb(t.border))
                .child(
                    div()
                        .id(("ws-row", ix))
                        .h(px(32.))
                        .px_4()
                        .flex()
                        .items_center()
                        .gap_2()
                        .when(expandable, |d| d.cursor_pointer().hover(|d| d.bg(rgb(t.hover))))
                        .child(icon(path, 14., tint))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .when(entry.kind != WsKind::Info, |d| d.font_family(MONO))
                                .text_color(rgb(if entry.kind == WsKind::Error { t.error } else { t.text }))
                                .child(entry.summary.clone()),
                        )
                        .when_some(entry.tag.clone(), |d, tag| {
                            d.child(
                                div()
                                    .flex_none()
                                    .px_1()
                                    .rounded(px(2.))
                                    .bg(rgb(t.bg_secondary))
                                    .text_color(rgb(t.text_secondary))
                                    .child(tag),
                            )
                        })
                        .child(div().text_color(rgb(t.text_tertiary)).child(entry.at.clone()))
                        .when(expandable, |d| {
                            d.child(icon(
                                if entry.open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" },
                                14.,
                                t.text_secondary,
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.ws_log[ix].open = !this.ws_log[ix].open;
                                cx.notify();
                            }))
                        }),
                )
                .when(entry.open, |d| {
                    d.child(
                        div()
                            .mx_4()
                            .mb_2()
                            .p_2()
                            .flex()
                            .gap_2()
                            .rounded(px(4.))
                            .bg(rgb(t.bg_secondary))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_family(MONO)
                                    .whitespace_normal()
                                    .children(entry.detail.lines().map(|l| div().child(l.to_string()))),
                            )
                            .child(icon_button(("ws-copy", ix), "icons/copy.svg", t).on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                            })),
                    )
                })
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .child(
                div()
                    .id("ws-log")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .border_t_1()
                    .border_color(rgb(t.border))
                    .children(rows),
            )
            .into_any_element()
    }

    fn render_header(&self, t: &Tokens, cx: &mut Context<Self>) -> impl IntoElement {
        let crumbs = self
            .saved_id
            .map(|id| self.store.read(cx).ws.breadcrumb(id))
            .unwrap_or_default();
        div()
            .h(px(48.))
            .px_4()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .px_1()
                    .rounded(px(2.))
                    .border_1()
                    .border_color(rgb(t.text_tertiary))
                    .text_size(px(8.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(t.text_secondary))
                    .child(match self.request.method.as_str() {
                        WEBSOCKET => "WS",
                        SOCKET_IO => "Socket.IO",
                        GRPC => "gRPC",
                        _ => "HTTP",
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .gap_1()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .children(crumbs.into_iter().map(|c| {
                        div().text_color(rgb(t.text_secondary)).child(format!("{c} /"))
                    }))
                    .child(
                        div()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(t.text))
                            .child(self.title()),
                    ),
            )
            .child(
                ghost_button("save", t)
                    .child(icon("icons/save.svg", 16., t.text_secondary))
                    .child("Save")
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
    }
}

/// Read-only grid used for response headers and cookies.
pub fn table(t: &Tokens, columns: &[&'static str], rows: Vec<Vec<String>>) -> AnyElement {
    let row = |cells: Vec<SharedString>, head: bool| {
        div()
            .flex()
            .border_b_1()
            .border_color(rgb(t.border))
            .when(head, |d| d.text_color(rgb(t.text_secondary)).font_weight(FontWeight::SEMIBOLD))
            .children(cells.into_iter().enumerate().map(|(ix, text)| {
                cell(t)
                    .when(ix == 0, |d| d.border_l_0())
                    .when(ix == 0 && !head, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .child(div().truncate().child(text))
            }))
    };
    div()
        .id("table")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .px_4()
        .child(
            div()
                .border_1()
                .border_color(rgb(t.border))
                .rounded(px(4.))
                .child(row(columns.iter().map(|c| SharedString::from(*c)).collect(), true))
                .children(rows.into_iter().map(|r| row(r.into_iter().map(SharedString::from).collect(), false))),
        )
        .into_any_element()
}

impl Render for RequestEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Tokens::get(cx);
        let main = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(self.render_header(&t, cx))
            .child(self.render_url_row(&t, cx))
            .child(
                div().flex_1().min_h_0().child(
                    v_resizable("request-split")
                        .child(
                            resizable_panel()
                                .size(px(300.))
                                .size_range(px(120.)..px(2000.))
                                .child(self.render_sections(&t, cx)),
                        )
                        .child(
                            resizable_panel().child(
                                div()
                                    .size_full()
                                    .border_t_1()
                                    .border_color(rgb(t.border))
                                    .child(if self.request.is_socket() || self.request.is_grpc() {
                                        self.render_messages(&t, cx)
                                    } else {
                                        self.render_response(&t, cx)
                                    }),
                            ),
                        ),
                ),
            );
        let ws = self.request.is_socket() || self.request.is_grpc();
        let code_panel = (self.show_code && !ws).then(|| {
            div()
                .w(px(380.))
                .flex_none()
                .h_full()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(rgb(t.border))
                .child(
                    div()
                        .h(px(48.))
                        .px_4()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("Code snippet"))
                        .child(icon_button("close-code", "icons/close.svg", &t).on_click(
                            cx.listener(|this, _, window, cx| this.toggle_code(window, cx)),
                        )),
                )
                .child(
                    div()
                        .px_4()
                        .pb_2()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .id("code-lang")
                                .relative()
                                .flex()
                                .items_center()
                                .gap_1()
                                .cursor_pointer()
                                .text_color(rgb(t.text_secondary))
                                .child(snippets::LANGUAGES[self.code_lang].0)
                                .child(icon("icons/chevron-down.svg", 14., t.text_secondary))
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::CodeLanguage, cx)))
                                .when(self.menu == Some(Menu::CodeLanguage), |d| {
                                    d.child(popover(menu(&t).children(snippets::LANGUAGES.iter().enumerate().map(
                                        |(ix, (label, _))| {
                                            menu_item(("code-lang", ix), &t).child(*label).on_click(cx.listener(
                                                move |this, _, window, cx| {
                                                    this.code_lang = ix;
                                                    this.code = None;
                                                    this.menu = None;
                                                    this.refresh_code(window, cx);
                                                    cx.notify();
                                                },
                                            ))
                                        },
                                    ))))
                                }),
                        )
                        .child({
                            let vars = self.store.read(cx).ws.variables(self.saved_id);
                            let snippet = snippets::generate(self.code_lang, &self.request, &vars);
                            icon_button("copy-code", "icons/copy.svg", &t).on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(snippet.clone()))
                            })
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .px_2()
                        .font_family(MONO)
                        .when_some(self.code.as_ref(), |d, code| {
                            d.child(Editor::new(code).appearance(false).readonly(true).h_full())
                        }),
                )
        });
        let context_bar = div()
            .w(px(48.))
            .flex_none()
            .h_full()
            .pt_3()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .border_l_1()
            .border_color(rgb(t.border))
            .when(!ws, |d| {
                d.child(
                    icon_button("toggle-code", "icons/code.svg", &t)
                        .size(px(32.))
                        .when(self.show_code, |d| d.bg(rgb(t.hover)))
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_code(window, cx))),
                )
            });
        div()
            .size_full()
            .flex()
            .bg(rgb(t.bg))
            .text_size(px(12.))
            .text_color(rgb(t.text))
            .on_action(cx.listener(|this, _: &crate::Dismiss, _, cx| {
                if this.menu.take().is_some() {
                    cx.notify();
                } else {
                    cx.propagate();
                }
            }))
            .child(main)
            .children(code_panel)
            .child(context_bar)
            .when(self.menu.is_some(), |d| {
                d.child(backdrop(window, cx.listener(|this, _, _, cx| {
                    this.menu = None;
                    cx.notify();
                })))
            })
    }
}
