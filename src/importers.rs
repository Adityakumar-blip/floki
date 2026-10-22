use crate::model::{Auth, BodyMode, KeyValue, METHODS, RawLanguage, Request};
use roxmltree::Node;
use serde_json::{json, Map, Value};

pub struct Imported {
    pub name: String,
    pub variables: Vec<KeyValue>,
    /// Folder name "" holds top-level requests.
    pub folders: Vec<(String, Vec<Request>)>,
}

type Folders = Vec<(String, Vec<Request>)>;

const SOAP11: &str = "http://schemas.xmlsoap.org/wsdl/soap/";

/// None when `text` is not an OpenAPI/Swagger/RAML/WSDL document.
pub fn parse(text: &str) -> Option<Result<Imported, String>> {
    let t = text.trim_start();
    if t.starts_with("#%RAML 1.0") {
        return Some(raml(t));
    }
    if t.starts_with('<') {
        return match roxmltree::Document::parse(t) {
            Ok(doc) => {
                let root = doc.root_element();
                (root.tag_name().name() == "definitions" && root.tag_name().namespace().is_some_and(|n| n.contains("wsdl")))
                    .then(|| wsdl(root))
            }
            Err(e) => t.contains("wsdl").then(|| Err(format!("Invalid WSDL XML: {e}"))),
        };
    }
    let doc = match serde_json::from_str::<Value>(t).ok().or_else(|| yaml(t).ok()) {
        Some(d) => d,
        None => return (t.contains("openapi") || t.contains("swagger")).then(|| Err("Could not parse the document as JSON or YAML".into())),
    };
    if doc.get("openapi").is_some() {
        Some(oas(&doc, false))
    } else if doc.get("swagger").is_some() {
        Some(oas(&doc, true))
    } else {
        None
    }
}

fn yaml(text: &str) -> Result<Value, String> {
    // Via serde_yaml::Value because unquoted keys like `200:` are integers, which serde_json::Value rejects directly.
    let v: serde_yaml::Value = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
    serde_json::to_value(v).map_err(|e| e.to_string())
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn add_var(vars: &mut Vec<KeyValue>, key: &str, value: &str) {
    if !vars.iter().any(|v| v.key == key) {
        vars.push(KeyValue::new(key, value));
    }
}

fn push(folders: &mut Folders, folder: &str, r: Request) {
    match folders.iter_mut().find(|(n, _)| n == folder) {
        Some((_, v)) => v.push(r),
        None => folders.push((folder.into(), vec![r])),
    }
}

/// `{id}` becomes `:id`, the path-variable syntax collections use.
fn colon_params(path: &str) -> String {
    let mut out = String::new();
    let mut open = false;
    for c in path.chars() {
        match c {
            '{' => {
                open = true;
                out.push(':');
            }
            '}' if open => open = false,
            c => out.push(c),
        }
    }
    out
}

fn new_request(method: &str, name: String, path: &str, params: Vec<KeyValue>) -> Request {
    let mut r = Request { method: method.into(), name, ..Request::default() };
    r.set_url(format!("{{{{baseUrl}}}}{}", colon_params(path)));
    r.set_params(params);
    r
}

fn set_json(r: &mut Request, v: &Value) {
    r.body_mode = BodyMode::Raw;
    r.raw_language = RawLanguage::Json;
    r.body_raw = serde_json::to_string_pretty(v).unwrap_or_default();
}

fn deref<'a>(root: &'a Value, mut v: &'a Value) -> &'a Value {
    for _ in 0..8 {
        match v.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix('#')) {
            Some(p) => match root.pointer(p) {
                Some(t) => v = t,
                None => break,
            },
            None => break,
        }
    }
    v
}

