//! Pre-request and post-response scripts. The API lives in prelude.js; Rust only passes JSON in and out.

use crate::http::{self, CookieJar, Response};
use crate::model::{KeyValue, Request, Scope, Scopes, Scripts, Settings, VarWrite};
use boa_engine::{js_string, Context, JsResult, JsValue, NativeFunction, Source};
use serde::Deserialize;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::sync::Arc;

const PRELUDE: &str = include_str!("prelude.js");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    PreRequest,
    Test,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct TestResult {
    pub name: String,
    pub passed: bool,
    #[serde(default)]
    pub skipped: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct LogLine {
    pub level: String,
    pub text: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawOutcome {
    tests: Vec<TestResult>,
    logs: Vec<LogLine>,
    writes: Vec<(String, String, Option<String>)>,
    errors: Vec<String>,
    /// Absent: unchanged; `null`: stop the run; a string: jump to that request.
    #[serde(default, deserialize_with = "some")]
    next: Option<Option<String>>,
    skip: bool,
    request: Option<RequestEdit>,
}

fn some<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

#[derive(Debug, Deserialize)]
struct RequestEdit {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// What a script run produced, across every script in the chain.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    pub tests: Vec<TestResult>,
    pub logs: Vec<LogLine>,
    pub writes: Vec<VarWrite>,
    /// Exceptions and syntax errors, prefixed with the script that raised them.
    pub errors: Vec<String>,
    pub next: Option<Option<String>>,
    pub skip: bool,
}

impl Outcome {
    fn absorb(&mut self, other: Outcome) {
        self.tests.extend(other.tests);
        self.logs.extend(other.logs);
        self.writes.extend(other.writes);
        self.errors.extend(other.errors);
        self.next = other.next.or(self.next.take());
        self.skip |= other.skip;
    }
}

/// Runner position, exposed to scripts as `info.iteration`.
#[derive(Clone, Copy, Debug)]
pub struct Iteration {
    pub index: usize,
    pub count: usize,
}

impl Default for Iteration {
    fn default() -> Self {
        Self { index: 0, count: 1 }
    }
}

thread_local! {
    /// Settings and cookie jar for `pm.sendRequest` while `send` runs scripts on this thread.
    static NETWORK: RefCell<Option<(Settings, Arc<CookieJar>)>> = const { RefCell::new(None) };
}

fn arg(args: &[JsValue], i: usize, cx: &mut Context) -> JsResult<String> {
    Ok(args.get(i).cloned().unwrap_or_default().to_string(cx)?.to_std_string_escaped())
}

fn response_json(r: &Response) -> Value {
    json!({
        "status": r.status, "reason": r.reason, "headers": r.headers, "body": r.text(),
        "time": r.elapsed_ms as u64, "size": r.size,
        "cookies": r.cookies.iter().map(|c| json!({"name": c.name, "value": c.value, "domain": c.domain, "path": c.path})).collect::<Vec<_>>(),
        "events": r.events.iter().map(|e| json!({"event": e.event, "data": e.data, "id": e.id, "time": e.at_ms as u64})).collect::<Vec<_>>(),
        "assembled": http::assemble(&r.events).map(|a| a.text),
    })
}

/// `__native_send(json)`: the blocking HTTP call behind pm.sendRequest.
fn native_send(_: &JsValue, args: &[JsValue], cx: &mut Context) -> JsResult<JsValue> {
    #[derive(Deserialize)]
    struct Spec {
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: String,
    }
    let out = match serde_json::from_str::<Spec>(&arg(args, 0, cx)?) {
        Err(e) => json!({ "error": format!("Invalid request: {e}") }),
        Ok(spec) => {
            let mut request = Request::default();
            request.method = spec.method;
            request.url = spec.url;
            request.headers = spec.headers.into_iter().map(|(k, v)| KeyValue::new(k, v)).collect();
            if !spec.body.is_empty() {
                request.body_mode = crate::model::BodyMode::Raw;
                request.raw_language = crate::model::RawLanguage::Text;
                request.body_raw = spec.body;
            }
            let network = NETWORK.with(|n| n.borrow().clone());
            match network {
                None => json!({ "error": "pm.sendRequest is only available while sending a request" }),
                Some((settings, jar)) => match http::execute_with(&request, &[], &settings, Some(jar)) {
                    Ok(r) => json!({ "response": response_json(&r) }),
                    Err(e) => json!({ "error": e }),
                },
            }
        }
    };
    Ok(JsValue::from(js_string!(out.to_string())))
}

/// `__native_crypto(op, algorithm, data_hex, key_hex)`: hashes and HMACs for CryptoJS, returned as hex.
fn native_crypto(_: &JsValue, args: &[JsValue], cx: &mut Context) -> JsResult<JsValue> {
    use hmac::{Hmac, Mac};
    use sha2::Digest;
    let [op, alg, data, key] = [0, 1, 2, 3].map(|i| arg(args, i, cx));
    let (op, alg, data, key) = (op?, alg?, hex::decode(data?).unwrap_or_default(), hex::decode(key?).unwrap_or_default());
    macro_rules! digest {
        ($hash:ty) => {
            if op == "hmac" {
                let mut mac = <Hmac<$hash>>::new_from_slice(&key).expect("HMAC takes any key length");
                mac.update(&data);
                mac.finalize().into_bytes().to_vec()
            } else {
                <$hash>::digest(&data).to_vec()
            }
        };
    }
    let bytes = match alg.as_str() {
        "MD5" => digest!(md5::Md5),
        "SHA1" => digest!(sha1::Sha1),
        "SHA224" => digest!(sha2::Sha224),
        "SHA256" => digest!(sha2::Sha256),
        "SHA384" => digest!(sha2::Sha384),
        "SHA512" => digest!(sha2::Sha512),
        other => return Err(boa_engine::JsNativeError::typ().with_message(format!("Unknown hash {other}")).into()),
    };
    Ok(JsValue::from(js_string!(hex::encode(bytes))))
}

fn object(pairs: &[(String, String)]) -> Value {
    Value::Object(pairs.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect())
}

/// Runs `scripts` (label, source) in order in one JavaScript context. `request` is edited in place by pre-request scripts.
pub fn run(
    phase: Phase,
    scripts: &[(String, &str)],
    request: &mut Request,
    response: Option<&Response>,
    scopes: &Scopes,
    iteration: Iteration,
) -> Outcome {
    let headers: Vec<(String, String)> =
        request.headers.iter().filter(|h| h.enabled && !h.key.is_empty()).map(|h| (h.key.clone(), h.value.clone())).collect();
    let ctx = json!({
        "phase": if phase == Phase::PreRequest { "prerequest" } else { "test" },
        "request": { "name": request.name, "method": request.method, "url": request.url, "headers": headers, "body": request.body_raw },
        "response": response.map(response_json),
        "globals": object(&scopes.globals),
        "collection": object(&scopes.collection),
        "environment": object(scopes.environment.as_deref().unwrap_or_default()),
        "hasEnvironment": scopes.environment.is_some(),
        "data": object(&scopes.data),
        "local": object(&scopes.local),
        "iteration": iteration.index,
        "iterationCount": iteration.count,
    });
    let mut js = Context::default();
    // A runaway loop or recursion in a script must not freeze the app.
    js.runtime_limits_mut().set_loop_iteration_limit(10_000_000);
    js.runtime_limits_mut().set_recursion_limit(2_000);
    let natives = [("__native_send", native_send as boa_engine::native_function::NativeFunctionPointer), ("__native_crypto", native_crypto)];
    for (name, f) in natives {
        js.register_global_builtin_callable(js_string!(name), 1, NativeFunction::from_fn_ptr(f)).expect("fresh context accepts globals");
    }
    if let Err(e) = js.eval(Source::from_bytes(format!("var __ctx = {ctx};").as_bytes())).and_then(|_| js.eval(Source::from_bytes(PRELUDE))) {
        return Outcome { errors: vec![format!("Script runtime failed to start: {e}")], ..Outcome::default() };
    }
    for (label, source) in scripts.iter().filter(|(_, s)| !s.trim().is_empty()) {
        let call = format!("__run({}, {});", Value::String(label.clone()), Value::String(source.to_string()));
        // Each script finishes, promises and timers included, before the next one starts.
        let finished = js.eval(Source::from_bytes(call.as_bytes())).and_then(|_| {
            for _ in 0..10_000 {
                js.run_jobs()?;
                if !js.eval(Source::from_bytes(b"__nextTimer()"))?.to_boolean() {
                    break;
                }
            }
            js.run_jobs()
        });
        if let Err(e) = finished {
            // __run catches script errors itself; this is a limit (loop/recursion) being hit.
            return Outcome { errors: vec![format!("{label}: {e}")], ..Outcome::default() };
        }
    }
    let raw: RawOutcome = match js.eval(Source::from_bytes(b"__finish()")).map(|v| v.as_string().map(|s| s.to_std_string_escaped())) {
        Ok(Some(text)) => serde_json::from_str(&text).unwrap_or_else(|e| RawOutcome {
            errors: vec![format!("Could not read script results: {e}")],
            ..RawOutcome::default()
        }),
        Ok(None) => RawOutcome::default(),
        Err(e) => RawOutcome { errors: vec![e.to_string()], ..RawOutcome::default() },
    };
    if let Some(edit) = raw.request {
        apply_edit(request, edit);
    }
    Outcome {
        tests: raw.tests,
        logs: raw.logs,
        writes: raw
            .writes
            .into_iter()
            .filter_map(|(scope, key, value)| {
                let scope = match scope.as_str() {
                    "globals" => Scope::Globals,
                    "collection" => Scope::Collection,
                    "environment" => Scope::Environment,
                    "local" => Scope::Local,
                    _ => return None,
                };
                Some(VarWrite { scope, key, value })
            })
            .collect(),
        errors: raw.errors,
        next: raw.next,
        skip: raw.skip,
    }
}

fn apply_edit(request: &mut Request, edit: RequestEdit) {
    request.method = edit.method;
    if edit.url != request.url {
        request.url = edit.url;
        request.params = crate::model::params_from_url(&request.url, &[]);
    }
    let current: Vec<(String, String)> =
        request.headers.iter().filter(|h| h.enabled && !h.key.is_empty()).map(|h| (h.key.clone(), h.value.clone())).collect();
    if edit.headers != current {
        request.headers = edit.headers.into_iter().map(|(k, v)| crate::model::KeyValue::new(k, v)).collect();
    }
    request.body_raw = edit.body;
}

/// One request as a run sees it: the request actually sent and everything its scripts did.
#[derive(Debug)]
pub struct Sent {
    pub request: Request,
    /// `None` when a pre-request script called `skipRequest()`.
    pub response: Option<Result<Response, String>>,
    pub pre: Outcome,
    pub post: Outcome,
    /// Scopes after both phases, for the next request of a run.
    pub scopes: Scopes,
}

impl Sent {
    pub fn tests(&self) -> impl Iterator<Item = &TestResult> {
        self.pre.tests.iter().chain(&self.post.tests)
    }

    pub fn writes(&self) -> impl Iterator<Item = &VarWrite> {
        self.pre.writes.iter().chain(&self.post.writes)
    }

    pub fn failed(&self) -> bool {
        !matches!(self.response, Some(Ok(_)) | None) || self.tests().any(|t| !t.passed) || !self.pre.errors.is_empty() || !self.post.errors.is_empty()
    }
}

/// Runs pre-request scripts, sends, then runs post-response scripts. `chain` holds collection and folder scripts, outermost first.
/// `jar` keeps cookies across the requests of a tab or run, `pm.sendRequest` included.
pub fn send(
    request: &Request,
    chain: &[(String, Scripts)],
    mut scopes: Scopes,
    settings: &Settings,
    iteration: Iteration,
    jar: &Arc<CookieJar>,
    on_event: http::OnEvent,
) -> Sent {
    NETWORK.with(|n| *n.borrow_mut() = Some((settings.clone(), jar.clone())));
    let sent = send_inner(request, chain, &mut scopes, settings, iteration, jar, on_event);
    NETWORK.with(|n| *n.borrow_mut() = None);
    Sent { scopes, ..sent }
}

fn send_inner(
    request: &Request,
    chain: &[(String, Scripts)],
    scopes: &mut Scopes,
    settings: &Settings,
    iteration: Iteration,
    jar: &Arc<CookieJar>,
    on_event: http::OnEvent,
) -> Sent {
    let mut request = request.clone();
    let own = request.scripts.clone();
    let pre_scripts: Vec<(String, &str)> = chain
        .iter()
        .map(|(label, s)| (format!("{label} pre-request"), s.pre_request.as_str()))
        .chain([("Pre-request".to_string(), own.pre_request.as_str())])
        .collect();
    let mut pre = Outcome::default();
    if pre_scripts.iter().any(|(_, s)| !s.trim().is_empty()) {
        pre.absorb(run(Phase::PreRequest, &pre_scripts, &mut request, None, scopes, iteration));
        scopes.apply(&pre.writes);
    }
    if pre.skip {
        return Sent { request, response: None, pre, post: Outcome::default(), scopes: Scopes::default() };
    }
    let response = http::execute_stream(&request, &scopes.merged(), settings, Some(jar.clone()), on_event);
    let mut post = Outcome::default();
    if let Ok(r) = &response {
        let test_scripts: Vec<(String, &str)> = chain
            .iter()
            .map(|(label, s)| (format!("{label} post-response"), s.test.as_str()))
            .chain([("Post-response".to_string(), own.test.as_str())])
            .collect();
        if test_scripts.iter().any(|(_, s)| !s.trim().is_empty()) {
            post.absorb(run(Phase::Test, &test_scripts, &mut request.clone(), Some(r), scopes, iteration));
            scopes.apply(&post.writes);
        }
    }
    Sent { request, response: Some(response), pre, post, scopes: Scopes::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, body: &str) -> Response {
        Response {
            status,
            reason: "OK".into(),
            elapsed_ms: 42,
            size: body.len(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            cookies: Vec::new(),
            content_type: "application/json".into(),
            body: body.as_bytes().to_vec(),
            truncated: false,
            events: Vec::new(),
        }
    }

    fn post(script: &str, r: &Response, scopes: &Scopes) -> Outcome {
        run(Phase::Test, &[("Post-response".into(), script)], &mut Request::default(), Some(r), scopes, Iteration::default())
    }

    fn results(o: &Outcome) -> Vec<(&str, bool)> {
        o.tests.iter().map(|t| (t.name.as_str(), t.passed)).collect()
    }

    #[test]
    fn floki_api_tests_and_variables() {
        let r = response(201, r#"{"id": 7, "user": {"name": "Ada"}, "tags": ["a", "b"]}"#);
        let scopes = Scopes { environment: Some(vec![("base".into(), "x".into())]), ..Scopes::default() };
        let o = post(
            r#"
            test("created", () => expect(res.status).toBe(201));
            test("body", () => {
                const body = res.json();
                expect(body.user.name).toEqual("Ada");
                expect(body).toHaveProperty("user.name", "Ada");
                expect(body.tags).toContain("b");
                expect(res.headers.get("content-type")).toMatch(/json/);
            });
            test("fails", () => expect(res.status).not.toBe(201));
            env.set("id", res.json().id);
            globals.unset("old");
            console.log("seen", { id: 7 });
            "#,
            &r,
            &scopes,
        );
        assert_eq!(results(&o), [("created", true), ("body", true), ("fails", false)]);
        assert_eq!(o.tests[2].error.as_deref(), Some("expected 201 not to be 201"));
        assert_eq!(o.writes[0], VarWrite { scope: Scope::Environment, key: "id".into(), value: Some("7".into()) });
        assert_eq!(o.writes[1], VarWrite { scope: Scope::Globals, key: "old".into(), value: None });
        assert_eq!(o.logs[0].text, r#"seen {"id":7}"#);
        assert!(o.errors.is_empty(), "{:?}", o.errors);
    }

    #[test]
    fn postman_scripts_run_unchanged() {
        let r = response(200, r#"{"token": "abc", "items": [1, 2, 3], "ok": true}"#);
        let scopes = Scopes { collection: vec![("limit".into(), "5".into())], ..Scopes::default() };
        let o = post(
            r#"
            pm.test("Status code is 200", function () { pm.response.to.have.status(200); });
            pm.test("ok", () => pm.response.to.be.ok);
            pm.test("header", () => pm.response.to.have.header("Content-Type"));
            pm.test("chai", function () {
                const json = pm.response.json();
                pm.expect(json.items).to.have.lengthOf(3);
                pm.expect(json.items).to.include(2);
                pm.expect(json).to.have.property("token").that.is.a("string");
                pm.expect(json.ok).to.be.true;
                pm.expect(json).to.deep.equal({ token: "abc", items: [1, 2, 3], ok: true });
                pm.expect(pm.response.responseTime).to.be.below(1000);
                pm.expect(pm.collectionVariables.get("limit")).to.eql("5");
            });
            pm.test("negated", () => pm.expect(1).to.not.equal(1));
            pm.environment.set("token", pm.response.json().token);
            tests["legacy"] = responseCode.code === 200;
            postman.setNextRequest("Login");
            "#,
            &r,
            &scopes,
        );
        assert_eq!(
            results(&o),
            [("Status code is 200", true), ("ok", true), ("header", true), ("chai", true), ("negated", false), ("legacy", true)]
        );
        assert_eq!(o.next, Some(Some("Login".into())));
        assert!(o.logs.iter().any(|l| l.level == "warn"), "env.set without an environment warns");
    }

    #[test]
    fn pre_request_edits_the_request() {
        let mut request = Request::default();
        request.url = "https://api.test/{{path}}".into();
        request.headers.push(crate::model::KeyValue::new("X-Old", "1"));
        let o = run(
            Phase::PreRequest,
            &[(
                "Pre-request".into(),
                r#"
                pm.request.headers.upsert({ key: "X-Sig", value: "s1" });
                req.headers.remove("x-old");
                vars.set("path", "users");
                req.body = JSON.stringify({ at: 1 });
                "#,
            )],
            &mut request,
            None,
            &Scopes::default(),
            Iteration::default(),
        );
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(request.headers.len(), 1);
        assert_eq!((request.headers[0].key.as_str(), request.headers[0].value.as_str()), ("X-Sig", "s1"));
        assert_eq!(request.body_raw, r#"{"at":1}"#);
        let mut scopes = Scopes::default();
        scopes.apply(&o.writes);
        assert_eq!(crate::model::resolve(&request.url, &scopes.merged()), "https://api.test/users");
    }

    #[test]
    fn send_runs_the_chain_around_a_real_request() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0_u8; 4096];
            let n = s.read(&mut buf).unwrap();
            let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();
            assert!(head.starts_with("get /items/42 "), "{head}");
            assert!(head.contains("x-trace: from-collection"), "{head}");
            let body = r#"{"next": "abc"}"#;
            write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        });
        let mut request = Request::default();
        request.url = format!("http://127.0.0.1:{port}/items/{{{{id}}}}");
        request.scripts.test = r#"test("ok", () => expect(res.status).toBe(200)); collection.set("cursor", res.json().next);"#.into();
        let chain = vec![(
            "Collection \"Demo\"".to_string(),
            Scripts { pre_request: r#"vars.set("id", "42"); req.headers.set("X-Trace", "from-collection");"#.into(), test: String::new() },
        )];
        let sent = send(&request, &chain, Scopes::default(), &Settings::default(), Iteration::default(), &CookieJar::new(Vec::new()), &mut |_| true);
        server.join().unwrap();
        assert!(matches!(sent.response, Some(Ok(ref r)) if r.status == 200));
        assert_eq!(sent.tests().map(|t| t.passed).collect::<Vec<_>>(), [true]);
        assert!(!sent.failed());
        assert!(sent.scopes.collection.contains(&("cursor".into(), "abc".into())));
    }

    #[test]
    fn crypto_js_async_tests_and_timers() {
        let r = response(200, "{}");
        let o = post(
            r#"
            const C = require("crypto-js");
            test("md5", () => expect(C.MD5("").toString()).toBe("d41d8cd98f00b204e9800998ecf8427e"));
            test("sha1", () => expect(CryptoJS.SHA1("abc").toString()).toBe("a9993e364706816aba3e25717850c26c9cd0d89d"));
            test("hmac", () => expect(C.HmacSHA256("The quick brown fox jumps over the lazy dog", "key").toString())
                .toBe("f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"));
            test("base64", () => {
                expect(C.enc.Base64.stringify(C.enc.Utf8.parse("héllo"))).toBe("aMOpbGxv");
                expect(C.enc.Base64.parse("aMOpbGxv").toString(C.enc.Utf8)).toBe("héllo");
                expect(C.HmacSHA1("a", "b").toString(C.enc.Base64).length).toBe(28);
            });
            test("uuid", () => expect(require("uuid").v4()).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4/));
            test("async passes", async () => { await null; expect(1).toBe(1); });
            test("async fails", async () => { await Promise.resolve(); expect(1).toBe(2); });
            test("never settles", () => new Promise(() => {}));
            const order = [];
            setTimeout(() => order.push("late"), 50);
            setTimeout(() => { order.push("early"); setTimeout(() => order.push("nested"), 100); }, 10);
            await new Promise((done) => setTimeout(done, 500));
            test("timers ran in order", () => expect(order).toEqual(["early", "late", "nested"]));
            "#,
            &r,
            &Scopes::default(),
        );
        assert!(o.errors.is_empty(), "{:?}", o.errors);
        assert_eq!(
            results(&o),
            [
                ("md5", true),
                ("sha1", true),
                ("hmac", true),
                ("base64", true),
                ("uuid", true),
                ("async passes", true),
                ("async fails", false),
                ("never settles", false),
                ("timers ran in order", true)
            ],
            "{:?}",
            o.tests
        );
        assert_eq!(o.tests[6].error.as_deref(), Some("expected 1 to be 2"));
        assert_eq!(o.tests[7].error.as_deref(), Some("Test did not finish"));
    }

    #[test]
    fn send_request_from_a_script_shares_the_cookie_jar() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for reply in [
                "HTTP/1.1 200 OK\r\nSet-Cookie: sid=s1; Path=/\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"token\": \"t9\"}",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            ] {
                let (mut s, _) = listener.accept().unwrap();
                let mut buf = [0_u8; 4096];
                let n = s.read(&mut buf).unwrap();
                seen.push(String::from_utf8_lossy(&buf[..n]).to_lowercase());
                s.write_all(reply.as_bytes()).unwrap();
            }
            seen
        });
        let mut request = Request::default();
        request.url = format!("http://127.0.0.1:{port}/main");
        request.scripts.pre_request = format!(
            r#"
            const login = await pm.sendRequest({{ url: "http://127.0.0.1:{port}/login", method: "POST",
                header: {{ "Content-Type": "application/json" }}, body: {{ mode: "raw", raw: JSON.stringify({{ u: 1 }}) }} }});
            vars.set("token", login.json().token);
            pm.sendRequest("http://127.0.0.1:1/unreachable", (err, res) => {{ if (err) console.log("callback error seen"); }});
            req.headers.set("Authorization", "Bearer {{{{token}}}}");
            "#
        );
        let jar = CookieJar::new(Vec::new());
        let sent = send(&request, &[], Scopes::default(), &Settings::default(), Iteration::default(), &jar, &mut |_| true);
        let seen = server.join().unwrap();
        assert!(sent.pre.errors.is_empty(), "{:?}", sent.pre.errors);
        assert!(seen[0].starts_with("post /login"), "{}", seen[0]);
        assert!(seen[0].ends_with(r#"{"u":1}"#), "{}", seen[0]);
        assert!(seen[1].contains("authorization: bearer t9"), "{}", seen[1]);
        assert!(seen[1].contains("cookie: sid=s1"), "the main request reuses the cookie from pm.sendRequest: {}", seen[1]);
        assert_eq!(sent.pre.logs[0].text, "callback error seen");
        assert_eq!(jar.cookies().len(), 1);
    }

    #[test]
    fn scripts_see_stream_events_and_the_assembled_answer() {
        let mut r = response(200, "");
        r.events = vec![
            http::SseEvent { event: "message".into(), data: r#"{"choices":[{"delta":{"content":"4"}}]}"#.into(), ..Default::default() },
            http::SseEvent { event: "message".into(), data: "[DONE]".into(), ..Default::default() },
        ];
        let o = post(
            r#"
            test("events", () => {
                expect(res.events).toHaveLength(2);
                expect(res.events[0].json().choices[0].delta.content).toBe("4");
                expect(res.events.at(-1).data).toBe("[DONE]");
            });
            test("assembled", () => expect(res.assembled).toBe("4"));
            "#,
            &r,
            &Scopes::default(),
        );
        assert_eq!(results(&o), [("events", true), ("assembled", true)], "{:?}", o.tests);
    }

    #[test]
    fn errors_are_reported_per_script_and_loops_are_stopped() {
        let r = response(200, "not json");
        let o = run(
            Phase::Test,
            &[
                ("Collection post-response".into(), "this is not javascript"),
                ("Post-response".into(), r#"test("json", () => res.json()); require("lodash");"#),
            ],
            &mut Request::default(),
            Some(&r),
            &Scopes::default(),
            Iteration::default(),
        );
        assert_eq!(o.errors.len(), 2, "{:?}", o.errors);
        assert!(o.errors[0].starts_with("Collection post-response: "));
        assert!(o.errors[1].contains("lodash"));
        assert_eq!(o.tests[0].error.as_deref(), Some("Response body is not valid JSON"));

        let o = post("while (true) {}", &r, &Scopes::default());
        assert_eq!(o.errors.len(), 1, "an endless loop stops with an error");
    }
}
