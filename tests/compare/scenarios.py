"""The scenarios: one task each, from what agents do most (level 1) to
what they rarely need (level 4), with a check of the page's state (or of
the reply, when the task is to report something) that decides whether the
server did it.

A scenario takes the adapter `a` and the context `c` (the site, the files
to upload) and returns nothing on success; it raises `Fail` (did not work)
or `Unsupported` (the server has no tool for it).
"""

import os
import re
import time

from adapters import Fail, T, Unsupported

SCENARIOS = []


def scenario(sid, level, title):
    def register(fn):
        SCENARIOS.append({"id": sid, "level": level, "title": title, "run": fn})
        return fn
    return register


def expect(condition, message):
    if not condition:
        raise Fail(message)


def eventually(a, expression, expected, seconds=5.0):
    """Waits (uncounted) until `expression` is `expected` in the page: for
    checks after a navigation, which may still be committing."""
    deadline = time.time() + seconds
    seen = None
    while time.time() < deadline:
        try:
            seen = a.value(expression)
        except Fail:
            seen = None
        if seen == expected:
            return
        time.sleep(0.2)
    raise Fail(f"{expression} is {seen!r}, expected {expected!r}")


def mentions(result, *words):
    text = result.text.lower()
    return all(word.lower() in text for word in words)


# ---- level 1: what every agent does ---------------------------------------

@scenario("nav.basic", 1, "Navigate to a page")
def _(a, c):
    a.goto(c.url("/basic.html"))
    expect(a.value("document.title") == "Basic", "title is not Basic")


@scenario("read.snapshot", 1, "Read the page's text and headings")
def _(a, c):
    a.goto(c.url("/basic.html"))
    text = a.snap().text
    expect("Some text to read" in text, "the paragraph is missing")
    expect("Basic page" in text, "the heading is missing")


@scenario("click.button", 1, "Click a button")
def _(a, c):
    a.goto(c.url("/basic.html"))
    a.click(a.find(T("button", "Count", "#count")))
    expect(a.value("window.__count") == 1, "the handler did not run once")


@scenario("click.link", 1, "Follow a link")
def _(a, c):
    a.goto(c.url("/basic.html"))
    a.click(a.find(T("link", "Go to second", "a[href='/second.html']")))
    eventually(a, "document.title", "Second")


@scenario("fill.text", 1, "Type into a labelled field")
def _(a, c):
    a.goto(c.url("/form.html"))
    a.fill(a.find(T("textbox", "Full name", "#name")), "Ada Lovelace")
    expect(a.value("document.getElementById('name').value") == "Ada Lovelace", "the field does not hold the text")


@scenario("fill.replace", 1, "Replace a field's value")
def _(a, c):
    a.goto(c.url("/form.html"))
    a.fill(a.find(T("textbox", "City", "#city")), "Paris")
    expect(a.value("document.getElementById('city').value") == "Paris", "the old value is still there")


@scenario("select.native", 1, "Pick a <select> option by its label")
def _(a, c):
    a.goto(c.url("/form.html"))
    a.select(a.find(T("combobox", "Country", "#country")), "France", "fr")
    expect(a.value("document.getElementById('country').value") == "fr", "France is not selected")


@scenario("check.checkbox", 1, "Tick a checkbox")
def _(a, c):
    a.goto(c.url("/form.html"))
    a.click(a.find(T("checkbox", "I accept the terms", "input[name=terms]")))
    expect(a.value("document.querySelector('input[name=terms]').checked") is True, "not ticked")


@scenario("check.radio", 1, "Choose a radio button")
def _(a, c):
    a.goto(c.url("/form.html"))
    a.click(a.find(T("radio", "Pro", "input[value=pro]")))
    expect(a.value("document.querySelector('input[value=pro]').checked") is True, "Pro is not chosen")


@scenario("form.submit", 1, "Fill and submit a form (POST)")
def _(a, c):
    a.goto(c.url("/form.html"))
    snapshot = a.snap().text if a.snapshot_pattern else None
    a.fill(a.find(T("textbox", "Full name", "#name"), snapshot), "Grace")
    a.click(a.find(T("button", "Send", "button[type=submit]"), snapshot))
    eventually(a, "document.title", "Posted")
    expect("name=Grace" in a.value("document.getElementById('fields').textContent"), "the post lacks the name")


