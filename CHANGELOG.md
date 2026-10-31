# Changelog

All notable changes to `browser-control-mcp` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`browser-control-mcp` is the successor of `chrome-debug-mcp` 1.5.6 and starts
from a copy of its code: entries describe the differences from that release.

## [Unreleased]

### Added

- `--secrets <file>` (dotenv): every tool result and error shows the
  file's values as `<secret:NAME>` (as is or escaped in JSON), and
  `fill_input` / `fill_form` type the value of a `<secret:NAME>` they are
  given, so the agent can fill a password without reading it. A
  convenience, not a security boundary: images are not masked and a page
  can transform a value. Values never reach the logs. (#14)
- `save_pdf`, in the `files` tool group: prints the tab to PDF
  (`landscape`, `print_background`, `scale`, `page_ranges`) and writes it to
  `path`, an absolute `.pdf` under the upload roots, answering `{path,
  bytes, pages}`; without `path` the PDF comes back as an embedded
  `application/pdf` resource (at most 5 MB). Chrome `Page.printToPDF`,
  Firefox `browsingContext.print`. (#13)
- `mock_route`, `list_routes`, `remove_route` and `set_offline`, in the new
  `network` tool group (`--tool-groups network`). A route answers the tab's
  requests whose URL matches a glob with a mocked status, body, content
  type and headers, without reaching the network; the latest matching
  route wins and `list_routes` counts its hits. Chrome uses the `Fetch`
  domain (not while `enable_proxy_auth` is active), Firefox a
  `beforeRequestSent` network intercept. `set_offline` takes the tab
  offline or back online (Chrome still serves its HTTP cache). (#12)

### Fixed

- The input tools (`click_element`, `hover`, `fill_input`, `fill_form`,
  `press_key`) fail at once, naming the pause, when the page is already
  paused in the debugger, and send nothing: the page handles input events
  only once it resumes. Chrome reported a click or a key as done, and
  both browsers blamed the field ("rejected the text") or, on Firefox, a
  covering `html` element. (#17)
- On Chrome `evaluate_js` answers at once while the page is paused in the
  debugger: it waited 10 s and failed with "no result after 10 s", even
  for `1 + 1`, because Chrome cannot await an evaluation while no
  microtask runs. It now evaluates without awaiting, reads a settled promise from
  its state, and fails at once on a pending one, saying to `resume` or use
  `evaluate_on_call_frame`. (#16)
- On Chrome a pause's location names its script: `resume`, `step_over`,
  the `{paused, location}` of `click_element` and `press_key`, and the
  "The page paused in the debugger at ..." message of an interrupted call
  gave `url: null`, because Chrome leaves the deprecated
  `callFrames[].url` empty. The URL now comes from the tab's parsed
  scripts, and the location also carries `script_id` (what
  `set_breakpoint` takes). (#10)

### Changed

- The package description (crates.io, npm, Homebrew `desc`) is 79
  characters instead of 213, within Homebrew's 80, so `brew style` no
  longer flags the formula's `desc` line as too long.

## [0.4.0-beta.7] - 2026-10-31

Public **BETA**. Closes the gaps a comparison with Playwright MCP showed:
`snapshot` and `find_in_snapshot` outline the page with refs every
selector accepts, selectors reach into shadow roots and iframes with `>>>`,
and new tools wait for content (`wait_for`), fill forms (`fill_form`),
upload files, press keys, hover, read and change cookies and web storage,
and emulate viewports. A click into a breakpoint or a dialog answers at once.
Some behaviour changes: the `raw`, `perf`, `storage`, `files` and
`emulation` tools are listed only with the new `--tool-groups` (start the
server with `--tool-groups raw,perf` or `all` to keep `send_cdp_command`,
`get_custom_events` and the performance tools), tool descriptions are about
half as long, and `click_element` succeeds (noting it) when the click opens
a dialog or hits a breakpoint.

### Added

- `--tool-groups` lists opt-in tool groups, comma separated: `storage`
  (`cookies`, `web_storage`), `files` (`upload_file`), `raw`
  (`send_cdp_command`, `send_bidi_command`, `send_rdp_request`,
  `get_custom_events`), `perf` (`get_performance_metrics`,
  `profile_page_performance`), `emulation` (`emulate`), or `all`. A tool of
  a group that is off fails naming the flag. The tool list stays fixed for
  the server's life (MCP 2026-07-28 forbids it to change as a side effect of
  a call), so there is no tool to switch groups on during a session.

- `emulate`: the tab's viewport (`width`, `height`, `device_scale_factor`,
  and on Chrome `mobile` with touch) and, on Chrome, CSS media features
  (`color_scheme`, `reduced_motion`, `forced_colors`, `media: print`);
  `reset` removes them. It answers what the page now sees, measured in it
  (size, pixel ratio, media queries). Firefox emulates the viewport and
  pixel ratio through `browsingContext.setViewport`; its WebDriver BiDi has
  no media or touch emulation yet, so those options fail there naming
  them. Responsive and dark-mode checks needed raw protocol commands. (#9)

- `cookies` and `web_storage` (Chrome and Firefox). `cookies` gets, sets
  and deletes cookies: those the page's URL (or `url`) would be sent, or
  those of a `domain`, optionally by `name` (Chrome `Network.*Cookie*`,
  Firefox BiDi `storage.*` in the tab's partition, with the same filters
  and replies). `web_storage` gets, sets, deletes and clears the page's
  `localStorage` or `sessionStorage`. Both hide values (often session
  tokens), giving their length, unless `show_values: true`. Cookies could
  only be imported from your Chrome before. (#8)

- `press_key` and `hover` (Chrome and Firefox), and `button`,
  `click_count` and `modifiers` for `click_element`. `press_key` presses
  one key (a character, Enter, Tab, Escape, Backspace, Delete, the arrows,
  Home, End, PageUp, PageDown, Insert, Space, F1 to F12) with Control,
  Shift, Alt or Meta held, in `selector` (focused first) or the focused
  element, as a user (Chrome `Input.dispatchKeyEvent`, Firefox key
  actions); a shortcut types nothing, and a dialog or debugger pause the key
  causes is noted like for a click. `hover` moves the mouse over an element
  after the same visibility checks as `click_element`. A right click opens
  context menus and `click_count: 2` double-clicks. These needed raw
  protocol commands or synthetic JavaScript events, which many pages
  ignore. (#7)

- `upload_file` (Chrome and Firefox): sets the files of an
  `<input type=file>` as a user picking them (Chrome
  `DOM.setFileInputFiles`, Firefox `input.setFiles`); the page gets `input`
  and `change`. Only files under the upload roots are accepted: the
  server's working directory and the system temp directory, or the
  directories of the new repeatable `--upload-root` flag, checked once `..`
  and symlinks are resolved, so the agent cannot hand a page any file of
  the machine. Uploads needed three raw CDP commands, on Chrome only. (#6)

- `fill_form` (Chrome and Firefox): fills several fields in one call, in
  order. Text fields, textareas, contenteditable elements and `<select>`s
  go through `fill_input`; a checkbox or radio takes `true` or `false` and
  is clicked only when its state differs. It answers
  `Filled N fields.` with each field's outcome (`filled`, `selected`,
  `checked`, `unchecked`, `unchanged`), never the values, and stops at the
  first field that fails, naming it and what was done before. A login form
  took one call per field. (#5)

- `snapshot` and `find_in_snapshot` (Chrome and Firefox): an outline of the
  page with one line per element with a role (`button "Sign in" [ref=e7]`,
  `textbox "Username" [ref=e3]`, `heading "Cart" [level=2] [ref=e1]`...),
  its name, states and value, indented under its parent, through open
  shadow roots and same-origin iframes, hidden elements left out; and a
  search over that outline that returns only the matching lines under their
  ancestors (a 1500-row page: 143 characters to find a button instead of a
  50 000-character snapshot). The `selector` of `click_element`,
  `fill_input`, `scroll` and `wait_for` accepts the refs, so agents no
  longer write CSS from raw HTML. Computed in the page with the same
  JavaScript on both browsers (one outline, refs that resolve the same
  way); refs are kept per element while it is in the page and end when the
  page navigates; password values are never shown. (#4)

- Selectors of `click_element`, `fill_input`, `scroll` and `wait_for` reach
  into open shadow roots and same-origin iframes with `>>>`
  (`my-widget >>> button`, `#frame >>> #email`, chainable), on both
  browsers. `inspect_dom` showed those elements but no tool could act on
  them. A click inside iframes is checked for overlays in each document and
  lands at the right point of the top window; a part that is neither an
  open shadow host nor a same-origin iframe fails naming it. `fill_input`
  also recognises fields of another frame (they failed as "not an input").
  (#3)

- `wait_for` (Chrome and Firefox): waits until the page's visible text
  contains `text` or no longer contains `text_gone`, the first element
  matching `selector` is `visible`, `hidden`, `attached` or `detached`
  (`state`), and/or the tab's URL contains `url`, checking every 100 ms up
  to `timeout_ms` (default 5000, at most 30000). It answers
  `Waited N ms for ...` with `{elapsed_ms, url}`, or fails naming what is
  still missing and where the page is. Text includes open shadow roots and
  same-origin iframes; a check the page did not answer (a navigation) is
  retried; a JavaScript dialog or a debugger pause ends the wait at once.
  Agents no longer have to write polling promises in `evaluate_js`. (#2)

### Changed

- The `raw`, `perf`, `storage`, `files` and `emulation` tools are no longer
  listed by default: 33 tools are (plus `enable_proxy_auth` and the WebMCP
  tools behind their flags). Start the server with `--tool-groups raw,perf`
  (or `all`) to keep `send_cdp_command`, `get_custom_events` and the
  performance tools.
- Tool and argument descriptions are about half as long (the tool list of
  every tool went from 72 986 to 35 568 characters, about 18 000 to 9 000
  tokens; the 33 default tools take 23 555), and `instance_id` / `tab_id` no longer say "Chrome".

- `click_element` succeeds when the click opens a JavaScript dialog or its
  handler pauses in the debugger: the click happened, so the reply says so
  (`... The page opened a JavaScript confirm dialog ("Delete?"), which
  blocks it: call handle_dialog ...`), followed by `{"dialog": {...}}` or
  `{"paused": true, "location": ...}`. It was an error, which invited
  agents to click again. Calls whose answer is missing (`evaluate_js`) or
  that may have stopped half way (`fill_input`) still fail. (#11)

### Fixed

- A call whose own code hits a breakpoint (`click_element` on a button whose
  handler has one, `fill_input`, `scroll`, `evaluate_js`, `navigate`,
  `reload`) fails at once on both browsers with where the page paused and
  how to go on (`evaluate_on_call_frame`, `resume`, `step_over`). It waited
  for the 10 s command timeout and failed with `Input.dispatchMouseEvent`
  (Chrome) or `input.performActions` (Firefox) "timed out". On Chrome a
  pause must last 200 ms to count, so a page's own `debugger;` the server
  resumes does not fail the call. A Chrome click interrupted on the press
  still sends the release, so the button is not left held. (#1)

## [0.3.6-beta.6] - 2026-10-29

Public **BETA**. `fill_input` types the same way on both browsers,
`evaluate_js` gives objects the same value and names their class,
`inspect_dom` reads shadow roots and same-origin iframes, network logs
carry cookies and no longer wait on open streams, and raw commands that
would break the server's capture are refused. Some behaviour changes:
`fill_input` presses a key per character (about 1 ms each on Chrome),
`evaluate_js` values are their `JSON.stringify` form, `inspect_dom`
returns more HTML, and `send_cdp_command` refuses `*.disable` of the
captured domains and `Browser.close`.

### Fixed

- `set_breakpoint` on Chrome fails for a line with no code at or after it
  in the loaded script with that URL, saying where the script ends, and
  sets nothing (as Firefox does). It answered `resolved: false`, which the
  description explains as "until a script with that URL loads", for a
  script that was loaded already: the breakpoint never fired.

- `inspect_dom` includes open shadow roots (as `<template
  shadowrootmode="open">`) and the documents of same-origin iframes (each
  after an `<!-- iframe <src>: -->` comment), and notes the cross-origin
  iframes it cannot read. Text inside a web component or an iframe was
  reported as not found.

- `get_network_logs` asks for a response body only once the response
  finished loading, and the reply notes the ones still loading (an open
  event stream). On Firefox an open server-sent events stream made every
  call wait 10 s for its body; now it answers at once. Each body waits at
  most 2 s.

- `get_network_logs` on Chrome returns the `Cookie` and `Set-Cookie`
  headers (redacted unless `include_sensitive_headers: true`): Chrome
  reports them only in its ExtraInfo events, which were ignored, so they
  were missing even when asked for. On Firefox a repeated header (several
  `Set-Cookie`) keeps every value, joined by newlines as on Chrome; only
  the last one was kept.

- `capture_screenshot` checks `format` before capturing: `png`, `jpeg`
  (or `jpg`) and `webp`, any case; anything else fails saying so. Firefox
  answered a PNG labelled `image/gif` (or `image/jpg`), and Chrome a raw
  `Invalid image format`. The reply's MIME type is the one of the image's
  bytes.

- `send_cdp_command` refuses the commands that silently stopped what the
  server captures (`Network.disable`, `Page.disable`, `Runtime.disable`,
  `Log.disable`, `Debugger.disable`) or closed the browser
  (`Browser.close`), saying what depends on them; `Network.disable` used to
  leave `get_network_logs` empty without a word. `Fetch.enable` is sent with
  a note that requests now pause until answered.

- `evaluate_js` (and `evaluate_on_call_frame`) gives an object or array the
  same `value` on both browsers: what `JSON.stringify` gives (`toJSON`
  counts, functions drop out), and an object that is not a plain one has
  its class in `description`. `navigator` and `new URL(...)` answered `{}`
  without a word; now `{"value": {}, "description": "Navigator"}` and
  `{"value": "<href>", "description": "URL"}`. Nested functions were `{}`
  on Chrome and `{"type": "function"}` on Firefox, and `location` was `{}`
  on Firefox.

- `fill_input` types the same way on both browsers: Chrome now presses a
  key per character, as Firefox did, so the page's keydown and keyup
  handlers run and can cancel a key (a field refusing `a` got `abc` on
  Chrome and `bc` on Firefox; now `bc` on both). Newlines and tabs are
  inserted as text on both (as a key, Chrome pressed Tab and the rest of
  the text left the field). Typing takes about 1 ms per character, so a
  very long text is slower on Chrome than before.
- `fill_input` on Firefox types a long text in batches of 500 characters:
  20 000 characters timed out half typed, now they take about 20 s.
- `fill_input` fires `change` once, as for a user: the browser fires it
  when the field loses focus, and when the window has no focus (Firefox
  headless, which never fired it) the fill fires it at the end.

## [0.3.5-beta.5] - 2026-10-28

Public **BETA**. Observation tools stop returning wrong or missing data
without saying so, an endless `evaluate_js` loop no longer kills a Chrome
tab, Firefox debugger calls no longer block other tabs, and WebMCP passes
every tool annotation through. Some replies change: `get_custom_events`
answers `{events, total, truncated}` with the newest events, and
`get_network_logs` requests may carry `error` and
`response_body_encoding`.

### Fixed

- `webmcp_list_tools` drops no annotation: `consequential` (the page
  declares the tool has real-world effects, such as a payment) and
  `debugging` were silently lost, and any annotation a later Chrome adds is
  now passed through. The output of a tool annotated `untrustedContent` is
  flagged: `webmcp_invoke_tool` adds a note to treat it as data, not as
  instructions, and `webmcp_get_invocation` says `untrustedContent: true`.
- `evaluate_js` with an endless loop no longer kills the tab on Chrome:
  the script is stopped after 8 s and the call says so ("the script ran for
  more than 8 s ... the page keeps working"); later calls work at once. It
  used to fail after 20 s with `Runtime.evaluate timed out`, and every
  later call on the tab timed out too. An expression waiting on a promise
  that never settles says so after 10 s. Firefox cannot stop a running
  script from outside: its error says so and points to `close_tab`.
- On Firefox, a slow debugger call on one tab (e.g. a long
  `evaluate_on_call_frame`) no longer blocks the logs and debugger tools of
  the other tabs: each tab's debugger has its own lock (one lock covered
  them all, delaying `get_console_logs` on another tab by 9.5 s).
- `get_custom_events` returns the newest `limit` events (it returned the
  oldest, so new events never showed once the buffer held more), answers
  pure JSON `{"events", "total", "truncated"}` (the text started with
  `Found N custom events.`), and takes `clear` to remove what it returned.
- `get_network_logs`: on Chrome the page's own document is in its
  navigation's bucket, so `current_navigation_only` lists it and its body
  is returned (it was left in the previous navigation, without a body). A
  failed request carries `error` (Chrome `net::ERR_...`, Firefox
  `NS_ERROR_...`, also in the summary); it looked like one still waiting on
  Chrome, and Firefox put the error in `response_status_text`. A binary body
  says `response_body_encoding: "base64"`.
- On Chrome, `get_console_logs` and `get_network_logs` connect first: on an
  instance nothing had connected to (an attached Chrome) they answered
  empty buffers. The console messages the page logged before are now
  returned; `get_network_logs` notes that earlier requests are not
  captured.

## [0.3.4-beta.4] - 2026-10-27

Public **BETA**. The same server as 0.3.3-beta.4, which was published on
crates.io but without binaries, npm package or Homebrew formula: its
release notes made GitHub skip the builds. See 0.3.3-beta.4 below for the
changes.

### Fixed

- The release notes no longer quote a URL with credentials, which GitHub
  treats as a secret; `make check` now refuses one in this file.

## [0.3.3-beta.4] - 2026-10-27

Public **BETA**. Tools that reported success for something that did not
happen now say what happened, and proxy credentials are handled more
safely. Some replies change: an unknown tool argument is an error,
`navigate`/`reload` add `status`, a download URL fails, and
`restart_browser`/`stop_browser` on an attached Chrome refuse or only
disconnect.

### Fixed

- `open_instance` without `headless` follows the server's `--headless` flag.
  It always opened a window, even on a server started with `--headless`
  (where there may be no display, as in the Docker image).
- Credentials written in a proxy URL (a user and password before the host,
  from `--proxy-server`, `open_instance`'s `proxy` or `restart_browser`)
  no longer reach the browser: Chrome got them on its command line, readable
  by any local user with `ps` (and ignored them), and Firefox refused to
  launch. `enable_proxy_auth` now falls back to the credentials of
  `--proxy-server`.
- `enable_proxy_auth` with wrong credentials no longer loops: a request the
  proxy challenges again after two answers is cancelled. Firefox re-raised
  the same request forever (about 50 challenges a second, each carrying the
  password); Chrome retried tens of times. On Firefox, calling the tool
  again also removes the previous intercept (each call added one more).
- On Chrome, an instance attached to a browser that was already running on
  its port (not launched by the server) no longer reports a restart or stop
  that did not happen. `restart_browser` (and a `copy_cookies` relaunch)
  fails saying the server cannot relaunch it, and nothing is changed;
  `stop_browser` disconnects and says the browser keeps running. Both said
  "successfully" while the same process kept running without the presets.
- `navigate` and `reload` report the document's HTTP status (`"status":
  404`), and an error status (400 and up) adds a note. A 404 used to look
  like any success, and an error with an empty body failed on Chrome
  (`ERR_HTTP_RESPONSE_CODE_FAILURE`) but succeeded on Firefox: it is now a
  success with its status on both.
- `navigate` to a URL the browser downloads fails with `Navigation failed:
  the URL is a download...` on both browsers. Firefox answered "Navigated
  to" the previous page; Chrome failed with `net::ERR_ABORTED`.
- A tool call with an argument the tool does not have now fails before
  anything runs, naming the argument it was probably meant to be (`tabId`
  answers "did you mean `tab_id`?"). Unknown arguments were ignored, so a
  `tabId` typo ran on the active tab.

## [0.3.2-beta.3] - 2026-10-25

Public **BETA**. Replies up to 256 MB no longer drop the browser connection
(new `cdp-lite` and `bidi-lite` releases).

### Fixed

- Replies up to 256 MB no longer drop the connection: built with `cdp-lite`
  0.3.1 and `bidi-lite` 0.1.4, which accept WebSocket messages up to 256 MiB
  (the WebSocket library's default closed the connection on a frame over
  16 MiB, e.g. an `evaluate_js` string of 20 MB). The lost-connection error
  now names the 256 MB limit.

## [0.3.1-beta.3] - 2026-10-25

Public **BETA**. Size limits: large pages, results, scripts, logs and
screenshots no longer flood the agent or drop the browser connection, and
Chrome reconnects by itself when a reply over 16 MB drops it. Some replies
change (cut texts end with a `[cut: ...]` or `[Truncated: ...]` note,
`evaluate_js` may say `truncated: true`, `inspect_dom` positions are
characters).

### Fixed

- `capture_screenshot` with `full_page` captures at most 16 384 CSS pixels
  per side, from the top left, and then adds a text note naming the
  document's size (a 30 000 px page came back whole).
- Observation buffers keep a bounded copy of each item: a console message
  keeps 10 000 characters, a WebSocket payload 64 000 (`payload_len` is
  the whole length), custom event params over 64 000 characters of JSON
  become `{"truncated": true, "characters": N}`, and `get_network_logs`
  shows at most 64 000 characters of a request or response body. Each cut
  text ends with `… [cut: the first N of M characters]`. Chrome no longer
  fetches a response body whose `Content-Length` is over 2 MB (Firefox
  keeps none larger). Two 3 MB console messages made a 6 MB reply.
- `search_scripts` previews a long line (minified code) as the match with
  100 characters on each side, `…` where it was cut: one hit in a 1.6 MB
  one-line script returned the whole line.
- `evaluate_js` returns at most 100 000 characters of `value`: a longer
  string is cut, and an object or array whose JSON form is longer is
  measured in the page and only described ("too large to return (N
  characters of JSON)"), so it never crosses the connection; the reply
  says `"truncated": true`. A 3-million-element array used to time out on
  Firefox and a 20 MB object to drop the connection. A string result over
  16 MB still drops it (the next call reconnects).
- `inspect_dom` searches and cuts in the page, so a large document no
  longer crosses the connection (a 20 MB page used to drop it on Chrome).
  Without `query` it returns at most 100 000 characters, after a
  `[Truncated: ...]` note naming the total; `before`/`after` are capped at
  10 000. Positions are characters (they were bytes). On Chrome an
  exception in the page is an error (it was an empty success).
- A reply over 16 MB (a huge `evaluate_js` result, a 20 MB page in
  `inspect_dom`) makes the browser close the DevTools connection. Chrome
  then answered `CDP Error: Connection lost` to every later call until
  `restart_browser`; now the next call reconnects by itself (tab ids are
  re-assigned). The failing call explains what happened on both browsers.

## [0.3.0-beta.3] - 2026-10-25

Public **BETA**. Result of a tool-by-tool audit on real Chrome and Firefox:
tools that reported success for an action that did not happen now fail or
say what happened, secrets no longer leak into logs or replies, and a new
`handle_dialog` tool closes JavaScript dialogs. Some replies change (marked
in the entries below: `scroll`, `fill_input`, `click_element` errors,
Chrome `open_tab` ids).

### Added

- `handle_dialog` (both browsers): accepts or dismisses the JavaScript
  dialog (`alert`, `confirm`, `prompt` with `prompt_text`, `beforeunload`)
  open on a tab, and replies with its type and message.

### Changed

- Chrome: when the first call on an instance is `open_tab`, the browser's
  own first page is now registered too (as on Firefox), so the new tab is
  `tab-2` and stays inactive until `switch_tab`; before, it was `tab-1` and
  active.

### Fixed

- Chrome tracks popups (`window.open`) and tabs closed by the page or the
  user when the first call on an instance is `open_tab`; it only did after
  a call that used the instance's default page.
- `navigate` and `reload` keep their 30 s budget on slow pages (both
  browsers). A server slow to answer, or a page slow to fire `load`, hit
  the 10 s protocol command timeout and failed although the navigation went
  on; now the call waits for `load` up to 30 s from its start and answers
  then, with the not-loaded note when it did not come.
- `get_network_logs` `clear: true` removes only the requests and frames it
  returned (with `url_filter`, `type_filter` or `current_navigation_only`
  it used to empty everything, also what was never returned), as
  `get_console_logs` does. WebSocket connections keep their URL after a
  clear (later frames used to have `url: ""`).
- Chrome `pause_on_load` no longer leaves its `debugger;` script injected
  (after which every later load paused): calling it again replaces the
  previous script, and a `resume` Chrome refuses because the reload has not
  paused yet keeps the script pending, so the next `resume` removes it.
- JavaScript dialogs no longer hang the tools. A call during which the page
  opens a dialog (`evaluate_js` running `alert()`, a `click_element` on a
  button that asks `confirm()`, a `navigate` stopped by `beforeunload`)
  fails at once quoting the dialog instead of after a 10–20 s timeout, and
  while a dialog is open the page tools fail naming it and pointing to
  `handle_dialog`. On Chrome the tab used to stay blocked until a raw
  `Page.handleJavaScriptDialog`. Chrome errors are worded with the
  protocol's message instead of Rust debug output (`Timeout { method: ...
  }`).

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
- A page's own `debugger;` statements no longer pause it while the tab has
  no breakpoint (and, on Chrome, no `pause_on_load`). The debugger is
  always attached, so anti-debugging pages (`setInterval(() => {debugger})`)
  and a stray `debugger;` froze every tool on the tab until a timeout. The
  first breakpoint turns them back on, removing the last one turns them off
  (Chrome `Debugger.setSkipAllPauses`, re-applied after cross-process
  navigations; Firefox `shouldPauseOnDebuggerStatement`). Chrome's
  session-level connection drops its debugger once the page is a
  registered tab, so it no longer resumes the tab's pauses.
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

