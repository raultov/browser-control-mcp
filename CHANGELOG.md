# Changelog

All notable changes to `browser-control-mcp` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`browser-control-mcp` is the successor of `chrome-debug-mcp` 1.5.6 and starts
from a copy of its code: entries describe the differences from that release.

## [Unreleased]

### Fixed

- A tab a page opens while its instance already holds 16 tabs (the most
  it tracks) is named in the action's reply as not tracked, with how to
  make room (`close_tab`), and the agent's tab stays in front, on both
  browsers. It was dropped without a word: the new tab stayed in front and
  later actions on the agent's tab (now hidden) timed out (#95).
- Firefox: top-level contexts that are not tabs (Firefox gives them a
  numeric id) are no longer listed as tabs: `save_pdf` of a page with
  iframes left one per iframe (the copies Firefox makes to print), which
  `close_tab` could not close and which used up the instance's 16 tabs;
  a DevTools window was listed too (#95).
- `capture_screenshot` of an element puts the page's scroll back after the
  capture (and its scrollable boxes'), also when it fails; it was left
  where the element was. A hidden or zero-size element is refused before
  anything scrolls. An element outside the page fails saying so on both
  browsers (Firefox failed with a raw protocol error, Chrome answered an
  image of nothing), and one partly outside is cut to the page, with a
  note (#89).
- A click (or another input action) whose page closes its own tab
  (`window.close()`) is reported as done, with a note naming the tab that
  closed during the action and the tab now active, on both browsers. Firefox failed with a
  raw protocol error from `input.releaseActions` (the click had happened);
  Chrome said nothing of the closed tab (#90).
- `snapshot` shows a field's placeholder (`[placeholder="you@example.com"]`,
  often the format it expects) when it is not already the field's name; a
  labelled field hid it (#91).
- Chrome: `emulate` with `mobile` says why the page lays out wider than
  asked (a page without a viewport meta tag gets a phone's 980 px layout
  viewport); the width it answered was unexplained. `profile_page_performance`
  without a reload gives the current page's FCP, LCP, DCL and Load from the
  Web Performance APIs, as Firefox does, with a note saying so (or that the
  page reports none); they were all `null` without a word (#92).
- Chrome: `step_over`, `step_into`, `step_out`, `resume` and
  `pause_on_load` answer once the tab's debugger state recorded the new pause. A `call_stack` or
  `evaluate_on_call_frame` right after could still read the state before
  the pause ("the debugger is not paused") or the previous one (#94).
- Chrome: `navigate` waits for the `load` of the page it navigates to.
  The `load` of the previous page, when it was still loading, ended the
  wait early: the reply came before the new page had loaded (or even
  entered the history), and a navigation right after could cancel it, so
  `history: "back"` later went to the wrong page (#94).

## [0.12.0-beta.21] - 2026-11-05

Public **BETA**. Debugging, evaluation and diagnostics fixes found by a
comparison with other browser MCP servers. `set_breakpoint` takes a
`condition` and a `log` (logpoints) on both browsers, and a script's file
name instead of its URL. `evaluate_js` previews the values it answered as
`null` (Maps, Sets, typed arrays, Errors, cycles, getters that throw), the
same on both browsers. Chrome no longer stays stuck as paused after a
navigation while paused, and `pause_on_load` answers once the page paused.
`get_issues` counts Chrome's repeated reports once and names the elements
an issue is about; Firefox's `get_css_styles` names each rule's stylesheet
and finds every element. The schema of `emulate` and `pause_on_exceptions`
lists the values they accept. Behaviour changes: `evaluate_js` answers a
preview (`preview: true`) where it answered `value: null`;
`profile_page_performance` refuses an `action` it does not know (it
profiled without a reload); `get_issues` counts distinct reports.

### Added

- `set_breakpoint` takes `condition` (pause only when the expression is
  truthy) and `log` (a logpoint: the value goes to the console as
  `logpoint <url>:<line>: <value>` and the page never pauses; with
  `condition`, only when it holds), on both browsers. A `url` without a
  scheme (`app.js`) names the loaded script whose URL ends with it; several
  are refused, naming them. With none loaded, Chrome waits for the first
  script ending with it (it used to accept a URL that never bound, silently)
  and Firefox refuses it. A breakpoint not bound yet says so in its reply's
  first line (#87).
- `evaluate_js` previews values without a JSON form, the same on both
  browsers, with `preview: true`: a Map as `[key, value]` pairs, a Set or a
  typed array as its items, an Error as `{name, message, stack}`, a cycle as
  `"[Circular]"` and a getter that throws as `"[Thrown: message]"`; the
  `description` names it (`Map(2)`, `Uint8Array(3)`, `TypeError: bad`).
  They came back as `value: null` (#88).

### Fixed

- The input schemas of `emulate` (`cpu_performance_tier`) and
  `pause_on_exceptions` (`state`) list the values the tools accept
  (`low`, `uncaught`...). They listed the capitalized names (`Low`,
  `Uncaught`...), which the tools refused, so a client that sends only
  schema values could not use these options (#82).
- `profile_page_performance`'s `action` is an enum in the schema (`none`,
  `reload`). A value it does not list now fails the call; before, it was
  ignored and the page was profiled without a reload (#82).
- Chrome: navigating (or reloading) while the debugger is paused no longer
  leaves the tab stuck as paused. Chrome tears the paused page down
  without saying it resumed, so `click_element` and the other input tools
  kept refusing to run, `call_stack` showed the old stack and only a new
  instance got out of it. The pause is now forgotten when its page (or
  frame) goes, and when Chrome refuses a `resume` or a step as not paused;
  steps then answer "the debugger is not paused" instead of Chrome's raw
  error. Firefox: a navigation or reload of a paused tab forgets the pause
  it replaced once it answers, as the debugger can say it resumed only
  later, and input tools briefly refused to run on the new page (#83).
- Chrome: `pause_on_load` answers once the reloaded page paused, with
  where (or that it had not paused within 5 s). It answered "paused" before
  the pause, so `call_stack` right after said "not paused" and `resume`
  failed with Chrome's raw error. The pause, before the page's first
  script, is named: its frame's URL is `browser-control-mcp://pause_on_load`
  (it was an anonymous frame without a URL) and `call_stack` gives the
  reason `pause_on_load`; `search_scripts` leaves that script out. `resume`
  before the pause says it has not come yet (#84).
- `get_issues` on Chrome counts an issue once per distinct report: Chrome
  sends its form issues twice with the same details, so the counts were
  doubled (`[x4]` for two fields). The elements an issue of the tab is
  about follow its line as `selector [ref=eN]` (in `include_details`,
  `elements` with ref, selector and opening tag); before, only the first
  one's node id was in the details. The common codes and form problems
  have a title (`[GenericIssue] Form fields share an id: ...`). Firefox
  reports its deprecation warnings (a synchronous XHR...) as
  `DeprecationIssue` (#85).
- Firefox: `get_css_styles` names the stylesheet of each rule. Firefox 159
  names a rule's sheet only by an id its debugger cannot resolve here, so
  every rule was attributed to the page's URL; the page now finds the sheet
  holding a rule with the same selector and declarations (`@import`ed,
  nested, adopted and shadow-root sheets included). `@container` at-rules
  keep their container's name and query on both browsers (Firefox gave
  `@container` alone, Chrome dropped the name). The tool's description no
  longer says shadow roots and frames are out of reach on Firefox: both
  work (#86, #93).
- Firefox: `evaluate_js` describes an Error by its name and message (it
  said `Error`), a typed array by its class and length (`TypedArray`) and a
  function by its source (`function`); an object with a getter that throws
  no longer fails with a raw protocol error. On both browsers an exception
  whose position is outside the expression (Firefox's own wrapper, line
  306; where a rejected promise settled, on Chrome) is reported without it
  (#88).
- Firefox: `get_css_styles` no longer answers "Element not found" for an
  element that is there. In some Firefox instances the inspector's walker
  does not resolve the element's content DOM reference (more often for an
  element in a frame), and no retry helped; the element is then taken from
  the object the evaluation found it as (#93).
- Chrome: `reload` notices a pause in the debugger that comes right after
  Chrome answers the reload. It listened for pauses only once the reply was
  read, so such a pause was missed and the call waited 30 s for `load`
  (seen on a slow machine).

## [0.11.0-beta.20] - 2026-11-04

Public **BETA**. Deeper debugging and inspection on both browsers. The
debugger steps into and out of calls (`step_into`, `step_out`), lists the
paused stack (`call_stack`: frames, why it paused, what was thrown, and on
Chrome the async callers), evaluates in any frame (`frame_index`) and pauses
on exceptions (`pause_on_exceptions`). `get_css_styles` explains how an
element is styled (matched rules in cascade order, overridden declarations,
computed values). `get_issues` lists what the browser diagnosed that the
console hides (blocked cookies, CORS, CSP, mixed content; Chrome's Issues
panel). `emulate` takes `cpu_performance_tier` on Chrome. Behaviour changes:
Chrome enables its `Audits` domain in every tab and frame; pausing on
exceptions lets the page's `debugger;` statements pause it again, as a
breakpoint does.

### Added

- `get_css_styles` explains how an element is styled, on both browsers
  (#59): the rules that match it in cascade order, each with its selector,
  stylesheet and 0-based line and column, the at-rules around it and its
  declarations marked `important`, `invalid` or `overridden` (worked out
  the same way on both browsers); its pseudo-elements; optionally its
  ancestors' rules and the browser's own; and the computed values of what
  it declares. `properties` narrows it to some properties and `max_rules`
  bounds it. Chrome reads them with its `CSS` domain; Firefox with its
  inspector, which needs its debugger port.

- `get_issues` lists the problems the browser diagnosed in the tab that the
  console hides or words vaguely: a cookie blocked or warned and why, a CORS
  or Content-Security-Policy block, mixed content, one line each
  (`[CookieIssue] ExcludeSameSiteNoneInsecure: SetCookie cookie "a" @ …`),
  folded with their repeats, with `code`, `include_details` and `clear`. On
  Chrome they are its DevTools Issues panel (the `Audits` domain, now
  enabled in every tab and frame of another origin); Firefox has no such
  panel, and its security console messages are read as issues. (#60)

- The debugger goes deeper, on both browsers (#67): `step_into` and
  `step_out`; `call_stack` lists the paused frames (function, URL, 0-based
  line and column), why the page paused and what it threw, and on Chrome
  the async callers (`setTimeout`, promises); `evaluate_on_call_frame` takes
  `frame_index` to evaluate in a caller; `pause_on_exceptions` (`none`,
  `uncaught`, `all`) pauses on exceptions, per tab. Pausing on exceptions,
  like a breakpoint, lets the page's `debugger;` statements pause it again.

- `emulate` takes `cpu_performance_tier` (`low`, `mid`, `high`, `ultra`,
  `unknown`, or `none` to remove it) on Chrome: the tier
  `navigator.cpuPerformance` reports, so a page that picks a lighter
  experience on slow devices can be tested; it stays across calls, `reset`
  removes it, and the reply shows the tier and what the page reads
  (`cpu_performance`). Firefox has no `navigator.cpuPerformance`: the option
  fails there naming it. (#61)

## [0.11.0-beta.19] - 2026-11-03

Public **BETA**. Downloads are saved per instance and reported on both
browsers: an input action that downloads a file names the saved file,
`list_downloads` lists them, and `--downloads-dir` keeps them after the
instance closes. `get_network_logs` shows what it hid: sizes of bodies over
2 MB, redirect hops, CORS reasons, streams the page closed, and on Firefox
request types and bodies. Chrome watches dedicated workers (their requests
and console). Accepting a `beforeunload` waits for the navigation it allowed.
`snapshot` names tree items and shows descriptions, pressed, current and live
states; console counters and timers, `(in promise)` rejections and prompt
defaults behave the same on both browsers; a newline in a textarea is the
Enter key, `scroll` can turn the mouse wheel, and input actions note file
choosers and same-document URL changes. Behaviour changes: Chrome intercepts
file choosers (none opens on screen); `redirects` in `get_network_logs` are
`{url, status, location}` objects; an unset `same_site` is `null` on
Firefox too.

### Added

- Downloads are saved and reported on both browsers (#77). A click that
  downloads a file used to say only that the page reacted, and the agent
  could not find the file. Now each instance saves downloads in a directory
  of its own, the input action's reply says
  `The action downloaded data.csv (8 bytes) to <path>.` (or that the file
  is still downloading, or was canceled), and the new `list_downloads` tool
  lists them with their state, sizes and tab. The directory is
  `--downloads-dir <DIR>/<instance id>` (new flag), else a `downloads`
  folder inside the instance's temporary profile (deleted with it), else a
  new folder in the system temp directory (an attached browser, or
  `--user-profile`). A file is never replaced: Chrome saves each download
  under its guid, then the server names it `data.csv`, `data (1).csv`...;
  Firefox numbers them itself (`data(1).csv`).

### Fixed

- Chrome: `search_scripts` finds an inline script the debugger no longer
  holds. One that had already run could be released while its page sat in
  the back-forward cache, and Chrome does not announce it again when the
  page comes back, so a search for code visible in the page found nothing
  (an intermittent E2E failure). The page's `<script>` elements are now
  searched too: such a match has `scriptId: null`, `lineInScript` and a note
  to reload the page before setting a breakpoint in it. (#62)
- Input actions miss fewer effects (#81), the same on both browsers: in a
  textarea or contenteditable, `fill_input` types a newline as the Enter key
  (Chrome inserted it as text with no key; Firefox sent a key named `\n`),
  and a contenteditable whose new line is a block no longer reads as
  reformatted. `scroll` with `wheel: true` dispatches a real mouse wheel
  event at an element or the viewport's centre, for a canvas or map that
  listens to `wheel`. An action that opens a file chooser says so (Chrome
  now intercepts file choosers: none opens on screen; `upload_file` sets
  the files), and one that changes the URL without loading a page
  (`history.pushState`, a `#fragment`) names the new URL; on Firefox,
  `list_tabs` follows such URL changes too.
- Console and dialog details (#80). On Firefox, `console.count` and the
  timers keep their count and time (`counter: 2`, not `counter [x2]`), and
  its own messages (a rejected cookie, a CSP report) no longer read
  `Uncaught`. An unhandled rejection reads `Uncaught (in promise) …` on
  both browsers (Firefox: with its debugger port). Accepting a `prompt`
  without `prompt_text` answers its default, as a user pressing OK does (it
  answered `""`). A cookie without `SameSite` has `same_site: null` on both
  browsers (Firefox said `"Default"`).
- `snapshot` shows more of what assistive technology reads (#79), the same
  on both browsers: a tree item is named by its own text (`treeitem "src"`,
  not its nested items' text; it had no name), the cells of a `grid` are
  `gridcell`, and elements show `[description="…"]` (`aria-describedby`,
  `aria-description`), `[pressed=…]`, `[current=page]` and `[live=polite]`
  (an `aria-live` element without a role is now listed, as `generic`;
  `alert`, `status` and `log` are live regions).
- Accepting a `beforeunload` with `handle_dialog` returns once the
  navigation it allowed committed, and the reply names the URL reached
  (both browsers). A `navigate` right after used to race it: on Chrome it
  met the dialog again and the tab stayed blocked; on Firefox it failed with
  `Beforeunload prompt was rejected` while the tab ended on the page the
  dialog let it go to. (#76)
- Chrome: what a dedicated worker does reaches the tab's logs. Its
  requests were missing from `get_network_logs` and its script stayed
  `pending`; its `console.log` read as `info` (Chrome's `Log` domain has no
  `log` level). Each worker is now attached like a frame of another origin
  (its own session, `Runtime` and `Network`; within the 50 watched targets
  per tab), and the tab's copies of its console messages are dropped. (#75)
- `get_network_logs` shows what it used to hide (#78):
  - A response body over 2 MB has `response_body_omitted` with its size,
    instead of a bare `null`.
  - A redirected request is still one line, but it says
    `(after N redirects from <first URL>)` and `url_filter` matches any
    hop. Its `redirects` are now `{url, status, location}` objects instead
    of bare URLs.
  - A CORS block names its reason: Chrome
    `net::ERR_FAILED (blocked by CORS: MissingAllowOriginHeader)`. Firefox
    showed the response as `200 OK`; the request now fails with
    `blocked by CORS: <reason>`, read from Firefox's console error.
  - On Chrome, a request the page canceled after its response arrived (an
    `EventSource` it closed) keeps its status, with
    `canceled_by_page: true`, instead of `failed: net::ERR_ABORTED`.
  - Firefox: fetches and XHRs have a `resource_type` (`fetch`,
    `xmlhttprequest`), and request bodies are kept (`request_post_data`, with
    `request_post_data_encoding: "base64"` for a binary one).

## [0.11.0-beta.18] - 2026-11-03

Public **BETA**. Consistency fixes on both browsers. `webmcp_cancel_invocation`
cancels a pending WebMCP invocation; `webmcp_invoke_tool` waits 10 s by
default (`timeout_ms`), takes no `frameId` when one frame has the tool, and
`webmcp_list_tools` is compact. Tab tools behave the same on Chrome and
Firefox (`close_tab` defaults to the active tab, one rule for the next active
tab, `list_tabs` in tab order, Chrome registers its first tab after
`restart_browser`). Chrome `search_scripts` no longer counts or searches the
server's snippets or a previous page's scripts; `resume` and the other
debugger tools word "not paused" the same on both browsers. Chrome `emulate`
measures the page once a resize landed. Behaviour changes: `webmcp_list_tools`
answers `{frameId, tools}`; `webmcp_invoke_tool` waits 10 s instead of 30 s.

### Added

- `webmcp_cancel_invocation` (`--enable-webmcp`, Chrome) cancels a pending
  WebMCP invocation with `WebMCP.cancelInvocation`; one Chrome no longer
  knows (its page navigated away) is marked `Canceled`. (#73)

### Fixed

- Chrome: `emulate` measures what the page sees once a resize (or a reset)
  landed: the next `resize` event or two animation frames, at most 500 ms.
  Chrome answers the override before the renderer resizes, so the reply
  could hold the previous size on a fresh instance. (#74)
- Debugger tools no longer leak internals (#66):
  - Chrome: `search_scripts` neither counts nor searches the server's own
    snippets nor the scripts of a page the tab left. Its script list was
    never cleared on navigation: after a change of process Chrome reuses
    script ids, so a stale entry named the server's outline snippet, and a
    page with 5 scripts counted 38. The list is now cleared when the page's
    contexts are, and scripts created over the protocol (no URL and no
    stack) are left out; a page's own `eval` stays.
  - `search_scripts` with `debug` answers JSON (`{scripts, errors}`), not
    Rust's `Debug` format.
  - Chrome `evaluate_on_call_frame` on a page that is not paused fails with
    `the debugger is not paused`, as on Firefox and as `step_over`.
  - `resume` on a page that is not paused answers `The debugger was not
    paused: nothing to resume.` with `{paused: false}` on both browsers
    (Firefox failed with `wrongState`, Chrome answered either way).
  - Firefox RDP failures say `Firefox (RDP) protocol error in <request>`;
    they were labelled BiDi.
- Tab tools behave the same on both browsers (#69):
  - `close_tab` without `tab_id` closes the active tab, as every other tool
    omitting it acts on it (it failed with `missing field \`tab_id\``).
  - Closing the active tab activates the most recently active tab still
    open, else the newest one (by number: `tab-10` after `tab-9`). Chrome
    picked an arbitrary tab when none had been active, Firefox the highest
    id as text.
  - `list_tabs` lists the tabs by number on both browsers.
  - After `restart_browser`, Chrome registers the new browser's first tab
    (it listed none until the next call), as Firefox does.
- WebMCP tools are easier to drive (#73):
  - `webmcp_invoke_tool` waits 10 s by default (it blocked 30 s), or
    `timeout_ms` (at most 60 000); the pending reply says how to poll or
    cancel the invocation. `frameId` is optional when one frame registers
    the tool (a clear error names the frames otherwise).
  - A pending invocation whose page navigated away carries a `note` in
    `webmcp_get_invocation` and `webmcp_list_invocations` (it showed
    `Pending` forever).
  - `webmcp_list_invocations` takes `status` in any case.
  - `webmcp_list_tools` is compact JSON: `{frameId, tools}` with the frame
    once when shared, no empty `required` and no false annotations (on
    knot.kz, 789 tokens instead of ~1700).

## [0.11.0-beta.17] - 2026-11-02

Public **BETA**. Fixes found probing the debugger, tab and raw protocol tools
on both browsers. The browser's front tab now stays on the active tab (pages
in background tabs were hidden, with `requestAnimationFrame` stopped and
Chrome screenshots timing out); a click whose handler pauses or opens a
dialog after an `await` is noted at once; Chrome `snapshot`,
`find_in_snapshot` and `emulate` answer on a paused page; Firefox
`search_scripts` sees inline scripts and modules, `step_over` and `resume`
report a breakpoint hit again in a loop, and `get_custom_events` keeps every
subscribed event and RDP event packets; Chrome `navigate` to a hash route
reports the new URL; `webmcp_invoke_tool` passes a page tool's error and
blocks through. Behaviour changes: `open_tab` makes the new tab active.

### Changed

- `open_tab` makes the new tab the active one (the tools that omit `tab_id`
  act on it), as the browser shows it, Playwright MCP's `browser_tabs` and
  chrome-devtools-mcp's `new_page` do. Before, the previous tab stayed
  active while the browser showed the new one. (#68)

### Fixed

- Firefox: `profile_page_performance` with `action: "reload"` and
  `disable_cache: true` no longer fails with `Argument "ignoreCache" is
  not supported yet`: it bypasses the cache with `network.setCacheBehavior`
  (restored on every exit path). A reload Firefox aborts with
  `NS_BINDING_ABORTED` right after navigating to a page that mounts frames
  during hydration (as MDN does) is retried once after 50 ms instead of
  failing. `forced_reflows.total_ms` no longer serializes `-0.0` when there
  are none. (#58)
- Firefox: `search_scripts` finds every script of the page, not only the
  first one Firefox announced: inline scripts, modules and scripts loaded
  later were missing (`Total cached scripts: 1` on a page with five). Each
  thread is now asked for its `sources`, merged with the announced ones;
  pause locations and breakable columns use the same list. (#62)
- The browser's front tab is kept on the active tab. After a click that
  opened a tab (`target=_blank`, a popup, `window.open`), after
  `close_tab` and after a page or the user closed a tab, the active tab was
  left in the background: its page was `hidden`, `requestAnimationFrame`
  stopped (Chrome) or ran at about 1 s (Firefox), and on Chrome an element
  screenshot hung 10 s before a raw CDP timeout. On Chrome,
  `capture_screenshot` of a tab still in the background now fails at once,
  naming `switch_tab`. On Chrome, a click whose reply came before the new
  tab was registered no longer misses the "opened tab" note: it waits (at
  most 2 s) for the pages Chrome announced. (#68)
- Chrome: `snapshot`, `find_in_snapshot` and `emulate` answer at once on a
  page paused in the debugger, instead of failing after 10 s with
  `CDP Error: Command Runtime.evaluate timed out after 10s`: their
  synchronous snippets are no longer awaited (Chrome runs no microtask while
  paused). `emulate` with `geolocation` on a paused page says the position
  is read once the page resumes. (#64)
- An input action (`click_element`, `press_key`, `fill_form`, `drag`,
  `hover`) whose handler pauses in the debugger or opens a dialog after an
  async step (an `await`, a timer, a fetch) no longer waits 2 s and answers
  a plain success: the watch of the page settling ends at once and the
  reply notes `{paused, location}` or `{dialog}`, as for a handler that
  blocks right away. Both browsers. (#65)
- Chrome: `navigate` to another fragment of the current page (a hash route
  such as `#/contact`) answers the new URL; it often answered the previous
  one, read from a history entry Chrome had not updated yet. The URL now
  comes from the page's `Page.navigatedWithinDocument`. (#70)
- Firefox: `step_over` and `resume` that stop at the same place again (a
  breakpoint in a loop, hit on the next iteration) report the pause with
  its location, instead of waiting 2 s and answering `paused: false` while
  the page was paused. (#63)
- `webmcp_invoke_tool` maps a page tool's MCP-shaped result
  (`{content: [...], isError}`) to its own reply: the page's text and image
  blocks as they are, and `isError: true` as a tool error. Before, a failed
  page tool answered as a success with its error buried in the text, and
  its output came as JSON escaped inside a string. (#72)
- Firefox: `get_custom_events` gets every event the agent subscribed to
  with `send_bidi_command`, also those the server handles itself
  (`browsingContext.load`, `contextCreated`, `log.entryAdded`,
  `network.*`...), which were silently dropped. A tab opening or closing,
  and an event without a context (`script.realmDestroyed`), go to the
  active tab. The event packets an actor sends after a `send_rdp_request`
  to it (`evaluationResult` after `evaluateJSAsync`, `pageError` after
  `startListeners`) are kept too, as `rdp.<type>` (at most 16 actors
  watched). Firefox itself sends `script.realmCreated` only for a new
  tab's first document. (#71)

## [0.11.0-beta.16] - 2026-11-02

Public **BETA**. `audit_page` (`perf` group) audits the rendered page for
accessibility, SEO and best-practice problems on Chrome and Firefox, with the
same script and no Lighthouse bundle (each failing check lists its first 10
elements with their snapshot refs); `emulate` gains `cpu_throttling` and
`network_throttling` on Chrome; `profile_page_performance` adds INP, LCP image
discovery, DOM size, oversized/unsized images and third-party weight on both
browsers, and on Chrome extracts `forced_reflows` from the trace, measures
`unused_code` (JavaScript and CSS) on a reload and saves the raw trace with
`save_path`; `get_performance_metrics` takes `collect_garbage: true` and reads
`Memory.getDOMCounters` on Chrome. Behaviour changes: `get_performance_metrics`
sorts its metrics by name.

### Added

- `emulate` gains `cpu_throttling` (`1` to `20`, e.g. `4` for a 4× slowdown)
  and `network_throttling` (`slow-3g`, `fast-3g`, `4g` or `none`; Chrome
  DevTools names such as `Slow 3G` and `Fast 3G` are accepted too) on Chrome,
  kept in the tab's emulation state across calls, reported in the reply and
  cleared by `reset: true`. On Firefox, where WebDriver BiDi has no CPU or
  bandwidth/latency throttling, they fail naming them. (#52)
- `profile_page_performance` with `insights: true` adds Interaction to Next
  Paint (`inp`: worst interaction, target and input/processing/presentation
  breakdown), LCP image discovery (`lazy_loaded`, `fetch_priority`), DOM
  size (`nodes`, `max_depth`, `widest_element`, `max_children`), oversized
  or unsized images (`images`) and third-party origins (`third_parties`),
  with findings when they cross the web.dev and Lighthouse thresholds, on
  both Chrome and Firefox. (#53)
- `audit_page` (`perf` group) audits the rendered page for accessibility
  (images without alt text; fields, buttons and links without a name;
  `html lang`; heading order; duplicate ids; broken `aria-labelledby` and
  `aria-describedby`; `tabindex` above 0; targets under 24×24 px too close
  to another; text contrast under WCAG AA), SEO (title, meta description,
  viewport, vague link text, uncrawlable links, canonical, `noindex`) and
  best practices (doctype and quirks mode, HTTPS, mixed content, distorted
  images, console errors), with the same script on Chrome and Firefox and
  no Lighthouse bundle. Each failing check lists its first 10 elements with
  a snapshot ref, a selector, its start tag and a detail; `categories`
  narrows it. (#55)
- `get_performance_metrics` takes `collect_garbage: true`, which collects
  garbage before reading on Chrome (`HeapProfiler.collectGarbage`), and adds
  the live `Documents`, `Nodes` and `JSEventListeners` of
  `Memory.getDOMCounters`, so reads after repeating an action show what it
  leaked; its metrics are sorted by name. On Firefox, where WebDriver BiDi
  cannot force a collection, a note says so. (#57)
- `profile_page_performance` with `insights: true` and `action: "reload"`
  adds `unused_code` on Chrome: the bytes of JavaScript that never ran and
  of CSS that matched nothing during the load (`Profiler` precise coverage
  and `CSS` rule usage, as Lighthouse's "Reduce unused JavaScript / CSS"),
  with the 5 files that waste most and a finding over 50 KB. Without a
  reload, and on Firefox (no coverage over WebDriver BiDi), it is `null`
  with a note saying why. (#54)
- `profile_page_performance` on Chrome extracts `forced_reflows` from the
  trace when `insights: true` (`Layout` and `UpdateLayoutTree` events with a
  JavaScript stack, grouped by call site, with a finding over 30 ms; `null`
  with a note on Firefox) and takes `save_path` (`.json` or `.json.gz` under
  the upload roots) to write the raw trace for Chrome DevTools or Perfetto
  (refused on Firefox before running). (#56)

### Fixed

- `snapshot` names an element by the text slotted into its shadow tree
  (a web component's button labelled by a `<span><slot></span>`):
  `innerText` does not see slotted nodes, so it had no name. (#55)

## [0.10.0-beta.15] - 2026-11-02

Public **BETA**. Fixes from a comparison round against Playwright MCP,
chrome-devtools-mcp and a Puppeteer MCP (now repeatable with
`make compare`): `snapshot` lists a native `<select>`'s options, marks
rejected fields `[invalid]` with their error, and gives refs to draggable
elements and drop zones; `fill_input` picks several options of a
`<select multiple>`; `press_key` takes `Control+a`; `navigate` and `reload`
name HTTP authentication (Firefox waited 30 s on its prompt). Behaviour
changes: on a `<select multiple>`, `fill_input` selects exactly the options
its text names (it added one); the error for a missing option names it.

### Fixed

- `snapshot` and `find_in_snapshot` list the options of a native `<select>`
  under its combobox, with `[selected]` and `[disabled]` and optgroups as
  groups: only the chosen one showed, so an agent had to guess a label to
  learn the choices. `snapshot` lists the first 25 and says how many more;
  `find_in_snapshot` searches them all. Options have no ref: `fill_input`
  picks one by its label or value. (#46)
- `navigate` and `reload` to a page behind HTTP authentication (401) fail at
  once saying so, with the scheme and realm of its challenge and how to log
  in (the credentials in the URL, which the browser keeps for the session):
  Chrome only said `net::ERR_INVALID_AUTH_CREDENTIALS`, and Firefox showed
  its prompt while the call waited 30 s and answered the previous page.
  (#50)
- `press_key` takes a key written with its modifiers, `Control+a`,
  `Shift+Tab` or `ControlOrMeta+c` (Meta on macOS, Control elsewhere), as
  agents write it after Playwright; they add to `modifiers`. It failed with
  `Unknown key 'Control+a'`. (#51)
- `get_network_logs`' note on responses without a body says they are still
  loading or were never read by the page: Chrome never finishes a `fetch`
  whose body the page did not consume, and the old wording (an open event
  stream, no body until it ends) suggested that waiting would help. (#51)
- `snapshot` marks a field the page or the browser rejects `[invalid]`
  (`aria-invalid`, or `:user-invalid` once the user changed it or tried to
  submit), with `[error="..."]` from its `aria-errormessage`,
  `aria-describedby` or the browser's validation message: after a submit an
  agent could not tell which fields were rejected. (#49)
- `snapshot` gives a ref to an element made draggable (`draggable="true"`,
  marked `[draggable]`) and to a drop zone (an `ondrop` or `ondragover`
  attribute): both were plain text, so `drag` needed CSS selectors. (#48)
- `fill_input` (and `fill_form`) on a `<select multiple>` selects exactly
  the options named one per line of `text` (none for an empty one), firing
  `input` and `change` once; an unknown line fails naming it, with nothing
  changed. It could only add one option at a time. (#47)

## [0.10.0-beta.14] - 2026-11-01

Public **BETA**. `emulate` gains `user_agent`, `locale`, `timezone` and
`geolocation` on both browsers, and each call now adds to the earlier ones;
input actions say when they opened a tab; `evaluate_js` takes top-level
`await`; Chrome's snapshot reads closed shadow roots. Behaviour changes:
`navigate` and `reload` fail at once when the page pauses at a breakpoint or
opens a dialog while loading (Chrome waited 30 s); with a modal `<dialog>`
open, `snapshot` lists only the dialog; on Firefox a navigation that becomes
a download fails, as on Chrome; `emulate`'s reply has `user_agent`, `locale`
and `timezone`.

### Fixed

- Chrome: `snapshot` and `find_in_snapshot` read closed shadow roots (some
  design systems and third-party widgets), whose elements were invisible:
  Chrome finds them (`DOM.describeNode` with `pierce`) and hands them to
  the outline for that call only, so the page's own scripts never get them;
  their elements are then reached by ref like any other. Pages up to 20 000
  elements; `host >>> inner` selectors still stop at a closed root. Firefox
  has no equivalent: there they stay out of reach. (#42)
- Firefox: `navigate` to a URL Firefox saves rather than opens (a type such
  as CSV, without `Content-Disposition: attachment`) answered "Navigated
  to" the previous page; it now fails as a download, as on Chrome, from
  Firefox's `browsingContext.downloadWillBegin`. (#39)
- `evaluate_js` takes `await` at the top level (`const r = await
  fetch(u); r.json()`), answering the last statement's value as a console
  does: it failed with a SyntaxError on both browsers. A symbol is described
  as `Symbol(x)` on both (Firefox answered an empty object), and a function
  on Chrome by its code (it answered `{}`). (#43)
- `emulate`: each call adds to the earlier ones, for every option (Chrome
  kept an earlier viewport but dropped the earlier media settings), and it
  emulates `user_agent`, `locale`, `timezone` and `geolocation` on both
  browsers (the page is granted the geolocation permission, and the reply
  says the position it reads). An unknown time zone or locale fails without
  changing anything (Chrome kept the old zone silently). (#43)
- `click_element`, `press_key` and the other input tools say when the
  action opened tabs (a `target=_blank` link, `window.open`), naming each
  with its URL: the new tab was registered, but nothing told the agent to
  look at it. (#41)
- `snapshot` and `find_in_snapshot` show `<progress>` and `<meter>`
  (`progressbar`, `meter`, with their value as `40/100`), say whether a
  `<details>` summary is expanded, mark an open modal `<dialog>` `[modal]`,
  and leave out what the page made inert: `inert` subtrees and, while a
  modal dialog is open, everything outside it (it was listed, and clicking
  it failed as covered). (#40)
- Chrome: `navigate` and `reload` to a page that pauses at a breakpoint
  while it loads (in the page or in a frame of another origin) waited the
  whole 30 s, then answered as if the page were slow. They now fail at once
  naming where it paused, as on Firefox; `resume` lets the page finish
  loading. A page opening a dialog while it loads (`alert()` in an inline
  script) fails at once too, pointing to `handle_dialog`. (#45)

## [0.9.0-beta.13] - 2026-11-01

Public **BETA**. Frames of another origin are now observed and debugged, not
only read and acted in: on Chrome their console messages and requests (bodies
included) join the tab's logs, and on both browsers the debugger searches their
scripts, binds breakpoints in them and pauses, steps and resumes there.
Behaviour changes: a click on a disabled control now fails instead of reporting
success, a pointer action waits up to 2 s for its element to appear or be
enabled, date, time, range and colour inputs get their value set rather than
typed, and on Chrome the scripts of such frames have ids naming the frame
(`2.17`).

### Fixed

- Chrome: `get_console_logs` and `get_network_logs` now include the
  console messages and requests of frames of another origin (payment forms,
  widgets, ads), which run in a process of their own and were missing:
  each tab attaches to its frames as they appear, before their scripts run,
  and feeds their messages and requests to the tab's logs (at most 50
  frames per tab). Their response bodies are read from the frame. Firefox
  already reported them. (#36)
- Chrome: the debugger now reaches the scripts of frames of another
  origin. `search_scripts` finds them (their ids name the frame, `2.17`,
  as each frame numbers its scripts like the page), `set_breakpoint` by URL
  or hash binds in every frame (also frames that load later, before their
  scripts run) and by script id in its frame, and a pause in a frame is
  reported to the call that caused it, inspected, stepped and resumed
  there. A frame's own `debugger;` statements are skipped without
  breakpoints, as the page's. (#36)
- Firefox: the debugger reaches the scripts of frames of another origin too
  (Firefox runs them in their own process, with their own thread):
  `search_scripts` finds them, `set_breakpoint` by URL binds in them, and a
  pause in a frame is inspected, stepped and resumed in its thread. Needs
  `bidi-lite` 0.1.5 through `bidi-browser-lite` 0.1.6. (#36)
- Chrome: a click (or any mouse action) into a frame of another origin
  right after the page loaded was sometimes lost while reported as done:
  Chrome routes input to such an out-of-process frame only once it has
  registered it, and until then the parent page got the event on the
  `<iframe>`. Before the first mouse event into such a frame, the mouse now
  moves to the point until the frame sees it (at most about 1 s). (#44)
- `fill_input` and `fill_form` set `date`, `time`, `datetime-local`,
  `month`, `week`, `range` and `color` inputs: the text was typed into the
  date's segments (Chrome held `61031-02-02` for `2026-10-31`; Firefox left
  it empty), and range and colour inputs were refused. The value is now set
  through the input's native setter, with `input` and `change` events, in
  the format each type expects (`YYYY-MM-DD`, `HH:MM`, a number,
  `#rrggbb`); a range that snaps the value to its step or bounds is noted,
  and a malformed value fails naming the format, leaving the old one. (#38)
- `click_element`, `drag` and the checkboxes of `fill_form` no longer
  report success on a disabled control: its click handler never runs, so
  they now fail with "is disabled; nothing was clicked" and send no input.
  They also wait up to 2 s for an element a CSS selector does not find yet,
  or a control that is disabled, as pages add elements and enable buttons a
  moment after the previous action; the error says how long it waited. A
  ref that names nothing, a hidden or a covered element still fail at once,
  and `hover` on a disabled control works as before. (#37)

## [0.8.0-beta.12] - 2026-10-31

Public **BETA**. Reaches into frames of another origin (payment forms,
embedded editors, playgrounds, widgets), which Playwright MCP already did:
`snapshot` reads them in place with frame refs (`f2e5`), and the input tools
act in them through those refs, on Chrome and Firefox. Behaviour changes:
`snapshot` and `find_in_snapshot` now show the content of such frames
instead of a "not readable" line, iframes read get a `[frame=f2]` mark, and
selectors starting with `f` and a number (`f2e5`, `f2 >>> css`) name
elements of those frames.

### Added

- Frames of another origin (payment forms, embedded editors and playgrounds,
  widgets): `snapshot` and `find_in_snapshot` read them in place, at most 20
  and 5 deep, marking each iframe with its frame number (`[frame=f2]`) and
  prefixing its refs (`f2e5`); `snapshot` with `selector: "f2"` outlines one
  frame. Every selector argument takes those refs and `f2 >>> css`:
  `click_element`, `hover`, `drag`, `fill_input`, `fill_form`, `press_key`,
  `scroll`, `wait_for` and `upload_file` check and act in the frame. Chrome
  reaches an out-of-process frame through its own target (attached for the
  call) and an in-process one through its document, and sends mouse events
  to the tab offset by where the frame shows; Firefox acts in the frame's
  browsing context. Before, such a frame was a "not readable" line and
  nothing inside it could be reached. `evaluate_js` still runs in the page,
  and `capture_screenshot` refuses an element inside such a frame. (#27)

## [0.7.0-beta.11] - 2026-10-31

Public **BETA**. Closes gaps with Playwright MCP and chrome-devtools-mcp and
fixes editors found in real pages: a new `drag` tool (sortable lists,
boards, sliders, HTML5 drag and drop), `navigate` back and forward in the
tab's history, `fill_input` with `append`, and `capture_screenshot` of one
element; `fill_input` now types into Monaco, CodeMirror 6 on Firefox and
TinyMCE on Firefox, and `snapshot` shows the value of ARIA comboboxes,
spinbuttons and sliders. Behaviour changes: the default tool list has one
more tool (about 390 more tokens), `navigate`'s `url` is optional (pass it
or `history`), and `fill_input` on an editor whose content the page does
not expose answers that the text was not checked.

### Added

- `drag` tool: drags an element onto another with the mouse. It waits for
  running animations to end (a sortable list animating its last
  reordering starts no drag), presses on `from` (checked like
  `click_element`), moves in 10 steps 40 ms apart to the centre of `to` and
  releases, so pointer-driven libraries (sortable lists, boards, sliders)
  and HTML5 drag and drop both work. Chrome does both natively; Firefox's
  WebDriver input starts a native HTML5 drag but never drops it, leaving the
  page stuck mid-drag, so there the native `dragstart` is cancelled and the
  drag replayed by script along the path (the reply says so). Checked on
  SortableJS and the-internet's drag and drop on both browsers. (#30)
- `fill_input` takes `append: true`: the text is typed at the end of what
  the field already holds instead of replacing it, in text fields,
  textareas and contenteditable elements, rich text editors included (the
  caret goes to the end of their last text, so the editor records the
  edit). A number, email or date field, which has no caret to move, is
  emptied and retyped whole. The reply says `appended`; a field that did
  not change is an error. Adding to a draft took one `press_key` per
  character before. (#32)
- `navigate` takes `history: "back"` or `"forward"` instead of `url`: it
  moves one entry in the tab's history and waits for that page to load,
  answering like a URL navigation (`Went back to <url>.`). A page restored
  from the back-forward cache, or an entry of the same document
  (`pushState`), answers at once; at either end of the history it fails
  without moving. Before, going back took `evaluate_js` with
  `history.back()`, which answered before the page loaded. (#31)
- `capture_screenshot` takes a `selector` and captures only that element
  (CSS, `host >>> inner` into shadow roots and iframes, or a snapshot ref):
  scrolled into view, clipped to its box, whole even when taller than the
  viewport (at most 16 384 px per side, with a note). Checking one component
  no longer costs a whole viewport image. (#33)

### Fixed

- On Firefox, `fill_input`, `fill_form` and `press_key` type into an
  element inside an iframe (TinyMCE): its script focus left the top
  document's focus on its `<body>`, where Firefox sent the keys, so nothing
  reached the editor and `fill_input` reported that the field held
  "something other than the text". Each iframe the element is in is now
  focused first. (#35)
- `fill_input` replaces the content of CodeMirror 6 on Firefox: the text
  was typed before the old content, because CodeMirror puts its own caret
  back once focused and drops the selection #23 made with a DOM range. A
  contenteditable element now also gets the keys a user presses after the
  range: Control+A (Command+A on macOS) to replace, Control+End with
  `append`. Checked with CodeMirror 6, ProseMirror, Lexical and the test
  page's editor on both browsers. (#34)
- `fill_input` types into editors of their own, such as Monaco (VS Code
  for the web): the snapshot named Monaco's input `textbox "JavaScript
  Input"`, but `fill_input` refused it (not an input, textarea or
  contenteditable element) on Chrome, where it is an `EditContext`
  element, and on Firefox, where it is a hidden textarea, reported success
  while the editor never got the text. Such an editor (an `EditContext`,
  a focusable `textbox` role, or a text field without a visible box) now
  gets a click on its visible container when its input has none, Control+A
  (Command+A on macOS) or, with `append`, Control+End, then the text; its
  content cannot be read back, so the reply says it was not checked. (#28)
- `snapshot` and `find_in_snapshot` show the value of ARIA widgets: an
  `<input role="combobox">` its text (MUI Autocomplete after a pick), a
  spinbutton or slider its `aria-valuetext` or `aria-valuenow` (MUI X
  DatePicker sections), and a button-like combobox the option it shows
  (Radix Select). Before, only text fields, sliders and `<select>` had one,
  so the agent could not see that a pick took. (#29)

## [0.6.0-beta.10] - 2026-10-31

Public **BETA**. Fixes four input and snapshot bugs found while driving a
real web app (Discord): `fill_input` now types into rich text editors
(Slate, Lexical, ProseMirror) so the editor records the edit, styled
checkboxes covered by their own label are clicked, controls shown only on
hover are revealed and clicked, and `snapshot` lists the links and buttons
inside a clickable container. Behaviour change: a covered element now gets
a mouse move before the click is refused.

### Fixed

- `click_element`, `hover` and `fill_form` reach controls shown only while
  the mouse is over something (a message's actions, a row's toolbar): a
  covered element now gets the mouse first and is checked again 100 ms
  later. Discord's message "More" button failed with "covered by
  'div.message…'". An element that stays covered still fails, naming what
  covers it, without a click. (#26)
- `snapshot` and `find_in_snapshot` list what a clickable container holds:
  an element without a role that has a click handler or a pointer cursor
  was a leaf named from its whole text, so a container that sets
  `cursor: pointer` hid every link and button inside it (Discord's whole
  channel list was one line). A clickable that holds interactive elements
  now lists them; one with text only is still one named line. (#25)
- `click_element`, `hover` and `fill_form` click a control covered by its
  own label (or the element its `aria-labelledby` names), as styled
  checkboxes and switches are: they failed with "covered by 'span…'". An
  element under anything else still fails, naming it. (#24)
- `fill_input` and `fill_form` work in rich text editors (Slate, Lexical,
  ProseMirror, Draft.js): a contenteditable element was emptied with
  `textContent = ''`, which removed the nodes the editor renders from its
  own model, so the typed text showed in the page but the editor did not
  record it (Discord's channel topic was never saved), and the tool
  reported success. The content is now selected and the first typed key
  replaces it, as when a user selects all and types; an empty text deletes
  it through the editing commands. (#23)

## [0.6.0-beta.9] - 2026-10-31

Public **BETA**. Spends fewer tokens and answers what the agent needs, after
a side-by-side study with Playwright MCP and chrome-devtools-mcp (now in the
README): `get_network_logs` and `get_console_logs` answer one line per entry
(details on demand), `snapshot` no longer repeats table text, input actions
wait for the page to settle, and `profile_page_performance` explains its
numbers with `insights`. Behaviour changes: the two log tools answer text
instead of JSON unless `include_details: true`, layout tables leave only
their content in `snapshot`, and input actions take ~50 ms more when the
page does not react.

### Added

- `profile_page_performance` takes `insights: true`: what explains the
  numbers, read in the page with the Web Performance APIs on both browsers:
  the document's TTFB, server time and compression, the LCP element with
  its breakdown (TTFB, load delay, load, render delay), layout-shift
  culprits, render-blocking requests, the slowest requests and the scripts
  of long animation frames (at most 5 each), and `findings` from the web.dev
  thresholds (LCP over 2.5 s, CLS over 0.1, a slow server, an uncompressed
  document, render-blocking requests). Firefox reports no layout shifts,
  render-blocking status or long frames: those are `null`, with notes. The
  reply grows from ~60 to ~600-700 tokens on Wikipedia. (#22)

### Changed

- `click_element`, `hover`, `fill_input`, `fill_form` and `press_key` wait
  for the page to settle before answering: until the DOM stays unchanged for
  50 ms (style changes aside) and the requests the action started finished,
  at most 1 s, on both browsers. They answered in 1-2 ms, before the page
  reacted, so a read right after a click that fetches data saw the old page.
  An action nothing reacts to now takes ~50 ms and keeps its reply; one the
  page reacts to adds how (`The page reacted (the DOM changed, 1 request(s))
  and settled after 134 ms.`), or that it was still changing after 1 s. Not
  done when the action left a dialog open or the page paused. (#21)
- `snapshot` and `find_in_snapshot` no longer print a table's text two or
  three times. A table without headers (`th`, `thead`, `caption`,
  `summary`) and without a role is layout: its `table`, `row` and `cell`
  lines are left out and only their content stays, as in Chrome's
  accessibility tree. A cell is named by its content only when that content
  is plain text (folded into the name, up to 100 characters); a cell with
  links or controls keeps them as children, unnamed. Hacker News went from
  ~8 500 to ~4 500 tokens. (#20)
- `get_console_logs` answers one line per message by default: `[level]
  text @ url:line` (lines from 1), repeats in a row folded (`[x3]`),
  uncaught errors marked (`Uncaught ...`) with up to 5 stack lines
  indented; `include_details: true` keeps the previous JSON. A page with
  six messages went from ~550 to ~150 tokens. (#19)
- `get_network_logs` answers one line per request by default (`id METHOD
  url => status [type]`, oldest first), then one per WebSocket frame with a
  payload preview: ~100 tokens for a page of four requests instead of ~2 250.
  The new `request_id` shows one request's headers and body;
  `include_details: true` (now off by default) keeps the previous JSON with
  every request's details. (#18)

### Fixed

- `get_console_logs` previews objects and arrays as the console shows
  them (`hello {n: 1, s: 'x'} [1, 2, 3]`): Chrome printed `Object`, Firefox
  `Object(3) Array(3)`. On Chrome a `console.*` call keeps its location
  (`url`, `line_number`), which was always empty, and `undefined`
  arguments are printed. (#19)
- `get_network_logs` no longer lists the browser's own requests: a freshly
  launched Chrome logged its new-tab page (`chrome://resources/...`, `data:`
  images) before the first navigation. `chrome://`, `devtools://`, `data:`,
  `about:` and extension URLs are not logged. (#18)

## [0.5.0-beta.8] - 2026-10-31

Public **BETA**. Closes the rest of the Playwright MCP comparison: the new
`network` tool group mocks requests (`mock_route`, `list_routes`,
`remove_route`) and takes a tab offline (`set_offline`), `save_pdf` prints
a tab to PDF, and `--secrets` keeps known values out of what the agent
reads. Debugging a paused page works better: `evaluate_js` answers at once
on Chrome, a pause names its script, and the input tools fail at once
naming the pause instead of reporting a click or a text that has not
happened (a behaviour change: they used to "succeed").

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