@scenario("press.enter", 1, "Submit with Enter")
def _(a, c):
    a.goto(c.url("/keys.html"))
    field = a.find(T("textbox", "Search", "#q"))
    a.fill(field, "hello")
    a.press("Enter", field)
    eventually(a, "document.getElementById('out').textContent", "submitted hello", 3)


@scenario("wait.text", 1, "Wait for text that appears later")
def _(a, c):
    a.goto(c.url("/dynamic.html"))
    a.click(a.find(T("button", "Start", "button:nth-of-type(1)")))
    a.wait_text("Loaded result")
    expect("Loaded result" in a.value("document.body.innerText"), "the text is not there")


@scenario("shot.viewport", 1, "Take a screenshot")
def _(a, c):
    a.goto(c.url("/basic.html"))
    result = a.shot()
    expect(result.images and result.images[0], f"no image: {result.short()}")


@scenario("console.read", 1, "Read console messages of every level")
def _(a, c):
    a.goto(c.url("/observe.html"))
    a.click(a.find(T("button", "Log", "button:nth-of-type(1)")))
    text = a.console().text
    for word in ("bench log", "bench warn", "bench error"):
        expect(word in text, f"{word!r} is missing")


@scenario("network.read", 1, "List the page's requests")
def _(a, c):
    a.goto(c.url("/observe.html"))
    a.click(a.find(T("button", "Post", "button:nth-of-type(3)")))
    time.sleep(0.5)
    expect("/api/echo" in a.network().text, "the fetch is not listed")


@scenario("eval.object", 1, "Evaluate JavaScript returning an object")
def _(a, c):
    a.goto(c.url("/basic.html"))
    result = a.ok(a.eval_raw("({answer: 42, list: [1, 2, 3]})"), "evaluate")
    expect("42" in result.text and "answer" in result.text, f"the object is not in the reply: {result.short()}")


@scenario("tabs.new", 1, "Follow a link that opens a new tab")
def _(a, c):
    a.goto(c.url("/tabs.html"))
    a.click(a.find(T("link", "Open second in new tab", "a[target=_blank]")))
    time.sleep(1)
    a.switch_tab("second.html")
    eventually(a, "document.title", "Second")


# ---- level 2: common -------------------------------------------------------

@scenario("nav.redirect", 2, "Report the final URL after redirects")
def _(a, c):
    result = a.goto(c.url("/chain"))
    eventually(a, "document.title", "Second")
    expect("second.html" in result.text, f"the reply does not name the final URL: {result.short()}")


@scenario("nav.back", 2, "Go back in history")
def _(a, c):
    a.goto(c.url("/basic.html"))
    a.goto(c.url("/second.html"))
    a.back()
    eventually(a, "document.title", "Basic")


@scenario("read.form_names", 2, "Snapshot names form controls by their labels")
def _(a, c):
    a.goto(c.url("/form.html"))
    text = a.snap().text
    wanted = [T("textbox", "Full name", ""), T("textbox", "Email", ""), T("textbox", "Message", ""),
              T("combobox", "Country", ""), T("checkbox", "I accept the terms", ""),
              T("radio", "Pro", ""), T("button", "Send", "")]
    missing = []
    for target in wanted:
        try:
            a.find(target, text)
        except Fail:
            missing.append(repr(target))
    expect(not missing, f"missing: {', '.join(missing)}")


@scenario("read.states", 2, "Snapshot shows checked, disabled and expanded")
def _(a, c):
    a.goto(c.url("/states.html"))
    text = a.snap().text
    line = lambda name: next((l for l in text.splitlines() if f'"{name}"' in l), "")
    expect("checked" in line("Remember me"), f"checked is missing: {line('Remember me')!r}")
    expect("disabled" in line("Disabled action"), f"disabled is missing: {line('Disabled action')!r}")
    expect("expanded" in line("Menu") or "expandable" in line("Menu"), f"expanded is missing: {line('Menu')!r}")


@scenario("read.hidden", 2, "Snapshot leaves out hidden elements")
def _(a, c):
    a.goto(c.url("/states.html"))
    expect("Hidden button" not in a.snap().text, "a display:none button is listed")


@scenario("fill.textarea", 2, "Type several lines into a textarea")
def _(a, c):
    a.goto(c.url("/form.html"))
    a.fill(a.find(T("textbox", "Message", "#msg")), "line one\nline two")
    expect(a.value("document.getElementById('msg').value") == "line one\nline two", "the lines are not kept")


