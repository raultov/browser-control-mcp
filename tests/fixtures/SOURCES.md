# Fixture origins

Real messages captured from Firefox 157.0.1. Sanitised: local ports become
`127.0.0.1:8000`, profile paths and process ids removed, user agent generic.
The `Cookie`/`Set-Cookie` headers in the network fixtures are the probe's own
test cookie (`probe=1`), kept on purpose for the redaction tests.

| File(s) | Origin |
| --- | --- |
| `bidi/event-load.json`, `bidi/event-log-entry.json`, `bidi/get-tree.json`, `bidi/session-new*.json`, `bidi/session-status-busy.json`, `rdp/*.json` without `bbl_` prefix | copied from `bidi-lite/tests/fixtures/` (its spike, Firefox 157.0.1; see that repo for hand-made ones: `rdp/source-long-string.json`) |
| `bidi/bbl_*.json`, `rdp/bbl_*.json` | copied from `bidi-browser-lite/src/test_support/fixtures/` (its spike, Firefox 157.0.1) |
| `bidi/script-evaluate-*.json`, `bidi/input-perform-actions.json`, `bidi/capture-screenshot.json` (data truncated), `bidi/event-*.json` (except `event-load`/`event-log-entry`), `bidi/session-capabilities.json` | browser-control-mcp F5 spike, `docs/specs/spikes/transcripts/f5_main.jsonl` |
| `bidi/network-get-data.json` | F5 spike, `f5_short_headless.jsonl` |
| `bidi/navigate-complete.json` | F5 spike, `H2 navigate` (re-typed from the logged result; same values) |
| `bidi/navigate-unknown-host.json` | F11.3, 2026-10-13: Firefox 157 answered `browsingContext.navigate` to `http://nonexistent-host.invalid/` with error code `unknown error` and message `Error: NS_ERROR_UNKNOWN_HOST` (read through `send_bidi_command`; the frame around them re-typed, `stacktrace` emptied). The same Firefox answered `Error: deniedPortAccess` for `http://127.0.0.1:1/` |
| `cdp/runtime-evaluate-*.json` | F11.4, 2026-10-13: Chrome 155.0.8059.39 (headless) through `send_cdp_command`: `Runtime.evaluate` of `x_not_defined` and `(()=>{throw 'boom'})()` (`returnByValue: true`, frames re-typed with an id), and the `result` RemoteObjects of `returnByValue: false` evaluations on `data:text/html,<div id="t" class="a b">x</div>` (keyed by what was evaluated; `object_by_value` is the `Runtime.callFunctionOn` `return this` reply) |
| `bidi/script-evaluate-values.json`, `bidi/script-evaluate-throw-string.json` | F11.4, 2026-10-13: Firefox 157.0.1 through `send_bidi_command` (`script.evaluate` of an array of non-JSON values, and of `(()=>{throw 'boom'})()`); frame re-typed with an id |
| `bidi/event-navigation-committed.json` | F11.6, 2026-10-13: Firefox 157.0.1, `browsingContext.navigationCommitted` after navigating to `http://github.com/` (read through `get_custom_events`; the context id replaced by the one the other event fixtures use) |
| `cdp/webmcp-tools-added.json` | RW-17, 2026-10-28: Chrome 156 (headless, `--enable-webmcp`), the `WebMCP.toolsAdded` event of `document.modelContext.registerTool({name: 'pay', ..., annotations: {consequentialHint: true, untrustedContentHint: true, ...}})` read through `get_custom_events` (stack trace cut to one frame) |
