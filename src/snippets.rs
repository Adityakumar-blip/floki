use crate::model::{graphql_body, resolve, to_curl, Auth, BodyMode, Request};
use base64::Engine;

/// (label, code-editor highlight language) in menu order, cURL first.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("cURL", "bash"),
    ("Python - Requests", "python"),
    ("JavaScript - Fetch", "javascript"),
    ("NodeJs - Axios", "javascript"),
    ("Go - Native", "go"),
    ("HTTPie", "bash"),
    ("PHP - cURL", "php"),
    ("Java - OkHttp", "java"),
    ("C# - HttpClient", "csharp"),
    ("Rust - reqwest", "rust"),
];

type Pairs = Vec<(String, String)>;

enum Body {
    None,
    Raw(String),
    Form(Pairs),
    /// (key, value or file path, is_file)
    Multi(Vec<(String, String, bool)>),
}

struct Snip {
    method: String,
    url: String,
    headers: Pairs,
    body: Body,
}

/// Snippet for `LANGUAGES[index]` with `{{variables}}` resolved.
pub fn generate(index: usize, r: &Request, vars: &[(String, String)]) -> String {
    if index == 0 {
        return to_curl(r, vars);
    }
    let s = build(r, vars);
    match index {
        1 => python(&s),
        2 => fetch(&s),
        3 => axios(&s),
        4 => go(&s),
        5 => httpie(&s),
        6 => php(&s),
        7 => okhttp(&s),
        8 => csharp(&s),
        _ => rust(&s),
    }
}