@scenario("fill.date", 2, "Set a date input")
def _(a, c):
    a.goto(c.url("/form.html"))
    a.fill(a.find(T("textbox", "Start date", "#start")), "2026-10-31")
    expect(a.value("document.getElementById('start').value") == "2026-10-31", "the date is not set")


@scenario("fill.unicode", 2, "Type accents, emoji and other scripts")
def _(a, c):
    text = "Ñandú 🐦 日本語 مرحبا"
    a.goto(c.url("/text.html"))
    a.fill(a.find(T("textbox", "Unicode field", "#t")), text)
    expect(a.value("document.getElementById('t').value") == text, "the text changed on the way")


@scenario("hover.menu", 2, "Open a hover menu and click its item")
def _(a, c):
    a.goto(c.url("/widgets.html"))
    a.hover(a.find(T("button", "Account", ".menu button")))
    a.click(a.find(T("link", "Settings", ".sub a")))
    expect(a.value("document.getElementById('out').textContent") == "settings opened", "the item was not clicked")


@scenario("widget.tabs", 2, "Switch an ARIA tab")
def _(a, c):
    a.goto(c.url("/widgets.html"))
    a.click(a.find(T("tab", "Details", "#t2")))
    expect(a.value("document.getElementById('p2').hidden") is False, "the panel did not open")


@scenario("widget.listbox", 2, "Pick from a custom (ARIA) dropdown")
def _(a, c):
    a.goto(c.url("/widgets.html"))
    a.click(a.find(T("button", "Choose fruit", "#dd")))
    a.click(a.find(T("option", "Banana", "#lb li:nth-child(2)")))
    expect(a.value("window.__fruit") == "Banana", "Banana was not picked")


@scenario("dialog.alert", 2, "Close an alert a click opened")
def _(a, c):
    a.goto(c.url("/dialogs.html"))
    try:
        a.click(a.find(T("button", "Alert", "button:nth-of-type(1)")))
    except Fail:
        pass  # Some servers report the open dialog as the click's error.
    a.dialog(True)
    expect(a.value("document.title") == "Dialogs", "the page does not answer after the alert")


@scenario("dialog.confirm", 2, "Accept a confirm")
def _(a, c):
    a.goto(c.url("/dialogs.html"))
    try:
        a.click(a.find(T("button", "Confirm", "button:nth-of-type(2)")))
    except Fail:
        pass
    a.dialog(True)
    eventually(a, "document.getElementById('out').textContent", "confirmed", 3)


@scenario("dialog.prompt", 2, "Answer a prompt")
def _(a, c):
    a.goto(c.url("/dialogs.html"))
    try:
        a.click(a.find(T("button", "Prompt", "button:nth-of-type(3)")))
    except Fail:
        pass
    a.dialog(True, "Bench")
    eventually(a, "document.getElementById('out').textContent", "name=Bench", 3)


@scenario("wait.gone", 2, "Wait for text to go away")
def _(a, c):
    a.goto(c.url("/dynamic.html"))
    a.click(a.find(T("button", "Remove", "button:nth-of-type(2)")))
    a.wait_gone("Will disappear")
    expect(a.value("!!document.getElementById('gone')") is False, "the text is still there")


@scenario("settle.fetch", 2, "Read the result of a click that fetches, without waiting")
def _(a, c):
    a.goto(c.url("/dynamic.html"))
    a.click(a.find(T("button", "Fetch slowly", "button:nth-of-type(5)")))
    seen = a.value("document.getElementById('out').textContent")
    expect(seen == "fetched", f"right after the click the page shows {seen!r}")


@scenario("dynamic.spawn", 2, "Click a button that appears after a click")
def _(a, c):
    a.goto(c.url("/dynamic.html"))
    a.click(a.find(T("button", "Spawn", "button:nth-of-type(3)")))
    try:
        a.wait_text("Appeared")
    except Unsupported:
        time.sleep(1)
    a.click(a.find(T("button", "Appeared", "body > button:last-of-type")))
    expect(a.value("window.__appeared") == 1, "the new button was not clicked")


@scenario("scroll.nested", 2, "Click a button inside a scrolled container")
def _(a, c):
    a.goto(c.url("/scroll.html"))
    a.click(a.find(T("button", "Deep button", "#box button")))
    expect(a.value("window.__deep") == 1, "not clicked")


