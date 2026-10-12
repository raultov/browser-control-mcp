# AGENTS.md — conventions for browser-control-mcp

Rules for autonomous agents working in this repository. Specs and phase plans
(`docs/specs/`, gitignored) are local-only working documents provided by the
human; they are never committed and may be absent in a fresh clone. This file
is the standing contract.

## What this project is

An MCP server, written in Rust, that lets an LLM agent **control and debug**
real browsers: Chrome/Chromium over the Chrome DevTools Protocol, and Firefox
over WebDriver BiDi plus the Firefox Remote Debugging Protocol for the
JavaScript debugger. Its differentiator is runtime JavaScript debugging
(breakpoints, pause on load, call frames, evaluation in a paused frame,
stepping, script search) on both browsers. It is the successor of
`chrome-debug-mcp` and started from a copy of its code (not its history).

Browser lifecycle and protocol clients live in sibling crates:
`cdp-browser-lite` → `cdp-lite` (Chrome) and `bidi-browser-lite` →
`bidi-lite` (Firefox). This repo owns the protocol-neutral `Backend`
abstraction and the MCP tools. Do not translate CDP into BiDi: each backend
implements the semantic operations with its own protocol.

## Ground rules

1. **Write only inside this repository** (plus the E2E profile homes used by
   `tests/firefox.sh` and `tests/chrome.sh`: `~/snap/firefox/common/browser-control-mcp/`
   with snap Firefox, `~/.local/share/browser-control-mcp/e2e/` otherwise, or
   `$BROWSER_CONTROL_MCP_E2E_HOME`). Never write into sibling checkouts
   (`../cdp-lite`, `../cdp-browser-lite`, `../bidi-lite`, `../bidi-browser-lite`,
   `../chrome-control-mcp`); write needed changes down as requests for the human.
2. **Rust:** edition 2024, `rust-version = "1.98"`; **no `unsafe`** (denied);
   **no `#[allow]`** (`allow_attributes` is a warning and warnings are
   errors); `#[expect(..., reason = "...")]` only as a last resort and reported
   to the human. Fix clippy/dupes findings by refactoring, never by silencing.
   Never loosen `clippy.toml` or `dupes.toml`; only tighten them, recording the
   measured curve.
3. All code, comments, docs, README and CHANGELOG in **English**.
4. **Cross-platform** (Linux, macOS, Windows): no shelling out and no
   OS-specific paths in `src/`; browser launching lives in the lifecycle
   crates. `cfg(unix)` only for SIGTERM handling. Shell scripts live only in
   `tests/` and are not part of `make check`.
