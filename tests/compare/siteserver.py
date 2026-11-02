"""The test site: the pages under `site/` plus endpoints a static server
cannot give (redirects, status codes, slow answers, form posts, cookies,
basic auth, downloads, a PDF, a CSP header). It runs in a thread of the
benchmark on 127.0.0.1 and a free port; `localhost` on the same port is
another origin, for frames and requests across origins.
"""

import base64
import json
import os
import threading
import time
import urllib.parse
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

SITE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "site")

# A one-page PDF saying "PDF page".
PDF = (b"%PDF-1.4\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n"
       b"2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n"
       b"3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 144]/Contents 4 0 R"
       b"/Resources<</Font<</F1 5 0 R>>>>>>endobj\n"
       b"4 0 obj<</Length 41>>stream\nBT /F1 24 Tf 40 70 Td (PDF page) Tj ET\nendstream endobj\n"
       b"5 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\n"
       b"trailer<</Root 1 0 R>>\n%%EOF\n")


class Handler(SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=SITE, **kwargs)

    def log_message(self, *args):
        pass

    def end_headers(self):
        # Every answer fresh: a cached page would hide a request.
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def send_body(self, status, body, content_type="text/html; charset=utf-8", headers=()):
        data = body.encode() if isinstance(body, str) else body
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        for name, value in headers:
            self.send_header(name, value)
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    def redirect(self, status, to):
        self.send_response(status)
        self.send_header("Location", to)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        url = urllib.parse.urlsplit(self.path)
        query = dict(urllib.parse.parse_qsl(url.query))
        path = url.path
        if path == "/redirect":
            return self.redirect(302, query.get("to", "/basic.html"))
        if path == "/chain":
            return self.redirect(301, "/chain2")
        if path == "/chain2":
            return self.redirect(302, "/second.html")
        if path.startswith("/status/"):
            code = int(path.rsplit("/", 1)[1])
            return self.send_body(code, f"<!doctype html><title>Status {code}</title><h1>Status {code}</h1>")
        if path == "/slow":
            time.sleep(int(query.get("ms", "2000")) / 1000)
            return self.send_body(200, "<!doctype html><title>Slow</title><h1>Slow page</h1>")
        if path == "/setcookie":
            cookies = [("Set-Cookie", "session=abc123; Path=/; HttpOnly"), ("Set-Cookie", "theme=dark; Path=/")]
            return self.send_body(200, "<!doctype html><title>Cookies</title><h1>Cookies set</h1>", headers=cookies)
        if path == "/auth":
            expected = "Basic " + base64.b64encode(b"user:pass").decode()
            if self.headers.get("Authorization") != expected:
                return self.send_body(401, "<!doctype html><title>Unauthorized</title>",
                                      headers=[("WWW-Authenticate", 'Basic realm="bench"')])
            return self.send_body(200, "<!doctype html><title>Authorized</title><h1>Welcome</h1>")
        if path == "/download":
            return self.send_body(200, "a,b\n1,2\n", "text/csv", [("Content-Disposition", "attachment; filename=data.csv")])
        if path == "/data.csv":
            return self.send_body(200, "a,b\n1,2\n", "text/csv")
        if path == "/doc.pdf":
            return self.send_body(200, PDF, "application/pdf")
        if path == "/csp":
            body = ("<!doctype html><title>CSP</title><h1>CSP page</h1>"
                    "<script>document.title = 'CSP ran'</script>")
            return self.send_body(200, body, headers=[("Content-Security-Policy", "script-src 'none'")])
        if path == "/api/echo":
            return self.send_body(200, json.dumps({"method": "GET", "query": query}), "application/json")
        if path == "/":
            return self.redirect(302, "/basic.html")
        return super().do_GET()

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length).decode("utf-8", "replace")
        path = urllib.parse.urlsplit(self.path).path
        if path == "/api/echo":
            answer = {"method": "POST", "body": body, "type": self.headers.get("Content-Type")}
            return self.send_body(200, json.dumps(answer), "application/json")
        if path == "/post":
            fields = urllib.parse.parse_qsl(body, keep_blank_values=True)
            items = "".join(f"<li>{urllib.parse.quote(k)}={urllib.parse.quote(v)}</li>" for k, v in fields)
            page = f"<!doctype html><title>Posted</title><h1>Posted</h1><ul id='fields'>{items}</ul>"
            return self.send_body(200, page)
        return self.send_body(404, "not found", "text/plain")


class Site:
    """The site, served until `stop`."""

    def __init__(self):
        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.httpd.daemon_threads = True
        self.port = self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)
        self.thread.start()

    def url(self, path):
        return f"http://127.0.0.1:{self.port}{path}"

    def other_origin(self, path):
        return f"http://localhost:{self.port}{path}"

    def stop(self):
        self.httpd.shutdown()