@scenario("scroll.far", 2, "Click a button far below, under a sticky header")
def _(a, c):
    a.goto(c.url("/scroll.html"))
    a.click(a.find(T("button", "Far button", "body > button")))
    expect(a.value("window.__far") == 1, "not clicked")


@scenario("upload.single", 2, "Upload a file")
def _(a, c):
    a.goto(c.url("/upload.html"))
    a.upload(a.find(T("button", "Attachment", "#one")), [c.files[0]])
    expect(a.value("window.__one") == ["a.txt:5"], f"window.__one is {a.value('window.__one')!r}")


@scenario("console.exception", 2, "See an uncaught exception")
def _(a, c):
    a.goto(c.url("/observe.html"))
    a.click(a.find(T("button", "Throw", "button:nth-of-type(2)")))
    time.sleep(0.3)
    text = a.console().text
    expect("boom" in text or "null" in text, "the exception is not reported")


@scenario("network.body", 2, "Read a request's POST body")
def _(a, c):
    a.goto(c.url("/observe.html"))
    a.click(a.find(T("button", "Post", "button:nth-of-type(3)")))
    time.sleep(0.5)
    expect("bench" in a.request_body("/api/echo").text, "the body is not shown")


@scenario("network.errors", 2, "See failed requests and their status")
def _(a, c):
    a.goto(c.url("/observe.html"))
    snapshot = a.snap().text if a.snapshot_pattern else None
    a.click(a.find(T("button", "Fail500", "button:nth-of-type(4)"), snapshot))
    a.click(a.find(T("button", "Refused", "button:nth-of-type(5)"), snapshot))
    time.sleep(0.8)
    text = a.network().text
    expect("500" in text, "the 500 is not shown")
    expect("refused" in text.lower() or "failed" in text.lower(), "the refused request is not shown as failed")


@scenario("shot.full", 2, "Take a full-page screenshot of a long page")
def _(a, c):
    a.goto(c.url("/scroll.html"))
    result = a.shot(full=True)
    dims = result.images[0] if result.images else None
    expect(dims and dims[1] > 3000, f"the image is {dims}")


@scenario("shot.element", 2, "Take a screenshot of one element")
def _(a, c):
    a.goto(c.url("/basic.html"))
    result = a.shot(handle=a.find(T("button", "Count", "#count")))
    dims = result.images[0] if result.images else None
    expect(dims and dims[0] < 300, f"the image is {dims}")


@scenario("eval.promise", 2, "Evaluate an expression that returns a promise")
def _(a, c):
    a.goto(c.url("/basic.html"))
    result = a.ok(a.eval_raw("fetch('/api/echo').then(r => r.status)"), "evaluate")
    expect("200" in result.text, f"the promise was not awaited: {result.short()}")


@scenario("eval.throw", 2, "An expression that throws reports the error")
def _(a, c):
    a.goto(c.url("/basic.html"))
    result = a.eval_raw("(() => { throw new Error('bench boom') })()")
    expect("bench boom" in result.text, f"the error is not reported: {result.short()}")


@scenario("frames.same", 2, "Click a button in a same-origin iframe")
def _(a, c):
    a.goto(c.url("/frames.html"))
    time.sleep(0.5)
    a.click(a.find(T("button", "Inner button same", "#same")))
    eventually(a, "window.__msgs.join()", "clicked same", 3)


@scenario("frames.cross", 2, "Click a button in an iframe of another origin")
def _(a, c):
    a.goto(c.url("/frames.html"))
    time.sleep(0.5)
    a.click(a.find(T("button", "Inner button cross", "#cross")))
    eventually(a, "window.__msgs.join()", "clicked cross", 3)


@scenario("spa.history", 2, "Navigate a single-page app and go back")
def _(a, c):
    a.goto(c.url("/spa.html"))
    a.click(a.find(T("link", "About", "nav a")))
    eventually(a, "document.title", "SPA about", 3)
    a.back()
    eventually(a, "document.title", "SPA home", 3)


@scenario("cookies.read", 2, "Read cookies, HttpOnly included")
def _(a, c):
    a.goto(c.url("/setcookie"))
    text = a.cookies().text
    expect("session" in text and "theme" in text, "the cookies are not listed")


# ---- level 3: less common --------------------------------------------------

@scenario("click.double", 3, "Double-click")
def _(a, c):
    a.goto(c.url("/widgets.html"))
    a.click(a.find(T("button", "Double me", "#dbl")), double=True)
    expect(a.value("document.getElementById('out').textContent") == "double clicked", "no dblclick")