fn build(r: &Request, vars: &[(String, String)]) -> Snip {
    let mut url = resolve(&r.url, vars);
    let live = |kv: &&crate::model::KeyValue| kv.enabled && !kv.key.is_empty();
    let mut headers: Pairs = r
        .headers
        .iter()
        .filter(live)
        .map(|h| (resolve(&h.key, vars), resolve(&h.value, vars)))
        .collect();
    match &r.auth {
        Auth::Bearer { token } => {
            headers.push(("Authorization".into(), format!("Bearer {}", resolve(token, vars))))
        }
        Auth::Basic { username, password } => {
            let cred = format!("{}:{}", resolve(username, vars), resolve(password, vars));
            let b64 = base64::engine::general_purpose::STANDARD.encode(cred);
            headers.push(("Authorization".into(), format!("Basic {b64}")));
        }
        Auth::ApiKey { key, value, in_query } if !key.is_empty() => {
            let (k, v) = (resolve(key, vars), resolve(value, vars));
            if *in_query {
                // ponytail: not percent-encoded, same as the sent request; encode if keys need it.
                url.push(if url.contains('?') { '&' } else { '?' });
                url.push_str(&format!("{k}={v}"));
            } else {
                headers.push((k, v));
            }
        }
        Auth::OAuth2(o) if !o.access_token.is_empty() => {
            let value = format!("{} {}", resolve(&o.header_prefix, vars), resolve(&o.access_token, vars));
            headers.push(("Authorization".into(), value.trim().into()))
        }
        // ponytail: Digest and AWS signatures are computed at send time; only the cURL snippet carries them.
        _ => {}
    }
    let pairs = |rows: &[crate::model::KeyValue]| -> Pairs {
        rows.iter()
            .filter(live)
            .map(|f| (resolve(&f.key, vars), resolve(&f.value, vars)))
            .collect()
    };
    let (body, content_type) = match r.body_mode {
        BodyMode::Raw => (Body::Raw(resolve(&r.body_raw, vars)), Some(r.raw_language.content_type())),
        BodyMode::GraphQl => (Body::Raw(graphql_body(r, vars)), Some("application/json")),
        BodyMode::UrlEncoded => (
            Body::Form(pairs(&r.body_form)),
            Some("application/x-www-form-urlencoded"),
        ),
        BodyMode::FormData => (
            Body::Multi(
                r.body_multipart
                    .iter()
                    .filter(live)
                    .map(|f| (resolve(&f.key, vars), resolve(&f.value, vars), f.file))
                    .collect(),
            ),
            None,
        ),
        // ponytail: binary file bodies only appear in the cURL snippet; add per-language file reads if asked.
        _ => (Body::None, None),
    };
    let has_ct = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    if let (Some(ct), false) = (content_type, has_ct) {
        headers.push(("Content-Type".into(), ct.into()));
    }
    Snip { method: r.method.clone(), url, headers, body }
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn q(s: &str) -> String {
    format!("\"{}\"", esc(s))
}

/// Single-quoted string for shells and PHP, where `$` and `\` stay literal.
fn sq(s: &str, shell: bool) -> String {
    if shell {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn form_string(p: &Pairs) -> String {
    p.iter()
        .map(|(k, v)| format!("{}={}", urlenc(k), urlenc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn base(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn is_ct(k: &str) -> bool {
    k.eq_ignore_ascii_case("content-type")
}

/// One formatted line per item, indented, joined by newlines.
fn lines<T>(items: impl IntoIterator<Item = T>, f: impl Fn(T) -> String) -> String {
    items.into_iter().map(f).collect::<Vec<_>>().join("\n")
}

fn python(s: &Snip) -> String {
    let mut out = format!("import requests\n\nurl = {}\n\n", q(&s.url));
    let args = match &s.body {
        Body::None => "",
        Body::Raw(b) => {
            out += &format!("payload = {}\n", q(b));
            ", data=payload"
        }
        Body::Form(p) => {
            let items = lines(p, |(k, v)| format!("    ({}, {}),", q(k), q(v)));
            out += &format!("payload = [\n{items}\n]\n");
            ", data=payload"
        }
        Body::Multi(p) => {
            let items = lines(p, |(k, v, file)| {
                if *file {
                    format!("    ({}, ({}, open({}, 'rb'), 'application/octet-stream')),", q(k), q(base(v)), q(v))
                } else {
                    format!("    ({}, (None, {})),", q(k), q(v))
                }
            });
            out += &format!("files = [\n{items}\n]\n");
            ", files=files"
        }
    };
    let hs = lines(&s.headers, |(k, v)| format!("    {}: {},", q(k), q(v)));
    out += &if hs.is_empty() { "headers = {}\n\n".to_string() } else { format!("headers = {{\n{hs}\n}}\n\n") };
    out += &format!(
        "response = requests.request({}, url, headers=headers{args})\n\nprint(response.text)\n",
        q(&s.method)
    );
    out
}

fn fetch(s: &Snip) -> String {
    let mut out = String::from("const myHeaders = new Headers();\n");
    out += &lines(&s.headers, |(k, v)| format!("myHeaders.append({}, {});", q(k), q(v)));
    out += "\n\n";
    let body = match &s.body {
        Body::None => "",
        Body::Raw(b) => {
            out += &format!("const raw = {};\n\n", q(b));
            "raw"
        }
        Body::Form(p) => {
            out += "const urlencoded = new URLSearchParams();\n";
            out += &lines(p, |(k, v)| format!("urlencoded.append({}, {});", q(k), q(v)));
            out += "\n\n";
            "urlencoded"
        }
        Body::Multi(p) => {
            out += "const formdata = new FormData();\n";
            out += &lines(p, |(k, v, file)| {
                if *file {
                    format!("formdata.append({}, fileInput.files[0], {});", q(k), q(v))
                } else {
                    format!("formdata.append({}, {});", q(k), q(v))
                }
            });
            out += "\n\n";
            "formdata"
        }
    };
    let body = if body.is_empty() { String::new() } else { format!("  body: {body},\n") };
    out += &format!(
        "const requestOptions = {{\n  method: {},\n  headers: myHeaders,\n{body}  redirect: \"follow\"\n}};\n\n",
        q(&s.method)
    );
    out += &format!(
        "fetch({}, requestOptions)\n  .then((response) => response.text())\n  .then((result) => console.log(result))\n  .catch((error) => console.error(error));\n",
        q(&s.url)
    );
    out
}

fn axios(s: &Snip) -> String {
    let mut out = String::from("const axios = require('axios');\n");
    let multi = matches!(s.body, Body::Multi(_));
    if multi {
        out += "const FormData = require('form-data');\nconst fs = require('fs');\n";
    }
    let data = match &s.body {
        Body::None => false,
        Body::Raw(b) => {
            out += &format!("let data = {};\n", q(b));
            true
        }
        Body::Form(p) => {
            out += "let data = new URLSearchParams();\n";
            out += &lines(p, |(k, v)| format!("data.append({}, {});", q(k), q(v)));
            out += "\n";
            true
        }
        Body::Multi(p) => {
            out += "let data = new FormData();\n";
            out += &lines(p, |(k, v, file)| {
                if *file {
                    format!("data.append({}, fs.createReadStream({}));", q(k), q(v))
                } else {
                    format!("data.append({}, {});", q(k), q(v))
                }
            });
            out += "\n";
            true
        }
    };
    let mut hs = lines(&s.headers, |(k, v)| format!("    {}: {},", q(k), q(v)));
    if multi {
        hs += "\n    ...data.getHeaders(),";
    }
    let hs = match hs.trim_start_matches('\n') {
        "" => String::new(),
        hs => format!("\n{hs}\n  "),
    };
    let data = if data { "\n  data: data," } else { "" };
    out += &format!(
        "\nlet config = {{\n  method: {},\n  maxBodyLength: Infinity,\n  url: {},\n  headers: {{{hs}}},{data}\n}};\n\n",
        q(&s.method.to_lowercase()),
        q(&s.url)
    );
    out += "axios.request(config)\n  .then((response) => {\n    console.log(JSON.stringify(response.data));\n  })\n  .catch((error) => {\n    console.log(error);\n  });\n";
    out
}

fn go(s: &Snip) -> String {
    let mut imports = vec!["fmt", "io", "net/http"];
    let mut ct_line = String::new();
    let mut pre = String::new();
    let payload = match &s.body {
        Body::None => "nil".to_string(),
        Body::Raw(b) => {
            imports.push("strings");
            format!("strings.NewReader({})", q(b))
        }
        Body::Form(p) => {
            imports.push("strings");
            format!("strings.NewReader({})", q(&form_string(p)))
        }
        Body::Multi(p) => {
            imports.extend(["bytes", "mime/multipart"]);
            pre += "\tpayload := &bytes.Buffer{}\n\twriter := multipart.NewWriter(payload)\n";
            for (k, v, file) in p {
                if *file {
                    imports.extend(["os", "path/filepath"]);
                    pre += &format!(
                        "\t{{\n\tfile, err := os.Open({})\n\tif err != nil {{\n\t\tfmt.Println(err)\n\t\treturn\n\t}}\n\tdefer file.Close()\n\tpart, err := writer.CreateFormFile({}, filepath.Base({}))\n\tif err != nil {{\n\t\tfmt.Println(err)\n\t\treturn\n\t}}\n\t_, _ = io.Copy(part, file)\n\t}}\n",
                        q(v), q(k), q(v)
                    );
                } else {
                    pre += &format!("\t_ = writer.WriteField({}, {})\n", q(k), q(v));
                }
            }
            pre += "\tif err := writer.Close(); err != nil {\n\t\tfmt.Println(err)\n\t\treturn\n\t}\n\n";
            ct_line = "\treq.Header.Set(\"Content-Type\", writer.FormDataContentType())\n".into();
            "payload".into()
        }
    };
    imports.sort();
    imports.dedup();
    let imports = lines(imports, |i| format!("\t\"{i}\""));
    let hs = lines(&s.headers, |(k, v)| format!("\treq.Header.Add({}, {})", q(k), q(v)));
    format!(
        "package main\n\nimport (\n{imports}\n)\n\nfunc main() {{\n\turl := {}\n\tmethod := {}\n\n{pre}\tclient := &http.Client{{}}\n\treq, reqErr := http.NewRequest(method, url, {payload})\n\tif reqErr != nil {{\n\t\tfmt.Println(reqErr)\n\t\treturn\n\t}}\n{hs}\n{ct_line}\n\tres, err := client.Do(req)\n\tif err != nil {{\n\t\tfmt.Println(err)\n\t\treturn\n\t}}\n\tdefer res.Body.Close()\n\n\tbody, err := io.ReadAll(res.Body)\n\tif err != nil {{\n\t\tfmt.Println(err)\n\t\treturn\n\t}}\n\tfmt.Println(string(body))\n}}\n",
        q(&s.url),
        q(&s.method)
    )
}

fn httpie(s: &Snip) -> String {
    let (flag, prefix) = match &s.body {
        Body::Form(_) => (" --form", String::new()),
        Body::Multi(_) => (" --multipart", String::new()),
        Body::Raw(b) => ("", format!("printf '%s' {} | ", sq(b, true))),
        Body::None => ("", String::new()),
    };
    let mut parts = vec![format!("{prefix}http --follow{flag} {} {}", s.method, sq(&s.url, true))];
    // ponytail: HTTPie special chars (`=`, `:`, `@`, `\`) in keys are not backslash-escaped.
    parts.extend(s.headers.iter().map(|(k, v)| sq(&format!("{k}:{v}"), true)));
    match &s.body {
        Body::Form(p) => parts.extend(p.iter().map(|(k, v)| sq(&format!("{k}={v}"), true))),
        Body::Multi(p) => parts.extend(p.iter().map(|(k, v, file)| {
            sq(&format!("{k}{}{v}", if *file { "@" } else { "=" }), true)
        })),
        _ => {}
    }
    parts.join(" \\\n  ")
}

fn php(s: &Snip) -> String {
    let fields = match &s.body {
        Body::None => String::new(),
        Body::Raw(b) => format!("  CURLOPT_POSTFIELDS => {},\n", sq(b, false)),
        Body::Form(p) => format!("  CURLOPT_POSTFIELDS => {},\n", sq(&form_string(p), false)),
        Body::Multi(p) => {
            let items = p
                .iter()
                .map(|(k, v, file)| {
                    let val = if *file { format!("new CURLFILE({})", sq(v, false)) } else { sq(v, false) };
                    format!("{} => {val}", sq(k, false))
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("  CURLOPT_POSTFIELDS => array({items}),\n")
        }
    };
    let hs = s
        .headers
        .iter()
        .map(|(k, v)| sq(&format!("{k}: {v}"), false))
        .collect::<Vec<_>>()
        .join(",\n    ");
    format!(
        "<?php\n\n$curl = curl_init();\n\ncurl_setopt_array($curl, array(\n  CURLOPT_URL => {},\n  CURLOPT_RETURNTRANSFER => true,\n  CURLOPT_ENCODING => '',\n  CURLOPT_MAXREDIRS => 10,\n  CURLOPT_TIMEOUT => 0,\n  CURLOPT_FOLLOWLOCATION => true,\n  CURLOPT_HTTP_VERSION => CURL_HTTP_VERSION_1_1,\n  CURLOPT_CUSTOMREQUEST => {},\n{fields}  CURLOPT_HTTPHEADER => array(\n    {hs}\n  ),\n));\n\n$response = curl_exec($curl);\n\ncurl_close($curl);\necho $response;\n",
        sq(&s.url, false),
        sq(&s.method, false)
    )
}

fn okhttp(s: &Snip) -> String {
    let ct = s
        .headers
        .iter()
        .find(|(k, _)| is_ct(k))
        .map(|(_, v)| v.as_str())
        .unwrap_or("text/plain");
    let mut out = String::from("OkHttpClient client = new OkHttpClient().newBuilder().build();\n");
    let body = match &s.body {
        Body::None => {
            if matches!(s.method.as_str(), "GET" | "HEAD") {
                "null"
            } else {
                "RequestBody.create(null, new byte[0])"
            }
        }
        Body::Raw(b) => {
            out += &format!("MediaType mediaType = MediaType.parse({});\nRequestBody body = RequestBody.create(mediaType, {});\n", q(ct), q(b));
            "body"
        }
        Body::Form(p) => {
            out += &format!("MediaType mediaType = MediaType.parse({});\nRequestBody body = RequestBody.create(mediaType, {});\n", q(ct), q(&form_string(p)));
            "body"
        }
        Body::Multi(p) => {
            out += "RequestBody body = new MultipartBody.Builder().setType(MultipartBody.FORM)\n";
            out += &lines(p, |(k, v, file)| {
                if *file {
                    format!("  .addFormDataPart({}, {}, RequestBody.create(MediaType.parse(\"application/octet-stream\"), new File({})))", q(k), q(base(v)), q(v))
                } else {
                    format!("  .addFormDataPart({}, {})", q(k), q(v))
                }
            });
            out += "\n  .build();\n";
            "body"
        }
    };
    let hs = lines(&s.headers, |(k, v)| format!("  .addHeader({}, {})", q(k), q(v)));
    out += &format!(
        "Request request = new Request.Builder()\n  .url({})\n  .method({}, {body})\n{hs}\n  .build();\nResponse response = client.newCall(request).execute();\n",
        q(&s.url),
        q(&s.method)
    );
    out
}

fn csharp(s: &Snip) -> String {
    let mut out = format!(
        "var client = new HttpClient();\nvar request = new HttpRequestMessage(new HttpMethod({}), {});\n",
        q(&s.method),
        q(&s.url)
    );
    // Content-Type belongs to the content object; HttpClient throws if it is added as a request header.
    let ct = s.headers.iter().find(|(k, _)| is_ct(k)).map(|(_, v)| v.as_str()).unwrap_or("text/plain");
    for (k, v) in s.headers.iter().filter(|(k, _)| !is_ct(k)) {
        out += &format!("request.Headers.Add({}, {});\n", q(k), q(v));
    }
    match &s.body {
        Body::None => {}
        Body::Raw(b) => out += &format!("var content = new StringContent({}, null, {});\n", q(b), q(ct)),
        Body::Form(p) => {
            let items = lines(p, |(k, v)| format!("  new KeyValuePair<string, string>({}, {}),", q(k), q(v)));
            out += &format!("var collection = new List<KeyValuePair<string, string>>\n{{\n{items}\n}};\nvar content = new FormUrlEncodedContent(collection);\n");
        }
        Body::Multi(p) => {
            out += "var content = new MultipartFormDataContent();\n";
            out += &lines(p, |(k, v, file)| {
                if *file {
                    format!("content.Add(new StreamContent(File.OpenRead({})), {}, {});", q(v), q(k), q(base(v)))
                } else {
                    format!("content.Add(new StringContent({}), {});", q(v), q(k))
                }
            });
            out += "\n";
        }
    }
    if !matches!(s.body, Body::None) {
        out += "request.Content = content;\n";
    }
    out += "var response = await client.SendAsync(request);\nresponse.EnsureSuccessStatusCode();\nConsole.WriteLine(await response.Content.ReadAsStringAsync());\n";
    out
}

fn rust(s: &Snip) -> String {
    let std_method = matches!(s.method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS");
    let method = if std_method {
        format!("reqwest::Method::{}", s.method)
    } else {
        format!("reqwest::Method::from_bytes(b{})?", q(&s.method))
    };
    // .form()/.multipart() set their own Content-Type.
    let own_ct = matches!(s.body, Body::Form(_) | Body::Multi(_));
    let mut pre = String::new();
    let body = match &s.body {
        Body::None => String::new(),
        Body::Raw(b) => format!("\n        .body({})", q(b)),
        Body::Form(p) => {
            let items = p.iter().map(|(k, v)| format!("({}, {})", q(k), q(v))).collect::<Vec<_>>().join(", ");
            format!("\n        .form(&[{items}])")
        }
        Body::Multi(p) => {
            pre += "    let form = reqwest::blocking::multipart::Form::new()";
            for (k, v, file) in p {
                pre += &if *file {
                    format!("\n        .file({}, {})?", q(k), q(v))
                } else {
                    format!("\n        .text({}, {})", q(k), q(v))
                };
            }
            pre += ";\n";
            "\n        .multipart(form)".into()
        }
    };
    let hs: String = s
        .headers
        .iter()
        .filter(|(k, _)| !(own_ct && is_ct(k)))
        .map(|(k, v)| format!("\n        .header({}, {})", q(k), q(v)))
        .collect();
    format!(
        "fn main() -> Result<(), Box<dyn std::error::Error>> {{\n    let client = reqwest::blocking::Client::new();\n{pre}    let response = client\n        .request({method}, {}){hs}{body}\n        .send()?;\n    println!(\"{{}}\", response.text()?);\n    Ok(())\n}}\n",
        q(&s.url)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::KeyValue;

    fn json_post() -> Request {
        Request {
            method: "POST".into(),
            url: "{{host}}/items".into(),
            headers: vec![KeyValue::new("X-Id", "7")],
            auth: Auth::Bearer { token: "tok".into() },
            body_mode: BodyMode::Raw,
            body_raw: "{\"a\": \"b\"}\nline2".into(),
            ..Request::default()
        }
    }

    fn vars() -> Vec<(String, String)> {
        vec![("host".into(), "https://h.test".into())]
    }

    #[test]
    fn json_post_across_languages() {
        let r = json_post();
        let escaped = "\"{\\\"a\\\": \\\"b\\\"}\\nline2\"";
        let py = generate(1, &r, &vars());
        assert!(py.contains("url = \"https://h.test/items\""));
        assert!(py.contains("\"Authorization\": \"Bearer tok\""));
        assert!(py.contains("\"X-Id\": \"7\""));
        assert!(py.contains(&format!("payload = {escaped}")));
        let js = generate(2, &r, &vars());
        assert!(js.contains("myHeaders.append(\"Authorization\", \"Bearer tok\");"));
        assert!(js.contains(&format!("const raw = {escaped};")));
        let go = generate(4, &r, &vars());
        assert!(go.contains(&format!("strings.NewReader({escaped})")));
        assert!(go.contains("req.Header.Add(\"Content-Type\", \"application/json\")"));
    }

    #[test]
    fn multipart_file_in_python_and_axios() {
        let mut file = KeyValue::new("doc", "/tmp/a.txt");
        file.file = true;
        let r = Request {
            method: "POST".into(),
            url: "https://h.test".into(),
            body_mode: BodyMode::FormData,
            body_multipart: vec![KeyValue::new("n", "v"), file],
            ..Request::default()
        };
        let py = generate(1, &r, &[]);
        assert!(py.contains("(\"n\", (None, \"v\"))"));
        assert!(py.contains("open(\"/tmp/a.txt\", 'rb')"));
        assert!(!py.contains("Content-Type"));
        let ax = generate(3, &r, &[]);
        assert!(ax.contains("data.append(\"doc\", fs.createReadStream(\"/tmp/a.txt\"));"));
        assert!(ax.contains("...data.getHeaders()"));
    }

    #[test]
    fn every_language_renders_a_get() {
        let r = Request { url: "https://h.test/x".into(), ..Request::default() };
        for (i, (name, _)) in LANGUAGES.iter().enumerate() {
            let out = generate(i, &r, &[]);
            assert!(out.contains("https://h.test/x"), "{name}");
        }
    }
}
