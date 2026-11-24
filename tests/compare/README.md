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
```

`footprint.py` measures the README's "Advantages of a native Rust binary":
how long each server takes to answer `initialize`, the memory of its own
processes (browsers left out; npm's and Selenium's `chromedriver` apart)
and the first navigation, for `bc`, `pw`, `cd` (with `npx`, then with
`node`) and `se`:

```sh
make build && python3 -I tests/compare/footprint.py [--servers bc,pw] [--runs 10]
```

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
