"""One adapter per MCP server: the same operations, each with that server's
own tools, used the way an agent would use them.

Elements are named by `T(role, name, css)`. A server with a snapshot finds
its own handle there (a ref or uid: the snapshot is part of the task and is
counted); one without (Puppeteer) takes the CSS selector. An operation a
server has no tool for raises `Unsupported`; one that does not work raises
`Fail`. The harness's own checks (`value`) are not counted.
"""

import json
import re

from mcp_client import Server


class Fail(Exception):
    """The scenario did not work on this server."""


class Unsupported(Exception):
    """The server has no tool for an operation the scenario needs."""


class T:
    """An element: its accessible role and name, and a CSS selector."""

    def __init__(self, role, name, css):
        self.role, self.name, self.css = role, name, css

    def __repr__(self):
        return f'{self.role} "{self.name}"'


def js_value(expression):
    """An expression that evaluates `expression` (a promise is awaited) and
    wraps it so its value is found in any server's reply (JSON in a JSON
    string). Every server awaits the promise it returns."""
    return f"(async () => JSON.stringify({{__z: 1, __b: await ({expression})}}))()"


def parse_value(text):
    """The value [`js_value`] wrapped, from a reply's text."""
    decoder = json.JSONDecoder()
    at = text.find('"{\\"__z\\":1')
    if at >= 0:
        literal, _ = decoder.raw_decode(text[at:])
        return json.loads(literal).get("__b")
    at = text.find('{"__z":1')
    if at >= 0:
        found, _ = decoder.raw_decode(text[at:])
        return found.get("__b")
    raise Fail(f"no value in the reply: {text.strip()[:200]!r}")


class Adapter:
    name = "?"
    browser = "chrome"

    # Folders the server may read (MCP roots): where the upload files are.
    roots = []

    def __init__(self, command, env=None, timeout=60):
        self.server = Server(command, env, timeout, Adapter.roots)
        self.calls = []
        self.counting = True

    # -- bookkeeping --------------------------------------------------------

    def call(self, tool, args=None, timeout=None):
        result = self.server.call(tool, args or {}, timeout)
        if self.counting:
            self.calls.append(result)
        return result

    def ok(self, result, what):
        """`result`, or `Fail` when the server answered an error."""
        if result.is_error:
            raise Fail(f"{what}: {result.short(220)}")
        return result

    def totals(self):
        return {"calls": len(self.calls), "ms": sum(c.ms for c in self.calls),
                "tokens": sum(c.tokens for c in self.calls)}

    def value(self, expression):
        """The value of `expression` in the page: a check of the harness,
        not counted."""
        self.counting = False
        try:
            return parse_value(self.ok(self.eval_raw(js_value(expression)), "evaluate").text)
        finally:
            self.counting = True

    def close(self):
        self.server.close()

    # -- elements -----------------------------------------------------------

    snapshot_pattern = None

    def find(self, target, snapshot=None):
        """The server's handle on `target`, from `snapshot` (taken if not given)."""
        if self.snapshot_pattern is None:
            return target.css
        text = snapshot if snapshot is not None else self.snap().text
        pattern = self.snapshot_pattern.format(role=target.role, name=re.escape(target.name))
        found = re.search(pattern, text)
        if not found:
            raise Fail(f"{target!r} is not in the snapshot")
        return found.group(1)

    # -- operations (Unsupported unless a server has them) ------------------

    def _unsupported(self, what):
        raise Unsupported(what)

    def goto(self, url): self._unsupported("navigate")
    def back(self): self._unsupported("history back")
    def reload(self): self._unsupported("reload")
    def snap(self): self._unsupported("snapshot")
    def click(self, handle, double=False, right=False): self._unsupported("click")
    def fill(self, handle, text): self._unsupported("fill")
    def select(self, handle, label, value): self._unsupported("select")
    def hover(self, handle): self._unsupported("hover")
    def press(self, key, handle=None): self._unsupported("press a key")
    def drag(self, source, destination): self._unsupported("drag")
    def upload(self, handle, paths): self._unsupported("upload")
    def dialog(self, accept, text=None): self._unsupported("dialog")
    def wait_text(self, text): self._unsupported("wait for text")
    def wait_gone(self, text): self._unsupported("wait for text to go")
    def shot(self, full=False, handle=None): self._unsupported("screenshot")
    def console(self): self._unsupported("console")
    def network(self): self._unsupported("network")
    def request_body(self, url_part): self._unsupported("request body")
    def tabs(self): self._unsupported("tabs")
    def switch_tab(self, url_part): self._unsupported("switch tab")
    def cookies(self): self._unsupported("cookies")
    def eval_raw(self, expression): self._unsupported("evaluate")
    def eval_statements(self, code): self._unsupported("top-level await")
    def form(self, fields): self._unsupported("fill a form at once")
    def select_many(self, handle, labels): self._unsupported("select several options")
    def storage_get(self, key): self._unsupported("read localStorage")
    def route(self, path, body): self._unsupported("mock a request")
    def unroute(self): pass
    def dark(self): self._unsupported("emulate dark mode")
    def resize(self, width, height): self._unsupported("resize the viewport")
    def pdf(self, path): self._unsupported("save a PDF")
    def offline(self, on): self._unsupported("go offline")


