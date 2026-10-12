# Changelog

All notable changes to `browser-control-mcp` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

`browser-control-mcp` is the successor of `chrome-debug-mcp` 1.5.6 and starts
from a copy of its code: entries describe the differences from that release.

## [Unreleased]

### Added
- Project scaffolding: quality gates (`make check`: format, clippy with
  warnings as errors, tests, docs, duplication, packaging and version
  consistency), CI on Linux, macOS and Windows, `AGENTS.md`, MIT licence.
- The server code of chrome-debug-mcp 1.5.6 (35 tools, Chrome over CDP),
  copied without its history and renamed: crate and binary
  `browser-control-mcp`, server title "Browser Control MCP".

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

### Fixed
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

### Security
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

