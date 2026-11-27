# Browser MCP comparison

Runs the same tasks on several browser MCP servers and records, for each
task and server, whether it worked and the calls, time and tokens it took.
It is how test rounds against other servers are made repeatable: a task is
a scenario with a check of the page's state, not an impression.

Not part of `make check` (it starts real browsers and fetches the other
servers with `npx`). Python 3.10+, standard library only.

```sh
make compare                                   # release build, then every server
make compare COMPARE_ARGS="--servers bc,pw"    # some servers
python3 -I tests/compare/run.py --only 'frames|shadow' --level 2
python3 -I tests/compare/run.py --save-baseline   # keep a run you trust
```

`bench.py` measures the README's comparison table ("Measured": tool lists,
snapshots of live pages, a login, latencies, edge cases) on `bc`, `pw`, `cd`
and `se`, and saves the numbers as JSON:

```sh
make build && python3 -I tests/compare/bench.py [--servers bc,pw] [--runs 10]
python3 -I tests/compare/bench.py --servers bc,pw --context-only   # tool lists and instructions only
```

Besides the tool list, it counts the server's `instructions` (Claude Code
and opencode put them in the system prompt) and, per tool, the tokens of
its description and of its input schema.

`footprint.py` measures the README's "Advantages of a native Rust binary":
how long each server takes to answer `initialize`, the memory of its own
processes (browsers left out; npm's and Selenium's `chromedriver` apart)
and the first navigation, for `bc`, `pw`, `cd` (with `npx`, then with
`node`) and `se`:

```sh
make build && python3 -I tests/compare/footprint.py [--servers bc,pw] [--runs 10]
```

`agent_eval/` gives the same goals to real agents, with browser-control-mcp
as their only tool server, and checks the page afterwards. The other
harnesses call tools through adapters, so no model reads a description
there; here one does, and a description that got worse shows as a goal an
agent no longer reaches, or reaches with more calls and error replies:

```sh
make build && cp target/release/browser-control-mcp /tmp/bc-baseline   # before a change
python3 -I tests/compare/agent_eval/run.py --bin /tmp/bc-baseline --label baseline
python3 -I tests/compare/agent_eval/run.py --label after --baseline tests/compare/results/agent-<time>-baseline/results.json
python3 -I tests/compare/agent_eval/run.py --only 'form|select' --runs 1   # some tasks, once
```

- Clients: `claude -p` (Claude Code; `--claude-model`, default `sonnet`)
  and `opencode2 run --standalone` (opencode 2; `--opencode-model`,
  default `google/antigravity-gemini-3.8-flash-medium`), each with
  browser-control-mcp as its only tool server and no other tool. The
  harness removes `ANTHROPIC_*` and `OPENCODE*` from their environment.
- `tasks.py`: 13 goals in plain words that name no tool, each with an
  oracle: a value read in the page after the agent finished, the tabs
  open, or words its final answer must hold.
- `proxy.py` and `relay.py`: the CLI starts `relay.py`, which pipes its
  stdio to a proxy the harness owns; the proxy owns the server and logs each
  tool call (name, error, reply size; not the arguments). CLIs kill their
  MCP servers when they exit (Claude Code: SIGINT, SIGTERM, then a kill),
  which would close the browser before the check: here only the relay
  dies, and the harness reads the page through the proxy, with calls the
  agent never sees.
- `run.py`: `--runs 3` per task and client, `--jobs 3` at once, then
  `results/agent-<time>-<label>/` (`report.md`, `results.json`, the call
  logs). With `--baseline`, the report compares per task and gives the
  verdict: total passes not lower, no task falling from all its runs to a
  third or less, calls and error replies at most 10 % higher.

It spends model tokens (78 short agent runs by default) and its outcome
varies from run to run: compare runs of the same CLIs and models only.

## Servers

| Key | Server | How it runs |
|---|---|---|
| `bc` | browser-control-mcp on Chrome | `target/release/browser-control-mcp --headless --tool-groups all` (`--bc-bin` for another) |
| `bc-ff` | browser-control-mcp on Firefox | the same, in an instance opened with `open_instance` |
| `pw` | Playwright MCP | `npx -y @playwright/mcp@<pinned>` with every capability |
| `cd` | chrome-devtools-mcp | `npx -y chrome-devtools-mcp@<pinned>` |
| `pp` | Puppeteer MCP | `npx -y @hisma/server-puppeteer@<pinned>`, Chrome from `--chrome` or `PATH` |
| `se` | Selenium MCP (the official one, SeleniumHQ/mcp) | `node <checkout>/bin/selenium-mcp.js --headless --allow-evaluate --caps pdf,storage,vision` (`--selenium-bin` for another checkout) |

The versions of the other servers are pinned in `run.py`: raising one is a
deliberate change, and its effect shows against the baseline.

The Selenium MCP server is not on npm yet (0.1.0, "v1 in progress"), and
its `package.json` takes `selenium-webdriver` from GitHub Packages, which
needs a token. A checkout at the pinned commit (`SELENIUM_COMMIT` in
`run.py`), with `selenium-webdriver` 4.54 from npm:

```sh
git clone https://github.com/SeleniumHQ/mcp ~/.local/share/selenium-mcp/mcp
cd ~/.local/share/selenium-mcp/mcp && git checkout de8899c && rm .npmrc
npm pkg set dependencies.selenium-webdriver=^4.54.0 && npm install --omit=dev
```

`run.py` skips `se` when there is no checkout. Selenium Manager downloads
the `chromedriver` of the installed Chrome on the first start, and sends
usage statistics to plausible.io once a day unless `SE_AVOID_STATS=true`,
which the Selenium adapter sets.

## Layout

- `mcp_client.py`: a stdio MCP client; times calls and measures replies in
  tokens with one rule for every server (text / 4, images as the API counts
  them).
- `siteserver.py` and `site/`: the test site, served on `127.0.0.1` and a
  free port (`localhost` on the same port is another origin), with
  endpoints for redirects, status codes, slow pages, posts, cookies, basic
  auth, downloads, a PDF and a CSP header.
- `adapters.py`: one adapter per server, mapping the operations scenarios
  use to that server's tools as an agent would use them (a snapshot to find
  a ref, then the action). An operation a server has no tool for raises
  `Unsupported`.
- `scenarios.py`: the tasks, from level 1 (what every agent does) to level 4
  (rare), each with its check.
- `bench.py`: the README's comparison table (see above).
- `footprint.py`: start time and memory of each server (see above).
- `agent_eval/`: real agents on the same site (see above).
- `run.py`: runs them and writes `results/<time>/` (`report.md`,
  `results.json`, `run.log`; git-ignored); with `baseline.json`, the report
  lists the scenarios whose status changed.

## Reading the report

`✓` the check passed, `✗` it did not (the note says why), `–` the server
has no tool for it, `E` the harness or the server broke. For passed
scenarios a cell gives calls / tokens. Checks read the page through the
server's own evaluation tool, outside the counted calls.

## Adding a scenario

Write it in `scenarios.py` with `@scenario(id, level, title)`: navigate, act
through the adapter (`a.click(a.find(T(role, name, css)))`), then check the
page with `a.value(expression)` or `eventually(...)`. If it needs an
operation no adapter has, add it to `Adapter` (raising `Unsupported`) and to
the adapters of the servers that can do it. Run it on every server before
trusting its result.
