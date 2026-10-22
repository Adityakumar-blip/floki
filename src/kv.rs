use crate::model::KeyValue;
use crate::theme::Tokens;
use crate::ui::{checkbox, icon};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::*;

struct Row {
    enabled: bool,
    file: bool,
    key: Entity<InputState>,
    value: Entity<InputState>,
}

pub struct KvChanged;

/// Key/value grid: a trailing blank row that grows as you type.
pub struct KvEditor {
    rows: Vec<Row>,
    cache: Vec<KeyValue>,
    key_label: &'static str,
    value_label: &'static str,
    /// form-data mode: each row can be a text field or a file.
    files: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<KvChanged> for KvEditor {}

impl KvEditor {
    pub fn new(
        values: &[KeyValue],
        labels: (&'static str, &'static str),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editor = Self {
            rows: Vec::new(),
            cache: Vec::new(),
            key_label: labels.0,
            value_label: labels.1,
            files: false,
            _subscriptions: Vec::new(),
        };
        editor.set_values(values, window, cx);
        editor
    }

    pub fn with_files(mut self) -> Self {
        self.files = true;
        self
    }

    pub fn values(&self, cx: &App) -> Vec<KeyValue> {
        self.rows
            .iter()
            .map(|row| KeyValue {
                key: row.key.read(cx).value().to_string(),
                value: row.value.read(cx).value().to_string(),
                enabled: row.enabled,
                file: row.file,
            })
            .filter(|kv| !(kv.key.is_empty() && kv.value.is_empty()))
            .collect()
    }

    /// Replaces the rows without emitting a change, e.g. when the URL rewrites the params.
    pub fn set_values(&mut self, values: &[KeyValue], window: &mut Window, cx: &mut Context<Self>) {
        if values == self.cache.as_slice() && !self.rows.is_empty() {
            return;
        }
        let wanted = values.len() + 1;
        self.rows.truncate(wanted);
        while self.rows.len() < wanted {
            self.push_row(window, cx);
        }
        let blank = KeyValue::default();
        for (row, kv) in self.rows.iter_mut().zip(values.iter().chain([&blank])) {
            row.enabled = kv.enabled || kv.key.is_empty() && kv.value.is_empty();
            row.file = kv.file;
            for (input, text) in [(&row.key, &kv.key), (&row.value, &kv.value)] {
                if input.read(cx).value().as_ref() != text.as_str() {
                    input.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
                }
            }
        }
        self.cache = values.to_vec();
        cx.notify();
    }

    fn push_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let make = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let key = make(self.key_label, window, cx);
        let value = make(self.value_label, window, cx);
        for input in [&key, &value] {
            let sub = cx.subscribe_in(input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.changed(window, cx);
                }
            });
            self._subscriptions.push(sub);
        }
        self.rows.push(Row {
            enabled: true,
            file: false,
            key,
            value,
        });
    }

    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let last_used = self.rows.last().is_some_and(|row| {
            !row.key.read(cx).value().is_empty() || !row.value.read(cx).value().is_empty()
        });
        if last_used {
            self.push_row(window, cx);
        }
        let values = self.values(cx);
        if values != self.cache {
            self.cache = values;
            cx.emit(KvChanged);
        }
        cx.notify();
    }

    fn toggle(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(ix) {
            row.enabled = !row.enabled;
        }
        self.changed(window, cx);
    }

    fn toggle_file(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get_mut(ix) {
            row.file = !row.file;
            row.value.update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.changed(window, cx);
    }

    fn pick_file(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
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
                if let Some(row) = this.rows.get(ix) {
                    let path = path.to_string_lossy().into_owned();
                    row.value.update(cx, |s, cx| s.set_value(path, window, cx));
                    this.changed(window, cx);
                }
            });
        })
        .detach();
    }

    fn remove(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix + 1 < self.rows.len() {
            self.rows.remove(ix);
        }
        self.changed(window, cx);
    }
}

pub fn cell(t: &Tokens) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .h(px(32.))
        .px_2()
        .flex()
        .items_center()
        .border_l_1()
        .border_color(rgb(t.border))
}

impl Render for KvEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Tokens::get(cx);
        let last = self.rows.len().saturating_sub(1);
        let header = div()
            .flex()
            .border_b_1()
            .border_color(rgb(t.border))
            .text_color(rgb(t.text_secondary))
            .font_weight(FontWeight::SEMIBOLD)
            .child(div().w(px(36.)).flex_none())
            .child(cell(&t).child(self.key_label))
            .child(cell(&t).child(self.value_label))
            .child(div().w(px(32.)).flex_none());
        let rows = self.rows.iter().enumerate().map(|(ix, row)| {
            let is_blank = ix == last;
            div()
                .id(("kv-row", ix))
                .group("kv-row")
                .flex()
                .when(!is_blank, |d| d.border_b_1().border_color(rgb(t.border)))
                .hover(|s| s.bg(rgb(t.bg_secondary)))
                .child(
                    div()
                        .w(px(36.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(!is_blank, |d| {
                            d.child(
                                checkbox(("kv-check", ix), row.enabled, &t)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.toggle(ix, window, cx)
                                    })),
                            )
                        }),
                )
                .child(
                    cell(&t)
                        .child(div().flex_1().min_w_0().child(Input::new(&row.key).appearance(false).text_size(px(12.))))
                        .when(self.files, |d| {
                            d.child(
                                div()
                                    .id(("kv-type", ix))
                                    .flex_none()
                                    .px_1()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .rounded(px(4.))
                                    .cursor_pointer()
                                    .text_color(rgb(t.text_secondary))
                                    .hover(|s| s.bg(rgb(t.hover)))
                                    .child(if row.file { "File" } else { "Text" })
                                    .child(icon("icons/chevron-down.svg", 12., t.text_secondary))
                                    .on_click(cx.listener(move |this, _, window, cx| this.toggle_file(ix, window, cx))),
                            )
                        }),
                )
                .child(cell(&t).map(|d| {
                    if !(self.files && row.file) {
                        return d.child(Input::new(&row.value).appearance(false).text_size(px(12.)));
                    }
                    let path = row.value.read(cx).value().to_string();
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    d.child(
                        div()
                            .id(("kv-file", ix))
                            .px_2()
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .gap_1()
                            .min_w_0()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(rgb(t.border))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(t.hover)))
                            .text_color(rgb(if name.is_empty() { t.text_secondary } else { t.text }))
                            .child(div().truncate().child(if name.is_empty() { "Select file".to_string() } else { name }))
                            .on_click(cx.listener(move |this, _, window, cx| this.pick_file(ix, window, cx))),
                    )
                }))
                .child(
                    div()
                        .w(px(32.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(!is_blank, |d| {
                            d.child(
                                div()
                                    .id(("kv-remove", ix))
                                    .invisible()
                                    .group_hover("kv-row", |s| s.visible())
                                    .cursor_pointer()
                                    .p_1()
                                    .rounded(px(4.))
                                    .hover(|s| s.bg(rgb(t.hover)))
                                    .child(icon("icons/delete.svg", 14., t.text_secondary))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.remove(ix, window, cx)
                                    })),
                            )
                        }),
                )
        });
        div()
            .border_1()
            .border_color(rgb(t.border))
            .rounded(px(4.))
            .text_size(px(12.))
            .text_color(rgb(t.text))
            .child(header)
            .children(rows)
    }
}