@scenario("click.right", 3, "Right-click (context menu)")
def _(a, c):
    a.goto(c.url("/widgets.html"))
    a.click(a.find(T("button", "Right click me", "#ctx")), right=True)
    expect(a.value("document.getElementById('out').textContent") == "context menu", "no contextmenu")


@scenario("fill.range", 3, "Set a range input")
def _(a, c):
    a.goto(c.url("/form.html"))
    a.fill(a.find(T("slider", "Volume", "#volume")), "70")
    expect(a.value("document.getElementById('volume').value") == "70", "the value is not 70")


@scenario("fill.editable", 3, "Type into a contenteditable editor")
def _(a, c):
    a.goto(c.url("/widgets.html"))
    a.fill(a.find(T("textbox", "Notes editor", "#editor")), "Hello rich")
    expect(a.value("document.getElementById('editor').textContent") == "Hello rich", "the editor does not hold it")


@scenario("keys.select_all", 3, "Select all with Ctrl+A and type over it")
def _(a, c):
    a.goto(c.url("/keys.html"))
    field = a.find(T("textbox", "Search", "#q"))
    a.press("Control+a", field)
    a.press("x")
    held = a.value("document.getElementById('q').value")
    expect(held == "x", f"the field holds {held!r}")


@scenario("keys.escape", 3, "Press Escape")
def _(a, c):
    a.goto(c.url("/keys.html"))
    a.press("Escape")
    expect(a.value("document.getElementById('out').textContent") == "escaped", "the page did not get it")


@scenario("upload.multiple", 3, "Upload several files")
def _(a, c):
    a.goto(c.url("/upload.html"))
    a.upload(a.find(T("button", "Photos", "#many")), c.files[1:3])
    expect(a.value("window.__many") == ["p1.jpg", "p2.jpg"], f"window.__many is {a.value('window.__many')!r}")


@scenario("drag.html5", 3, "Drag and drop (HTML5)")
def _(a, c):
    a.goto(c.url("/drag.html"))
    snapshot = a.snap().text if a.snapshot_pattern else None
    source = a.find(T("generic", "Card", "#src"), snapshot) if a.snapshot_pattern is None else _text_handle(a, snapshot, "Card")
    target = a.find(T("generic", "Drop zone", "#dst"), snapshot) if a.snapshot_pattern is None else _text_handle(a, snapshot, "Drop zone")
    a.drag(source, target)
    expect(a.value("window.__dropped") == "card", "nothing dropped")


def _text_handle(a, snapshot, text):
    """The handle of a line whose text is `text` (a div with no role)."""
    for line in snapshot.splitlines():
        if text in line:
            found = re.search(r"\[ref=(\w+)\]", line) or re.search(r"uid=(\S+)", line)
            if found:
                return found.group(1)
    raise Fail(f"{text!r} has no handle in the snapshot")


@scenario("frames.srcdoc", 3, "Click a button in a srcdoc iframe")
def _(a, c):
    a.goto(c.url("/frames.html"))
    time.sleep(0.5)
    a.click(a.find(T("button", "Srcdoc button", "#doc")))
    expect(a.value("window.__srcdoc") == 1, "not clicked")


@scenario("frames.cross_fill", 3, "Type into a field of an iframe of another origin")
def _(a, c):
    a.goto(c.url("/frames.html"))
    time.sleep(0.5)
    a.fill(a.find(T("textbox", "Inner field cross", "#cross")), "abc")
    eventually(a, "window.__msgs.slice(-1)[0]", "typed abc", 3)


@scenario("shadow.open", 3, "Click a button in an open shadow root")
def _(a, c):
    a.goto(c.url("/shadow.html"))
    a.click(a.find(T("button", "Open shadow button", "#open >>> button")))
    expect(a.value("window.__open") == 1, "not clicked")


@scenario("stale.ref", 3, "A ref of a re-rendered element is not silently clicked")
def _(a, c):
    a.goto(c.url("/dynamic.html"))
    snapshot = a.snap().text if a.snapshot_pattern else None
    old = a.find(T("button", "Item 2", "#list button"), snapshot)
    a.click(a.find(T("button", "Rerender", "button:nth-of-type(4)"), snapshot))
    try:
        a.click(old)
    except Fail:
        return  # Refused, saying why: fine.
    item = a.value("window.__item || null")
    expect(item == "new 2", f"the click on a stale ref answered success, and window.__item is {item!r}")


