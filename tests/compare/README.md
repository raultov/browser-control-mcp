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

## Servers

| Key | Server | How it runs |
|---|---|---|
| `bc` | browser-control-mcp on Chrome | `target/release/browser-control-mcp --headless --tool-groups all` (`--bc-bin` for another) |
| `bc-ff` | browser-control-mcp on Firefox | the same, in an instance opened with `open_instance` |
| `pw` | Playwright MCP | `npx -y @playwright/mcp@<pinned>` with every capability |
| `cd` | chrome-devtools-mcp | `npx -y chrome-devtools-mcp@<pinned>` |
| `pp` | Puppeteer MCP | `npx -y @hisma/server-puppeteer@<pinned>`, Chrome from `--chrome` or `PATH` |

The versions of the other servers are pinned in `run.py`: raising one is a
deliberate change, and its effect shows against the baseline.

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
