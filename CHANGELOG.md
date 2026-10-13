# Changelog

All notable changes to `browser-control-mcp` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`browser-control-mcp` is the successor of `chrome-debug-mcp` 1.5.6 and starts
from a copy of its code: entries describe the differences from that release.

## [Unreleased]

### Changed

- Lifecycle crates bumped: `cdp-browser-lite` 0.4.2 and `bidi-browser-lite`
  0.1.4 (`bidi-lite` 0.1.2). Their mocks now record wire parameters, so the
  Firefox page, input, network and debugger tools and the Chrome domain
  setup are covered by hermetic tests instead of only by the E2E smoke.

### Fixed

- The "Chrome not found, use Firefox?" hint no longer ends the library's
  sentence with two periods.

## [0.1.1-beta.1] - 2026-10-12

Public **BETA**: known bugs and Chrome/Firefox differences are listed in the
README ("Limitations and TODO"). The GitHub Release, the shell/PowerShell
installers, the MSI and the npm package start with this version (0.1.0 was
only published to crates.io).

### Changed
- README: per-platform installation section (shell and PowerShell
  installers, npm/npx, Homebrew, MSI, Cargo/binstall, manual download,
  Docker, source) and a one-prompt install for AI agents.
- Releases of beta versions are published as the latest GitHub Release and
  with the npm `latest` tag, so `releases/latest/download/...` installers
  and `npm install -g @raultov/browser-control-mcp` get them.

### Fixed
- A Windows-only unit test failure (path separator in the cookie-import
  error assertion).

## [0.1.0] - 2026-10-12

First public release, **BETA**: known bugs and Chrome/Firefox differences are
listed in the README ("Limitations and TODO").

### Added
- When Chrome is not installed and a Firefox is, the launch error says so
  and tells the agent to ask the user before using `"browser": "firefox"`.
- E2E smoke over stdio MCP against real, visible Chrome and Firefox
  (`make e2e`: launched and attach modes, SIGTERM and relaunch), with
  `tests/chrome.sh` and `tests/firefox.sh` for dedicated test browsers.
- Project scaffolding: quality gates (`make check`: format, clippy with
  warnings as errors, tests, docs, duplication, packaging and version
  consistency), CI on Linux, macOS and Windows, `AGENTS.md`, MIT licence.
- The server code of chrome-debug-mcp 1.5.6 (35 tools, Chrome over CDP),
  copied without its history and renamed: crate and binary
  `browser-control-mcp`, server title "Browser Control MCP".

- `--browser <chrome|firefox>` selects the browser of the default instance
  and `open_instance` takes a `browser` argument (Firefox lands in later
  tasks; until then a Firefox instance answers "not implemented yet").
- Every tool declares what it needs; a tool an instance's browser cannot
  serve fails with an error naming the tool and the browser, before anything
  is sent. `list_instances` shows each instance's `browser` and
  `capabilities`.
- On exit (end of stdin, Ctrl-C, SIGTERM on Unix) the server stops every
  browser it started; the exit code reports instances that did not stop.

- **Firefox support** over WebDriver BiDi (`--browser firefox`, or
  `open_instance` with `browser: "firefox"`): tabs, `navigate`, `reload`,
  `evaluate_js` (values as plain JSON), `inspect_dom`, `scroll`,
  `click_element` (real pointer input), `fill_input` (real key input),
  `capture_screenshot`, `get_console_logs`, `get_network_logs` (with response
  bodies; no WebSocket frames) and `get_custom_events`; instance tools
  (`open_instance`, `close_instance`, `restart_browser`, `stop_browser`,
  `list_instances` with the Firefox version). Events of iframes are kept with
  their tab. Firefox instances are launched with an ephemeral profile, or
  attached with `--firefox-port`.
- **JavaScript debugging on Firefox** over the Firefox Remote Debugging
  Protocol: `set_breakpoint` (by `url`), `remove_breakpoint`, `resume`,
  `step_over` (reports the new location), `evaluate_on_call_frame` and
  `search_scripts`. The debugger port is on by default for launched Firefox
  instances; `--no-firefox-debugger` turns it off. `pause_on_load` is Chrome
  only for now.
