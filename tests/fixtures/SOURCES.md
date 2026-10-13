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