class BrowserControl(Adapter):
    name = "browser-control"
    snapshot_pattern = r'- {role} "{name}"[^\n]*?\[ref=(\w+)\]'

    def __init__(self, command, env=None, firefox=False):
        super().__init__(command, env)
        self.extra = {}
        if firefox:
            self.browser = "firefox"
            self.name = "browser-control (Firefox)"
            opened = self.server.call("open_instance", {"browser": "firefox", "label": "ff", "headless": True})
            if opened.is_error:
                raise RuntimeError(f"Firefox did not start: {opened.short()}")
            self.extra = {"instance_id": "ff"}

    def bc(self, tool, args=None, timeout=None):
        return self.call(tool, {**self.extra, **(args or {})}, timeout)

    def goto(self, url): return self.ok(self.bc("navigate", {"url": url}), "navigate")
    def back(self): return self.ok(self.bc("navigate", {"history": "back"}), "back")
    def reload(self): return self.ok(self.bc("reload"), "reload")
    def snap(self): return self.ok(self.bc("snapshot"), "snapshot")

    def click(self, handle, double=False, right=False):
        args = {"selector": handle}
        if double:
            args["click_count"] = 2
        if right:
            args["button"] = "right"
        return self.ok(self.bc("click_element", args), "click")

    def fill(self, handle, text): return self.ok(self.bc("fill_input", {"selector": handle, "text": text}), "fill")
    def select(self, handle, label, value): return self.fill(handle, label)
    def hover(self, handle): return self.ok(self.bc("hover", {"selector": handle}), "hover")

    def press(self, key, handle=None):
        *held, key = key.split("+") if len(key) > 1 else [key]
        args = {"key": key}
        if held:
            args["modifiers"] = held
        if handle:
            args["selector"] = handle
        return self.ok(self.bc("press_key", args), "press")

    def drag(self, source, destination):
        return self.ok(self.bc("drag", {"from": source, "to": destination}), "drag")

    def upload(self, handle, paths):
        return self.ok(self.bc("upload_file", {"selector": handle, "paths": paths}), "upload")

    def dialog(self, accept, text=None):
        args = {"accept": accept}
        if text is not None:
            args["prompt_text"] = text
        return self.ok(self.bc("handle_dialog", args), "dialog")

    def wait_text(self, text): return self.ok(self.bc("wait_for", {"text": text}), "wait")
    def wait_gone(self, text): return self.ok(self.bc("wait_for", {"text_gone": text}), "wait gone")

    def shot(self, full=False, handle=None):
        args = {"full_page": full}
        if handle:
            args["selector"] = handle
        return self.ok(self.bc("capture_screenshot", args), "screenshot")

    def console(self): return self.ok(self.bc("get_console_logs"), "console")
    def network(self): return self.ok(self.bc("get_network_logs"), "network")

    def request_body(self, url_part):
        # Request bodies are redacted unless asked for (they may hold credentials).
        args = {"url_filter": url_part, "include_details": True, "include_sensitive_headers": True}
        return self.ok(self.bc("get_network_logs", args), "request details")

    def tabs(self): return self.ok(self.bc("list_tabs"), "tabs")

    def switch_tab(self, url_part):
        listed = json.loads(self.tabs().text)
        for tab in listed.get("tabs", []):
            if url_part in tab.get("url", ""):
                return self.ok(self.bc("switch_tab", {"tab_id": tab["tab_id"]}), "switch tab")
        raise Fail(f"no tab shows {url_part}")

    def cookies(self): return self.ok(self.bc("cookies", {"action": "get"}), "cookies")

    def form(self, fields):
        listed = [{"selector": handle, "value": value} for handle, value, _ in fields]
        return self.ok(self.bc("fill_form", {"fields": listed}), "fill_form")

    def storage_get(self, key):
        return self.ok(self.bc("web_storage", {"action": "get", "key": key, "show_values": True}), "web_storage")

    def route(self, path, body):
        return self.ok(self.bc("mock_route", {"url_pattern": f"*{path}*", "body": body, "content_type": "text/plain"}), "mock_route")

    def unroute(self):
        self.bc("remove_route")

    def dark(self):
        result = self.bc("emulate", {"color_scheme": "dark"})
        if result.is_error and "not available on Firefox" in result.text:
            raise Unsupported("emulate dark mode (Firefox: WebDriver BiDi has no media emulation)")
        return self.ok(result, "emulate")

    def resize(self, width, height):
        return self.ok(self.bc("emulate", {"width": width, "height": height}), "emulate")

    def pdf(self, path): return self.ok(self.bc("save_pdf", {"path": path}), "save_pdf")
    def offline(self, on): return self.ok(self.bc("set_offline", {"offline": on}), "set_offline")

    def find(self, target, snapshot=None):
        """As an agent does: a page whose snapshot was cut is searched with
        find_in_snapshot."""
        text = snapshot if snapshot is not None else self.snap().text
        try:
            return super().find(target, text)
        except Fail:
            if "truncated" not in text.lower() and "cut" not in text.lower():
                raise
        found = self.ok(self.bc("find_in_snapshot", {"text": target.name}), "find_in_snapshot")
        return super().find(target, found.text)
    def eval_raw(self, expression): return self.bc("evaluate_js", {"expression": expression})
    def eval_statements(self, code): return self.eval_raw(code)