- Firefox flags: `--firefox-path`, `--firefox-port`, `--firefox-rdp-port`,
  `--no-firefox-debugger`. `--enable-automation` is reported as ignored on
  Firefox instances by `list_instances`.

- `send_bidi_command` (raw WebDriver BiDi; refuses `session.new`,
  `session.end`, `browser.close` and removing the server's own subscription;
  its `session.subscribe` events feed `get_custom_events`) and
  `send_rdp_request` (raw Firefox RDP, with `thread`/`console`/`root`
  shortcuts). Firefox only.
- `enable_proxy_auth` on Firefox (`network.authRequired` →
  `network.continueWithAuth`, credentials only for 407). **Untested against a
  real authenticating proxy.**

- Distribution: cargo-dist release workflow (shell, PowerShell, npm
  `@raultov/browser-control-mcp`, Homebrew, MSI with its own GUIDs) behind a
  preflight that refuses a tag not matching `Cargo.toml`/`package.json` or
  without a CHANGELOG section; release notes are that section. Docker image
  with Google Chrome and Firefox (amd64). glama.json exposes `--browser`.

### Changed
- `cdp-browser-lite`'s test mocks are no longer linked into the binary.
- Dependencies updated to their latest releases; unused test dependencies
  (`hyper`, `hyper-util`, `http-body-util`, `futures-util`,
  `tokio-tungstenite`) removed.
- `rust-mcp-sdk` stays on 1.x (MCP 2025-11-25) with only its `server`,
  `macros` and `stdio` features: the binary no longer links the SDK's HTTP,
  SSE and OAuth client stack.

- `navigate`, `reload`, `evaluate_js`, `evaluate_on_call_frame`,
  `set_breakpoint`, `remove_breakpoint` and `send_cdp_command` return the
  protocol reply as pretty JSON after their usual prefix, instead of the Rust
  `Debug` of the whole message (`WsResponse { .. }`).

- **Renamed tools:** `restart_chrome` → `restart_browser`, `stop_chrome` →
  `stop_browser`. Calling an old name returns an error naming the new one.
- Generated instance ids carry the browser name (`chrome-2`, `firefox-3`).
- Server title "Browser Control MCP"; new description and instructions that
  cover both browsers.

- Tool descriptions say which tools are Chrome only and no longer mention
  tools that do not exist (`pause_on_exceptions`, `step_out`);
  `remove_breakpoint` no longer claims the page must be paused.

### Fixed
- `stop_browser` / `restart_browser` on Chrome could deadlock with a tool
  making the instance's first connection (two locks taken in opposite order).
- Chrome `open_instance` checks the instance limit and the label before
  launching: a refused instance no longer leaves an untracked Chrome running.
- No instance can take the id `default` (it hijacked the default instance).
- `open_tab` checks the tab limit and the label before creating the page,
  keeps labels unique when target discovery registered the page first, and
  closes the page if it still cannot register it (Chrome and Firefox).
- `close_tab` forgets a tab only once the browser closed it (Chrome and
  Firefox).
- Firefox: a lost WebSocket no longer leaves a dead event pump (empty console
  and network logs): the next tool starts a new session.
- Firefox: tab ids are not reused after `restart_browser`; the iframe map
  forgets a destroyed context's whole subtree; debuggers of tabs closed by the
  page are dropped.
- Firefox `enable_proxy_auth` subscribes to `network.authRequired` before the
  intercept exists, so no challenge is missed (and lags are logged).
- Firefox `set_breakpoint` with `line_number` 4294967295 is an argument error
  instead of an overflow.
- Shutdown: tool calls are refused once the server is stopping, and a
  debugger tool in flight no longer delays stopping Firefox past its budget.
- The server exits on SIGTERM/Ctrl-C: it used to stop its browsers and then
  hang, waiting for a stdin read that only returns on input.