@scenario("read.big", 3, "Snapshot a page with 3000 table rows, then click the last")
def _(a, c):
    a.goto(c.url("/big.html"))
    a.click(a.find(T("button", "Edit 2999", "tbody tr:last-child button")))
    expect(a.value("window.__last") == 1, "not clicked")


@scenario("busy.click", 3, "Click on a page that changes every 30 ms (in under 3 s)")
def _(a, c):
    a.goto(c.url("/anim.html"))
    start = time.time()
    a.click(a.find(T("button", "Busy page button", "button")))
    took = time.time() - start
    expect(a.value("window.__busy") == 1, "not clicked")
    expect(took < 3, f"it took {took:.1f} s")


@scenario("nav.slow", 3, "Navigate to a page that answers after 3 s")
def _(a, c):
    a.goto(c.url("/slow?ms=3000"))
    eventually(a, "document.title", "Slow")


@scenario("nav.status", 3, "Say that a page answered 404")
def _(a, c):
    result = a.goto(c.url("/status/404"))
    expect("404" in result.text, f"the reply does not say 404: {result.short()}")


@scenario("eval.await", 3, "Evaluate code with top-level await")
def _(a, c):
    a.goto(c.url("/basic.html"))
    result = a.ok(a.eval_statements("const r = await fetch('/api/echo'); r.status"), "evaluate")
    expect("200" in result.text, f"the value is not 200: {result.short()}")


# ---- level 4: rare ---------------------------------------------------------

@scenario("shadow.closed", 4, "Click a button in a closed shadow root")
def _(a, c):
    a.goto(c.url("/shadow.html"))
    a.click(a.find(T("button", "Closed shadow button", "#closed button")))
    expect(a.value("window.__closed") == 1, "not clicked")


@scenario("read.invalid", 4, "Snapshot says a field is invalid, and why")
def _(a, c):
    a.goto(c.url("/states.html"))
    text = a.snap().text
    line = next((l for l in text.splitlines() if '"Zip code"' in l), "")
    expect("invalid" in line, f"invalid is missing: {line!r}")
    expect("Zip must be digits" in text, "the description is missing")


@scenario("nav.download", 4, "Say that a URL is a download")
def _(a, c):
    a.goto(c.url("/basic.html"))
    try:
        result = a.goto(c.url("/download"))
    except Fail as error:
        expect("download" in str(error).lower(), f"failed without saying why: {error}")
        return
    expect("download" in result.text.lower(), f"the reply does not say download: {result.short()}")


@scenario("nav.pdf", 4, "Open a PDF")
def _(a, c):
    start = time.time()
    a.goto(c.url("/doc.pdf"))
    expect(time.time() - start < 15, "it took over 15 s")


@scenario("nav.auth", 4, "Say that a page needs HTTP authentication")
def _(a, c):
    # A reply with the status or an error naming the authentication will do
    try:
        said = a.goto(c.url("/auth")).text
    except Fail as error:
        said = str(error)
    expect("401" in said or "authentication" in said.lower(), f"it does not say so: {said[:200]}")


@scenario("console.csp", 4, "See a Content-Security-Policy violation")
def _(a, c):
    a.goto(c.url("/csp"))
    time.sleep(0.3)
    text = a.console().text
    expect("Content Security Policy" in text or "Content-Security-Policy" in text or "CSP" in text,
           "the violation is not reported")


@scenario("nav.beforeunload", 4, "Leave a page that asks before unloading")
def _(a, c):
    a.goto(c.url("/dialogs.html"))
    a.click(a.find(T("button", "Guard", "button:nth-of-type(4)")))
    try:
        try:
            a.goto(c.url("/second.html"))
        except Fail:
            a.dialog(True)
        eventually(a, "document.title", "Second", 5)
    finally:
        # The guard must not hold up the next scenarios.
        try:
            a.eval_raw("window.onbeforeunload = null")
        except Exception:
            pass


# ---- more of each level ----------------------------------------------------

