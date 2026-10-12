# browser-control-mcp

**Browser Control MCP** lets an AI agent control **and debug** real browsers
at runtime: breakpoints, pause on load, call frames, evaluation in a paused
frame, stepping and script search on **Chrome (CDP)** and **Firefox
(WebDriver BiDi + Firefox RDP)**, plus navigation, DOM, input, screenshots,
console and network capture.

It is the successor of
[chrome-debug-mcp](https://github.com/raultov/chrome-debug-mcp) (it starts
from a copy of its code) and complements Playwright MCP or
chrome-devtools-mcp with runtime JavaScript debugging on both browsers.

> **Status: BETA (0.1.0).** This version contains known bugs and differences
> between Chrome and Firefox (see [Limitations and TODO](#limitations-and-todo))
> and parts that have not been verified (see [Untested](#untested)). Tool
> replies may change in incompatible ways before 1.0. Please report problems
> in the [issues](https://github.com/raultov/browser-control-mcp/issues).

## Supported browsers

| Browser | Protocol | Validated with |
| --- | --- | --- |
| Chrome / Chromium | Chrome DevTools Protocol | Google Chrome 155.0.8059.39 (Linux) |
| Firefox | WebDriver BiDi, plus the Firefox Remote Debugging Protocol (RDP) for the JavaScript debugger | Firefox 157.0.1 (Linux, snap) |

The default browser is Chrome. Every instance runs one browser for its whole
life: start the server with `--browser firefox`, or pass
`"browser": "firefox"` to `open_instance`. If Chrome is not installed but Firefox
is, the launch error says so and tells the agent to ask you before switching.

## Feature matrix

✓ supported, ✗ not available (the tool answers a capability error naming the
tool, the browser and the reason; nothing is sent to the browser), partial:
see the note.

| Tool | Chrome | Firefox | Notes |
| --- | --- | --- | --- |
| `open_instance` | ✓ | partial | `features`, `copy_cookies` and `source_profile` are Chrome only |
| `close_instance` | ✓ | ✓ | |
| `list_instances` | ✓ | ✓ | each instance reports its `browser` and `capabilities` |
| `restart_browser` | ✓ | ✓ | was `restart_chrome` |
| `stop_browser` | ✓ | ✓ | was `stop_chrome`; on an attached Firefox it ends the session, the browser keeps running |
| `open_tab` | ✓ | ✓ | |
| `close_tab` | ✓ | ✓ | |
| `list_tabs` | ✓ | ✓ | |
| `switch_tab` | ✓ | ✓ | |
| `navigate` | ✓ | ✓ | |
| `reload` | ✓ | ✓ | |
| `scroll` | ✓ | ✓ | |
| `inspect_dom` | ✓ | ✓ | |
| `capture_screenshot` | ✓ | ✓ | |
| `click_element` | ✓ | ✓ | |
| `fill_input` | ✓ | ✓ | |
| `evaluate_js` | ✓ | ✓ | |
| `get_console_logs` | ✓ | ✓ | |
| `get_network_logs` | ✓ | partial | Firefox: HTTP requests and response bodies, no WebSocket frames |
| `get_custom_events` | ✓ | partial | Firefox: events of the modules subscribed with `send_bidi_command` |
| `get_performance_metrics` | ✓ | ✗ | Firefox has no equivalent of CDP's Performance domain |
| `profile_page_performance` | ✓ | ✗ | Firefox has no equivalent of CDP's Tracing domain |
| `set_breakpoint` | ✓ | partial | Firefox: by URL only (no `script_id` / `script_hash`) |
| `remove_breakpoint` | ✓ | ✓ | |
| `resume` | ✓ | ✓ | |
| `step_over` | ✓ | ✓ | |
| `evaluate_on_call_frame` | ✓ | ✓ | |
| `search_scripts` | ✓ | ✓ | |
| `pause_on_load` | ✓ | ✗ | TODO on Firefox, see [Limitations and TODO](#limitations-and-todo) |
| `enable_proxy_auth` | ✓ | partial | needs `--proxy-server`; untested on Firefox against a real proxy |
| `send_cdp_command` | ✓ | ✗ | raw Chrome DevTools Protocol |
| `send_bidi_command` | ✗ | ✓ | raw WebDriver BiDi; refuses `session.new`, `session.end`, `browser.close` |
| `send_rdp_request` | ✗ | ✓ | raw Firefox RDP request to an actor (`thread`, `console`, `root` shortcuts) |
| `webmcp_list_tools` | ✓ | ✗ | needs `--enable-webmcp`; WebMCP is a Chrome testing feature |
| `webmcp_invoke_tool` | ✓ | ✗ | needs `--enable-webmcp` |
| `webmcp_get_invocation` | ✓ | ✗ | needs `--enable-webmcp` |
| `webmcp_list_invocations` | ✓ | ✗ | needs `--enable-webmcp` |

The JavaScript debugger tools on Firefox need its debugger (RDP) port: on by
default for a launched Firefox, `--firefox-rdp-port` for an attached one.

## 📦 Installation

Every method installs the **same pre-compiled binaries** published on each
[GitHub Release](https://github.com/raultov/browser-control-mcp/releases); a Rust toolchain is only needed for the Cargo and
source routes. The npm package is a small wrapper whose `postinstall` fetches
the pre-compiled binary for your platform from GitHub Releases: no compiling,
no Rust toolchain. Install Chrome and/or Firefox yourself: the server launches
them, it does not ship them (except the Docker image).

> ⚠️ **Always use the scoped npm name `@raultov/browser-control-mcp`.** The
> unscoped `browser-control-mcp` on npm is a different, unrelated project.

Prefer zero effort? The one-prompt route below lets your AI agent do all of
it. Otherwise, methods are grouped by platform and ordered by popularity.

### 🤖 One-Prompt Install (any platform)

Paste this prompt into any AI coding agent's chat (Claude Code, opencode,
Cursor, Codex CLI, Copilot, ...) and let it install the server and register
it with every MCP-capable client it detects on your machine:

```text
Install and set up the browser-control-mcp MCP server on this machine. Do not launch Chrome or Firefox yourself: the server launches and manages them.

1. Install the server binary with the first method that works on this OS:
   - Homebrew available (macOS/Linux): brew install raultov/tap/browser-control-mcp
   - Node.js >= 14 available (any OS): npm install -g @raultov/browser-control-mcp
   - macOS/Linux fallback: curl --proto '=https' --tlsv1.2 -LsSf https://github.com/raultov/browser-control-mcp/releases/latest/download/browser-control-mcp-installer.sh | sh
   - Windows fallback: download and run the .msi from https://github.com/raultov/browser-control-mcp/releases/latest
   Verify with: browser-control-mcp --version (add the install dir to PATH if it is not found).

2. Register the MCP server with every AI client you detect on this machine:
   - Claude Code: claude mcp add --scope user browser-control-mcp browser-control-mcp
   - opencode: merge into ~/.config/opencode/opencode.json —
     "mcp": { "browser-control-mcp": { "type": "local", "command": ["browser-control-mcp"] } }
   - Codex CLI: append to ~/.codex/config.toml —
     [mcp_servers.browser-control-mcp]
     command = "browser-control-mcp"
   - Any other MCP-capable client (Cursor, Windsurf, VS Code, ...): add it with its own MCP settings, command "browser-control-mcp" with no arguments.

3. Report what you installed and which clients you configured, and remind me to restart each client so the MCP server loads. Do not enable --user-profile or --allow-cookie-import unless I ask for them.
```

### macOS

**1. Homebrew**, the natural option on macOS:
```bash
brew install raultov/tap/browser-control-mcp
```
The formula fetches the pre-compiled binary (Apple Silicon and Intel) from
GitHub Releases, and `brew upgrade` keeps you current.

**2. npm / npx**, the MCP-client convention (requires Node.js >= 14):
```bash
npm install -g @raultov/browser-control-mcp   # installs the `browser-control-mcp` command
npx -y @raultov/browser-control-mcp           # ...or just run it once, nothing to install
```

**3. Shell installer**, a one-liner that installs to `~/.cargo/bin`:
```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/raultov/browser-control-mcp/releases/latest/download/browser-control-mcp-installer.sh | sh
```

**4. Cargo**, if you already have Rust:
```bash
cargo install browser-control-mcp     # compiles from crates.io
cargo binstall browser-control-mcp    # with cargo-binstall: fetches the pre-compiled binary, no compile
```

**5. Manual download**: the `aarch64-apple-darwin` or `x86_64-apple-darwin`
`.tar.xz` archive from the [Releases](https://github.com/raultov/browser-control-mcp/releases) page.

### Linux

**1. npm / npx** (requires Node.js >= 14):
```bash
npm install -g @raultov/browser-control-mcp   # installs the `browser-control-mcp` command
npx -y @raultov/browser-control-mcp           # ...or just run it once, nothing to install
```

**2. Shell installer**, a one-liner that installs to `~/.cargo/bin`:
```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/raultov/browser-control-mcp/releases/latest/download/browser-control-mcp-installer.sh | sh
```
The Linux binaries require glibc >= 2.35 (Ubuntu 22.04+, Debian 12+, Fedora 36+).

**3. Homebrew (Linuxbrew)**, if you already use
[Homebrew on Linux](https://docs.brew.sh/Homebrew-on-Linux):
```bash
brew install raultov/tap/browser-control-mcp
```

**4. Cargo**, if you already have Rust:
```bash
cargo install browser-control-mcp     # compiles from crates.io
cargo binstall browser-control-mcp    # with cargo-binstall: fetches the pre-compiled binary, no compile
```

**5. Manual download**: the `x86_64-unknown-linux-gnu` or
`aarch64-unknown-linux-gnu` `.tar.xz` archive from the [Releases](https://github.com/raultov/browser-control-mcp/releases) page.

### Windows

**1. npm / npx** (requires Node.js >= 14):
```powershell
npm install -g @raultov/browser-control-mcp   # installs the `browser-control-mcp` command
npx -y @raultov/browser-control-mcp           # ...or just run it once, nothing to install
```

**2. MSI installer**, the native Windows experience: download
`browser-control-mcp-x86_64-pc-windows-msvc.msi` from the [Releases](https://github.com/raultov/browser-control-mcp/releases)
page and double-click it.

**3. PowerShell installer**, a one-liner:
```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/raultov/browser-control-mcp/releases/latest/download/browser-control-mcp-installer.ps1 | iex"
```

**4. Cargo**, if you already have Rust:
```powershell
cargo install browser-control-mcp     # compiles from crates.io
cargo binstall browser-control-mcp    # with cargo-binstall: fetches the pre-compiled binary, no compile
```

**5. Manual download**: the `x86_64-pc-windows-msvc` `.zip` archive from the
[Releases](https://github.com/raultov/browser-control-mcp/releases) page.

### Docker (amd64)

```bash
docker build -t browser-control-mcp .
docker run -i --rm browser-control-mcp --headless   # Chrome and Firefox inside
```

### From source

```bash
git clone https://github.com/raultov/browser-control-mcp && cd browser-control-mcp
make install   # release build copied to ~/.cargo/bin (or cargo install --path . --locked)
```

### Browser discovery

`CHROME_PATH`, then `google-chrome`, `chromium` and the usual install paths;
`--firefox-path`, then `FIREFOX_PATH`, then `firefox` and the usual install
paths.

## MCP client configuration

Universal JSON:

```json
{
  "mcpServers": {
    "browser-control-mcp": { "command": "browser-control-mcp", "args": [] },
    "browser-control-firefox": {
      "command": "browser-control-mcp",
      "args": ["--browser", "firefox"]
    },
    "browser-control-docker": {
      "command": "docker",
      "args": ["run", "-i", "--rm", "browser-control-mcp", "--headless"]
    }
  }
}
```

Claude Code:

```bash
claude mcp add browser-control-mcp browser-control-mcp
claude mcp add browser-control-firefox -- browser-control-mcp --browser firefox
claude mcp add browser-control-mcp -- npx -y @raultov/browser-control-mcp
```

## Usage concepts

- **Instances.** A `default` instance always exists and uses the `--browser`
  flag. `open_instance` with a `label` starts another one (its id is the
  label), with `browser` to pick Chrome or Firefox. Browsers are launched
  lazily, on the first tool that needs them, with an ephemeral profile.
- **Tabs.** `open_tab`, `list_tabs`, `switch_tab`, `close_tab`. Every tool
  takes `instance_id` and `tab_id`; without them it targets the default
  instance and its active tab.
- **Per-tab state.** Console logs, network traffic, breakpoints and custom
  events are kept per tab, in bounded buffers.
- **Firefox allows a single automation session.** Each Firefox instance holds
  one WebDriver BiDi session shared by its tabs; `close_instance`,
  `stop_browser` and every server shutdown path end it, so the next run can
  attach again.

## CLI flags

| Flag | Default | Chrome | Firefox |
| --- | --- | --- | --- |
| `--browser <chrome\|firefox>` | `chrome` | browser of the default instance | browser of the default instance |
| `--local` | off | navigation limited to local addresses | same |
| `--headless` | off | headless Chrome | headless Firefox |
| `--host <HOST>` | `127.0.0.1` | debugging host | debugging host |
| `--port <PORT>` | `9222` | debugging port of the default instance | ignored (use `--firefox-port`) |
| `--max-instances <N>` | `8` | instance cap | shared with Chrome |
| `--enable-automation` | off | shows the automation infobar | ignored; `list_instances` shows a warning |
| `--user-profile` | off | your default profile (no `--user-data-dir`) | only with `--firefox-port` (attach to your running Firefox) |
| `--allow-cookie-import` | off | exposes `copy_cookies` | capability error |
| `--enable-webmcp` | off | WebMCP switches and `webmcp_*` tools | capability error |
| `--proxy-server <URL>` | none | proxy for every instance; exposes `enable_proxy_auth` | `http://` and `socks5://` proxies |
| `--proxy-username <USER>` | none | answers proxy (407) challenges | same (untested) |
| `--proxy-password <PASS>` | none | never logged | same |
| `--firefox-path <PATH>` | discovered | — | Firefox binary to launch |
| `--firefox-port <PORT>` | none (launch) | — | attach to a running Firefox's BiDi port |
| `--firefox-rdp-port <PORT>` | none | — | debugger port of the attached Firefox (needs `--firefox-port`) |
| `--no-firefox-debugger` | off | — | launch Firefox without its debugger port (no debugger tools) |

`--proxy` and `--webmcp` are aliases. To attach to your own Firefox, start it
with `--remote-debugging-port <port>` (and `--start-debugger-server ws:<rdp>`
plus `devtools.debugger.remote-enabled` and `devtools.chrome.enabled` for the
debugger), then pass `--browser firefox --firefox-port <port>
--firefox-rdp-port <rdp>`.

## Architecture

```
 MCP client (agent)
     │  JSON-RPC over stdio (tools/list, tools/call)
     ▼
 browser-control-mcp ── tool table: flag gate → capability check → per-browser handler
     │                                   │
     │ Chrome instance                   │ Firefox instance
     ▼                                   ▼
 cdp-browser-lite / cdp-lite         bidi-browser-lite / bidi-lite
 (launch, ports, profiles,           (launch, prefs, one BiDi session,
  CDP over WebSocket)                 RDP client for the debugger)
     │  CDP commands/events              │  BiDi commands/events   │  RDP packets
     ▼                                   ▼                         ▼
   Chrome                              Firefox ◄───────────────────┘
```

A tool call resolves the instance (`instance_id`, default `default`) and the
tab (`tab_id`, default active), checks the instance can serve it, then runs
the implementation for that instance's browser. Events are pumped into
per-tab state: on Chrome, one listener per CDP domain and tab; on Firefox,
one subscription per session, routed by browsing context id.

## Debugging JavaScript

Chrome and Firefox take the same calls:

1. `navigate` to the page (or `open_tab` + `switch_tab`).
2. `search_scripts` with a needle to find the script URL and line.
3. `set_breakpoint` with `url` and `line_number` (0-based).
4. Trigger the code (`click_element`, `evaluate_js` with a `setTimeout`, a
   reload...). The page pauses.
5. `evaluate_on_call_frame` reads locals of the paused frame; `step_over`
   moves one statement; `resume` continues.
6. `remove_breakpoint` with the id `set_breakpoint` returned.

Chrome also offers `pause_on_load` (pause on the first statement of the next
load). Firefox notes: breakpoints by URL only; the column is snapped to the
nearest breakable position automatically; the debugger needs the RDP port.

## Migration from chrome-debug-mcp

| chrome-debug-mcp 1.5.6 | browser-control-mcp |
| --- | --- |
| binary / crate `chrome-debug-mcp` | `browser-control-mcp` |
| npm `@raultov/chrome-debug-mcp` | `@raultov/browser-control-mcp` |
| tool `restart_chrome` | `restart_browser` |
| tool `stop_chrome` | `stop_browser` |
| replies printed as Rust `Debug` (`WsResponse { .. }`) | prefix + pretty JSON of the reply |
| `get_network_logs` returned credential headers | `<redacted>` unless `include_sensitive_headers: true` |
| `enable_proxy_auth` answered every auth challenge | answers proxy (407) challenges only |

All command-line flags are kept; `--browser`, `--firefox-path`,
`--firefox-port`, `--firefox-rdp-port` and `--no-firefox-debugger` are new.
In client configurations replace the command `chrome-debug-mcp` with
`browser-control-mcp` (or the npm package name). The old tool names answer an
error that names the new ones.

## Limitations and TODO

- **TODO (priority)** Unified `navigate` response: Chrome returns the CDP ids
  (`frameId`, `loaderId`) and Firefox `{navigation, url}`; both will return
  the final URL in the same shape.
- **TODO (priority)** Unified `evaluate_js` result: Chrome wraps the value in
  the CDP `RemoteObject` (`{"result": {"type", "value"}}`) and Firefox
  returns the bare value; both will return the same shape.
- **TODO (priority)** Navigation errors: on Chrome, `navigate` and `open_tab`
  to an unreachable host report success (the network error is only inside
  the reply, or absent); both browsers will fail with the network error.
- **TODO (priority)** Console buffer per navigation: `get_console_logs` will
  clear a tab's buffer when it navigates to a different site (kept across
  subdomains of the same site) and gain a limit, so one call cannot flood
  the client.
- **TODO (priority)** Unified debugger replies: `set_breakpoint`,
  `remove_breakpoint` and `step_over` answer differently on Chrome and
  Firefox (and Firefox's `step_over` location uses a 1-based line); both
  will return the same shape with 0-based lines.
- **TODO (priority)** Known bug: on Chrome `get_performance_metrics` returns
  `{}` because the Performance domain is never enabled. Workaround:
  `send_cdp_command` with `Performance.enable` first.
- **TODO (priority)** Partial performance on Firefox from the standard Web
  Performance APIs (navigation timings, FCP, LCP), stating what Firefox
  cannot give (long tasks, CLS, JS heap).
- **TODO (priority)** Same default window size on both browsers (today
  Chrome 921x648, Firefox 1280x887): the screen size when known, else
  1920x1080.
- **TODO (priority)** Firefox WebSocket frames and the full console natively
  over the Firefox debugger (RDP watcher `websocket`, `error-message` and
  `css-message` resources), so both browsers return the same shape without
  any page injection.
- **TODO** `pause_on_load` on Firefox: neither a preload `debugger;`
  statement nor an RDP interrupt pauses a reloading page (spike verdict).
- **TODO** `rust-mcp-sdk` 2.0: the server stays on 1.1.0 (MCP protocol
  2025-11-25). 2.0 moves to the stateless MCP protocol (2026-07-28), which
  today's MCP clients probably do not speak yet; upgrade once the clients do.
- `get_performance_metrics` and `profile_page_performance` are Chrome only:
  Firefox has no Performance or Tracing domain.
- WebSocket frames in `get_network_logs` are Chrome only: WebDriver BiDi has
  no WebSocket frame events.
- `webmcp_list_tools`, `webmcp_invoke_tool`, `webmcp_get_invocation` and
  `webmcp_list_invocations` are Chrome only (a Chrome testing feature).
- `send_cdp_command` is Chrome only; `send_bidi_command` and
  `send_rdp_request` are Firefox only.
- Cookie import (`--allow-cookie-import`) and launch presets (`features`) are
  Chrome only.
- Firefox breakpoints are set by URL only.
- `--local` checks the URLs the server navigates to and the known navigation
  methods of the raw tools; JavaScript running in the page can still navigate
  anywhere.
- Firefox: after a lost connection a launched Firefox gets a new session on
  the next tool call; an attached Firefox keeps its orphan session, so
  restart that Firefox.
- Firefox debugger: it attaches to the tab Firefox has selected right after
  activating the requested one; switching tabs at that very moment can bind
  it to the wrong page.
- `get_network_logs` with response bodies has no total size limit (each
  Firefox body is capped at 2 MB): use `url_filter` on busy pages.
- The Docker image is amd64 only.

## Untested

- Proxy authentication on Firefox against a real proxy.
- macOS and Windows with real browsers (CI builds and unit-tests them; the
  E2E smoke ran on Linux only).
- Firefox ESR.
- The Docker image on arm64.
- The Windows clippy cross-check (`make clippy-windows`) on a machine with
  mingw.

## Security notes

- `--allow-cookie-import` copies your real Chrome cookies into an isolated
  profile; the agent must ask you before using `copy_cookies`.
- The proxy password is never logged and is given only to proxy (407)
  challenges, never to a site's own authentication.
- The Firefox debugger port needs `devtools.chrome.enabled`: any local
  process that reaches that port gets chrome (privileged) access to the
  browser. Use `--no-firefox-debugger` when you do not need the debugger.
- Credential headers (`Authorization`, `Cookie`, `Set-Cookie`, ...) are
  redacted in `get_network_logs` unless `include_sensitive_headers: true`.
- Logs carry tool names, ids, protocol method names and sizes, never
  payloads, cookies or URLs with queries.

## Development

```sh
make check        # fmt, clippy -D warnings, tests, docs, duplication, packaging, version
make e2e          # E2E smoke against real, visible Chrome and Firefox (Linux, macOS)
make check-all    # both
```

`make check` is hermetic: it never starts a browser or touches the network.
`make e2e` runs `tests/run_all_e2e.sh`: the launched-mode smoke for both
browsers, then the attach mode against browsers started by `tests/chrome.sh`
and `tests/firefox.sh`. See [AGENTS.md](AGENTS.md) for the conventions.

## Releasing

For the maintainer: bump the version in `Cargo.toml` and `package.json`,
close the `[Unreleased]` section of `CHANGELOG.md`, run `make check-all`,
tag `vX.Y.Z` and push the tag (the release workflow builds the installers and
publishes npm and Homebrew; it needs the `NPM_TOKEN` and
`HOMEBREW_TAP_TOKEN` secrets), then `cargo publish` by hand.

## License

MIT, see [LICENSE](LICENSE).
