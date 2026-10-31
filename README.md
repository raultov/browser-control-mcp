# browser-control-mcp

[![Discord](https://img.shields.io/discord/1566088811354656778?label=Discord&logo=discord)](https://discord.gg/JDfcmD73A)

**Browser Control MCP** lets an AI agent control **and debug** real browsers
at runtime: breakpoints, pause on load, call frames, evaluation in a paused
frame, stepping and script search on **Chrome (CDP)** and **Firefox
(WebDriver BiDi + Firefox RDP)**, plus navigation, DOM, input, screenshots,
console and network capture.

It is the successor of
[chrome-debug-mcp](https://github.com/raultov/chrome-debug-mcp) (it starts
from a copy of its code) and complements Playwright MCP or
chrome-devtools-mcp with runtime JavaScript debugging on both browsers.

> **Status: BETA (0.6.0-beta.10).** This version contains known bugs and differences
> between Chrome and Firefox (see [Limitations and TODO](#limitations-and-todo))
> and parts that have not been verified (see [Untested](#untested)). Tool
> replies may change in incompatible ways before 1.0. Please report problems
> in the [issues](https://github.com/raultov/browser-control-mcp/issues).

💬 Questions, ideas or feedback: join us on [Discord](https://discord.gg/JDfcmD73A), where every release is announced in #announcements.

## Comparison with Playwright MCP and chrome-devtools-mcp

Measured on 2026-10-31 with browser-control-mcp built from the current
sources, `@playwright/mcp` 0.0.83 and `chrome-devtools-mcp` 1.12.0, driven by
the same stdio client through the same tasks on Chrome 156 (headless,
Linux). Every outcome was checked in the page, not taken from the reply.
Tokens are estimated as characters / 4 for every server.

**What each one can do**

| | browser-control-mcp | Playwright MCP | chrome-devtools-mcp |
| --- | --- | --- | --- |
| JavaScript debugger (breakpoints, call frames, stepping, script search) | **✓ Chrome and Firefox** (pause on load: Chrome) | ✗ | ✗ |
| Firefox | **✓ next to Chrome, same tools** | ✓ one browser per server | ✗ |
| Several browsers and instances at once | **✓** | ✗ | ✗ |
| Selectors | CSS, snapshot refs, `host >>> inner` into shadow roots and iframes | CSS, snapshot refs | snapshot refs only |
| Snapshot of a long page | **bounded at 50 000 characters**, `find_in_snapshot` searches the rest | unbounded | unbounded |
| Waits for the page to react after an action | ✓ ~50 ms when nothing reacts | ✓ ~500 ms | ✓ ~200 ms |
| Clicking a covered or hidden element | **fails at once, naming the overlay** | fails after a 5 s timeout | reports success (the overlay gets the click) |
| Mock requests, offline mode | ✓ | ✓ | offline only |
| PDF | ✓ file or inline | ✓ file | ✗ |
| Cookies and web storage | ✓ values hidden unless asked | ✓ values shown | ✗ |
| Secrets masked in output (`--secrets`) | ✓ | ✓ | ✗ |
| Performance | vitals + insights (LCP breakdown, layout shifts, render-blocking, long scripts) | trace file | trace + insights, Lighthouse |
| Heap snapshots, CSS styles, screencast | ✗ | video, tracing | ✓ |
| Generates Playwright test code | ✗ | ✓ | ✗ |
| Telemetry | **none** | none found | usage statistics on by default; traced URLs sent to CrUX |
| Single native binary (no Node.js) | **✓** | ✗ | ✗ |

**Measured**

| Task | browser-control-mcp | Playwright MCP | chrome-devtools-mcp |
| --- | --- | --- | --- |
| Tool list in context, default tools | 6 430 tokens | 5 330 tokens | 7 770 tokens |
| Tool list in context, every opt-in tool on | 9 880 tokens | 12 040 tokens | not measured (optional categories) |
| Snapshot of Hacker News | **4 588 tokens** | 12 356 tokens | 6 869 tokens |
| Snapshot of Wikipedia "Web browser" | **12 643 tokens** (cut, searchable) | 34 349 tokens | 27 656 tokens |
| Snapshot of MDN `<input>` | **12 625 tokens** (cut, searchable) | 158 077 tokens | 76 654 tokens |
| Log in step by step (8 calls) | **298 tokens, 1.8 s** | 521 tokens, 2.4 s | 364 tokens, 2.6 s |
| Log in with `fill_form` (5 calls) | **321 tokens, 1.6 s** | 428 tokens, 1.9 s | 352 tokens, 2.0 s |
| Click, median | 52 ms | 531 ms | 234 ms |
| Evaluate JavaScript, median | **0 ms** | 504 ms | 204 ms |
| Navigate (local page), median | **13 ms** | 31 ms | 119 ms |
| Full-page screenshot of a 31 700 px page | cut at 16 384 px, with a note | 3.3 MB image in the reply | saved to a file |
| Navigation to an unknown host | error | error | success with the error in the text |
| Navigate, then read the console (log, warning, errors) | 249 tokens | 191 tokens | 117 tokens |
| Navigate, then read the network log (3 requests) | 168 tokens | 48 tokens | 54 tokens |

## Supported browsers

| Browser | Protocol | Validated with |
| --- | --- | --- |
| Chrome / Chromium | Chrome DevTools Protocol | Google Chrome 155.0.8059.39 (Linux) |
| Firefox | WebDriver BiDi, plus the Firefox Remote Debugging Protocol (RDP) for the JavaScript debugger | Firefox 157.0.1 (Linux, snap) |

The default browser is Chrome. Every instance runs one browser for its whole
life: start the server with `--browser firefox`, or pass
`"browser": "firefox"` to `open_instance`. If Chrome is not installed but Firefox
is, the launch error says so and tells the agent to ask you before switching.

## Tool groups

Every tool listed costs the agent context on every turn, so niche and risky
tools are off by default and listed only with `--tool-groups`:

| Group | Tools | Why it is opt-in |
| --- | --- | --- |
| `storage` | `cookies`, `web_storage` | reads and changes session tokens |
| `files` | `upload_file`, `save_pdf` | reads and writes files of the machine |
| `raw` | `send_cdp_command`, `send_bidi_command`, `send_rdp_request`, `get_custom_events` | anything the protocol allows |
| `perf` | `get_performance_metrics`, `profile_page_performance` | niche |
| `emulation` | `emulate` | niche |
| `network` | `mock_route`, `list_routes`, `remove_route`, `set_offline` | changes what the page receives |

`--tool-groups all` lists every group. The page, input, observation and
debugger tools are always listed (34 tools), plus `enable_proxy_auth` with
`--proxy-server` and the WebMCP tools with `--enable-webmcp`. A tool of a
group that is off fails naming the flag. The list is fixed for the server's
life: it never changes during a session, so clients can cache it.

## Feature matrix

✓ supported, ✗ not available (the tool answers a capability error naming the
tool, the browser and the reason; nothing is sent to the browser), partial:
see the note.

| Tool | Chrome | Firefox | Notes |
| --- | --- | --- | --- |
| `open_instance` | ✓ | partial | `features`, `copy_cookies` and `source_profile` are Chrome only |
| `close_instance` | ✓ | ✓ | |
| `list_instances` | ✓ | ✓ | each instance reports its `browser` and `capabilities` |
| `restart_browser` | ✓ | ✓ | was `restart_chrome`; refused on a Chrome already running on the port (attached, not launched by the server) |
| `stop_browser` | ✓ | ✓ | was `stop_chrome`; on an attached Chrome or Firefox it only disconnects (Firefox: ends the session), the browser keeps running and the reply says so |
| `open_tab` | ✓ | ✓ | |
| `close_tab` | ✓ | ✓ | |
| `list_tabs` | ✓ | ✓ | |
| `switch_tab` | ✓ | ✓ | |
| `navigate` | ✓ | ✓ | `history: "back"` or `"forward"` instead of `url` moves one entry in the tab's history and waits for the page to load (also when it comes from the back-forward cache, which fires no `load`); at either end it fails without moving |
| `reload` | ✓ | ✓ | |
| `scroll` | ✓ | ✓ | reports the window positions before and after |
| `emulate` | ✓ | partial | `emulation` group; `width`/`height`, `device_scale_factor`, `mobile` (touch), `color_scheme`, `reduced_motion`, `forced_colors`, `media` (`print`), `reset`; answers what the page now sees; Firefox: viewport and device pixel ratio only (BiDi has no media or touch emulation yet; those options fail naming them) |
| `wait_for` | ✓ | ✓ | waits (checking every 100 ms, at most 30 s) until a visible text appears or is gone, a selector is `visible` / `hidden` / `attached` / `detached`, or the URL contains a text; a dialog or debugger pause ends the wait at once |
| `inspect_dom` | ✓ | ✓ | includes open shadow roots and same-origin iframes (cross-origin ones are noted); at most 100 000 characters without `query` (cut in the page) |
| `snapshot` | ✓ | ✓ | outline of the page: role, name, states, value and a ref (`e12`) per element, through open shadow roots and same-origin iframes; tables without headers (`th`, `thead`, `caption`) are layout and leave only their content, and a cell is named by its text only when it holds nothing else; comboboxes, spinbuttons and sliders built with ARIA show their value (the field's text, `aria-valuetext` or `aria-valuenow`, the option a button-like combobox shows); an element that is only clickable (a handler or a pointer cursor) is one named line, unless it holds links, buttons or fields, which are listed under it; at most 50 000 characters (`selector` and `depth` narrow it); the same JavaScript on both browsers |
| `find_in_snapshot` | ✓ | ✓ | `text` (case-insensitive) or `regex` over the whole outline; matching lines under their ancestors, at most 20 |
| `save_pdf` | ✓ | ✓ | `files` group; prints the tab as the browser's print does (`landscape`, `print_background`, `scale` 0.1–2, `page_ranges` like `1-3, 5`); with `path` (an absolute `.pdf` under the upload roots) writes the file and answers `{path, bytes, pages}`, else returns the PDF as an embedded `application/pdf` resource (at most 5 MB); Chrome `Page.printToPDF`, Firefox `browsingContext.print` |
| `capture_screenshot` | ✓ | ✓ | `format` `png`, `jpeg` (or `jpg`) or `webp`; `full_page` at most 16 384 px per side (a text note says when cut); `selector` captures one element (CSS, `host >>> inner` or a snapshot ref), scrolled into view, whole even when taller than the viewport |
| `click_element` | ✓ | ✓ | scrolls the element into view; `button` (left/right/middle), `click_count` (2 = double click) and `modifiers`; fails without clicking when it is hidden or covered (its own label on top does not count: styled checkboxes and switches are clicked through it; a covered element first gets the mouse, so controls shown only on hover are revealed and clicked); `host >>> inner` selectors reach open shadow roots and same-origin iframes (also in `fill_input`, `scroll`, `wait_for`); a dialog or debugger pause the click causes is noted in a successful reply (`{dialog}` or `{paused, location}`) |
| `fill_input` | ✓ | ✓ | picks `<select>` options; fails without typing on elements that cannot take the text; `append: true` types after the content instead of replacing it (a number, email or date field, which has no caret, is retyped whole); an editor of its own (an element with an `EditContext` or a focusable `textbox` role, or the hidden input of a code editor such as Monaco or Ace) gets the keys a user presses: a click on the editor when its input has no visible box, Control+A (Command+A on macOS) or Control+End, then the text, and the reply says its content could not be checked; in a contenteditable element (including rich text editors such as Slate, Lexical, ProseMirror or CodeMirror 6) it selects the content (a range, then Control+A as a user would) and types over it, so the editor records the edit; types key by key on both (key handlers run; newlines and tabs inserted as text), about 1 ms per character; `change` on leaving the field, or at the end when the window has no focus |
| `press_key` | ✓ | ✓ | one key (a character, Enter, Tab, Escape, arrows, F1–F12...) with Control / Shift / Alt / Meta held, in `selector` (focused first) or the focused element; a shortcut types nothing |
| `hover` | ✓ | ✓ | moves the mouse over the element (scrolled into view; fails when hidden or covered) |
| `drag` | ✓ | ✓ | presses on `from` (checked like `click_element`), moves in steps to the centre of `to` (which must then be in the viewport) and releases: sortable lists, boards, sliders; HTML5 drag and drop is real on Chrome and simulated with script events on Firefox (its WebDriver input starts a native drag but never drops it), and the reply says so |
| `fill_form` | ✓ | ✓ | several fields in one call: text and `<select>` as `fill_input`, a checkbox or radio takes `true`/`false` and is clicked only when its state differs; stops at the first failing field naming what was done; never echoes values |
| `upload_file` | ✓ | ✓ | `files` group; sets the files of an `<input type=file>` (Chrome `DOM.setFileInputFiles`, Firefox `input.setFiles`); files must be under the upload roots (`--upload-root`, default the working and temp directories) |
| `handle_dialog` | ✓ | ✓ | accepts or dismisses the open `alert` / `confirm` / `prompt` / `beforeunload` dialog |
| `evaluate_js` | ✓ | ✓ | `value` at most 100 000 characters (`truncated: true`); Chrome stops a script after 8 s, Firefox cannot (a looping tab is closed with `close_tab`) |
| `cookies` | ✓ | ✓ | `storage` group; `get` / `set` / `delete`: the cookies sent to the page (or `url`) or of a `domain`; values hidden unless `show_values: true` (Chrome `Network.*Cookie*`, Firefox BiDi `storage.*` in the tab's partition) |
| `web_storage` | ✓ | ✓ | `storage` group; `get` / `set` / `delete` / `clear` on the page's `localStorage` or `sessionStorage`; values hidden unless `show_values: true` |
| `get_console_logs` | ✓ | ✓ | one line per message, oldest first: `[level] text @ url:line` (lines from 1), objects and arrays previewed as the console shows them (`{n: 1}`, `[1, 2, 3]`), repeats in a row folded (`[x3]`), uncaught errors marked with up to 5 stack lines indented; `include_details: true` gives the JSON with every field; a message keeps at most 10 000 characters; Firefox: page errors, CSS warnings and platform messages need the debugger port |
| `get_network_logs` | ✓ | ✓ | one line per request, oldest first (`id METHOD url => status [type]`, or `failed: <reason>` / `pending`), then one per WebSocket frame (payload preview); `request_id` shows one request's headers and body, `include_details: true` every request's as JSON (a binary body has `response_body_encoding: "base64"`; bodies at most 64 000 characters, none over 2 MB; a WebSocket payload keeps 64 000, `payload_len` is the whole); the browser's own requests (`chrome://`, `devtools://`, `data:`, `about:`, extensions) are not logged; Firefox: WebSocket frames need the debugger port |
| `mock_route` | ✓ | ✓ | `network` group; requests whose URL matches a glob (`*`, `?`, `\` escapes) get a mocked `status`, `body` (text, at most 1 MB), `content_type` and `headers` without reaching the network; the latest matching route wins; at most 50 routes per tab, kept until `remove_route` or the tab closes (Chrome `Fetch`, not while `enable_proxy_auth` is active; Firefox a `network` intercept) |
| `list_routes` | ✓ | ✓ | `network` group; the tab's routes with the `hits` each answered |
| `remove_route` | ✓ | ✓ | `network` group; one route by id, or all |
| `set_offline` | ✓ | ✓ | `network` group; requests fail as without a network and `navigator.onLine` is false, until set back (Chrome `Network.emulateNetworkConditions`, Firefox `emulation.setNetworkConditions`); mocked routes still answer; Chrome still serves what its HTTP cache holds, as an offline browser does, Firefox fails every request |
| `get_custom_events` | ✓ | partial | `raw` group; `{events, total, truncated}`, the newest `limit` events; `clear` removes the returned ones; params over 64 000 characters of JSON are replaced by `{truncated, characters}`; Firefox: events of the modules subscribed with `send_bidi_command` |
| `get_performance_metrics` | ✓ | partial | `perf` group; Firefox: `Nodes`, `Frames`, `Resources`, `TransferSize` from the Web Performance APIs (no Performance domain), plus a note |
| `profile_page_performance` | ✓ | partial | `perf` group; `insights: true` adds what explains the numbers, read with the Web Performance APIs (same JavaScript on both): the document's TTFB, server time and compression, the LCP element and its breakdown (TTFB, load delay, load, render delay), layout-shift culprits, render-blocking requests, the slowest requests and the scripts of long animation frames (at most 5 each), plus `findings` from the web.dev thresholds; Firefox: FCP, LCP, DCL and Load; no trace, so `total_blocking_time_ms` and `top_long_tasks` are `null`, and its insights have no layout shifts, render-blocking requests or long frames (`null`, with notes) |
| `set_breakpoint` | ✓ | partial | Firefox: by URL only (no `script_id` / `script_hash`) |
| `remove_breakpoint` | ✓ | ✓ | |
| `resume` | ✓ | ✓ | |
| `step_over` | ✓ | ✓ | |
| `evaluate_on_call_frame` | ✓ | ✓ | |
| `search_scripts` | ✓ | ✓ | a long line's preview is 100 characters on each side of the match |
| `pause_on_load` | ✓ | ✗ | TODO on Firefox, see [Limitations and TODO](#limitations-and-todo) |
| `enable_proxy_auth` | ✓ | partial | needs `--proxy-server`; a request the proxy rejects twice is cancelled; untested on Firefox against a real proxy |
| `send_cdp_command` | ✓ | ✗ | `raw` group; raw Chrome DevTools Protocol; refuses `Network`/`Page`/`Runtime`/`Log`/`Debugger.disable` and `Browser.close`; `Fetch.enable` comes with a note |
| `send_bidi_command` | ✗ | ✓ | `raw` group; raw WebDriver BiDi; refuses `session.new`, `session.end`, `browser.close` |
| `send_rdp_request` | ✗ | ✓ | `raw` group; raw Firefox RDP request to an actor (`thread`, `console`, `root` shortcuts) |
| `webmcp_list_tools` | ✓ | ✗ | needs `--enable-webmcp`; WebMCP is a Chrome testing feature; every annotation Chrome reports is listed (`readOnly`, `consequential`, `untrustedContent`, `debugging`, `autosubmit`, newer ones as they are) |
| `webmcp_invoke_tool` | ✓ | ✗ | needs `--enable-webmcp`; the output of an `untrustedContent` tool comes with a note to treat it as data |
| `webmcp_get_invocation` | ✓ | ✗ | needs `--enable-webmcp` |
| `webmcp_list_invocations` | ✓ | ✗ | needs `--enable-webmcp` |

After `click_element`, `hover`, `drag`, `fill_input`, `fill_form` and
`press_key` the server waits for the page to settle before answering: until its DOM
stays unchanged for 50 ms (style changes, often animations, aside) and the
requests the action started finished, at most 1 s, on both browsers. An
action nothing reacts to costs those 50 ms and keeps its reply; otherwise
the reply adds `The page reacted (the DOM changed, 1 request(s)) and settled
after 134 ms.`, or says the page was still changing after 1 s. A click that
navigates waits for the new page's requests. Use `wait_for` for anything
slower (a timer, a long request).

The JavaScript debugger tools on Firefox need its debugger (RDP) port: on by
default for a launched Firefox, `--firefox-rdp-port` for an attached one. The
same port gives Firefox's WebSocket frames and the console messages BiDi does
not report (script errors with their file and line, cookie and security
warnings, CSS warnings, platform messages), in the same shapes as Chrome.
Each tab's debugger watches them: the session's first tabs are attached when
the session starts and `open_tab` attaches its tab before loading the URL; a
tab the page opened is attached by the first `get_console_logs` or
`get_network_logs` on it. Firefox itself says which RDP tab shows a BiDi
tab (its Remote Agent maps the BiDi context to the tab's `browserId`), so
tabs on the same URL, several windows and error pages are told apart, and a
tab is never activated or brought to the front to attach it. Frames are
captured from the moment a tab is attached.

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
  instance and its active tab. An argument a tool does not have fails the
  call before anything runs (a `tabId` typo answers "did you mean
  `tab_id`?"), so a typo never falls back to the active tab.
- **Same replies on both browsers.** `navigate` and `reload` answer once the
  page fired `load` (at most 30 s, else with a note): a first line
  (`Navigated to <final URL>.` / `Page reloaded.`), then
  `{"url": "<final URL, after redirects>", "tab_id": "tab-N", "browser": {...}}`,
  where `browser` holds the protocol's ids (CDP `frameId`/`loaderId`, BiDi
  `navigation`), plus `"status"`, the document's HTTP status, for http(s)
  pages (an error status, 400 and up, also adds a note; an error page with
  an empty body is reported the same way on both browsers). A URL the
  browser downloads fails with `Navigation failed: the URL is a
  download...`. `open_tab` answers `{tab_id, target_id, url, label}` with the
  final URL. A network error (unknown host, refused connection...) fails with
  `Navigation failed: <browser's code> (<url>)`, e.g.
  `net::ERR_NAME_NOT_RESOLVED` on Chrome and `NS_ERROR_UNKNOWN_HOST` on
  Firefox; from `open_tab` the error names the tab, which stays open.
  `evaluate_js` answers `{"value", "type", "subtype"?, "description"?}`:
  `type` is JavaScript's `typeof`, `subtype` refines objects (`array`,
  `null`, `node`, `date`, `map`...), `value` is the JSON form and is `null`
  when there is none (functions, DOM nodes, symbols, `NaN`, bigints,
  maps...), `description` then says what it was (Chrome describes functions
  by their source, Firefox only as `function`). An object or array's
  `value` is what `JSON.stringify` gives, on both browsers (`toJSON` counts:
  a `URL` is its `href`; nested functions drop out), and an object that is
  not a plain one has its class in `description` (`navigator`:
  `{"value": {}, "description": "Navigator"}`). A thrown exception or a
  rejected promise fails with what was thrown and its 1-based line and
  column, e.g. `ReferenceError: x is not defined (line 1, column 1)`.
  A `value` over 100 000 characters is not returned whole: a string keeps
  its first 100 000, an object or array is measured in the page and only
  described; the reply then carries `"truncated": true`.
- **Acting on elements.** `snapshot` outlines the page, one line per
  element with a role, e.g. `textbox "Username" [ref=e3]`,
  `button "Sign in" [ref=e7]`, `iframe [ref=e9]` with the frame's elements
  under it; `find_in_snapshot` returns only the lines that match, under
  their ancestors. The `selector` of `click_element`, `fill_input`,
  `scroll` and `wait_for` takes such a ref, a CSS selector, or
  `host >>> inner` to reach into an open shadow root or a same-origin
  iframe (`#checkout >>> #card-number`, chainable). An element keeps its ref
  in later snapshots while it is in the page; refs end when the page
  navigates (a stale ref fails saying so). Roles and names are computed in
  the page, an approximation of the browser's accessibility tree that is
  the same on both browsers; password values never appear.
- **Per-tab state.** Console logs, network traffic, breakpoints and custom
  events are kept per tab, in bounded buffers. A tab's console buffer (last
  1000 messages) is cleared when the tab moves to another site (registrable
  domain from the Public Suffix List: `www.knot.kz` and `docs.knot.kz` are
  one site, `a.github.io` and `b.github.io` two) and kept across reloads and
  same-page navigations. `get_console_logs` answers
  `{"messages": [...], "total", "truncated", "notes"?}`: at most `limit`
  (default 200) of the most recent matching messages, oldest first; `clear`
  removes only the returned ones; a note says when a site change cleared
  messages. `get_custom_events` answers `{"events", "total", "truncated"}`
  the same way. On Chrome both log tools connect first, so on an attached
  browser the first call already sees the page's console messages (Chrome
  replays them; network requests sent before the connection are not
  captured, and the reply says so).
- **Firefox allows a single automation session.** Each Firefox instance holds
  one WebDriver BiDi session shared by its tabs; `close_instance`,
  `stop_browser` and every server shutdown path end it, so the next run can
  attach again.

## CLI flags

| Flag | Default | Chrome | Firefox |
| --- | --- | --- | --- |
| `--browser <chrome\|firefox>` | `chrome` | browser of the default instance | browser of the default instance |
| `--local` | off | navigation limited to local addresses | same |
| `--headless` | off | headless Chrome (also the default of `open_instance`'s `headless`) | headless Firefox (same) |
| `--window-size <W>x<H>` | `1920x1080` | window of launched Chrome (`--window-size`) | window of launched Firefox (`--width`/`--height`) |
| `--host <HOST>` | `127.0.0.1` | debugging host | debugging host |
| `--port <PORT>` | `9222` | debugging port of the default instance | ignored (use `--firefox-port`) |
| `--max-instances <N>` | `8` | instance cap | shared with Chrome |
| `--enable-automation` | off | shows the automation infobar | ignored; `list_instances` shows a warning |
| `--user-profile` | off | your default profile (no `--user-data-dir`) | only with `--firefox-port` (attach to your running Firefox) |
| `--allow-cookie-import` | off | exposes `copy_cookies` | capability error |
| `--secrets <FILE>` | none | dotenv file (`NAME=value`, at least 4 characters per value) whose values every tool result and error shows as `<secret:NAME>`; `fill_input` and `fill_form` type the value of a `<secret:NAME>` (see [Security notes](#security-notes)) | same |
| `--tool-groups <GROUPS>` | none | lists the opt-in tool groups, comma separated: `storage`, `files`, `raw`, `perf`, `emulation`, `network` or `all` (see [Tool groups](#tool-groups)) | same |
| `--upload-root <DIR>` | working + temp dir | directory `upload_file` may read files from and `save_pdf` may write PDFs to (repeatable; replaces the defaults) | same |
| `--enable-webmcp` | off | WebMCP switches and `webmcp_*` tools | capability error |
| `--proxy-server <URL>` | none | proxy for every instance; exposes `enable_proxy_auth` | `http://` and `socks5://` proxies |
| `--proxy-username <USER>` | none | answers proxy (407) challenges | same (untested) |
| `--proxy-password <PASS>` | none | never logged | same |
| `--firefox-path <PATH>` | discovered | — | Firefox binary to launch |
| `--firefox-port <PORT>` | none (launch) | — | attach to a running Firefox's BiDi port |
| `--firefox-rdp-port <PORT>` | none | — | debugger port of the attached Firefox (needs `--firefox-port`) |
| `--no-firefox-debugger` | off | — | launch Firefox without its debugger port (no debugger tools) |

Credentials written in a proxy URL (`http://user:pass@host:port`) never reach
the browser's command line; `enable_proxy_auth` uses those of `--proxy-server`
when neither its arguments nor `--proxy-username`/`--proxy-password` give
them.

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
   reload...). The page pauses. A `click_element` whose handler hits the
   breakpoint answers at once, as a success: the click happened, and the
   reply adds `The page paused in the debugger at <url> line N, column M
   (0-based)` (the URL when the browser gives it) and
   `{"paused": true, "location": ...}`. Any other call the pause leaves
   unfinished (`evaluate_js`, `fill_input`, `navigate`...) fails at once
   with that location.
5. `evaluate_on_call_frame` reads locals of the paused frame; `step_over`
   moves one statement; `resume` continues. While paused, the input tools
   (`click_element`, `hover`, `drag`, `fill_input`, `fill_form`, `press_key`) fail
   at once naming the pause and do nothing: the page would handle their
   events only after `resume`. `evaluate_js` still answers
   while paused, in the page's global scope. A promise it returns cannot
   always settle meanwhile: Chrome reads a settled one and fails at once on
   a pending one; Firefox settles it when its microtasks can run.
6. `remove_breakpoint` with the id `set_breakpoint` returned.

Both browsers answer the same replies, with 0-based lines everywhere:

| Tool | Reply |
| --- | --- |
| `set_breakpoint` | `{"breakpoint_id": "bp-N", "url", "line_number", "column_number", "resolved"}`: where the browser bound it; `resolved` is false until a script with that URL loads |
| `remove_breakpoint` | `{"removed": "bp-N"}`; an unknown id fails listing the tab's ids |
| `step_over`, `resume` | `{"paused", "location": {"url", "script_id"?, "line_number", "column_number"} \| null}`: where the thread stopped next (`script_id` on Chrome, for `set_breakpoint`) (`step_over` waits up to 2 s; `resume` reports a pause only if one follows within 250 ms) |
| `evaluate_on_call_frame` | the `evaluate_js` shape; an exception fails with `Evaluation threw: <what was thrown>` |

Breakpoint ids are per tab and MCP-level (`bp-1`, `bp-2`...), like tab ids.

While a tab has no breakpoint (and, on Chrome, no pending `pause_on_load`),
the page's own `debugger;` statements do not pause it: the debugger is
always attached, and anti-debugging pages loop on `debugger;` to freeze
every tool. The first breakpoint turns them back on.

Chrome also offers `pause_on_load` (pause on the first statement of the next
load). Firefox notes: breakpoints by URL only; without `column_number` the
breakpoint binds to the first breakable column of the line, in whichever of
the page's scripts with that URL has one (inline `<script>` blocks share the
page's URL), and needs the script loaded; the debugger needs the RDP port.

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

### Migration from 0.1.x to 0.2.0

0.2.0 makes the replies the same on Chrome and Firefox; these changes are
incompatible:

| Tool | 0.1.x | 0.2.0 |
| --- | --- | --- |
| `navigate`, `reload` | Chrome: raw `Page.navigate` reply (`frameId`, `loaderId`, `errorText`) without waiting, `{}` on reload; Firefox: `{navigation, url}` | `{url, tab_id, browser, status?, notes?}` after `load`; `url` is the final URL; protocol ids under `browser`; `status` is the document's HTTP status |
| `navigate`, `open_tab` to an unreachable host | Chrome: success (`errorText` inside, or a `chrome-error://` tab) | error `Navigation failed: <code> (<url>)`; `open_tab` names the tab, which stays open |
| `evaluate_js` | Chrome: `{"result": {type, value, ...}}`; Firefox: the bare value | `{value, type, subtype?, description?}` |
| `set_breakpoint` | Chrome: CDP id (`1:3:0:http://…`) and `locations`; Firefox: `{breakpointId, url, lineNumber, columnNumber}` | `{breakpoint_id: "bp-N", url, line_number, column_number, resolved}` |
| `remove_breakpoint` | Chrome: CDP id, reply `{}` | `bp-N` id, reply `{removed}` |
| `step_over`, `resume` | Chrome: text only; Firefox `step_over`: `{paused, location: {line (1-based), column}}` | `{paused, location: {url, line_number, column_number} \| null}`, 0-based |
| `evaluate_on_call_frame` | Chrome: raw `RemoteObject` (exceptions as success); Firefox: bare value | the `evaluate_js` shape; exceptions are errors (`Evaluation threw: ...`) |
| `get_custom_events` | `Found N custom events. Captured events: [...]`, the oldest events | `{events, total, truncated}`, the newest `limit` events; `clear` removes the returned ones |
| `get_console_logs` | a JSON array; `clear` emptied the whole buffer | `{messages, total, truncated, notes?}`, at most `limit` (200) messages; `clear` removes the returned ones; the buffer is cleared on cross-site navigation |

## Limitations and TODO

- **TODO** Default window size from the screen size: launched browsers use
  `--window-size` (default 1920x1080 on both); reading the screen size needs
  native APIs per OS, left out of the server.
- **TODO** `pause_on_load` on Firefox: neither a preload `debugger;`
  statement nor an RDP interrupt pauses a reloading page (spike verdict).
- **TODO** `rust-mcp-sdk` 2.0: the server stays on 1.1.0 (MCP protocol
  2025-11-25). 2.0 moves to the stateless MCP protocol (2026-07-28), which
  today's MCP clients probably do not speak yet; upgrade once the clients do.
- `get_performance_metrics` and `profile_page_performance` are partial on
  Firefox: it has no Performance or Tracing domain, so they read what the
  Web Performance APIs give and say what is missing (JS heap, layout and
  style counts, long tasks, total blocking time).
- Firefox WebSocket frames and page errors need the Firefox debugger port
  (WebDriver BiDi has no WebSocket frame events); binary frames are reported
  as Firefox gives them, not base64 as on Chrome.
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
- Firefox debugger: tabs are mapped to the debugger through Firefox's Remote
  Agent (verified on Firefox 157 only); a Firefox without it answers the
  debugger tools with an error instead of guessing a tab.
- `get_network_logs` with `include_details: true` has no total size limit (each
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
- `cookies` and `web_storage` hide values (often session tokens) unless
  `show_values: true`; values never reach the logs.
- `upload_file` hands the page a file's contents: it only reads files under
  the upload roots (the server's working directory and the system temp
  directory, or the `--upload-root` directories), resolved so `..` and
  symlinks cannot leave them. Paths are never logged.
- `--secrets <file>` keeps known values (test passwords, API keys) out of
  what the agent reads: every tool result and error (text, embedded text,
  structured content) shows them as `<secret:NAME>`, also when escaped
  inside JSON, and `fill_input` / `fill_form` type the value of a
  `<secret:NAME>` they are given, so the agent can log in without seeing
  the password. It is a convenience, not a security boundary: the page can
  transform a value (encode it, split it) past the masking, images
  (screenshots, PDFs) are not masked, and the server stops at start-up if
  the file has a malformed line, a value shorter than 4 characters or a
  name defined twice (the error names the line, never a value). Values
  never reach the logs.
- `save_pdf` writes only `.pdf` files, under the same upload roots and
  resolved the same way; it overwrites an existing `.pdf` there.
- The proxy password is never logged and is given only to proxy (407)
  challenges, never to a site's own authentication.
- The Firefox debugger port needs `devtools.chrome.enabled`: any local
  process that reaches that port gets chrome (privileged) access to the
  browser. Use `--no-firefox-debugger` when you do not need the debugger.
- Credential headers (`Authorization`, `Cookie`, `Set-Cookie`, and any
  header whose name contains `token`, `secret`, `api-key`, `password`,
  `session` or `csrf`) and request bodies are redacted in
  `get_network_logs` unless `include_sensitive_headers: true`. URLs and
  response bodies are returned as they are.
- Logs carry tool names, ids, protocol method names and sizes, never
  payloads, cookies or URLs with queries.

## Development

```sh
make check        # fmt, clippy -D warnings, tests, docs, duplication, packaging, version
make e2e          # E2E smoke against real Chrome and Firefox, headless (E2E_VISIBLE=1 shows them; Linux, macOS)
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
