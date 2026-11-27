"""The tasks an agent is given: a goal in plain words that names no tool,
and an oracle that decides whether it was reached.

An oracle reads the page through the harness's own calls after the agent
finished (`("eval", expression, expected)`: the expression's value in the
active tab; `("tabs", [texts])`: every text in `list_tabs`), or, for a task
whose goal is to report something, checks the agent's final answer
(`("answer", [words])`: every word in it, ignoring case; none of them is in
the prompt).

They are chosen where a tool's description carries what the agent cannot
guess from its name and schema: a select is set with `fill_input`, several
fields at once with `fill_form`, refs into frames of another origin and
shadow roots, a cut snapshot searched with `find_in_snapshot`, top-level
`await`, dialogs, tabs a click opens, request bodies, console errors, the
wait after a click.
"""

TASKS = [
    {"id": "form.many",
     "prompt": "Open {url}/form.html. Fill in the form: Full name \"Linus\", Country \"Germany\", tick "
               "\"I accept the terms\" and choose the \"Pro\" plan. Do not submit it.",
     "oracle": ("eval", "[document.getElementById('name').value, document.getElementById('country').value, "
                        "document.querySelector('input[name=terms]').checked, "
                        "document.querySelector('input[value=pro]').checked]", ["Linus", "de", True, True])},
    {"id": "select.native",
     "prompt": "Open {url}/form.html and choose France as the country.",
     "oracle": ("eval", "document.getElementById('country').value", "fr")},
    {"id": "select.multiple",
     "prompt": "Open {url}/form.html and select exactly two toppings: Cheese and Olives.",
     "oracle": ("eval", "Array.from(document.getElementById('toppings').selectedOptions).map(o => o.text)",
                ["Cheese", "Olives"])},
    {"id": "frames.cross",
     "prompt": "Open {url}/frames.html and click the \"Inner button cross\" button.",
     "oracle": ("eval", "window.__msgs.includes('clicked cross')", True)},
    {"id": "shadow.open",
     "prompt": "Open {url}/shadow.html and click the \"Open shadow button\".",
     "oracle": ("eval", "window.__open === 1", True)},
    {"id": "read.big",
     "prompt": "Open {url}/big.html and click the Edit button of the last row of the table.",
     "oracle": ("eval", "window.__last === 1", True)},
    {"id": "eval.await",
     "prompt": "Open {url}/basic.html. From that page, with JavaScript, send a POST request to /api/echo whose "
               "body is the text \"zebra\", and tell me the exact value of the \"type\" field in the JSON the "
               "server answers.",
     "oracle": ("answer", ["text/plain"])},
    {"id": "dialog.prompt",
     "prompt": "Open {url}/dialogs.html, click \"Prompt\" and answer the question it asks with \"Bench\".",
     "oracle": ("eval", "document.getElementById('out').textContent", "name=Bench")},
    {"id": "tabs.new",
     "prompt": "Open {url}/tabs.html and follow the link \"Open second in new tab\". In the page it opens, "
               "follow the link \"Back to basic\".",
     "oracle": ("tabs", ["/tabs.html", "/basic.html"])},
    {"id": "network.body",
     "prompt": "Open {url}/observe.html, click \"Post\", and tell me the exact body of the request the page sent.",
     "oracle": ("answer", ["hello", "bench"])},
    {"id": "console.exception",
     "prompt": "Open {url}/observe.html, click \"Throw\" and tell me the error the page reported in its console.",
     "oracle": ("answer", ["boom"])},
    {"id": "scroll.far",
     "prompt": "Open {url}/scroll.html and click the \"Far button\".",
     "oracle": ("eval", "window.__far === 1", True)},
    {"id": "settle.fetch",
     "prompt": "Open {url}/dynamic.html, click \"Fetch slowly\" and tell me the text the page shows as the result.",
     "oracle": ("answer", ["fetched"])},
]