fn example(root: &Value, schema: &Value, depth: u8) -> Value {
    let s = deref(root, schema);
    if depth > 5 {
        return Value::Null;
    }
    if let Some(v) = s.get("example").or_else(|| s.get("default")).or_else(|| s.get("enum").and_then(|e| e.get(0))) {
        return v.clone();
    }
    let mut obj = Map::new();
    for part in s.get("allOf").and_then(Value::as_array).into_iter().flatten() {
        if let Value::Object(o) = example(root, part, depth + 1) {
            obj.extend(o);
        }
    }
    for (k, v) in s.get("properties").and_then(Value::as_object).into_iter().flatten() {
        obj.insert(k.clone(), example(root, v, depth + 1));
    }
    let ty = match s.get("type") {
        Some(Value::String(t)) => t.as_str(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).find(|t| *t != "null").unwrap_or(""),
        _ => "",
    };
    if !obj.is_empty() || ty == "object" {
        return Value::Object(obj);
    }
    if let Some(first) = s.get("oneOf").or_else(|| s.get("anyOf")).and_then(|a| a.get(0)) {
        return example(root, first, depth + 1);
    }
    match ty {
        "string" => json!("<string>"),
        "integer" | "number" => json!(0),
        "boolean" => json!(true),
        "array" => json!([s.get("items").map_or(Value::Null, |i| example(root, i, depth + 1))]),
        _ if s.get("items").is_some() => json!([example(root, &s["items"], depth + 1)]),
        _ => Value::Null,
    }
}

fn oas(root: &Value, swagger: bool) -> Result<Imported, String> {
    let paths = root
        .get("paths")
        .and_then(Value::as_object)
        .ok_or("The API description has no valid `paths` object")?;
    let mut vars = Vec::new();
    add_var(&mut vars, "baseUrl", &oas_base(root, swagger));
    let mut folders = Folders::new();
    for (path, item) in paths {
        let item = deref(root, item);
        for method in METHODS {
            let Some(op) = item.get(method.to_lowercase()).filter(|o| o.is_object()) else { continue };
            let r = oas_request(root, swagger, method, path, item, op, &mut vars);
            let folder = op.pointer("/tags/0").and_then(Value::as_str).unwrap_or("");
            push(&mut folders, folder, r);
        }
    }
    let name = root.pointer("/info/title").and_then(Value::as_str).unwrap_or("Imported API");
    Ok(Imported { name: name.into(), variables: vars, folders })
}

fn oas_base(root: &Value, swagger: bool) -> String {
    let base = if swagger {
        let scheme = root.pointer("/schemes/0").and_then(Value::as_str).unwrap_or("https");
        let host = root.get("host").and_then(Value::as_str).map_or(String::new(), |h| format!("{scheme}://{h}"));
        format!("{host}{}", root.get("basePath").and_then(Value::as_str).unwrap_or(""))
    } else {
        let server = root.pointer("/servers/0");
        let mut url = server.and_then(|s| s.get("url")).and_then(Value::as_str).unwrap_or("").to_string();
        for (k, v) in server.and_then(|s| s.get("variables")).and_then(Value::as_object).into_iter().flatten() {
            url = url.replace(&format!("{{{k}}}"), &text(&v["default"]));
        }
        url
    };
    base.trim_end_matches('/').to_string()
}

fn param_value(p: &Value) -> String {
    let v = ["example", "default"]
        .iter()
        .find_map(|k| p.get(*k).or_else(|| p.pointer(&format!("/schema/{k}"))));
    v.map(text).unwrap_or_default()
}

fn schema_rows(root: &Value, schema: &Value) -> Vec<KeyValue> {
    let s = deref(root, schema);
    let props = s.get("properties").and_then(Value::as_object);
    props
        .into_iter()
        .flatten()
        .map(|(k, p)| {
            let p = deref(root, p);
            let file = p["format"] == "binary" || p["type"] == "file";
            let value = if file { String::new() } else { text(&example(root, p, 1)) };
            KeyValue { file, ..KeyValue::new(k, value) }
        })
        .collect()
}