class Playwright(Adapter):
    name = "playwright"
    snapshot_pattern = r'- {role} "{name}"[^\n]*?\[ref=(\w+)\]'

    def goto(self, url): return self.ok(self.call("browser_navigate", {"url": url}), "navigate")
    def back(self): return self.ok(self.call("browser_navigate_back"), "back")
    def snap(self): return self.ok(self.call("browser_snapshot"), "snapshot")

    def click(self, handle, double=False, right=False):
        args = {"target": handle, "element": handle}
        if double:
            args["doubleClick"] = True
        if right:
            args["button"] = "right"
        return self.ok(self.call("browser_click", args), "click")

    def fill(self, handle, text):
        return self.ok(self.call("browser_type", {"target": handle, "element": handle, "text": text}), "fill")

    def select(self, handle, label, value):
        args = {"target": handle, "element": handle, "values": [label]}
        return self.ok(self.call("browser_select_option", args), "select")

    def hover(self, handle):
        return self.ok(self.call("browser_hover", {"target": handle, "element": handle}), "hover")

    def press(self, key, handle=None):
        if handle:
            self.click(handle)
        return self.ok(self.call("browser_press_key", {"key": key}), "press")

    def drag(self, source, destination):
        args = {"startTarget": source, "startElement": source, "endTarget": destination, "endElement": destination}
        return self.ok(self.call("browser_drag", args), "drag")

    def upload(self, handle, paths):
        self.click(handle)
        return self.ok(self.call("browser_file_upload", {"paths": paths}), "upload")

    def dialog(self, accept, text=None):
        args = {"accept": accept}
        if text is not None:
            args["promptText"] = text
        return self.ok(self.call("browser_handle_dialog", args), "dialog")

    def wait_text(self, text): return self.ok(self.call("browser_wait_for", {"text": text}), "wait")
    def wait_gone(self, text): return self.ok(self.call("browser_wait_for", {"textGone": text}), "wait gone")

    def shot(self, full=False, handle=None):
        args = {"type": "png", "scale": "css", "fullPage": full}
        if handle:
            args.update({"target": handle, "element": handle})
            del args["fullPage"]
        return self.ok(self.call("browser_take_screenshot", args), "screenshot")

    def console(self): return self.ok(self.call("browser_console_messages", {"level": "debug"}), "console")
    def network(self): return self.ok(self.call("browser_network_requests", {"static": True}), "network")

    def request_body(self, url_part):
        listed = self.network().text
        for line in listed.splitlines():
            found = re.match(r"\s*(\d+)\. .*", line)
            if found and url_part in line:
                args = {"index": int(found.group(1)), "part": "request-body"}
                return self.ok(self.call("browser_network_request", args), "request details")
        raise Fail(f"no request to {url_part} listed")

    def tabs(self): return self.ok(self.call("browser_tabs", {"action": "list"}), "tabs")

    def switch_tab(self, url_part):
        for line in self.tabs().text.splitlines():
            found = re.match(r"\s*- (\d+):", line)
            if found and url_part in line:
                return self.ok(self.call("browser_tabs", {"action": "select", "index": int(found.group(1))}), "switch tab")
        raise Fail(f"no tab shows {url_part}")

    def cookies(self): return self.ok(self.call("browser_cookie_list"), "cookies")

    def form(self, fields):
        listed = [{"target": handle, "element": handle, "name": handle, "type": kind, "value": value}
                  for handle, value, kind in fields]
        return self.ok(self.call("browser_fill_form", {"fields": listed}), "fill_form")

    def select_many(self, handle, labels):
        args = {"target": handle, "element": handle, "values": labels}
        return self.ok(self.call("browser_select_option", args), "select")

    def storage_get(self, key): return self.ok(self.call("browser_localstorage_get", {"key": key}), "localStorage")

    def route(self, path, body):
        args = {"pattern": f"**{path}*", "body": body, "contentType": "text/plain"}
        return self.ok(self.call("browser_route", args), "route")

    def unroute(self): self.call("browser_unroute")
    def dark(self): return self.ok(self.call("browser_emulate_media", {"colorScheme": "dark"}), "emulate_media")
    def resize(self, width, height): return self.ok(self.call("browser_resize", {"width": width, "height": height}), "resize")
    def pdf(self, path): return self.ok(self.call("browser_pdf_save", {"filename": path}), "pdf")

    def offline(self, on):
        return self.ok(self.call("browser_network_state_set", {"state": "offline" if on else "online"}), "network state")
    def eval_raw(self, expression): return self.call("browser_evaluate", {"function": f"() => ({expression})"})


