//! Collection runner: sends every request in a collection or folder with its scripts, in order.
//! Shared by the Runner tab and `floki run` on the command line.

use crate::http::CookieJar;
use crate::model::{Item, Request, Scope, Scopes, StoredCookie, VarWrite, Workspace};
use crate::script::{self, Iteration, LogLine, TestResult};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// 0 runs once per data row, or once without data.
    pub iterations: usize,
    pub delay_ms: u64,
    /// One map of variables per iteration, from a CSV or JSON data file.
    pub data: Vec<Vec<(String, String)>>,
    /// Stop the whole run at the first failed request or test.
    pub bail: bool,
}

impl Options {
    pub fn iteration_count(&self) -> usize {
        match self.iterations {
            0 => self.data.len().max(1),
            n => n,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct StepResult {
    pub iteration: usize,
    pub name: String,
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    pub reason: String,
    pub time_ms: u128,
    pub size: usize,
    /// The request could not be sent (bad URL, network error).
    pub error: Option<String>,
    pub tests: Vec<TestResult>,
    pub script_errors: Vec<String>,
    pub logs: Vec<LogLine>,
    /// Not an HTTP request, or skipped by `skipRequest()`.
    pub skipped: Option<String>,
}

impl StepResult {
    pub fn failed(&self) -> bool {
        self.error.is_some() || !self.script_errors.is_empty() || self.tests.iter().any(|t| !t.passed)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub iterations: usize,
    pub requests: usize,
    pub failed_requests: usize,
    pub tests: usize,
    pub failed_tests: usize,
    pub elapsed: Duration,
    pub cancelled: bool,
}

pub enum Event {
    Step(StepResult),
    /// `writes` are the variable changes to keep (environment, collection, globals); `cookies` is the jar afterwards.
    Done { summary: Summary, writes: Vec<VarWrite>, cookies: Vec<StoredCookie> },
}

/// Requests under a collection or folder, depth first, as (id, request).
pub fn requests_in(ws: &Workspace, target: u64) -> Vec<(u64, Request)> {
    fn flatten(items: &[Item], out: &mut Vec<(u64, Request)>) {
        for item in items {
            match item {
                Item::Request(r) => out.push((r.id, r.request.clone())),
                Item::Folder(f) => flatten(&f.items, out),
            }
        }
    }
    fn find(items: &[Item], target: u64) -> Option<&[Item]> {
        items.iter().find_map(|item| match item {
            Item::Folder(f) if f.id == target => Some(f.items.as_slice()),
            Item::Folder(f) => find(&f.items, target),
            Item::Request(_) => None,
        })
    }
    let items = ws
        .collections
        .iter()
        .find(|c| c.id == target)
        .map(|c| c.items.as_slice())
        .or_else(|| ws.collections.iter().find_map(|c| find(&c.items, target)));
    let mut out = Vec::new();
    flatten(items.unwrap_or_default(), &mut out);
    out
}

/// Runs the requests under `target`, reporting each one through `on`. Blocking; run it on its own thread.
pub fn run(ws: &Workspace, target: u64, options: &Options, cancel: &AtomicBool, mut on: impl FnMut(Event)) {
    let started = Instant::now();
    let steps = requests_in(ws, target);
    let mut summary = Summary { iterations: options.iteration_count(), ..Summary::default() };
    let mut writes = Vec::new();
    let mut scopes = ws.scopes(steps.first().map(|(id, _)| *id));
    let jar = CookieJar::new(ws.cookies.clone());
    // A script that keeps jumping back must not run forever.
    let max_steps = steps.len().saturating_mul(100).max(1000);
    'run: for iteration in 0..summary.iterations {
        scopes.local.clear();
        scopes.data = options.data.get(iteration % options.data.len().max(1)).cloned().unwrap_or_default();
        let mut ix = 0;
        let mut taken = 0;
        while ix < steps.len() && taken < max_steps {
            if cancel.load(Ordering::Relaxed) {
                summary.cancelled = true;
                break 'run;
            }
            if taken > 0 && options.delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(options.delay_ms));
            }
            taken += 1;
            let (id, request) = &steps[ix];
            let (result, next) = step(ws, *id, request, &mut scopes, &mut writes, &jar, Iteration { index: iteration, count: summary.iterations });
            summary.requests += 1;
            summary.tests += result.tests.len();
            summary.failed_tests += result.tests.iter().filter(|t| !t.passed).count();
            let failed = result.failed();
            summary.failed_requests += usize::from(failed);
            on(Event::Step(result));
            if failed && options.bail {
                break 'run;
            }
            ix = match next {
                None => ix + 1,
                Some(None) => break,
                Some(Some(name)) => match steps.iter().position(|(_, r)| r.name == name) {
                    Some(target) => target,
                    None => {
                        on(Event::Step(StepResult {
                            iteration,
                            name: format!("setNextRequest(\"{name}\")"),
                            error: Some(format!("No request named \"{name}\" in this run")),
                            ..StepResult::default()
                        }));
                        summary.failed_requests += 1;
                        break;
                    }
                },
            };
        }
    }
    summary.elapsed = started.elapsed();
    on(Event::Done { summary, writes, cookies: jar.cookies() });
}

fn step(
    ws: &Workspace,
    id: u64,
    request: &Request,
    scopes: &mut Scopes,
    writes: &mut Vec<VarWrite>,
    jar: &Arc<CookieJar>,
    iteration: Iteration,
) -> (StepResult, Option<Option<String>>) {
    let mut result = StepResult {
        iteration: iteration.index,
        name: request.title(),
        method: crate::model::method_label(&request.method).to_string(),
        url: crate::model::resolve(&request.url, &scopes.merged()),
        ..StepResult::default()
    };
    if request.is_socket() || request.is_grpc() {
        result.skipped = Some("Only HTTP requests run in the runner".into());
        return (result, None);
    }
    let sent = script::send(request, &ws.script_chain(id), scopes.clone(), &ws.settings, iteration, jar, &mut stream_deadline());
    *scopes = sent.scopes.clone();
    writes.extend(sent.writes().filter(|w| w.scope != Scope::Local).cloned());
    result.url = crate::model::resolve(&sent.request.url, &scopes.merged());
    result.tests = sent.tests().cloned().collect();
    result.script_errors = sent.pre.errors.iter().chain(&sent.post.errors).cloned().collect();
    result.logs = sent.pre.logs.iter().chain(&sent.post.logs).cloned().collect();
    match &sent.response {
        None => result.skipped = Some("Skipped by skipRequest()".into()),
        Some(Ok(r)) => {
            result.status = Some(r.status);
            result.reason = r.reason.clone();
            result.time_ms = r.elapsed_ms;
            result.size = r.size;
        }
        Some(Err(e)) => result.error = Some(e.clone()),
    }
    let next = sent.post.next.clone().or(sent.pre.next.clone());
    (result, next)
}

/// Endless event streams (live feeds) must not stall a run, so each one is read for at most this long.
pub const STREAM_LIMIT: Duration = Duration::from_secs(120);

fn stream_deadline() -> impl FnMut(&crate::http::SseEvent) -> bool {
    let started = Instant::now();
    move |_| started.elapsed() < STREAM_LIMIT
}

/// Parses a runner data file: a JSON array of objects, or CSV with a header row.
pub fn parse_data(text: &str) -> Result<Vec<Vec<(String, String)>>, String> {
    let text = text.trim_start_matches('\u{feff}');
    if text.trim_start().starts_with('[') {
        let rows: Vec<serde_json::Map<String, Value>> =
            serde_json::from_str(text).map_err(|e| format!("Data file is not a JSON array of objects: {e}"))?;
        return Ok(rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|(k, v)| (k, if let Value::String(s) = v { s } else { v.to_string() }))
                    .collect()
            })
            .collect());
    }
    let mut records = csv_records(text).into_iter();
    let header = records.next().ok_or("Data file is empty")?;
    Ok(records
        .filter(|r| !(r.len() == 1 && r[0].is_empty()))
        .map(|r| header.iter().cloned().zip(r.into_iter().chain(std::iter::repeat(String::new()))).collect())
        .collect())
}

