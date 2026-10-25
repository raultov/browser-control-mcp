# Changelog

All notable changes to `browser-control-mcp` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`browser-control-mcp` is the successor of `chrome-debug-mcp` 1.5.6 and starts
from a copy of its code: entries describe the differences from that release.

## [Unreleased]

### Fixed

- Security: with `RUST_LOG=trace` the MCP transport logged every incoming
  tool call in full, including the `enable_proxy_auth` password and raw
  protocol params. Its logs are now capped at `info`, like `cdp-lite`'s.
- Security: copies of the user's cookies (`copy_cookies`) could stay in the
  temp directory: when a launch failed, when a seed was replaced before
  use, when a browser failed to stop, when a partial copy failed, and at
  shutdown after the seeded Chrome had crashed. The seeded profile now
  deletes itself whenever it is dropped, and shutdown drops the seed of an
  instance that is no longer running.
- Security: `get_network_logs` returned request bodies (login forms with
  passwords, token exchanges) and API-key headers such as `X-Api-Key` in
  full by default. Request bodies and every header whose name contains
  `token`, `secret`, `api-key`, `apikey`, `password`, `session`, `csrf` or
  `xsrf` are now `<redacted>` unless `include_sensitive_headers: true`.
- `search_scripts` positions are right, so they can be passed to
  `set_breakpoint` as they are. A match at the start of a line was reported
  at the end of the previous line (both browsers); matches in inline scripts
  were counted from the script, not the page (Chrome); columns now count
  UTF-16 units, as the browsers do, instead of bytes.
- Firefox `set_breakpoint` no longer sets breakpoints that never fire. A
  `column_number` Firefox cannot break on (it ignores those silently) moves
  to the next breakable column of the line once the script is loaded, as on
  Chrome, and the reply says where; a column past every breakable one is an
  error. A second breakpoint at the same location is refused naming the
  first: Firefox keeps one per location, so removing either id used to
  remove both.
- Firefox `evaluate_js` no longer changes the type of nested values: `NaN`
  and `±Infinity` inside objects and arrays were strings (`"NaN"`), `-0`
  was `"-0"`, a BigInt was a string of digits, `undefined` was `null` in
  objects and a cycle was `{}`. Nested values now follow JSON as on Chrome
  (`null`, `0`, `undefined` left out); an object holding a BigInt or a
  cycle has no JSON form and is described (`"Object"`, `value: null`), and
  an object reached twice without a cycle is serialised both times.
- `webmcp_list_tools` no longer hides page tools registered without an
  `inputSchema` (optional in WebMCP); they are listed without one and can
  be invoked.
- Chrome `restart_browser` without `proxy_server` keeps the instance's own
  proxy (the one given to `open_instance` or the previous restart), as on
  Firefox; it used to fall back to `--proxy-server`, or none, silently.
- `get_console_logs`: `console.warn` is level `warning` on Firefox too (it
  was `warn`, so `level_filter: "warning"` found nothing there and `"warn"`
  nothing on Chrome); `level_filter` accepts `warn` as an alias.
- Chrome: the first call on a fresh instance (usually `navigate`) waits
  for the browser's page to register as a tab and uses it. It used to run
  on a session-level connection whose captured state stopped being read
  once the tab registered a moment later, so the first page's network
  requests were missing from `get_network_logs` and `search_scripts` found
  nothing right after it.
- Chrome `enable_proxy_auth` no longer stops answering after falling behind
  a burst of events (it left request interception on with nobody
  continuing the tab's requests, so they hung); whatever ends the handler
  turns interception off. It also subscribes before enabling interception,
  so a request paused right away is not missed.
- A browser that crashed or was killed from outside is reported: every call
  on that instance answers "The browser of this instance closed or crashed
  ... Call 'restart_browser'", its dead tabs are forgotten, and nothing is
  relaunched behind the agent's back. Chrome used to answer `CDP Error:
  Disconnected` forever while silently starting an unused new Chrome (and
  leaking the dead one's pool entry); Firefox answered "the browser was
  stopped" forever. `restart_browser`, `stop_browser` and `close_instance`
  work as usual afterwards. Chrome `open_tab` checks `--local` before it
  starts a browser.
- `fill_input` no longer types into the wrong element or reports a fill
  that did not happen (both browsers). An element that cannot take the text
  (a plain element, disabled, read-only, a non-text input such as a
  checkbox, or one that cannot take focus) fails with the reason and nothing
  is typed; before, the text went to whichever element had focus. A
  `<select>` gets the option whose value or label equals the text (it used
  to be emptied on Chrome). After typing, a field left empty (it rejected the
  text, e.g. letters in a number field) is an error, and a field holding
  something else (a mask, `maxlength`) succeeds with a note; the text is
  never echoed back. The field is emptied through the native `value` setter,
  which React-controlled inputs observe. On Chrome, an invalid selector
  reports its `SyntaxError` instead of "Element not found". The description
  no longer promises a `change` event: it fires when the field loses focus,
  as for a user.
- `click_element` no longer reports a click that hit another element or
  nothing (both browsers). The element is scrolled into view first (Chrome
  used to click off-screen coordinates into the void; Firefox failed with
  "move target out of bounds"), and the call fails without clicking when it
  has no visible box (Chrome used to click the page corner at (0, 0)) or
  when another element covers the point, naming it (an overlay used to get
  the click on both). Both browsers click the same checked viewport point;
  Firefox no longer leaves the pointer pressed when its actions fail.
- `scroll` no longer reports success when nothing moved (both browsers). The
  reply adds the window positions as JSON `{before, after}` (plus
  `element_in_view` for a selector) after its first line, which says why the
  window did not move when it did not (at the edge, or the page scrolls an
  inner element). Scrolling to a hidden element fails. `x` is no longer
  dropped when scrolling by `pages`.

## [0.2.0-beta.2] - 2026-10-14

Public **BETA**. Firefox debugger internals only; tool replies are the same
as in 0.2.0-beta.1.

### Changed

- `bidi-browser-lite` 0.1.5 (`bidi-lite` 0.1.3). Firefox tabs are attached to
  their debugger with `Debugger::attach_to_context`: Firefox's Remote Agent
  maps each BiDi context to its RDP tab, replacing the match by position in
  the tab lists. Tabs on the same URL, several windows and error pages are
  told apart, and a debugger tool never activates a tab any more (a tab
  Firefox cannot map is an error, not a guess).
- Firefox `set_breakpoint` uses bidi-lite's `set_breakpoint_resolved`: the
  bound column comes from the library (every inline script with the URL is
  searched). A column-less breakpoint for a script not loaded yet fails with
  an argument error asking for `column_number`, as before.

## [0.2.0-beta.1] - 2026-10-13

Public **BETA**. Chrome and Firefox now answer the same replies; several are
incompatible with 0.1.x (marked **Breaking**; see the README's "Migration
from 0.1.x to 0.2.0").

### Changed

- **Breaking:** `navigate` and `reload` answer the same shape on Chrome and
  Firefox once the page fired `load` (at most 30 s): `{url, tab_id, browser,
  notes?}`, `url` being the final URL after redirects and `browser` the
  protocol's ids (Chrome used to answer the raw `Page.navigate` reply without
  waiting, and `{}` on reload). `open_tab` reports the final URL too, and on
  Firefox it now creates the tab blank and navigates once it is registered,
  as on Chrome, so the page's first events are captured.
- **Breaking:** a network error fails `navigate` and `open_tab` on both
  browsers with `Navigation failed: <browser's code> (<url>)` (Chrome used to
  report success with `errorText` inside, or a tab on `chrome-error://`); from
  `open_tab` the error names the tab, which stays open.