class DevTools(Adapter):
    name = "chrome-devtools"
    snapshot_pattern = r'uid=(\S+) {role} "{name}"'

    def __init__(self, command, env=None):
        super().__init__(command, env)
        self.page = 1

    def p(self, args=None):
        return {"pageId": self.page, **(args or {})}

    def goto(self, url):
        return self.ok(self.call("navigate_page", self.p({"type": "url", "url": url})), "navigate")

    def back(self): return self.ok(self.call("navigate_page", self.p({"type": "back"})), "back")
    def reload(self): return self.ok(self.call("navigate_page", self.p({"type": "reload"})), "reload")
    def snap(self): return self.ok(self.call("take_snapshot", self.p()), "snapshot")

    def click(self, handle, double=False, right=False):
        if right:
            raise Unsupported("right click")
        args = {"uid": handle}
        if double:
            args["dblClick"] = True
        return self.ok(self.call("click", self.p(args)), "click")

    def fill(self, handle, text): return self.ok(self.call("fill", self.p({"uid": handle, "value": text})), "fill")
    def select(self, handle, label, value): return self.fill(handle, label)
    def hover(self, handle): return self.ok(self.call("hover", self.p({"uid": handle})), "hover")

    def press(self, key, handle=None):
        if handle:
            self.click(handle)
        return self.ok(self.call("press_key", self.p({"key": key})), "press")

    def drag(self, source, destination):
        return self.ok(self.call("drag", self.p({"from_uid": source, "to_uid": destination})), "drag")

    def upload(self, handle, paths):
        return self.ok(self.call("upload_file", self.p({"uid": handle, "filePaths": paths})), "upload")

    def dialog(self, accept, text=None):
        args = {"action": "accept" if accept else "dismiss"}
        if text is not None:
            args["promptText"] = text
        return self.ok(self.call("handle_dialog", self.p(args)), "dialog")

    def wait_text(self, text): return self.ok(self.call("wait_for", self.p({"text": [text]})), "wait")

    def shot(self, full=False, handle=None):
        args = {"fullPage": full}
        if handle:
            args = {"uid": handle}
        return self.ok(self.call("take_screenshot", self.p(args)), "screenshot")

    def console(self): return self.ok(self.call("list_console_messages", self.p()), "console")
    def network(self): return self.ok(self.call("list_network_requests", self.p()), "network")

    def request_body(self, url_part):
        for line in self.network().text.splitlines():
            found = re.match(r"\s*reqid=(\d+) ", line)
            if found and url_part in line:
                return self.ok(self.call("get_network_request", self.p({"reqid": int(found.group(1))})), "request details")
        raise Fail(f"no request to {url_part} listed")

    def tabs(self): return self.ok(self.call("list_pages"), "tabs")

    def switch_tab(self, url_part):
        for line in self.tabs().text.splitlines():
            found = re.match(r"\s*(\d+): ", line)
            if found and url_part in line:
                self.page = int(found.group(1))
                return self.ok(self.call("select_page", {"pageId": self.page}), "switch tab")
        raise Fail(f"no tab shows {url_part}")

    def eval_raw(self, expression):
        return self.call("evaluate_script", self.p({"function": f"() => ({expression})"}))

    def form(self, fields):
        listed = [{"uid": handle, "value": value} for handle, value, _ in fields]
        return self.ok(self.call("fill_form", self.p({"elements": listed})), "fill_form")

    def dark(self): return self.ok(self.call("emulate", self.p({"colorScheme": "dark"})), "emulate")

    def resize(self, width, height):
        return self.ok(self.call("resize_page", self.p({"width": width, "height": height})), "resize")

    def offline(self, on):
        args = {"networkConditions": "Offline"} if on else {}
        return self.ok(self.call("emulate", self.p(args)), "emulate")


class Puppeteer(Adapter):
    name = "puppeteer"

    def goto(self, url): return self.ok(self.call("puppeteer_navigate", {"url": url}), "navigate")

    def click(self, handle, double=False, right=False):
        if double or right:
            raise Unsupported("double or right click")
        return self.ok(self.call("puppeteer_click", {"selector": handle}), "click")

    def fill(self, handle, text): return self.ok(self.call("puppeteer_fill", {"selector": handle, "value": text}), "fill")

    def select(self, handle, label, value):
        return self.ok(self.call("puppeteer_select", {"selector": handle, "value": value}), "select")

    def hover(self, handle): return self.ok(self.call("puppeteer_hover", {"selector": handle}), "hover")

    def shot(self, full=False, handle=None):
        if full:
            raise Unsupported("full-page screenshot")
        args = {"name": "bench", "encoded": False}
        if handle:
            args["selector"] = handle
        return self.ok(self.call("puppeteer_screenshot", args), "screenshot")

    def eval_raw(self, expression): return self.call("puppeteer_evaluate", {"script": expression})
    def eval_statements(self, code): return self.eval_raw(code)