fn oas_request(root: &Value, swagger: bool, method: &str, path: &str, item: &Value, op: &Value, vars: &mut Vec<KeyValue>) -> Request {
    let str_of = |k: &str| op.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let name = str_of("summary")
        .or_else(|| str_of("operationId"))
        .map_or_else(|| format!("{method} {path}"), String::from);

    let (mut query, mut headers, mut form) = (Vec::new(), Vec::new(), Vec::new());
    let mut json_body = None;
    let declared = [item.get("parameters"), op.get("parameters")];
    for p in declared.iter().flat_map(|a| a.and_then(Value::as_array)).flatten() {
        let p = deref(root, p);
        let name = p["name"].as_str().unwrap_or("");
        let row = || KeyValue { enabled: p["required"] == true, ..KeyValue::new(name, param_value(p)) };
        match p["in"].as_str().unwrap_or("") {
            "query" => query.push(row()),
            "header" => headers.push(row()),
            "formData" => form.push(KeyValue { file: p["type"] == "file", ..KeyValue::new(name, param_value(p)) }),
            "body" => json_body = Some(p.get("schema").map_or(Value::Null, |s| example(root, s, 0))),
            _ => {}
        }
    }
    let mut r = new_request(method, name, path, query);
    r.headers = headers;

    if let Some(v) = json_body {
        set_json(&mut r, &v);
    } else if !form.is_empty() {
        let consumes = op.get("consumes").or_else(|| root.get("consumes")).map(text).unwrap_or_default();
        if consumes.contains("multipart") || form.iter().any(|f| f.file) {
            r.body_mode = BodyMode::FormData;
            r.body_multipart = form;
        } else {
            r.body_mode = BodyMode::UrlEncoded;
            r.body_form = form;
        }
    } else if let Some(content) = op.get("requestBody").map(|b| deref(root, b)).and_then(|b| b.get("content")) {
        let media = |k: &str| content.get(k).map(|m| (m, m.get("schema")));
        if let Some((m, schema)) = media("application/json") {
            let ex = m.get("example").or_else(|| {
                let first = m.get("examples")?.as_object()?.values().next()?;
                deref(root, first).get("value")
            });
            let v = ex.cloned().unwrap_or_else(|| schema.map_or(Value::Null, |s| example(root, s, 0)));
            set_json(&mut r, &v);
        } else if let Some((_, Some(s))) = media("application/x-www-form-urlencoded") {
            r.body_mode = BodyMode::UrlEncoded;
            r.body_form = schema_rows(root, s);
        } else if let Some((_, Some(s))) = media("multipart/form-data") {
            r.body_mode = BodyMode::FormData;
            r.body_multipart = schema_rows(root, s);
        }
    }
    r.auth = oas_auth(root, swagger, op, vars);
    r
}

fn oas_auth(root: &Value, swagger: bool, op: &Value, vars: &mut Vec<KeyValue>) -> Auth {
    let required = op.get("security").or_else(|| root.get("security")).and_then(Value::as_array);
    let defs = root.pointer(if swagger { "/securityDefinitions" } else { "/components/securitySchemes" });
    let names = required.into_iter().flatten().filter_map(Value::as_object).flat_map(|o| o.keys());
    for name in names {
        let Some(s) = defs.and_then(|d| d.get(name)).map(|s| deref(root, s)) else { continue };
        let scheme = s["scheme"].as_str().unwrap_or("").to_lowercase();
        match (s["type"].as_str().unwrap_or(""), scheme.as_str()) {
            ("http", "bearer") => {
                add_var(vars, "bearerToken", "");
                return Auth::Bearer { token: "{{bearerToken}}".into() };
            }
            ("http", "basic") | ("basic", _) => {
                add_var(vars, "basicAuthUsername", "");
                add_var(vars, "basicAuthPassword", "");
                return Auth::Basic { username: "{{basicAuthUsername}}".into(), password: "{{basicAuthPassword}}".into() };
            }
            ("apiKey", _) => {
                add_var(vars, "apiKey", "");
                let key = s["name"].as_str().unwrap_or("").to_string();
                return Auth::ApiKey { key, value: "{{apiKey}}".into(), in_query: s["in"] == "query" };
            }
            // ponytail: oauth2/openIdConnect ignored; add when the app has an OAuth auth type.
            _ => {}
        }
    }
    Auth::None
}

// ponytail: traits, resourceTypes, !include, named types/libraries and securedBy are not resolved.
fn raml(src: &str) -> Result<Imported, String> {
    let doc = yaml(src).map_err(|e| format!("Invalid RAML: {e}"))?;
    let root = doc.as_object().ok_or("RAML document must be a YAML mapping")?;
    let version = root.get("version").map(text).unwrap_or_default();
    let base = root.get("baseUri").map(text).unwrap_or_default().replace("{version}", &version);
    let mut vars = Vec::new();
    add_var(&mut vars, "baseUrl", base.trim_end_matches('/'));
    let mut folders = Folders::new();
    for (k, v) in root.iter().filter(|(k, _)| k.starts_with('/')) {
        let folder = k.trim_start_matches('/').split('/').next().unwrap_or("");
        raml_resource(k, v, folder, &mut folders);
    }
    let name = root.get("title").map_or_else(|| "Imported RAML".into(), text);
    Ok(Imported { name, variables: vars, folders })
}

