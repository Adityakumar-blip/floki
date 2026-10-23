<p align="center"><img src="assets/floki.svg" width="112" alt="Floki logo"></p>

# Floki

Floki is a fast, native, local-first API client for Linux and Windows, built with Rust and GPUI. It sends HTTP, WebSocket, Socket.IO and gRPC requests, keeps your collections and environments on your own machine, and can optionally sync shared workspaces through a self-hosted server. No account is required and there is no telemetry.

## Features

- **Workbench:** app header, icon rail with Collections / Environments / History, resizable sidebar, tabs with unsaved-change dots, environment selector, request header with breadcrumb and Save.
- **Request builder:** method menu, URL bar (Enter or Ctrl+Enter sends; pasting a `curl` command converts it), Params grid kept in sync with the URL, Headers grid, and Settings (SSL verification, redirects) and Docs (request description).
- **Authorization:** Basic, Bearer, Digest, OAuth 2.0 (client-credentials or password grant with **Get New Access Token**), AWS Signature v4, and API key in a header or query.
- **Bodies:** form-data with text and file fields, `x-www-form-urlencoded`, raw Text / JavaScript / JSON / HTML / XML with Beautify, GraphQL query and variables, and binary files.
- **Response pane:** status, time, size, Pretty/Raw body with syntax highlighting, Cookies and Headers tables, copy, and cancel while sending. **Save as example** keeps a response under its request in the sidebar.
- **WebSocket and Socket.IO:** pick them in the method menu or enter a `ws://` / `wss://` URL. Custom handshake headers and query params, a message composer with `{{variables}}` (Text/JSON/HTML/XML, or Binary as Base64/Hex), and a newest-first message log with pretty-printed messages. Socket.IO (v3/v4 servers) adds event names, acknowledgements, namespaces and a configurable handshake path.
- **gRPC:** pick **gRPC** in the method menu or enter a `grpc://` (plaintext) or `grpcs://` (TLS) URL. Load services by server reflection or from a `.proto` file, fill a JSON message (**Use Example Message** generates one), send metadata, and see response metadata, every streamed message, status and trailers.
- **Collections and variables:** folders, environments, globals and per-collection variables. `{{variable}}` resolution: environment overrides collection, which overrides globals; unknown names are sent as written. History is grouped by day.
- **Import and export:** collection v2.0/v2.1 JSON, environment JSON, cURL, OpenAPI 3 / Swagger 2 (JSON or YAML), RAML 1.0 and WSDL 1.1 (SOAP 1.1). Collections export as v2.1 JSON.
- **Code snippets:** cURL, Python, JavaScript fetch, Node Axios, Go, HTTPie, PHP, Java OkHttp, C# and Rust.
- **Proxy and certificates** (Settings menu): proxy server (otherwise the system proxy), extra CA certificate, and client certificate and key (PEM). These apply to HTTP requests.
- **Workspaces:** the **Workspaces** menu switches between workspaces and creates new ones. A workspace can stay on this computer or live on a Floki server as a personal or team workspace. **Share** adds people by email as Admin, Editor or Viewer. Shared workspaces sync every few seconds; if two people edit at once, your version is kept as a "(conflicted copy)" workspace. History, theme and proxy settings stay on each computer.
- **Accounts:** sign in with email and password (verified with a 6-digit code) or with Google. See [server/README.md](server/README.md) to run the server.
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
| Ctrl+O | Import |
| Esc | Close menus and dialogs |

### Not yet supported

Pre-request scripts and tests, a collection runner, a cookie manager, response preview, NTLM / OAuth 1.0 / Hawk / JWT auth, the OAuth 2.0 authorization-code flow, external `$ref` files and RAML includes in imports, Digest/AWS auth and binary bodies in non-cURL snippets, Socket.IO v2 servers and binary attachments, multiple messages on a gRPC client stream, and gRPC with self-signed certificates.

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