- Chrome `profile_page_performance` no longer times out at random on tabs
  opened with `open_tab`: when target discovery and `open_tab` both attached
  the page, the second registration replaced the tab's listeners with ones on
  a session nobody sent commands to.
- Chrome `open_tab` with a URL records the page's first requests: the tab is
  created blank and navigated once its domains are enabled.
- `profile_page_performance` reports a failed `Tracing.end` instead of a
  timeout.
- `reload` right after `navigate` on Chrome no longer fails with "Not attached
  to an active page": the transient error is retried once after 500 ms.
- `inspect_dom` no longer crashes the server when the context window around a
  match falls inside a multi-byte character.

- The server can no longer panic on a poisoned lock or an unexpected value:
  every `unwrap`/`expect` outside tests was removed; the instance registry
  uses a single lock (two locks taken in different orders could deadlock).
- Custom-domain listeners started by `send_cdp_command` on a tab are stopped
  when the tab closes (a reference cycle kept them running forever), and no
  longer leak one static string per domain name.
- Per-tab state fed by the browser is bounded: at most 2,000 parsed scripts,
  100 WebSocket connections (with their frames) and 1,000 WebMCP
  invocations; the oldest are forgotten first.

- `search_scripts` / script cache: Chrome's numeric `startLine`/`startColumn`
  were read as strings and always stored as 0.
- `evaluate_on_call_frame` no longer uses a stale frame after the page
  resumed (`Debugger.resumed` clears it).
- `pause_on_load` pauses once: the injected `debugger;` script is removed on
  the next `resume` instead of pausing every later navigation.
- `get_custom_events` with `limit: 0` returns every event (it returned none).
- `get_network_logs`: a redirect keeps one entry per request id, with the
  final URL and a `redirects` list.
- `profile_page_performance` restores the cache and its completion channel
  when tracing fails or times out, and keeps the top 5 long tasks its
  description promises (it kept 10).
- `fill_input` replaces the field's content, as its description says.
- Closing the active tab activates the most recently used tab (it picked an
  arbitrary one); `list_tabs` reports each tab's current URL; a tab opened
  with `open_tab` can no longer be registered twice.
- The session-level WebMCP listener starts before `WebMCP.enable`, so the
  initial tool list is not missed.

- A Firefox launched by the server no longer starts on `about:home`, where
  BiDi refuses page scripts: its first tab is moved to `about:blank`.

### Security
- `--local` treats `192.168.x.x` only as an IP address: a domain name such
  as `192.168.attacker.com` is no longer accepted as local.
- `--local` also covers RDP `navigateTo` in `send_rdp_request`.
- `send_bidi_command` refuses `session.unsubscribe` of the events the server
  listens to (by event or module name), not only of its subscription id.
- The start-up log no longer prints the proxy password, username or a
  `user:pass@` embedded in `--proxy-server` (chrome-debug-mcp printed every
  argument). Logs now go through `tracing` to stderr (`RUST_LOG`, default
  `info`) and carry ids, method names and error kinds, never payloads;
  `cdp-lite`'s payload logging is capped at `info` whatever `RUST_LOG` says.
- `enable_proxy_auth` answers only proxy (407) challenges; a site's 401 gets
  the browser's default handling instead of the proxy credentials.
- `enable_proxy_auth` checks the credentials and `--local` (pre-warm URL)
  before enabling Fetch, so a refused call no longer leaves every request
  paused; enabling it again replaces the previous handler instead of stacking
  a second one; the pre-warm URL is logged as scheme and host only.
- `scroll`, `click_element` and `fill_input` embed the CSS selector as a JSON
  string literal; a selector with a backslash could break out of the
  JavaScript string before.
- `--local` also blocks `Target.createTarget` sent through `send_cdp_command`
  (only `Page.navigate` was checked).
- Cookie import refuses a `source_profile` that is not a single profile
  directory name (`../..` could read outside the browser's user-data root).
- `get_network_logs` returns `Authorization`, `Proxy-Authorization`, `Cookie`
  and `Set-Cookie` header values as `<redacted>` unless the new
  `include_sensitive_headers: true` is passed.