@scenario("form.many", 2, "Fill several fields of a form in one go")
def _(a, c):
    a.goto(c.url("/form.html"))
    snapshot = a.snap().text if a.snapshot_pattern else None
    find = lambda role, name, css: a.find(T(role, name, css), snapshot)
    a.form([(find("textbox", "Full name", "#name"), "Linus", "textbox"),
            (find("combobox", "Country", "#country"), "Germany", "combobox"),
            (find("checkbox", "I accept the terms", "input[name=terms]"), "true", "checkbox"),
            (find("radio", "Pro", "input[value=pro]"), "true", "radio")])
    seen = a.value("[document.getElementById('name').value, document.getElementById('country').value, document.querySelector('input[name=terms]').checked, document.querySelector('input[value=pro]').checked]")
    expect(seen == ["Linus", "de", True, True], f"the form holds {seen!r}")


@scenario("read.select_options", 2, "Snapshot lists a <select>'s options")
def _(a, c):
    a.goto(c.url("/form.html"))
    text = a.snap().text
    expect("France" in text and "Germany" in text, "the options are not listed")


@scenario("select.multiple", 3, "Pick two options of a multiple <select>")
def _(a, c):
    a.goto(c.url("/form.html"))
    a.select_many(a.find(T("listbox", "Toppings", "#toppings")), ["Cheese", "Olives"])
    seen = a.value("Array.from(document.getElementById('toppings').selectedOptions).map(o => o.text)")
    expect(seen == ["Cheese", "Olives"], f"selected: {seen!r}")


@scenario("keys.tab", 3, "Move the focus with Tab")
def _(a, c):
    a.goto(c.url("/form.html"))
    a.press("Tab", a.find(T("textbox", "Full name", "#name")))
    seen = a.value("document.activeElement.name")
    expect(seen == "email", f"the focus is on {seen!r}")


@scenario("console.frame", 3, "See console messages of an iframe of another origin")
def _(a, c):
    a.goto(c.url("/frames.html"))
    time.sleep(0.8)
    text = a.console().text
    expect("inner loaded cross" in text, "the frame's message is missing")


@scenario("eval.large", 3, "Evaluate a 2 MB string without flooding the context")
def _(a, c):
    a.goto(c.url("/basic.html"))
    result = a.eval_raw("'x'.repeat(2000000)")
    expect(result.tokens < 60000, f"the reply is {result.tokens} tokens")


@scenario("storage.local", 3, "Read a localStorage item")
def _(a, c):
    a.goto(c.url("/basic.html"))
    a.value("(localStorage.setItem('bench-key', 'bench-value'), 1)")
    expect("bench-value" in a.storage_get("bench-key").text, "the value is not in the reply")


@scenario("emulate.dark", 3, "Emulate dark mode")
def _(a, c):
    a.goto(c.url("/basic.html"))
    a.dark()
    expect(a.value("matchMedia('(prefers-color-scheme: dark)').matches") is True, "the page is not dark")


@scenario("viewport.resize", 3, "Resize the viewport to a phone width")
def _(a, c):
    a.goto(c.url("/basic.html"))
    a.resize(500, 800)
    seen = a.value("innerWidth")
    expect(seen == 500, f"innerWidth is {seen}")


@scenario("route.mock", 4, "Mock a request's response")
def _(a, c):
    a.goto(c.url("/basic.html"))
    a.route("/api/echo", "mocked-bench")
    try:
        seen = a.value("fetch('/api/echo?fresh=' + Date.now()).then(r => r.text())")
    finally:
        a.unroute()
    expect(seen == "mocked-bench", f"the fetch got {seen!r}")


@scenario("network.offline", 4, "Go offline: a fetch fails, then works again")
def _(a, c):
    a.goto(c.url("/basic.html"))
    a.offline(True)
    try:
        seen = a.value("fetch('/api/echo?o=' + Date.now()).then(() => 'online', () => 'failed')")
    finally:
        a.offline(False)
    expect(seen == "failed", f"offline, the fetch {seen}")
    again = a.value("fetch('/api/echo?o=' + Date.now()).then(() => 'online', () => 'failed')")
    expect(again == "online", "the page is still offline")


@scenario("pdf.save", 4, "Save the page as a PDF")
def _(a, c):
    a.goto(c.url("/basic.html"))
    path = os.path.join(os.path.dirname(c.files[0]), "page.pdf")
    if os.path.exists(path):
        os.remove(path)
    result = a.pdf(path)
    named = re.search(r"(/\S+\.pdf)", result.text)
    written = path if os.path.exists(path) else (named.group(1) if named else None)
    expect(written and os.path.exists(written), f"no PDF written: {result.short()}")
    with open(written, "rb") as f:
        expect(f.read(5) == b"%PDF-", "the file is not a PDF")
