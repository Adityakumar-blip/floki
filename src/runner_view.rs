//! The Runner tab: options, a live list of results, and a summary.

use crate::runner::{self, Event, Options, StepResult, Summary};
use crate::store::Store;
use crate::theme::{Tokens, FONT, MONO};
use crate::ui::*;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::*;
use gpui_kit::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct RunnerView {
    store: Entity<Store>,
    pub target: u64,
    iterations: Entity<InputState>,
    delay: Entity<InputState>,
    data_file: Option<PathBuf>,
    data: Vec<Vec<(String, String)>>,
    data_error: Option<String>,
    bail: bool,
    running: Option<Arc<AtomicBool>>,
    results: Vec<StepResult>,
    open: Vec<bool>,
    summary: Option<Summary>,
    only_failed: bool,
}

impl RunnerView {
    pub fn new(store: Entity<Store>, target: u64, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            store,
            target,
            iterations: cx.new(|cx| InputState::new(window, cx).default_value("1")),
            delay: cx.new(|cx| InputState::new(window, cx).default_value("0")),
            data_file: None,
            data: Vec::new(),
            data_error: None,
            bail: false,
            running: None,
            results: Vec::new(),
            open: Vec::new(),
            summary: None,
            only_failed: false,
        }
    }

    pub fn title(&self, cx: &App) -> String {
        let ws = &self.store.read(cx).ws;
        format!("Run {}", ws.scripts_of(self.target).map(|(n, _)| n).unwrap_or_default())
    }

    fn choose_data(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Use data file".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let parsed = std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| runner::parse_data(&t));
            let _ = this.update(cx, |this, cx| {
                match parsed {
                    Ok(rows) => {
                        this.data = rows;
                        this.data_error = None;
                        this.data_file = Some(path);
                    }
                    Err(e) => this.data_error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let number = |input: &Entity<InputState>, cx: &App| input.read(cx).value().trim().parse::<u64>().ok();
        let options = Options {
            // An empty or 0 count means once per data row.
            iterations: number(&self.iterations, cx).unwrap_or(0) as usize,
            delay_ms: number(&self.delay, cx).unwrap_or(0),
            data: self.data.clone(),
            bail: self.bail,
        };
        let ws = self.store.read(cx).ws.clone();
        let target = self.target;
        let cancel = Arc::new(AtomicBool::new(false));
        self.running = Some(cancel.clone());
        self.results.clear();
        self.open.clear();
        self.summary = None;
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        std::thread::spawn(move || runner::run(&ws, target, &options, &cancel, |e| drop(tx.unbounded_send(e))));
        cx.spawn_in(window, async move |this, cx| {
            use futures::StreamExt;
            while let Some(event) = rx.next().await {
                let alive = this.update(cx, |this, cx| {
                    match event {
                        Event::Step(step) => {
                            this.results.push(step);
                            this.open.push(false);
                        }
                        Event::Done { summary, writes, cookies } => {
                            this.running = None;
                            this.summary = Some(summary);
                            this.store.update(cx, |s, cx| s.update(cx, |ws| ws.cookies = cookies));
                            if !writes.is_empty() {
                                // The first request decides which collection's variables are written.
                                let ws = &this.store.read(cx).ws;
                                let first = runner::requests_in(ws, target).first().map(|(id, _)| *id);
                                this.store.update(cx, |s, cx| s.update(cx, |ws| ws.apply_writes(first, &writes)));
                            }
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = &self.running {
            cancel.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }

    fn render_options(&self, t: &Tokens, cx: &mut Context<Self>) -> Div {
        let field = |label: &'static str, input: &Entity<InputState>| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().text_color(rgb(t.text_secondary)).child(label))
                .child(
                    div()
                        .w(px(72.))
                        .h(px(28.))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(rgb(t.border))
                        .child(Input::new(input).appearance(false).text_size(px(12.))),
                )
        };
        let data_label = match (&self.data_file, &self.data_error) {
            (_, Some(e)) => div().text_color(rgb(t.error)).child(e.clone()),
            (Some(path), None) => div().text_color(rgb(t.text_secondary)).child(format!(
                "{} ({} rows)",
                path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                self.data.len()
            )),
            (None, None) => div().text_color(rgb(t.text_tertiary)).child("No data file"),
        };
        let action = if self.running.is_some() {
            outline_button("run-stop", "Stop", t).on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
        } else {
            primary_button("run-start", "Run", t).on_click(cx.listener(|this, _, window, cx| this.start(window, cx)))
        };
        div()
            .px_4()
            .py_3()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_4()
            .border_b_1()
            .border_color(rgb(t.border))
            .child(field("Iterations", &self.iterations))
            .child(field("Delay (ms)", &self.delay))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        outline_button("run-data", "Data file…", t)
                            .on_click(cx.listener(|this, _, window, cx| this.choose_data(window, cx))),
                    )
                    .child(data_label)
                    .when(self.data_file.is_some(), |d| {
                        d.child(ghost_button("run-data-clear", t).child("Clear").on_click(cx.listener(|this, _, _, cx| {
                            this.data_file = None;
                            this.data.clear();
                            cx.notify();
                        })))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(checkbox("run-bail", self.bail, t).on_click(cx.listener(|this, _, _, cx| {
                        this.bail = !this.bail;
                        cx.notify();
                    })))
                    .child("Stop on first failure"),
            )
            .child(div().flex_1())
            .child(action)
    }

    fn render_summary(&self, t: &Tokens, cx: &mut Context<Self>) -> Option<Div> {
        if self.results.is_empty() && self.summary.is_none() {
            return None;
        }
        let failed_requests = self.results.iter().filter(|r| r.failed()).count();
        let tests: usize = self.results.iter().map(|r| r.tests.len()).sum();
        let failed_tests: usize = self.results.iter().map(|r| r.tests.iter().filter(|t| !t.passed).count()).sum();
        let state = match &self.summary {
            None => ("Running…".to_string(), t.warning),
            Some(s) if s.cancelled => (format!("Stopped after {:.1} s", s.elapsed.as_secs_f32()), t.warning),
            Some(s) => (format!("Finished in {:.1} s", s.elapsed.as_secs_f32()), if failed_requests > 0 { t.error } else { t.success }),
        };
        let toggle = |id: &'static str, label: &'static str, on: bool, value: bool, cx: &mut Context<Self>| {
            section_tab(id, label, on, t).on_click(cx.listener(move |this, _, _, cx| {
                this.only_failed = value;
                cx.notify();
            }))
        };
        Some(
            div()
                .h(px(40.))
                .px_4()
                .flex()
                .items_center()
                .gap_4()
                .border_b_1()
                .border_color(rgb(t.border))
                .child(toggle("run-all", "All", !self.only_failed, false, cx))
                .child(toggle("run-failed", "Failed", self.only_failed, true, cx))
                .child(div().flex_1())
                .child(div().text_color(rgb(t.text_secondary)).child(format!("{} requests", self.results.len())))
                .child(div().text_color(rgb(t.success)).child(format!("{} tests passed", tests - failed_tests)))
                .child(div().text_color(rgb(if failed_tests > 0 { t.error } else { t.text_tertiary })).child(format!("{failed_tests} failed")))
                .child(div().text_color(rgb(state.1)).child(state.0)),
        )
    }

    fn render_row(&self, ix: usize, r: &StepResult, t: &Tokens, cx: &mut Context<Self>) -> Div {
        let (path, tint) = match () {
            _ if r.skipped.is_some() => ("icons/info.svg", t.text_tertiary),
            _ if r.failed() => ("icons/circle-alert.svg", t.error),
            _ => ("icons/check.svg", t.success),
        };
        let status = match (r.status, &r.error, &r.skipped) {
            (Some(code), _, _) => div().text_color(rgb(t.status(code))).child(format!("{code} {}", r.reason)),
            (_, Some(_), _) => div().text_color(rgb(t.error)).child("Error"),
            (_, _, Some(reason)) => div().text_color(rgb(t.text_tertiary)).child(reason.clone()),
            _ => div(),
        };
        let passed = r.tests.iter().filter(|t| t.passed).count();
        let open = self.open.get(ix).copied().unwrap_or(false);
        let detail = open.then(|| {
            let line = |text: String, color: u32| div().font_family(MONO).text_color(rgb(color)).child(text);
            div()
                .pl_10()
                .pr_4()
                .pb_2()
                .flex()
                .flex_col()
                .gap_1()
                .child(line(r.url.clone(), t.text_secondary))
                .children(r.error.iter().map(|e| line(e.clone(), t.error)))
                .children(r.script_errors.iter().map(|e| line(e.clone(), t.error)))
                .children(r.tests.iter().map(|test| {
                    let mark = if test.passed { "✓" } else { "✗" };
                    let text = match &test.error {
                        Some(e) if !test.passed => format!("{mark} {} — {e}", test.name),
                        _ => format!("{mark} {}", test.name),
                    };
                    line(text, if test.passed { t.success } else { t.error })
                }))
                .children(r.logs.iter().map(|l| line(l.text.clone(), t.text_tertiary)))
        });
        div()
            .border_b_1()
            .border_color(rgb(t.border))
            .child(
                div()
                    .id(("run-row", ix))
                    .h(px(32.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .cursor_pointer()
                    .hover(|d| d.bg(rgb(t.hover)))
                    .child(icon(path, 14., tint))
                    .child(div().w(px(28.)).text_color(rgb(t.text_tertiary)).child(format!("#{}", r.iteration + 1)))
                    .child(div().w(px(64.)).text_color(rgb(t.method(&r.method))).font_weight(FontWeight::SEMIBOLD).child(r.method.clone()))
                    .child(div().flex_1().min_w_0().truncate().child(r.name.clone()))
                    .when(!r.tests.is_empty(), |d| {
                        d.child(div().text_color(rgb(if passed == r.tests.len() { t.success } else { t.error })).child(format!(
                            "{passed}/{} tests",
                            r.tests.len()
                        )))
                    })
                    .child(status)
                    .child(div().w(px(64.)).text_color(rgb(t.text_tertiary)).child(if r.status.is_some() { format!("{} ms", r.time_ms) } else { String::new() }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(o) = this.open.get_mut(ix) {
                            *o = !*o;
                        }
                        cx.notify();
                    })),
            )
            .children(detail)
    }
}

impl Render for RunnerView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Tokens::get(cx);
        let count = runner::requests_in(&self.store.read(cx).ws, self.target).len();
        let rows: Vec<_> = self
            .results
            .iter()
            .enumerate()
            .filter(|(_, r)| !self.only_failed || r.failed())
            .map(|(ix, r)| self.render_row(ix, r, &t, cx))
            .collect();
        let empty = rows.is_empty().then(|| {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(rgb(t.text_secondary))
                .child(icon("icons/send.svg", 40., t.text_tertiary))
                .child(match (self.summary.is_some(), count) {
                    (true, _) => "No failed requests".to_string(),
                    (_, 0) => "This has no requests to run".to_string(),
                    (_, n) => format!("Click Run to send {n} request{} with their scripts", if n == 1 { "" } else { "s" }),
                })
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(t.bg))
            .font_family(FONT)
            .text_size(px(12.))
            .text_color(rgb(t.text))
            .child(
                div()
                    .h(px(48.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(t.border))
                    .child(icon("icons/send.svg", 16., t.text_secondary))
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(14.)).child(self.title(cx))),
            )
            .child(self.render_options(&t, cx))
            .children(self.render_summary(&t, cx))
            .children(empty)
            .when(!rows.is_empty(), |d| d.child(div().id("run-results").flex_1().min_h_0().overflow_y_scroll().children(rows)))
    }
}