- **Breaking:** `evaluate_js` answers `{value, type, subtype?, description?}`
  on both browsers (Chrome used to wrap the value in the CDP `RemoteObject`,
  Firefox to return the bare value). Values without a JSON form (functions,
  DOM nodes, symbols, `NaN`, bigints, maps...) have `value: null` and a
  `description`; on Chrome they no longer fail with "Object couldn't be
  returned by value" or answer `{}` (array-likes such as `NodeList` are
  described, e.g. `NodeList(6)`, as on Firefox). A thrown exception fails
  with the same wording on both: what was thrown plus its 1-based line and
  column.
- **Breaking:** the debugger tools answer the same shapes on both browsers,
  with 0-based lines everywhere: `set_breakpoint` returns an MCP id
  (`bp-N`, also on Chrome instead of the CDP id) with the bound location and
  `resolved`; `remove_breakpoint` takes that id and answers `{removed}`;
  `step_over` and `resume` answer `{paused, location}` (Firefox's step
  location was 1-based; Chrome reported none); `evaluate_on_call_frame`
  answers the `evaluate_js` shape, and an exception in the frame is an error
  on Chrome too (`Evaluation threw: ...`; it used to be a success carrying
  `exceptionDetails`). `step_over` without a pause fails with "the debugger
  is not paused" on both.
- **Breaking:** `get_console_logs` answers `{messages, total, truncated,
  notes?}` instead of a bare array, returns at most `limit` messages (new
  argument, default 200, the most recent ones), and `clear` removes only the
  returned messages (it used to empty the whole buffer, even messages a
  `level_filter` had hidden).

### Added

- Partial `get_performance_metrics` and `profile_page_performance` on
  Firefox, from the standard Web Performance APIs: `Nodes`, `Frames`,
  `Resources` and `TransferSize`; FCP, LCP, DCL and Load (optionally after a
  reload, cache bypassed with `disable_cache`). What Firefox cannot give (JS
  heap, layout counts, long tasks, total blocking time) is `null` or listed
  in a note, never a silent empty value.
- Firefox WebSocket frames in `get_network_logs`, and the console messages
  WebDriver BiDi does not report (script errors with file and line, cookie
  and security warnings, CSS warnings, platform messages) in
  `get_console_logs`, from the RDP watcher of each tab's debugger, in the
  same shapes as Chrome. Each tab's debugger is attached when the session
  starts, when `open_tab` creates the tab, or at the first log call on a tab
  the page opened, matched by its position in BiDi's and RDP's tab lists
  (checked against their URLs), without bringing it to the front. Needs the
  Firefox debugger port (default for a launched Firefox). Messages both feeds report are kept once; console
  `source` labels say what a Firefox message is (`javascript`, `css`,
  `security`, `network`, `other`).
- `--window-size <W>x<H>` (default `1920x1080`): launched Chrome and Firefox
  get the same window size, headless too (they used to pick 921x648 and
  1280x887). Attached browsers keep their window.
- A tab's console buffer is cleared when the tab moves to another site
  (registrable domain, via the `psl` crate's embedded Public Suffix List)
  and kept across subdomains of one site, reloads and same-page
  navigations; the next `get_console_logs` says what was cleared.

- Lifecycle crates bumped: `cdp-browser-lite` 0.4.2 and `bidi-browser-lite`
  0.1.4 (`bidi-lite` 0.1.2). Their mocks now record wire parameters, so the
  Firefox page, input, network and debugger tools and the Chrome domain
  setup are covered by hermetic tests instead of only by the E2E smoke.

### Fixed

- Chrome `get_performance_metrics` returned `{}`: every tab now enables the
  Performance domain when it is registered, the tool enables it on a session
  that missed it, and an empty metric list is an error instead of `{}`.
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

