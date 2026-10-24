<p align="center"><img src="assets/floki.svg" width="112" alt="Floki logo"></p>

# Floki

Floki is a fast, native, local-first API client for Linux and Windows, built with Rust and GPUI. It sends HTTP, WebSocket, Socket.IO, MQTT and gRPC requests, keeps your collections and environments on your own machine, and can optionally sync shared workspaces through a self-hosted server. No account is required and there is no telemetry.

## Features

- **Workbench:** app header, icon rail with Collections / Environments / History, resizable sidebar, tabs with unsaved-change dots, environment selector, request header with breadcrumb and Save.
- **Request builder:** method menu, URL bar (Enter or Ctrl+Enter sends; pasting a `curl` command converts it), Params grid kept in sync with the URL, Headers grid, and Settings (SSL verification, redirects) and Docs (request description).
- **Authorization:** Basic, Bearer, Digest, OAuth 2.0 (**Get New Access Token** with the authorization code grant, with or without PKCE, via your browser, or the client-credentials or password grant), AWS Signature v4, API key in a header or query, JWT Bearer (HS, RS, PS and ES algorithms), OAuth 1.0 (HMAC-SHA1/256/512 or PLAINTEXT), Hawk, and NTLM (NTLMv2).
- **Server-sent events (SSE):** a response with `Content-Type: text/event-stream` is shown live as its events arrive, with **Stop** to end it early. **Events** lists each event with its type, timing and data (click to expand). **Assembled** joins the streamed answer of OpenAI-style (Chat Completions and Responses, also used by Mistral, Groq, DeepSeek and others), Anthropic and Gemini streams into rendered Markdown, with any thinking text, token counts and the finish reason. In the runner and `floki-cli`, a stream is read for at most two minutes.
- **Cookies:** cookies that servers set, redirects included, are kept per workspace and sent with later requests to the same site, as a browser would. **Cookies** in the Settings menu (or **Manage cookies** in a response) lists them by domain, and adds or deletes them.
- **Bodies:** form-data with text and file fields, `x-www-form-urlencoded`, raw Text / JavaScript / JSON / HTML / XML with Beautify, GraphQL query and variables, and binary files.
- **Response pane:** status, time, size, Pretty/Raw body with syntax highlighting, **Preview** for HTML pages and images (or **Open in browser**), Cookies and Headers tables, copy, and cancel while sending. **Save as example** keeps a response under its request in the sidebar.
- **WebSocket and Socket.IO:** pick them in the method menu or enter a `ws://` / `wss://` URL. Custom handshake headers and query params, a message composer with `{{variables}}` (Text/JSON/HTML/XML, or Binary as Base64/Hex), and a newest-first message log with pretty-printed messages. Socket.IO (v3/v4 servers) adds event names, acknowledgements, namespaces, a configurable handshake path, and an **Events** tab that limits the log to the events you listen to (mute any of them while connected). Filter the message log by text, or click an event tag to show only that event.
- **MQTT:** pick it in the method menu or enter an `mqtt://` / `mqtts://` URL. Connects to any MQTT 3.1.1 broker with optional username and password (Basic auth), subscribes to comma-separated topic filters, and publishes to a topic with QoS 0/1/2 and the retain flag. Received messages show their topic in the message log.
- **gRPC:** pick **gRPC** in the method menu or enter a `grpc://` (plaintext) or `grpcs://` (TLS) URL. Load services by server reflection or from a `.proto` file, fill a JSON message (**Use Example Message** generates one), send metadata, and see response metadata, every streamed message, status and trailers.
- **Collections and variables:** folders, environments, globals and per-collection variables. `{{variable}}` resolution (plus Postman's dynamic variables such as `{{$guid}}`, `{{$timestamp}}`, `{{$isoTimestamp}}`, `{{$randomInt}}` and `{{$randomEmail}}`): environment overrides collection, which overrides globals; unknown names are sent as written. History is grouped by day.
- **Scripts and tests:** each request's **Scripts** tab has a pre-request and a post-response script, and collections and folders have their own (**Edit scripts** in the sidebar menu), which run around every request inside. Results show in the response's **Tests** and **Console** tabs. Postman collection scripts (`pm.*`) run unchanged. See [Scripts](#scripts).
- **Collection runner:** **Run collection** / **Run folder** in the sidebar menu sends every request in order with its scripts. Set iterations and a delay, feed a CSV or JSON data file (one iteration per row), stop on the first failure, and filter to failed requests. Variables scripts set are saved when the run ends.
- **Command line:** `floki-cli run` runs an exported collection in CI and exits non-zero on failure. See [Command line](#command-line).
- **Import and export:** collection v2.0/v2.1 JSON, environment JSON, cURL, OpenAPI 3 / Swagger 2 (JSON or YAML), RAML 1.0 and WSDL 1.1 (SOAP 1.1). Collections export as v2.1 JSON.
- **Code snippets:** cURL, Python, JavaScript fetch, Node Axios, Go, HTTPie, PHP, Java OkHttp, C# and Rust.
- **Proxy and certificates** (Settings menu): proxy server (otherwise the system proxy), extra CA certificate, and client certificate and key (PEM). These apply to HTTP requests.
- **Workspaces:** the **Workspaces** menu switches between workspaces and creates new ones. A workspace can stay on this computer or live on a Floki server as a personal or team workspace. **Share** adds people by email as Admin, Editor or Viewer. Shared workspaces sync every few seconds; if two people edit at once, your version is kept as a "(conflicted copy)" workspace. History, theme and proxy settings stay on each computer.
- **Accounts:** sign in with email and password (verified with a 6-digit code) or with Google. To use your own server, run it (see [server/README.md](server/README.md)), then choose **Sign in**, **Use a self-hosted server** and enter its address; accounts, teams and shared workspaces then live only on that server.
- **Themes:** dark and light, with bundled Inter and IBM Plex Mono fonts.

### Keyboard shortcuts

| Keys | Action |
| --- | --- |
| Ctrl+Enter | Send |
| Ctrl+S | Save |
| Ctrl+T / Ctrl+W | New tab / close tab |
| Ctrl+Tab / Ctrl+Shift+Tab | Next / previous tab |
| Ctrl+L | Focus the URL bar |
| Ctrl+\ | Toggle the sidebar |
| Ctrl+K or Ctrl+P | Search requests, collections, folders, environments and history |
| Ctrl+O | Import |
| Esc | Close menus and dialogs |

### Not yet supported

`require()` of lodash, moment and other libraries beyond crypto-js, uuid and chai, scripting the cookie jar, `setInterval`, the OAuth 2.0 implicit grant and token refresh, Hawk payload hashes, OAuth 1.0 and Hawk in cURL export, external `$ref` files and RAML includes in imports, Digest/AWS auth and binary bodies in non-cURL snippets, Socket.IO v2 servers and binary attachments, MQTT 5 and MQTT over WebSockets, multiple messages on a gRPC client stream, and gRPC with self-signed certificates.

## Scripts

Scripts are JavaScript. Pre-request scripts run before the request is sent and can change it; post-response scripts run after the response arrives. Collection scripts run first, then folder scripts, then the request's own.

```js
// Post-response
test("status is 200", () => expect(res.status).toBe(200));
test("returns the user", () => {
  const body = res.json();
  expect(body.name).toEqual("Ada");
  expect(body).toHaveProperty("address.city");
});
env.set("token", res.json().token);

// Pre-request
req.headers.set("X-Request-Id", String(Date.now()));
vars.set("page", "2"); // used by {{page}} in this request only
```

| Name | What it is |
| --- | --- |
| `test(name, fn)` | A named test; it fails if `fn` throws. |
| `expect(value)` | Assertions: `toBe`, `toEqual`, `toBeTruthy`, `toBeFalsy`, `toBeNull`, `toBeDefined`, `toBeUndefined`, `toBeGreaterThan(OrEqual)`, `toBeLessThan(OrEqual)`, `toContain`, `toContainEqual`, `toMatch`, `toHaveLength`, `toHaveProperty`, `toBeOneOf`, `toBeType`, and `.not` before any of them. |
| `res` | The response: `status`, `statusText`, `time` (ms), `size`, `body`, `text()`, `json()`, `headers.get(name)`, `headers.has(name)`, `cookies.get(name)`, and for event streams `events` (each with `event`, `data`, `id`, `time`, `json()`) and `assembled` (the joined AI answer). |
| `req` | The request: `method`, `url`, `body`, `headers.get/set/add/remove`. Changes in a pre-request script are sent. |
| `env`, `globals`, `collection` | Variable scopes with `get`, `set`, `unset`, `has`, `clear`, `toObject`. Changes are saved. |
| `vars` | Reads a variable from every scope the way `{{name}}` does; `vars.set` lasts for this request (or runner iteration). |
| `data` | The current runner data row. |
| `info` | `requestName`, `iteration`, `iterationCount`, `eventName`. |
| `setNextRequest(name)` / `skipRequest()` | Runner flow: jump to a request by name (`null` stops the iteration), or skip sending. |
| `console.log/info/warn/error` | Output for the **Console** tab. |

Postman scripts work as they are: `pm.test`, `pm.expect` (Chai style, e.g. `pm.expect(x).to.have.property("id").that.is.a("number")`), `pm.response` (including `pm.response.to.have.status(200)` and `pm.response.to.be.ok`), `pm.request`, `pm.environment`, `pm.globals`, `pm.collectionVariables`, `pm.variables`, `pm.iterationData`, `pm.info`, `pm.execution.setNextRequest`, `pm.sendRequest` (with a callback or `await`), `pm.cookies`, and the older `tests["name"]`, `responseBody`, `responseCode` and `postman.setEnvironmentVariable`. Scripts can `await` at the top level, tests can be `async`, `setTimeout` works, and `require()` provides `crypto-js` (hashes, HMACs and encoders for request signing), `uuid` and `chai`.

## Command line

`floki-cli` runs a collection exported from Floki or Postman, for CI. It ships with the app and as a standalone download (`floki-cli-linux-x64`, `floki-cli-windows-x64.exe`) on the releases page.

```sh
floki-cli run api.postman_collection.json -e staging.postman_environment.json \
  -d users.csv --bail --reporter-json results.json
```

It prints each request and test, then exits with `0` when everything passed, `1` when a request or test failed, and `2` for bad arguments or files. Run `floki-cli --help` for every option, including `--folder`, `--iterations`, `--delay`, `--insecure`, `--export-environment` and `--export-globals`.

## Data and privacy

Data lives in `~/.local/share/Floki/` on Linux and `%LOCALAPPDATA%\Floki\` on Windows. `app.json` lists your workspaces and the signed-in account, and `workspaces/<id>.json` holds each workspace. Data from earlier versions is migrated on first start.

Environment values can contain credentials, and the workspace files are plain-text JSON, so keep them private. Floki only sends requests to the URLs you enter and has no telemetry.

## Build on Linux

Install stable Rust and the GPUI system libraries (Debian/Ubuntu):

```sh
sudo apt install build-essential pkg-config libfontconfig-dev libfreetype-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libvulkan-dev
cargo run --release
```

`scripts/install-linux.sh` builds a release binary and installs it for your user with a desktop entry and icon.

## Build on Windows

Install stable Rust and the Visual Studio 2022 C++ build tools with the Windows SDK and CMake. `dev.cmd` sets up the compiler environment and uses Visual Studio's bundled CMake when available.

```powershell
.\dev.cmd
```

## Development

Run `cargo build --locked` and `cargo test --locked` before committing (`.\dev.cmd check --locked` and `.\dev.cmd test --locked` on Windows).

Release builds come from GitHub Actions (`.github/workflows/build.yml`): run the **Build** workflow manually or push a `v*` tag. It produces `Floki-windows-x64-setup.exe` (an installer with Start menu entry and uninstaller) `Floki-linux-x64.deb` (double-click to install on Ubuntu/Debian) and `Floki-linux-x64.tar.gz` (run `./Floki/install.sh` on other distros); tagged runs attach both to a GitHub release.

## Credits

Fonts: Inter and IBM Plex Mono under the SIL Open Font License (`assets/fonts`). Icons: Lucide via gpui-kit. App icon: original artwork in `assets/floki.svg`.