fn raml_resource(path: &str, node: &Value, folder: &str, out: &mut Folders) {
    for (k, v) in node.as_object().into_iter().flatten() {
        if k.starts_with('/') {
            raml_resource(&format!("{path}{k}"), v, folder, out);
        } else if let Some(method) = METHODS.iter().find(|m| m.eq_ignore_ascii_case(k)) {
            push(out, folder, raml_request(method, path, v));
        }
    }
}

fn raml_rows(v: Option<&Value>) -> Vec<KeyValue> {
    v.and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(k, p)| KeyValue {
            enabled: p.get("required") != Some(&json!(false)),
            ..KeyValue::new(k, p.get("example").or_else(|| p.get("default")).map(text).unwrap_or_default())
        })
        .collect()
}

fn raml_example(v: &Value) -> Value {
    if let Some(e) = v.get("example") {
        return e.clone();
    }
    if let Some(props) = v.get("properties").and_then(Value::as_object) {
        return Value::Object(props.iter().map(|(k, p)| (k.trim_end_matches('?').to_string(), raml_example(p))).collect());
    }
    let ty = v.get("type").map_or_else(|| text(v), text);
    if ty.ends_with("[]") {
        return json!([Value::Null]);
    }
    match ty.as_str() {
        "integer" | "number" => json!(0),
        "boolean" => json!(true),
        "object" => json!({}),
        _ => json!("<string>"),
    }
}

fn raml_request(method: &str, path: &str, node: &Value) -> Request {
    let name = node
        .get("displayName")
        .map_or_else(|| format!("{method} {path}"), text);
    let mut r = new_request(method, name, path, raml_rows(node.get("queryParameters")));
    r.headers = raml_rows(node.get("headers"));
    if let Some(body) = node.get("body").filter(|b| b.is_object()) {
        let m = body.get("application/json").unwrap_or(body);
        if body.get("application/json").is_some() || m.get("type").is_some() || m.get("example").is_some() {
            match m.get("example") {
                Some(Value::String(s)) => {
                    r.body_mode = BodyMode::Raw;
                    r.body_raw = s.clone();
                }
                _ => set_json(&mut r, &raml_example(m)),
            }
        }
    }
    r
}