/// RFC 4180 CSV: quoted fields may hold commas, newlines and doubled quotes.
fn csv_records(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let (mut record, mut field) = (Vec::new(), String::new());
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            ('"', true) => quoted = false,
            ('"', false) if field.is_empty() => quoted = true,
            (',', false) => record.push(std::mem::take(&mut field)),
            ('\r', false) => {}
            ('\n', false) => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
            }
            (c, _) => field.push(c),
        }
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Folder, SavedRequest};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn parses_csv_and_json_data() {
        let rows = parse_data("user,note\r\nada,\"hi, there\"\nbob,\"say \"\"x\"\"\"\n").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], [("user".into(), "ada".into()), ("note".into(), "hi, there".into())]);
        assert_eq!(rows[1][1].1, "say \"x\"");
        let rows = parse_data(r#"[{"id": 1, "name": "a"}, {"id": 2}]"#).unwrap();
        assert_eq!(rows[1], [("id".to_string(), "2".to_string())]);
        assert!(parse_data("").is_err());
    }

    /// Echo server: answers each request with its path as JSON.
    fn serve(count: usize) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut paths = Vec::new();
            for _ in 0..count {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 4096];
                let n = s.read(&mut buf).unwrap();
                let path = String::from_utf8_lossy(&buf[..n]).split(' ').nth(1).unwrap_or_default().to_string();
                let body = format!(r#"{{"path": "{path}"}}"#);
                write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
                paths.push(path);
            }
            paths
        });
        (port, handle)
    }

    fn request(name: &str, url: String, test: &str) -> Item {
        let mut r = Request::default();
        r.name = name.into();
        r.url = url;
        r.scripts.test = test.into();
        Item::Request(SavedRequest { id: 0, request: r })
    }

    #[test]
    fn runs_iterations_with_data_variables_and_next_request() {
        let (port, server) = serve(4);
        let base = format!("http://127.0.0.1:{port}");
        let mut ws = Workspace::default();
        let c = ws.add_collection("Demo");
        let mut items = vec![
            request("Login", format!("{base}/login/{{{{user}}}}"), r#"env.set("seen", data.get("user")); test("ok", () => expect(res.status).toBe(200));"#),
            request("Skip me", format!("{base}/never"), ""),
            request("Profile", format!("{base}/profile"), r#"test("path", () => expect(res.json().path).toBe("/profile")); test("fails on purpose", () => expect(1).toBe(2));"#),
        ];
        // Login jumps straight to Profile.
        if let Item::Request(r) = &mut items[0] {
            r.request.scripts.pre_request = r#"pm.execution.setNextRequest("Profile");"#.into();
        }
        for item in &mut items {
            if let Item::Request(r) = item {
                r.id = ws.alloc_id();
            }
        }
        let folder = ws.alloc_id();
        ws.collections[0].items.push(Item::Folder(Folder { id: folder, name: "API".into(), items, expanded: true, scripts: Default::default() }));
        ws.environments.push(crate::model::Environment { id: 99, name: "Dev".into(), values: vec![] });
        ws.active_environment = Some(99);

        let options = Options { data: parse_data("user\nada\nbob\n").unwrap(), ..Options::default() };
        let mut steps = Vec::new();
        let mut done = None;
        run(&ws, c, &options, &AtomicBool::new(false), |e| match e {
            Event::Step(s) => steps.push(s),
            Event::Done { summary, writes, .. } => done = Some((summary, writes)),
        });
        let (summary, writes) = done.unwrap();
        // 2 iterations x (Login, Profile); "Skip me" is jumped over.
        assert_eq!(server.join().unwrap(), ["/login/ada", "/profile", "/login/bob", "/profile"]);
        assert_eq!(steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Login", "Profile", "Login", "Profile"]);
        assert_eq!(steps[2].url, format!("{base}/login/bob"));
        assert_eq!((summary.requests, summary.failed_requests, summary.tests, summary.failed_tests), (4, 2, 6, 2));
        assert_eq!(writes.last().unwrap().value.as_deref(), Some("bob"));
        ws.apply_writes(None, &writes);
        assert_eq!(ws.environments[0].values[0].value, "bob");
    }
}