5. **Server hygiene:** no `unwrap`/`expect`/`panic!` outside tests (a panic
   kills the agent's session); typed errors (`BackendError`, `ToolError`);
   every public item has `///` docs explaining the *why*; tool errors are
   clear messages, never a panic or a silent empty result.
6. **Secrets never reach logs.** Logs (tracing, stderr) carry tool names,
   instance/tab ids, protocol method names, actors, types, sizes and counts —
   never params, payloads, results, header values, cookies, `Args` or URLs
   with queries. Proxy passwords travel in `continueWithAuth`; tests assert
   they never appear in captured logs.
7. **Tests:** BDD scenarios from the specs become Rust tests inside the module
   they test (`#[cfg(test)] mod tests`), snake_case after the behaviour, with
   a `/// <ID>: <scenario>` doc line and `// Given`, `// When`, `// Then`
   comments. TDD: the failing test first (it must compile and fail for the
   expected reason). **Hermetic gate:** tests never start a browser, never
   touch the network and only bind `127.0.0.1:0` (in-process mocks of the
   lifecycle crates and `backend::mock`). Real traffic is captured once into
   `tests/fixtures/` (origins in `tests/fixtures/SOURCES.md`) and loaded with
   `include_str!`. Real-browser tests are `#[ignore]`d.
8. After every task `make check` must be green before committing, and the
   commit is conditioned on it (`make check && git add <files> && git commit`).
   User-visible change → `CHANGELOG.md` `[Unreleased]` in the same commit;
   public behaviour change → `README.md` in the same commit. Commits:
   `phaseN(FN.x): <summary>`, one per task.
9. If a spec is wrong about a protocol, an API or a path: fix the spec first,
   then the test, then the code, and log the deviation in the roadmap.
10. Git identity: `raultov <raultov@users.noreply.github.com>` (local config).
    No other e-mail in commits, files or metadata (`Cargo.toml` authors use
    `raultov@protonmail.com`).
11. Never commit `target/`, `.idea/`, `.knot/`, `docs/specs/`, `PROMPT.md` or
    scratch. Stage files explicitly (no `git add -A` / `.`). Push, merge, tag,
    `cargo publish`, npm publish or registry submissions only when the human
    explicitly asks; never push to `master`/`main` unprompted.
12. Use the knot MCP tools (`search_hybrid_context`, `find_callers`,
    `explore_file`) when reading indexed code (the sibling crates).

## Architecture

- **One tool list, many instances.** The handler exposes one tool table;
  each tool resolves the instance (`instance_id`, default `default`) and tab
  (`tab_id`, default active), checks the capability it needs, then calls the
  instance's `Backend`.
- **`Backend` trait + facets** (`src/backend/mod.rs`): core operations every
  browser supports, plus optional facets (`debugger()`, `performance()`,
  `tracing()`, `webmcp()`, `proxy_auth()`, `raw(protocol)`,
  `cookie_import()`). A missing facet is a capability error that names the
  tool and the browser; nothing is sent to the browser.
- **An instance never changes browser.** Chrome instances use `CdpBackend`
  (CDP); Firefox instances use `BidiBackend` (one shared BiDi session, events
  routed by context id; debugger over RDP).
- **Firefox sessions are single and outlive their socket.** Every shutdown
  path (`close_instance`, `stop_browser`, stdin EOF, Ctrl-C, SIGTERM) ends
  them (`instances/shutdown.rs`).
- **Ids are MCP-level:** instances `default`/label/`<browser>-N`; tabs
  `tab-N` mapped to CDP target ids or BiDi context ids.
- **Bounded state:** every observation buffer has a cap; every spawned task
  is owned by a `ListenerHandles` (abort on drop) and never holds a strong
  reference to its owner.

## Layout

```
src/
├── main.rs           thin binary: parse Args, call browser_control_mcp::run()
├── lib.rs            crate docs, run(): server info, handler, stdio server
├── cli.rs            Args (clap)
└── handler/          imported from chrome-debug-mcp 1.5.6 (CDP only, being reshaped)
    ├── mod.rs            McpHandler, per-session state, tool dispatch
    ├── schema_compat.rs  schema normalisation for strict tool-calling engines
    ├── cdp_domains/      one dir per CDP domain + event pump
    └── chrome_instance/  instance/tab registries, Chrome launch, cookie seeding
```

Planned (specs 00 §5): `server_info.rs`, `instances/`, `backend/{cdp,bidi}/`,
`tools/`, `util/`; `tests/` with fixtures, the stdio E2E driver and the
browser scripts. This section is updated whenever the tree changes.

## Gates

```sh
make check        # fmt-check, clippy -D warnings, test, doc, dupes (0), publish-check, version-check
make test-lib     # fast loop, not a gate
make e2e          # real Chrome + Firefox over stdio MCP (visible windows; not in make check)
make check-all    # make check + make e2e
cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu -- -D warnings   # Windows cross-check
```

## Running browsers for the smoke

```sh
tests/chrome.sh start    # visible Chrome, CDP on :9333
tests/firefox.sh start   # visible Firefox (never headless), BiDi on :9444/session, RDP on ws:6100
make e2e
tests/firefox.sh stop; tests/chrome.sh stop
```

Firefox allows a single BiDi session: always stop Firefox instances. With
snap Firefox, profiles must live under `$HOME`, never `/tmp`. Firefox refuses
top-level navigation to `data:` URLs: test pages are served on `127.0.0.1`.

## Adding a tool

1. Write the BDD scenario in the spec (both browsers: supported, partial or
   capability error), then the failing tests (tool against `MockBackend`;
   backend method against the protocol mock, asserting the exact wire params).
2. Add the neutral operation to `Backend` or a facet (request struct if more
   than 3 arguments besides `&self`), implement it in each backend that
   supports it; declare the capability.
3. Add the tool struct (`#[mcp_tool]`, `JsonSchema`) in `src/tools/`, its
   entry in the tool table with its capabilities and flag gate, and a
   description that states browser limits.
4. Update `server_info` instructions if relevant, the README feature matrix
   and CHANGELOG.

## Plans

Specs and phase plans (`docs/specs/`, gitignored) are provided by the human
for execution. Their roadmap file holds the phase order, the checklist and the
deviation log.