fn kids<'a, 'i>(n: Node<'a, 'i>, name: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn local(qname: &str) -> &str {
    qname.rsplit(':').next().unwrap_or(qname)
}

// ponytail: SOAP 1.2 bindings are skipped, and bodies come from message parts rather than the XSD types; one baseUrl for all ports.
fn wsdl(root: Node) -> Result<Imported, String> {
    let tns = root.attribute("targetNamespace").unwrap_or("");
    let location = root
        .descendants()
        .find(|n| n.tag_name().name() == "address")
        .and_then(|n| n.attribute("location"))
        .unwrap_or("");
    let mut vars = Vec::new();
    add_var(&mut vars, "baseUrl", location);
    let mut folders = Folders::new();
    for binding in kids(root, "binding") {
        if !kids(binding, "binding").any(|b| b.tag_name().namespace() == Some(SOAP11)) {
            continue;
        }
        let bname = binding.attribute("name").unwrap_or("");
        let port_type = kids(root, "portType").find(|p| p.attribute("name") == binding.attribute("type").map(local));
        for op in kids(binding, "operation") {
            let oname = op.attribute("name").unwrap_or("");
            let action = kids(op, "operation").find_map(|o| o.attribute("soapAction")).unwrap_or("");
            let message = port_type
                .and_then(|pt| kids(pt, "operation").find(|o| o.attribute("name") == Some(oname)))
                .and_then(|o| kids(o, "input").next())
                .and_then(|i| i.attribute("message"))
                .and_then(|m| kids(root, "message").find(|n| n.attribute("name") == Some(local(m))));
            let parts: Vec<Node> = message.into_iter().flat_map(|m| kids(m, "part")).collect();
            let inner = match parts.iter().find_map(|p| p.attribute("element").map(|e| (p, e))) {
                Some((p, el)) => {
                    let (prefix, name) = el.split_once(':').map_or((None, el), |(a, b)| (Some(a), b));
                    let ns = p.lookup_namespace_uri(prefix).unwrap_or(tns);
                    format!("<m:{name} xmlns:m=\"{ns}\"/>")
                }
                None => {
                    let fields: String = parts
                        .iter()
                        .map(|p| p.attribute("name").unwrap_or(""))
                        .map(|n| format!("\n      <{n}>?</{n}>"))
                        .collect();
                    let tail = if fields.is_empty() { "" } else { "\n    " };
                    format!("<m:{oname} xmlns:m=\"{tns}\">{fields}{tail}</m:{oname}>")
                }
            };
            let mut r = Request { method: "POST".into(), name: oname.into(), ..Request::default() };
            r.set_url("{{baseUrl}}".into());
            r.headers = vec![KeyValue::new("Content-Type", "text/xml; charset=utf-8"), KeyValue::new("SOAPAction", action)];
            r.body_mode = BodyMode::Raw;
            r.raw_language = RawLanguage::Xml;
            r.body_raw = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<soap:Envelope xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\">\n  <soap:Body>\n    {inner}\n  </soap:Body>\n</soap:Envelope>"
            );
            push(&mut folders, bname, r);
        }
    }
    if folders.is_empty() {
        return Err("No SOAP 1.1 operations found in the WSDL".into());
    }
    let name = root
        .attribute("name")
        .or_else(|| kids(root, "service").next().and_then(|s| s.attribute("name")))
        .unwrap_or("Imported WSDL");
    Ok(Imported { name: name.into(), variables: vars, folders })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Imported {
        parse(s).expect("recognised").expect("valid")
    }

    fn var<'a>(i: &'a Imported, k: &str) -> Option<&'a str> {
        i.variables.iter().find(|v| v.key == k).map(|v| v.value.as_str())
    }

    #[test]
    fn openapi3_yaml() {
        let i = ok(r#"
openapi: 3.0.0
info: {title: Pets}
servers:
  - url: https://{host}/v1/
    variables: {host: {default: api.example.com}}
security: [{bearer: []}]
components:
  securitySchemes:
    bearer: {type: http, scheme: bearer}
  schemas:
    Pet:
      type: object
      properties:
        name: {type: string}
        tags: {type: array, items: {type: string, enum: [a, b]}}
        owner: {$ref: '#/components/schemas/Owner'}
    Owner:
      type: object
      properties:
        pet: {$ref: '#/components/schemas/Pet'}
paths:
  /pets/{id}:
    get:
      tags: [pets]
      summary: Get pet
      parameters:
        - {name: id, in: path, required: true}
        - {name: limit, in: query, schema: {type: integer, default: 10}}
        - {name: verbose, in: query, required: true}
    post:
      tags: [pets]
      operationId: addPet
      requestBody:
        content:
          application/json:
            schema: {$ref: '#/components/schemas/Pet'}
  /ping:
    get: {}
"#);
        assert_eq!(i.name, "Pets");
        assert_eq!(var(&i, "baseUrl"), Some("https://api.example.com/v1"));
        assert_eq!(var(&i, "bearerToken"), Some(""));
        let pets = &i.folders.iter().find(|(n, _)| n == "pets").unwrap().1;
        let get = &pets[0];
        assert_eq!(get.name, "Get pet");
        assert_eq!(get.url, "{{baseUrl}}/pets/:id?verbose");
        assert!(!get.params.iter().find(|p| p.key == "limit").unwrap().enabled);
        assert_eq!(get.auth, Auth::Bearer { token: "{{bearerToken}}".into() });
        let post = &pets[1];
        assert_eq!(post.name, "addPet");
        let body: Value = serde_json::from_str(&post.body_raw).unwrap();
        assert_eq!(body["tags"], json!(["a"]));
        assert_eq!(body["owner"]["pet"]["name"], "<string>");
        assert!(i.folders.iter().any(|(n, r)| n.is_empty() && r[0].name == "GET /ping"));
    }

    #[test]
    fn swagger2_json() {
        let i = ok(r##"{"swagger":"2.0","info":{"title":"S"},"host":"h.io","basePath":"/api","schemes":["http"],
          "securityDefinitions":{"k":{"type":"apiKey","name":"X-Key","in":"header"}},
          "paths":{"/u":{"post":{"security":[{"k":[]}],"parameters":[
            {"name":"b","in":"body","schema":{"$ref":"#/definitions/U"}}]},
            "put":{"parameters":[{"name":"f","in":"formData","type":"file"},{"name":"n","in":"formData","type":"string"}]}}},
          "definitions":{"U":{"type":"object","properties":{"age":{"type":"integer"},"ok":{"type":"boolean"}}}}}"##);
        assert_eq!(var(&i, "baseUrl"), Some("http://h.io/api"));
        let reqs = &i.folders[0].1;
        let post = reqs.iter().find(|r| r.method == "POST").unwrap();
        assert_eq!(post.body_raw, "{\n  \"age\": 0,\n  \"ok\": true\n}");
        assert_eq!(post.auth, Auth::ApiKey { key: "X-Key".into(), value: "{{apiKey}}".into(), in_query: false });
        let put = reqs.iter().find(|r| r.method == "PUT").unwrap();
        assert_eq!(put.body_mode, BodyMode::FormData);
        assert!(put.body_multipart[0].file || put.body_multipart[1].file);
    }

    #[test]
    fn raml_doc() {
        let i = ok("#%RAML 1.0\ntitle: Shop\nversion: v2\nbaseUri: https://x.io/{version}\n/items:\n  get:\n    queryParameters:\n      q: {required: false}\n  /{id}:\n    put:\n      body:\n        application/json:\n          example: '{\"a\":1}'\n");
        assert_eq!(i.name, "Shop");
        assert_eq!(var(&i, "baseUrl"), Some("https://x.io/v2"));
        let (folder, reqs) = &i.folders[0];
        assert_eq!(folder, "items");
        assert_eq!(reqs[0].url, "{{baseUrl}}/items");
        assert_eq!(reqs[1].url, "{{baseUrl}}/items/:id");
        assert_eq!(reqs[1].body_raw, "{\"a\":1}");
    }

    #[test]
    fn wsdl_doc() {
        let i = ok(r#"<?xml version="1.0"?>
<definitions xmlns="http://schemas.xmlsoap.org/wsdl/" xmlns:soap="http://schemas.xmlsoap.org/wsdl/soap/"
  xmlns:tns="urn:calc" name="Calc" targetNamespace="urn:calc">
  <message name="AddIn"><part name="a" type="xsd:int"/><part name="b" type="xsd:int"/></message>
  <portType name="CalcPT"><operation name="Add"><input message="tns:AddIn"/></operation></portType>
  <binding name="CalcBinding" type="tns:CalcPT"><soap:binding transport="http://schemas.xmlsoap.org/soap/http"/>
    <operation name="Add"><soap:operation soapAction="urn:Add"/></operation></binding>
  <service name="CalcService"><port name="p" binding="tns:CalcBinding"><soap:address location="http://c.io/calc"/></port></service>
</definitions>"#);
        assert_eq!(i.name, "Calc");
        assert_eq!(var(&i, "baseUrl"), Some("http://c.io/calc"));
        let r = &i.folders[0].1[0];
        assert_eq!(r.method, "POST");
        assert!(r.headers.iter().any(|h| h.key == "SOAPAction" && h.value == "urn:Add"));
        assert!(r.body_raw.contains("<m:Add xmlns:m=\"urn:calc\">") && r.body_raw.contains("<a>?</a>"));
    }

    #[test]
    fn not_a_spec() {
        assert!(parse(r#"{"info":{"name":"c"},"item":[]}"#).is_none());
        assert!(parse("curl -X POST https://x.io -d 'a=1'").is_none());
        assert!(parse("<html></html>").is_none());
        assert!(parse(r#"{"openapi":"3.0.0"}"#).unwrap().is_err());
    }
}
